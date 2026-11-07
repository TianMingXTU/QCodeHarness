//! 任务级恢复：把一个任务的快照读回成"可以接着跑"的状态。
//!
//! 任务级恢复与会话级恢复（`AgentRunner::resume`）解决的是两个不同
//! 层次的问题。会话级恢复回答"这个 runner 停在哪"，任务级恢复回答
//! "这个任务做到哪"。后者多三件事：
//!
//! 1. **进度的真相来源**是任务，不是 runner。任务停在第 3 步，恢复
//!    就从第 3 步开始——前面两步不重做。
//! 2. **每一步的结论要交回给模型**。恢复后的第 3 步必须看得见第 1、
//!    2 步查出了什么，否则它只能从零重推——那正是长任务恢复后跑偏的
//!    根源。
//! 3. **恢复前要核对三件事**：任务的步骤与快照里的会话对不对得上、
//!    会话 ID 一不一致、工作区有没有被人动过。三者任一不对，就明确
//!    报错或要求确认，绝不静默继续。
//!
//! 核对放在恢复之前而不是之后：恢复错了还能重来，恢复对了却带着
//! 错的状态跑下去，副作用就出去了。

use std::sync::Arc;

use crate::application::agent_run::{AgentRunner, RunState};
use crate::application::error::ApplicationError;
use crate::application::port::EvidenceSink;
use crate::application::snapshot::{RunSnapshot, StoreError, TaskSnapshot};
use crate::application::step_prompt;
use crate::application::worktree::WorktreeScanner;
use crate::domain::ids::SessionId;
use crate::domain::task::StepOutput;
use crate::domain::worktree::{WorktreeChange, WorktreeManifest};

/// 工作区被改动时，恢复该怎么办。
///
/// 这是**用户**的决定，不是程序能替他做的：改动可能正是上一个 agent
/// 留下的半成品（那就该继续），也可能是一份与任务无关的手改（那就
/// 该停下来）。
///
/// 默认 [`WorktreeChangePolicy::Refuse`]：宁可多问一句，也不要在
/// 别人刚改过的工作区上继续改。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WorktreeChangePolicy {
    /// 检测到改动就拒绝恢复。
    #[default]
    Refuse,
    /// 用户已经看过差异并确认过了，照常恢复。
    Confirmed,
}

/// 恢复前的核对结果。
///
/// 核对不通过时不是"恢复失败"，而是"恢复前必须有人拍板"——工作区被
/// 改过这一条尤其如此：硬失败会让用户以为程序坏了，而实际上只要他
/// 说一句"我知道，继续"就该能接着跑。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResumeCheck {
    /// 三项核对全过，可以直接恢复。
    Clean,
    /// 工作区自快照以来被改过。`changes` 列出差异。
    ///
    /// 这是**警告**而不是错误：改动的可能正是上一个 agent 留下的
    /// 半成品，而用户可能知道自己在改什么。静默继续才是错的——
    /// 模型会拿着过期的计划继续改，把用户刚做的修改覆盖掉。
    WorktreeChanged {
        /// 差异清单。
        changes: Vec<WorktreeChange>,
    },
    /// 工作区扫不出来（目录没了、权限不足）。检查降级为"查不了"。
    ///
    /// 与 [`ResumeCheck::Clean`] 分开：查不了不等于没改动。把"查不了"
    /// 报成"没改动"就是在撒谎。
    WorktreeUnreadable {
        /// 原因。
        reason: String,
    },
}

/// 恢复前的核对失败。
///
/// 这类失败是**明确拒绝**，不是"尽力恢复"：任务步骤与会话对不上、
/// 会话 ID 不一致，都说明手里的快照与要恢复的东西不是一回事。
/// 在这种状态下继续跑，恢复出来的模型会在错误的上下文里工作。
#[derive(Debug, thiserror::Error)]
pub enum ResumeRejection {
    /// 任务快照里的步骤与会话里的步骤交代对不上。
    #[error("任务快照与会话历史不一致：{reason}")]
    Inconsistent {
        /// 哪里对不上。
        reason: String,
    },
    /// 会话 ID 与任务期望的不一致。
    #[error("会话 ID 不一致：快照是 {found}，任务要求 {expected}")]
    SessionMismatch {
        /// 快照里的会话 ID。
        found: String,
        /// 期望的会话 ID。
        expected: String,
    },
    /// 快照版本不匹配。
    #[error(transparent)]
    Version(#[from] StoreError),
}

impl From<ResumeRejection> for ApplicationError {
    fn from(rejection: ResumeRejection) -> Self {
        match rejection {
            ResumeRejection::Version(error) => ApplicationError::Snapshot(error),
            other => ApplicationError::Snapshot(StoreError::Corrupt(other.to_string())),
        }
    }
}

/// 恢复前的三项核对。
///
/// # Errors
///
/// 快照版本不匹配、任务与会话不自洽、会话 ID 对不上时返回
/// [`ResumeRejection`]。
pub fn check_resumable(
    snapshot: &TaskSnapshot,
    expected_session: Option<&SessionId>,
) -> Result<(), ResumeRejection> {
    snapshot.check_version()?;
    check_steps_match(snapshot)?;
    check_session_matches(snapshot, expected_session)?;
    Ok(())
}

/// 核对任务的步骤与会话里的步骤交代。
///
/// 会话历史里每一步的交代是一条用户消息。任务快照里的步骤与这些交代
/// 必须**逐字对得上**：第 i 条交代必须正是任务第 i 步会生成的交代。
///
/// 为什么逐字比而不是只数条数：条数只能回答"开了几步"，回答不了
/// "开的是不是这个任务的步"。而两者对不上，意味着手里的任务与手里的
/// 会话不是同一个任务的两半——拿它恢复等于用 A 任务的进度去续 B 任务
/// 的历史。
///
/// 只比已经开跑的步：任务还有几步没跑，那几步的交代从来没进过历史，
/// 这是正常的（任务正是在那一步被中断的）。
///
/// 步骤说明在会话里是自然语言，但它由 [`step_prompt::step_prompt`]
/// 唯一生成——构造与比对共用同一份文本，所以这里比的是确定的东西。
fn check_steps_match(snapshot: &TaskSnapshot) -> Result<(), ResumeRejection> {
    let task = &snapshot.task;
    let total = task.steps().len();
    let prompts: Vec<&str> = snapshot
        .run
        .session
        .turns
        .iter()
        .filter_map(|turn| match turn {
            crate::domain::session::Turn::User(text) => {
                if crate::domain::task::is_handoff_message(text) {
                    None
                } else {
                    Some(text.as_str())
                }
            }
            _ => None,
        })
        .collect();

    if prompts.len() > total {
        return Err(ResumeRejection::Inconsistent {
            reason: format!(
                "会话历史里开了 {} 步，任务总共只有 {total} 步",
                prompts.len()
            ),
        });
    }
    for (index, text) in prompts.iter().enumerate() {
        let expected = step_prompt::step_prompt(task.goal(), &task.steps()[index], index, total);
        if text != &expected {
            return Err(ResumeRejection::Inconsistent {
                reason: format!(
                    "会话历史里的第 {} 条交代与任务的第 {} 步不是一回事；两者必须一一对应",
                    index + 1,
                    index + 1
                ),
            });
        }
    }
    Ok(())
}

/// 核对会话 ID。
///
/// 快照里的会话 ID 与期望的不一致时说明手里的快照不是这个任务的
/// 那份——可能是用户复制了文件，也可能任务 ID 被复用了。两种情况都
/// 值得停下来问一句，而不是接着跑。
fn check_session_matches(
    snapshot: &TaskSnapshot,
    expected: Option<&SessionId>,
) -> Result<(), ResumeRejection> {
    let Some(expected) = expected else {
        return Ok(());
    };
    let found = &snapshot.run.session_id;
    if found != expected {
        return Err(ResumeRejection::SessionMismatch {
            found: found.as_str().to_owned(),
            expected: expected.as_str().to_owned(),
        });
    }
    Ok(())
}

/// 把会话快照里的半轮现场合并进任务快照。
///
/// **为什么需要这一步。** 任务快照是**步骤边界**的快照：它在每完成一步
/// 之后落一次盘，里面记的运行状态因此总是"某一步刚做完"的样子。进程若
/// 死在某一步的半路上（等人审批、正在执行工具），任务快照就落后了——
/// 真正的现场（"停在第几次调用上、那次调用有没有跑过"）只存在于**会话
/// 快照**里，那才是 runner 每到一个保存点就写一份的文件。
///
/// 不合并的后果很具体：任务停在等人审批时被杀，恢复会退回步骤边界，
/// 整个中断的步骤从头再跑一遍；停在工具执行途中被杀，那次调用的副作用
/// 可能已经发生，而恢复路径上没有任何东西记得它。
///
/// **只合并"停在半轮"的会话快照。** 步骤边界的现场任务快照里已经有了，
/// 合并只会引入"哪一份更新"的问题。
///
/// **合并的前提是两边说的是同一件事**：同一个会话，且会话正停在任务要
/// 恢复的那一步上（会话开跑的步数 = 任务进度 + 1）。对不上就不合并——
/// 宁可按步骤边界恢复（重做那一步），也不要拿一个对不上的半轮现场去续。
///
/// 纯函数：不读盘、不取时钟。读盘由调用方做，好让这段判断能被单独测。
pub fn merge_session_state(snapshot: TaskSnapshot, session: Option<RunSnapshot>) -> TaskSnapshot {
    let Some(session) = session else {
        return snapshot;
    };
    if !matches!(
        session.state,
        RunState::Evaluating(_) | RunState::AwaitingApproval(_) | RunState::Dispatching(_)
    ) {
        return snapshot;
    }
    if session.session_id != snapshot.run.session_id {
        return snapshot;
    }
    // 会话停在任务要恢复的那一步上吗？
    let resumed_step = snapshot.task.current_index();
    if session.session.started_steps() != resumed_step + 1 {
        return snapshot;
    }
    let mut merged = snapshot;
    merged.run = session;
    merged
}

/// 比对快照时的清单与现在的清单，给出工作区有没有被动过的结论。
pub fn check_worktree(
    saved: &WorktreeManifest,
    scanner: &dyn WorktreeScanner,
) -> ResumeCheck {
    match scanner.scan() {
        Ok(now) => {
            let changes = saved.diff(&now);
            if changes.is_empty() {
                ResumeCheck::Clean
            } else {
                ResumeCheck::WorktreeChanged { changes }
            }
        }
        Err(reason) => ResumeCheck::WorktreeUnreadable { reason },
    }
}

/// 把已完成步骤的结论推进会话历史。
///
/// 结论以用户消息的形式推进历史。三个理由：
///
/// 1. **裁剪不会吃掉它**。`domain::context::compact` 丢弃的是轮次，
///    保留的是全部用户消息。结论走用户消息，就活得比历史轮次长——
///    而长任务恰恰是历史被裁得最狠的场景。
/// 2. **下一步的模型需要的是"结论原文"，不是"结论的概括"**。第 3 步
///    要动手改代码，它需要第 1 步查出的根因是什么、是什么形式（路径、
///    函数名、报错文本）。概括会丢掉这些。
/// 3. **可判定**。恢复完成后，"第 1 步的结论在不在历史里"是能测的
///    （见 `tests/task_resume.rs`），而不是只能靠人读一遍觉得像。
///
/// 代价是历史会多出几条用户消息——与步骤交代是同一类东西（都是
/// "给下一步的提示"），量级也可控：步数是几十的量级，不是几千。
///
/// 重复交接要避免：同一份快照恢复两次，第二次不该把同一句话再推一遍。
/// 判断依据是消息文本——[`StepOutput::handoff_message`] 由内容唯一
/// 决定，比对文本即可。
pub fn hand_off_completed_outputs(runner: &mut AgentRunner, outputs: &[StepOutput]) {
    for output in outputs {
        let message = output.handoff_message();
        if runner.session().has_user_message(&message) {
            continue;
        }
        runner.insert_user_message_before_step(message);
    }
}

/// 组装证据汇：恢复路径上证据要追加到同一个文件。
pub type EvidenceFactory =
    Arc<dyn Fn(&SessionId) -> std::io::Result<Arc<dyn EvidenceSink>> + Send + Sync>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::snapshot::{RUN_FORMAT, TASK_FORMAT};
    use crate::domain::agent::{AgentRole, AgentSpec};
    use crate::domain::budget::Consumption;
    use crate::domain::ids::TaskId;
    use crate::domain::session::Session;
    use crate::domain::task::{Task, TaskKind, TaskStep};

    /// 一串步骤交代装进会话：第一条交代就是会话的开局消息——
    /// 真实运行里 runner 用第一步的 `prompt` 起步（见 orchestrator 的
    /// `AgentRunner::new`），会话里没有独立于交代的"任务原文"。
    fn session_with_prompts(id: SessionId, prompts: usize) -> crate::domain::session::SessionSnapshot {
        let mut session = Session::new(id, "第 1 步的交代".to_owned());
        for n in 2..=prompts {
            session.push_user(format!("第 {n} 步的交代"));
        }
        session.snapshot()
    }

    /// 停在步骤边界的任务快照：前 `done_steps` 步已完成，进度停在下一步。
    /// 会话历史里的交代条数与进度一致——这是"正常"的任务快照。
    fn task_snapshot_at(done_steps: usize) -> TaskSnapshot {
        let steps: Vec<TaskStep> = (1..=done_steps + 1)
            .map(|n| TaskStep {
                description: format!("第 {n} 步"),
                role: AgentRole::Coder,
                done_when: "有产出".to_owned(),
            })
            .collect();
        let mut task = Task::new(TaskId::new("t1"), TaskKind::BugFix, "修好它", steps);
        task.start().expect("启动任务");
        for _ in 0..done_steps {
            task.advance().expect("推进任务");
        }

        let session_id = SessionId::new("s1");
        TaskSnapshot {
            format_version: TASK_FORMAT,
            task,
            run: RunSnapshot {
                format_version: RUN_FORMAT,
                session: session_with_prompts(session_id.clone(), done_steps + 1),
                session_id,
                spec: AgentSpec {
                    role: AgentRole::Coder,
                    system_prompt: "系统提示".to_owned(),
                    model: None,
                    effort: None,
                    verbosity: None,
                },
                state: RunState::AwaitingModel,
                consumption: Consumption {
                    turns: done_steps as u32,
                    elapsed: std::time::Duration::from_millis(500),
                },
                pending: None,
            },
            completed_outputs: Vec::new(),
            worktree: WorktreeManifest::default(),
        }
    }

    /// 一份"停在半轮"的会话快照：会话开跑了 `started_steps` 步，
    /// 状态可指定。`session_id` 默认与任务快照同源。
    fn half_round_session(started_steps: usize, state: RunState) -> RunSnapshot {
        half_round_session_with_id(SessionId::new("s1"), started_steps, state)
    }

    fn half_round_session_with_id(session_id: SessionId, started_steps: usize, state: RunState) -> RunSnapshot {
        let session = session_with_prompts(session_id.clone(), started_steps);
        RunSnapshot {
            format_version: RUN_FORMAT,
            session_id,
            spec: AgentSpec {
                role: AgentRole::Coder,
                system_prompt: "系统提示".to_owned(),
                model: None,
                effort: None,
                verbosity: None,
            },
            session,
            state,
            consumption: Consumption {
                turns: started_steps as u32,
                elapsed: std::time::Duration::from_millis(500),
            },
            pending: None,
        }
    }

    #[test]
    fn a_matching_half_round_session_is_merged() {
        // 正对照：会话与任务同一个 run、正停在要恢复的那一步上
        // （开跑了 resumed_step + 1 步），半轮现场必须盖过步骤边界。
        let snapshot = task_snapshot_at(1);
        let session = half_round_session(2, RunState::Dispatching(vec![]));
        let merged = merge_session_state(snapshot.clone(), Some(session.clone()));
        assert_eq!(merged.run, session, "对得上的半轮现场应当被合并");
    }

    #[test]
    fn a_session_from_another_run_is_not_merged() {
        // 会话 ID 对不上：手里的会话快照不是这个任务的那份。拿别人的
        // 半轮现场续自己的任务，等于把副作用记到错误的账上。
        let snapshot = task_snapshot_at(1);
        let session = half_round_session_with_id(SessionId::new("s2"), 2, RunState::Dispatching(vec![]));
        let merged = merge_session_state(snapshot.clone(), Some(session));
        assert_eq!(merged.run, snapshot.run, "会话 ID 不一致时不合并");
    }

    #[test]
    fn a_session_stopped_at_a_different_step_is_not_merged() {
        // 进度停在第 2 步（resumed_step=1，会话该开跑了 2 步），
        // 会话却跑到第 3 步：两边不是同一个位置，宁可按步骤边界恢复。
        let snapshot = task_snapshot_at(1);
        let session = half_round_session(3, RunState::Dispatching(vec![]));
        let merged = merge_session_state(snapshot.clone(), Some(session));
        assert_eq!(merged.run, snapshot.run, "步数对不上时不合并");
    }

    #[test]
    fn a_session_at_a_step_boundary_is_not_merged() {
        // 会话停在步骤边界（Completed）而不是半轮：任务快照里已经有
        // 这个现场了，合并只会引入"哪一份更新"的问题。
        let snapshot = task_snapshot_at(1);
        let session = half_round_session(2, RunState::Completed("收尾".to_owned()));
        let merged = merge_session_state(snapshot.clone(), Some(session));
        assert_eq!(merged.run, snapshot.run, "非半轮状态不合并");
    }

    #[test]
    fn no_session_leaves_the_snapshot_alone() {
        let snapshot = task_snapshot_at(0);
        let merged = merge_session_state(snapshot.clone(), None);
        assert_eq!(merged, snapshot, "没有会话快照就原样返回");
    }
}

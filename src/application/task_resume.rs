//! 任务级恢复：把任务快照读回成可以继续跑的状态。
//!
//! 与会话级恢复（`AgentRunner::resume`）分属两个层次。会话级回答
//! "runner 停在哪"，任务级回答"任务做到哪"。任务级多三件事：
//!
//! 1. 进度的真相来源是任务，不是 runner。任务停在第 3 步，恢复就从
//!    第 3 步开始，前两步不重做。
//! 2. 每一步的结论要交回模型。恢复后的第 3 步必须看到第 1、2 步的
//!    结果，否则只能从零重推，长任务恢复后偏离目标多源于此。
//! 3. 恢复前核对两件事：任务步骤与快照会话是否对应（不符即拒绝），
//!    工作区是否被改过（不符要用户确认或按策略处理）。
//!
//! 核对在恢复之前完成：恢复错了可以重来；带着错状态继续跑，副作用
//! 已经发生。

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

/// 工作区被改动时恢复的策略。
///
/// 是否继续由用户决定：改动可能是上一个 agent 留下的半成品，也可能是
/// 与本任务无关的手工修改。
///
/// 默认 [`WorktreeChangePolicy::Refuse`]，避免在别人刚改过的工作区上
/// 继续改。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WorktreeChangePolicy {
    /// 检测到改动就拒绝恢复。
    #[default]
    Refuse,
    /// 用户已确认差异，照常恢复。
    Confirmed,
}

/// 恢复前的核对结果。
///
/// 核对不通过表示恢复前需要用户决定，不是恢复失败。工作区被改过尤其
/// 如此：硬失败会让用户以为程序坏了，而用户确认后应当可以继续。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResumeCheck {
    /// 工作区清单无差异。这只覆盖工作区检查；版本与步骤核对在
    /// [`check_resumable`] 里完成，结果不经过本类型。
    Clean,
    /// 工作区自快照以来被改过，`changes` 列出差异。
    ///
    /// 这是警告，不是错误：改动可能是上一个 agent 的半成品，用户也
    /// 可能知道自己在改什么。不能静默继续，否则模型会按过期计划覆盖
    /// 用户刚做的修改。
    WorktreeChanged {
        /// 差异清单。
        changes: Vec<WorktreeChange>,
    },
    /// 工作区无法扫描（目录不存在、权限不足）。
    ///
    /// 与 [`ResumeCheck::Clean`] 分开：查不了与没改动是两回事，不能把
    /// 前者报成后者。
    WorktreeUnreadable {
        /// 原因。
        reason: String,
    },
}

/// 恢复前的核对失败。
///
/// 这是明确拒绝，不做尽力恢复：步骤对不上或会话 ID 不一致，说明手里
/// 的快照与要恢复的任务不是一回事。继续跑会让模型在错误的上下文里
/// 工作。
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

/// 核对任务步骤与会话里的步骤交代。
///
/// 每一步的交代是会话历史里的一条用户消息。任务快照的步骤必须与这些
/// 交代逐字一致：第 i 条交代即任务第 i 步生成的文本。
///
/// 只数条数不够，它回答不了开的是不是这个任务的步。对不上说明任务与
/// 会话不属于同一个任务，拿它恢复会混用 A 任务的进度与 B 任务的历史。
///
/// 只比已经开跑的步。未跑的步其交代从未进入历史，任务就在那一步被
/// 中断。
///
/// 步骤说明是自然语言，但由 [`step_prompt::step_prompt`] 唯一生成，
/// 构造与比对共用同一份文本，比对结果是确定的。
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
/// 不一致说明快照不属于这个任务，可能是用户复制了文件，也可能任务 ID
/// 被复用。两种情况都应停下来确认。
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
/// 任务快照只在步骤边界落盘，记录的运行状态总是"某一步刚做完"。进程
/// 若死在某一步中途（等待审批、执行工具），任务快照就落后了。真正的
/// 现场（停在第几次调用、那次调用有没有执行过）只在会话快照里，那是
/// runner 每到一个保存点就写一份的文件。
///
/// 不合并的后果：停在审批前被杀，恢复会退回步骤边界，整个步骤重跑；
/// 停在工具执行途中被杀，该调用的副作用可能已经发生，恢复路径却不
/// 记得它。
///
/// 只合并停在半轮的会话快照。步骤边界的现场任务快照已有，合并只会
/// 引入"哪一份更新"的问题。
///
/// 合并前提是两边指向同一位置：同一会话，且会话开跑的步数 = 任务
/// 进度 + 1。对不上就不合并，按步骤边界恢复（重做那一步），不能拿
/// 对不上的半轮现场续跑。
///
/// 纯函数：不读盘、不取时钟。读盘由调用方完成，便于单独测试。
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
    // 会话必须停在任务要恢复的那一步上。
    let resumed_step = snapshot.task.current_index();
    if session.session.started_steps() != resumed_step + 1 {
        return snapshot;
    }
    let mut merged = snapshot;
    merged.run = session;
    merged
}

/// 比对快照清单与当前清单，判断工作区是否被改动过。
pub fn check_worktree(saved: &WorktreeManifest, scanner: &dyn WorktreeScanner) -> ResumeCheck {
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
/// 结论以用户消息的形式写入，理由有三：
///
/// 1. 裁剪不会丢弃它。`domain::context::compact` 丢弃轮次，保留全部
///    用户消息。结论走用户消息，能活得比轮次长，长任务的历史最容易被
///    裁。
/// 2. 下一步需要结论原文。第 3 步要改代码，它需要第 1 步查出的根因、
///    路径、函数名、报错文本，概括会丢掉这些。
/// 3. 可判定。恢复后"第 1 步的结论在不在历史里"可以测（见
///    `tests/task_resume.rs`），不必靠人读一遍判断。
///
/// 代价是历史多出几条用户消息，与步骤交代同属"给下一步的提示"，量级
/// 可控：步数是几十的量级，不是几千。
///
/// 避免重复交接：同一份快照恢复两次，第二次不应再推同一句话。依据是
/// 消息文本，[`StepOutput::handoff_message`] 由内容唯一决定，比对文本
/// 即可。
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

    /// 把一串步骤交代装进会话。第一条交代是会话开局消息：真实运行里
    /// runner 用第一步的 `prompt` 起步（见 orchestrator 的
    /// `AgentRunner::new`），会话里没有独立于交代的任务原文。
    fn session_with_prompts(
        id: SessionId,
        prompts: usize,
    ) -> crate::domain::session::SessionSnapshot {
        let mut session = Session::new(id, "第 1 步的交代".to_owned());
        for n in 2..=prompts {
            session.push_user(format!("第 {n} 步的交代"));
        }
        session.snapshot()
    }

    /// 停在步骤边界的任务快照：前 `done_steps` 步已完成，进度停在下一步。
    /// 会话历史里的交代条数与进度一致。
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

    /// 一份停在半轮的会话快照：会话开跑了 `started_steps` 步，状态可指定。
    /// `session_id` 默认与任务快照同源。
    fn half_round_session(started_steps: usize, state: RunState) -> RunSnapshot {
        half_round_session_with_id(SessionId::new("s1"), started_steps, state)
    }

    fn half_round_session_with_id(
        session_id: SessionId,
        started_steps: usize,
        state: RunState,
    ) -> RunSnapshot {
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
    fn legacy_step_prompts_still_match_after_projecting_the_model_view() {
        use crate::application::context_policy::{ContextPolicy, DefaultContextPolicy};
        use crate::domain::context::ContextBudget;
        use crate::domain::session::ContextSlice;
        use crate::domain::workflow::WorkflowCatalog;

        let goal = "恢复已有快照并保留目标。".repeat(300);
        let steps = WorkflowCatalog::builtin()
            .find(TaskKind::BugFix)
            .unwrap()
            .expand(&goal);
        let mut snapshot = task_snapshot_at(2);
        snapshot.task = Task::new(TaskId::new("legacy"), TaskKind::BugFix, &goal, steps);
        snapshot.task.start().unwrap();
        snapshot.task.advance().unwrap();
        snapshot.task.advance().unwrap();
        let prompts: Vec<String> = snapshot.task.steps().iter().take(3).enumerate().map(|(index, step)| {
            format!("任务：{goal}\n当前步骤（第 {}/4 步）：{}\n完成判据：{}\n达成判据后停止调用工具，直接输出最终答案。", index + 1, step.description, step.done_when)
        }).collect();
        let mut session = Session::new(snapshot.run.session_id.clone(), &prompts[0]);
        for prompt in &prompts[1..] {
            session.push_user(prompt);
        }
        session.push_input(
            "constraint".to_owned(),
            "恢复后仍禁止修改公共接口".to_owned(),
        );
        snapshot.run.session = session.snapshot();
        let saved = snapshot.clone();
        check_resumable(&snapshot, Some(&snapshot.run.session_id)).unwrap();

        let session = Session::from_snapshot(
            snapshot.run.session_id.clone(),
            snapshot.run.session.clone(),
        )
        .unwrap();
        let view = DefaultContextPolicy.view(&session, &ContextBudget::default(), None);
        let goal_copies: usize = view
            .iter()
            .filter_map(|slice| match slice {
                ContextSlice::User(text) => Some(text.matches(&goal).count()),
                _ => None,
            })
            .sum();
        assert_eq!(goal_copies, 1, "旧快照也应共享一次目标");
        assert_eq!(session.snapshot(), saved.run.session);
        assert_eq!(session.started_steps(), 3);
        check_resumable(&snapshot, Some(&snapshot.run.session_id)).unwrap();
        assert_eq!(snapshot, saved);

        snapshot.run.session.turns[1] =
            crate::domain::session::Turn::User(prompts[1].replace(&goal, "另一个任务"));
        assert!(
            check_resumable(&snapshot, None).is_err(),
            "缩短模型视图不能放宽恢复核对"
        );
    }

    #[test]
    fn a_matching_half_round_session_is_merged() {
        // 正对照：会话与任务同属一个 run，停在要恢复的那一步（开跑了
        // resumed_step + 1 步），半轮现场应覆盖步骤边界。
        let snapshot = task_snapshot_at(1);
        let session = half_round_session(2, RunState::Dispatching(vec![]));
        let merged = merge_session_state(snapshot.clone(), Some(session.clone()));
        assert_eq!(merged.run, session, "对得上的半轮现场应当被合并");
    }

    #[test]
    fn a_session_from_another_run_is_not_merged() {
        // 会话 ID 对不上时快照不属于这个任务。拿它续跑会把副作用记到
        // 错误的账上。
        let snapshot = task_snapshot_at(1);
        let session =
            half_round_session_with_id(SessionId::new("s2"), 2, RunState::Dispatching(vec![]));
        let merged = merge_session_state(snapshot.clone(), Some(session));
        assert_eq!(merged.run, snapshot.run, "会话 ID 不一致时不合并");
    }

    #[test]
    fn a_session_stopped_at_a_different_step_is_not_merged() {
        // 进度停在第 2 步（resumed_step=1，会话应开跑 2 步），会话却跑到
        // 第 3 步：两边不在同一位置，应按步骤边界恢复。
        let snapshot = task_snapshot_at(1);
        let session = half_round_session(3, RunState::Dispatching(vec![]));
        let merged = merge_session_state(snapshot.clone(), Some(session));
        assert_eq!(merged.run, snapshot.run, "步数对不上时不合并");
    }

    #[test]
    fn a_session_at_a_step_boundary_is_not_merged() {
        // 会话停在步骤边界（Completed）而非半轮：任务快照已有这个现场，
        // 合并只会引入"哪一份更新"的问题。
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

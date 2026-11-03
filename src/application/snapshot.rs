//! 运行快照：会话恢复所需的全部状态，以及快照存储的端口。
//!
//! 快照放在应用层而不是 domain：它引用 `RunState` / `PendingRound`
//! 这两个应用层状态机类型，domain 反向依赖 application 会把分层倒过来。
//! 纯历史的 [`SessionSnapshot`] 仍在 domain——那是领域数据。
//!
//! `format_version` 是这份契约的版本号：快照是跨进程、跨版本的持久数据，
//! 静默兼容会把损坏或不兼容的状态带进运行，读到一个不认识的版本必须
//! 显式报错，而不是猜着填默认值。

use serde::{Deserialize, Serialize};

use crate::domain::agent::AgentSpec;
use crate::domain::budget::Consumption;
use crate::domain::ids::{SessionId, TaskId};
use crate::domain::session::SessionSnapshot;
use crate::domain::task::{StepOutput, Task};
use crate::domain::worktree::WorktreeManifest;

use super::agent_run::{PendingRound, RunState};

/// 当前快照格式版本。每次快照结构发生不兼容变化时 +1。
///
/// v2（P5）：`AgentRole` 从单变体扩展为四变体，P4 快照里的角色序列化
/// 形态随之改变——旧快照被显式拒绝而不是猜着解释。
///
/// v3（P10）：任务级快照落地。`RunSnapshot` 本身结构未变，但快照从
/// "只描述一个 runner"扩到"描述一个任务"，两者的兼容前提不同——
/// 一个 P10 之前写的快照没有任务上下文，恢复时无法回答"这是整个任务的
/// 哪一步"，因此明确拒绝而不是猜着填。
pub const RUN_FORMAT: u32 = 3;

/// 当前任务快照格式版本。
///
/// 与 [`RUN_FORMAT`] 分开：两者落在不同的文件里（`.qcodeharness/snapshots/`
/// 与 `.qcodeharness/tasks/`），版本号各自演进。共用一个号会让"改了一边
/// 就得改另一边"，而两边的结构没有这种依赖关系。
pub const TASK_FORMAT: u32 = 1;

/// 一次运行的可序列化快照：恢复所需的全部状态。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunSnapshot {
    /// 快照格式版本。读到时先校验，拒绝不认识的版本。
    pub format_version: u32,
    /// 会话 ID（同时是证据文件与快照文件名的来源）。
    pub session_id: SessionId,
    /// 本次运行的 Agent 规格。
    ///
    /// 规格草图里没有它，但恢复要用回同一份系统提示词——
    /// 换成别的提示词等于换了一个 agent 接手半截任务。
    pub spec: AgentSpec,
    /// 完整会话历史。
    pub session: SessionSnapshot,
    /// 状态机停在哪。
    pub state: RunState,
    /// 已消耗的量（轮数 + 耗时）。
    pub consumption: Consumption,
    /// 工具已经跑完、还没提交的那半轮。`None` 表示不在处理调用的中途。
    pub pending: Option<PendingRound>,
}

/// 一个任务的可序列化快照：任务级恢复所需的全部状态。
///
/// 与 [`RunSnapshot`] 的分工：`RunSnapshot` 描述"一个 runner 停在哪"，
/// 任务快照描述"一个任务做到哪"。后者多两样东西：
///
/// - `task`：任务本身的进度（`TaskState::Running { step }` 是进度的
///   唯一真相来源）。没有它，恢复时不知道该从第几步接着走。
/// - `completed_outputs`：每一步的结论。没有它，恢复后的下一步看不见
///   前面几步查出了什么，只能从零重推——那正是"长任务恢复后跑偏"的
///   根源。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskSnapshot {
    /// 快照格式版本。读到时先校验，拒绝不认识的版本。
    pub format_version: u32,
    /// 任务本身：目标、步骤与进度。
    pub task: Task,
    /// 本次运行的快照。会话 ID 与任务 ID 在恢复时必须对得上。
    pub run: RunSnapshot,
    /// 已完成步骤的结论，按步骤号升序。
    pub completed_outputs: Vec<StepOutput>,
    /// 存快照时的工作区清单。
    ///
    /// 恢复时拿它与"现在的清单"比对，得出工作区有没有被人动过。
    /// 快照里**必须**留着它：没有它就无从判断——拿现在的清单跟自己
    /// 比永远得到"没变过"，那不是检查，那是自欺。
    #[serde(default)]
    pub worktree: WorktreeManifest,
}

impl TaskSnapshot {
    /// 校验快照版本，不认识就报错。
    ///
    /// # Errors
    ///
    /// `format_version` 不是当前 [`TASK_FORMAT`] 时返回
    /// [`StoreError::VersionMismatch`]。
    pub fn check_version(&self) -> Result<(), StoreError> {
        if self.format_version == TASK_FORMAT {
            Ok(())
        } else {
            Err(StoreError::VersionMismatch {
                found: self.format_version,
                expected: TASK_FORMAT,
            })
        }
    }

    /// 任务 ID。
    pub fn task_id(&self) -> &TaskId {
        self.task.id()
    }
}

impl RunSnapshot {
    /// 校验快照版本，不认识就报错。
    ///
    /// # Errors
    ///
    /// `format_version` 不是当前 [`RUN_FORMAT`] 时返回
    /// [`StoreError::VersionMismatch`]。
    pub fn check_version(&self) -> Result<(), StoreError> {
        if self.format_version == RUN_FORMAT {
            Ok(())
        } else {
            Err(StoreError::VersionMismatch {
                found: self.format_version,
                expected: RUN_FORMAT,
            })
        }
    }
}

/// 快照存储：把运行状态写到磁盘、读回来。
///
/// 是端口而不是具体实现：编排器与 CLI 依赖这个抽象，
/// 落盘方式（单文件 JSON、SQLite、远端）由基础设施层决定。
pub trait SessionStore: Send + Sync {
    /// 保存快照。同名覆盖。
    ///
    /// # Errors
    /// 目录创建、序列化或写盘失败时返回 [`StoreError`]。
    fn save(&self, snapshot: &RunSnapshot) -> Result<(), StoreError>;

    /// 读取快照。文件不存在返回 `Ok(None)`。
    ///
    /// # Errors
    /// 文件存在但损坏或版本不匹配时返回 [`StoreError`]。
    fn load(&self, session_id: &SessionId) -> Result<Option<RunSnapshot>, StoreError>;

    /// 列出所有可恢复的会话 ID。
    ///
    /// # Errors
    /// 目录遍历失败时返回 [`StoreError`]。
    fn list(&self) -> Result<Vec<SessionId>, StoreError>;
}

/// 任务快照存储：把任务级状态写到磁盘、读回来。
///
/// 与 [`SessionStore`] 分开而不是合并：两者的键不同（任务 ID vs 会话 ID）、
/// 落盘位置不同（`tasks/` vs `snapshots/`）、回答的问题也不同（"这个任务
/// 做到哪" vs "这个 runner 停在哪"）。合成一个 trait 会让"只想读会话
/// 快照"的调用方也得实现任务快照的方法。
pub trait TaskStore: Send + Sync {
    /// 保存任务快照。同名覆盖。
    ///
    /// # Errors
    /// 目录创建、序列化或写盘失败时返回 [`StoreError`]。
    fn save(&self, snapshot: &TaskSnapshot) -> Result<(), StoreError>;

    /// 读取任务快照。文件不存在返回 `Ok(None)`。
    ///
    /// # Errors
    /// 文件存在但损坏或版本不匹配时返回 [`StoreError`]。
    fn load(&self, task_id: &TaskId) -> Result<Option<TaskSnapshot>, StoreError>;

    /// 列出所有可恢复的任务 ID。
    ///
    /// # Errors
    /// 目录遍历失败时返回 [`StoreError`]。
    fn list(&self) -> Result<Vec<TaskId>, StoreError>;
}

/// 快照存储的错误。
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// 目录创建、写盘、改名等文件系统失败。
    #[error("快照读写失败: {0}")]
    Io(#[from] std::io::Error),

    /// 快照内容不是合法 JSON 或结构与当前版本不符。
    #[error("快照解析失败: {0}")]
    Parse(#[from] serde_json::Error),

    /// 快照由别的格式版本写入。
    #[error("快照版本不匹配：文件由 v{found} 写入，当前程序是 v{expected}；拒绝加载以免带入不兼容状态")]
    VersionMismatch {
        /// 文件里的版本。
        found: u32,
        /// 当前程序认识的版本。
        expected: u32,
    },

    /// 快照内容能解析，但状态自相矛盾（如停在半轮却没有待配平的轮次）。
    #[error("快照状态损坏：{0}")]
    Corrupt(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::agent_run::RunState;
    use crate::domain::agent::{AgentRole, AgentTurn};
    use crate::domain::ids::CallId;
    use crate::domain::session::Session;
    use crate::domain::task::{StepOutput, Task, TaskKind, TaskState, TaskStep};
    use crate::domain::tool::{ToolCall, ToolOutcome};
    use serde_json::json;

    /// 造一份停在"半轮"状态的快照：调用已给出、观察收了一半。
    /// 这是恢复语义最挑剔的状态，round-trip 必须无损。
    fn mid_round_snapshot() -> RunSnapshot {
        let mut session = Session::new(SessionId::new("s1"), "任务");
        session
            .commit_round(
                AgentTurn {
                    text: String::new(),
                    calls: vec![],
                },
                Vec::new(),
            )
            .unwrap();
        RunSnapshot {
            format_version: RUN_FORMAT,
            session_id: SessionId::new("s1"),
            spec: AgentSpec {
                role: AgentRole::Coder,
                system_prompt: "系统提示".to_owned(),
            },
            session: session.snapshot(),
            state: RunState::Dispatching(vec![ToolCall {
                id: CallId::new("call_1"),
                name: "read_file".to_owned(),
                arguments: json!({"path": "a"}),
            }]),
            consumption: Consumption {
                turns: 1,
                elapsed: std::time::Duration::from_millis(1500),
            },
            pending: Some(PendingRound {
                turn: AgentTurn {
                    text: "想一下".to_owned(),
                    calls: vec![
                        ToolCall {
                            id: CallId::new("call_1"),
                            name: "read_file".to_owned(),
                            arguments: json!({"path": "a"}),
                        },
                        ToolCall {
                            id: CallId::new("call_2"),
                            name: "list_dir".to_owned(),
                            arguments: json!({}),
                        },
                    ],
                },
                outcomes: vec![ToolOutcome::success("第一条观察")],
            }),
        }
    }

    #[test]
    fn snapshot_json_round_trips_losslessly() {
        let snapshot = mid_round_snapshot();
        let text = serde_json::to_string(&snapshot).unwrap();
        let restored: RunSnapshot = serde_json::from_str(&text).unwrap();

        assert_eq!(restored, snapshot);
        assert!(restored.check_version().is_ok());
    }

    #[test]
    fn version_mismatch_is_rejected_with_an_explanation() {
        let mut snapshot = mid_round_snapshot();
        snapshot.format_version = 999;
        let error = snapshot.check_version().unwrap_err();
        assert!(matches!(
            error,
            StoreError::VersionMismatch { found: 999, expected: RUN_FORMAT }
        ));
        assert!(error.to_string().contains("v999"), "{error}");
    }

    #[test]
    fn snapshot_from_previous_format_is_rejected() {
        // P4 写的 v1 快照：AgentRole 扩展后序列化形态已变，必须明确拒绝
        // 而不是猜着兼容——这是 acceptance 的硬要求，也是静默兼容更危险
        // 的直接体现。
        let mut snapshot = mid_round_snapshot();
        snapshot.format_version = 1;
        let error = snapshot.check_version().unwrap_err();
        let StoreError::VersionMismatch { found: 1, expected } = error else {
            panic!("应报版本不匹配")
        };
        assert_eq!(expected, RUN_FORMAT);
    }

    /// P10 起旧快照（v2）也要被明确拒绝。
    ///
    /// v2 的快照结构本身还能读，但它只描述一个 runner——没有任务上下文，
    /// 恢复时回答不了"这是整个任务的哪一步"。任务级恢复要的是能回答
    /// 这个问题的快照，所以 v2 明确拒绝，而不是猜着补一个空任务。
    #[test]
    fn a_v2_snapshot_is_rejected_after_the_task_level_upgrade() {
        let mut snapshot = mid_round_snapshot();
        snapshot.format_version = 2;
        let error = snapshot.check_version().unwrap_err();
        let StoreError::VersionMismatch { found: 2, expected } = error else {
            panic!("应报版本不匹配，实际 {error}")
        };
        assert_eq!(expected, 3, "P10 之后当前格式是 v3");
        assert!(error.to_string().contains("v2"), "{error}");
    }

    // ── P10 切片 1b：任务快照 ──────────────────────────────────────────

    /// 一个三步任务、停在第 2 步、已完成第 1 步的任务快照。
    fn task_snapshot() -> TaskSnapshot {
        let steps = vec![
            TaskStep {
                description: "第一步".to_owned(),
                role: AgentRole::Planner,
                done_when: "有产出".to_owned(),
            },
            TaskStep {
                description: "第二步".to_owned(),
                role: AgentRole::Coder,
                done_when: "有产出".to_owned(),
            },
        ];
        let mut task = Task::new(TaskId::new("t1"), TaskKind::BugFix, "修好它", steps);
        task.start().unwrap();
        task.advance().unwrap();

        let session_id = SessionId::new("s1");
        let run = RunSnapshot {
            format_version: RUN_FORMAT,
            session_id: session_id.clone(),
            spec: AgentSpec {
                role: AgentRole::Coder,
                system_prompt: "系统提示".to_owned(),
            },
            session: Session::new(session_id, "修好它").snapshot(),
            state: RunState::AwaitingModel,
            consumption: Consumption {
                turns: 2,
                elapsed: std::time::Duration::from_millis(500),
            },
            pending: None,
        };
        TaskSnapshot {
            format_version: TASK_FORMAT,
            task,
            run,
            completed_outputs: vec![StepOutput {
                step: 0,
                role: AgentRole::Planner,
                answer: "根因在解析层".to_owned(),
            }],
            worktree: WorktreeManifest::default(),
        }
    }

    #[test]
    fn task_snapshot_round_trips_losslessly() {
        let snapshot = task_snapshot();
        let text = serde_json::to_string(&snapshot).unwrap();
        let restored: TaskSnapshot = serde_json::from_str(&text).unwrap();

        assert_eq!(restored, snapshot);
        assert!(restored.check_version().is_ok());
        // 进度与结论都在：这是任务级恢复的两条命脉。
        assert_eq!(restored.task.state(), TaskState::Running { step: 1 });
        assert_eq!(restored.completed_outputs.len(), 1);
        assert_eq!(restored.completed_outputs[0].answer, "根因在解析层");
    }

    #[test]
    fn task_snapshot_version_mismatch_is_rejected() {
        let mut snapshot = task_snapshot();
        snapshot.format_version = 999;
        let error = snapshot.check_version().unwrap_err();
        assert!(matches!(
            error,
            StoreError::VersionMismatch {
                found: 999,
                expected: TASK_FORMAT
            }
        ), "{error}");
    }

    /// 任务快照里的任务本身也走 `Task` 的重建校验。
    ///
    /// 任务快照是外部可改的文件：把 `state` 改成越界的进度下标，
    /// 整个任务快照就应当读不回来，而不是带着一个坏任务进入编排。
    #[test]
    fn a_task_snapshot_with_a_broken_task_is_rejected() {
        let snapshot = task_snapshot();
        let mut wire = serde_json::to_value(&snapshot).unwrap();
        wire["task"]["state"] = serde_json::json!({"Running": {"step": 99}});

        let error = serde_json::from_value::<TaskSnapshot>(wire).unwrap_err();

        assert!(
            error.to_string().contains("进度下标越界"),
            "越界的进度下标必须让整个任务快照读不回来: {error}"
        );
    }
}

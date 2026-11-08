//! 运行快照：会话恢复所需的全部状态，以及快照存储的端口。
//!
//! 快照放在应用层，因为它引用应用层状态机类型 `RunState` / `PendingRound`；
//! 放进 domain 会让 domain 反向依赖 application。纯历史的 [`SessionSnapshot`]
//! 仍在 domain，属于领域数据。
//!
//! `format_version` 是这份持久契约的版本号。快照跨进程、跨版本使用，
//! 读到不认识的版本必须显式报错；静默兼容会把损坏或不兼容的状态带进运行。

use serde::{Deserialize, Serialize};

use crate::domain::agent::AgentSpec;
use crate::domain::budget::Consumption;
use crate::domain::ids::{SessionId, TaskId};
use crate::domain::session::SessionSnapshot;
use crate::domain::task::{StepOutput, Task};
use crate::domain::worktree::WorktreeManifest;

use super::agent_run::{PendingRound, RunState};

/// 当前快照格式版本。快照结构发生不兼容变化时加 1。
///
/// v2：`AgentRole` 增加到四个变体，序列化形态比 v1 丰富，v1 快照被显式拒绝。
///
/// v3：快照描述整个任务，不再只是单个 runner。v2 快照没有任务上下文，
/// 恢复时无法判断当前进度，因此拒绝加载。
pub const RUN_FORMAT: u32 = 3;

/// 当前任务快照格式版本。
///
/// 与 [`RUN_FORMAT`] 分开：两者存放在不同目录（`.qcodeharness/snapshots/`
/// 与 `.qcodeharness/tasks/`），版本号独立演进。两边的结构互不依赖，
/// 共用版本号只会迫使一边跟着另一边改。
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
    /// 规格草图里没有它，但恢复必须用回同一份系统提示词；换提示词会让
    /// 另一个 agent 接手半截任务。
    pub spec: AgentSpec,
    /// 完整会话历史。
    pub session: SessionSnapshot,
    /// 状态机停在哪。
    pub state: RunState,
    /// 已消耗的轮数与耗时。
    pub consumption: Consumption,
    /// 工具已跑完但尚未提交的半轮。`None` 表示不在调用处理中途。
    pub pending: Option<PendingRound>,
}

/// 任务的可序列化快照：任务级恢复所需的全部状态。
///
/// 与 [`RunSnapshot`] 的分工：`RunSnapshot` 描述单个 runner 停在哪，
/// 任务快照描述任务做到哪。后者多两样东西：
///
/// - `task`：任务进度，`TaskState::Running { step }` 是进度的唯一来源。
///   缺少它，恢复时不知道从第几步继续。
/// - `completed_outputs`：各步结论。缺少它，恢复后的下一步看不到前面的
///   结果，只能从零重推，长任务恢复后容易跑偏。
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
    /// 保存快照时的工作区清单。
    ///
    /// 恢复时与当前清单比对，判断工作区是否被改过。快照里**必须**保留它；
    /// 没有基准清单，比对只能得到"没变过"。
    #[serde(default)]
    pub worktree: WorktreeManifest,
}

impl TaskSnapshot {
    /// 校验快照版本，不匹配时返回错误。
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
    /// 校验快照版本，不匹配时返回错误。
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

/// 会话快照存储端口：把运行状态写入磁盘并读回。
///
/// 编排器与 CLI 依赖这个抽象；落盘方式（单文件 JSON、SQLite、远端）
/// 由基础设施层决定。
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

/// 任务快照存储端口：把任务级状态写入磁盘并读回。
///
/// 与 [`SessionStore`] 分开：两者的键不同（任务 ID vs 会话 ID），
/// 落盘位置不同（`tasks/` vs `snapshots/`），回答的问题也不同
/// （任务做到哪 vs runner 停在哪）。合并成一个 trait 会迫使只想读会话
/// 快照的调用方也实现任务快照方法。
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

    /// 快照不是合法 JSON，或结构与当前版本不符。
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

    /// 快照可解析，但状态自相矛盾，例如停在半轮却没有待配平的轮次。
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

    /// 构造半轮状态的快照：调用已给出，观察只收到一半。
    /// 恢复语义在此最挑剔，往返必须无损。
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
                model: None,
                effort: None,
                verbosity: None,
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
        // v1 快照：AgentRole 扩展后序列化形态已变，必须明确拒绝。
        // 静默兼容会把不兼容状态带进运行。
        let mut snapshot = mid_round_snapshot();
        snapshot.format_version = 1;
        let error = snapshot.check_version().unwrap_err();
        let StoreError::VersionMismatch { found: 1, expected } = error else {
            panic!("应报版本不匹配")
        };
        assert_eq!(expected, RUN_FORMAT);
    }

    /// v2 快照必须被明确拒绝。
    ///
    /// v2 结构仍可解析，但只描述一个 runner，没有任务上下文，恢复时
    /// 无法判断当前步骤。任务级恢复需要任务上下文，因此拒绝加载，不补空任务。
    #[test]
    fn a_v2_snapshot_is_rejected_after_the_task_level_upgrade() {
        let mut snapshot = mid_round_snapshot();
        snapshot.format_version = 2;
        let error = snapshot.check_version().unwrap_err();
        let StoreError::VersionMismatch { found: 2, expected } = error else {
            panic!("应报版本不匹配，实际 {error}")
        };
        assert_eq!(expected, 3, "当前格式是 v3");
        assert!(error.to_string().contains("v2"), "{error}");
    }

    // ── 任务快照 ────────────────────────────────────────────────────────

    /// 一个两步任务、停在第 2 步、已完成第 1 步的任务快照。
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
                model: None,
                effort: None,
                verbosity: None,
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
        // 进度与结论都在，这是任务级恢复的两个关键。
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

    /// 任务快照里的任务也要通过 `Task` 的重建校验。
    ///
    /// 任务快照是外部可修改的文件。把 `state` 改成越界的进度下标后，
    /// 整个快照必须读取失败，不能带着坏任务进入编排。
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

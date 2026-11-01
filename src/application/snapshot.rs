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
use crate::domain::ids::SessionId;
use crate::domain::session::SessionSnapshot;

use super::agent_run::{PendingRound, RunState};

/// 当前快照格式版本。每次快照结构发生不兼容变化时 +1。
pub const RUN_FORMAT: u32 = 1;

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
}

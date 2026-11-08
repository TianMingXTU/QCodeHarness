//! 应用层错误类型。
//!
//! 统一使用 `thiserror`。这些错误穿过 `AgentRunner::run` 的边界交给 CLI，
//! 调用方按种类分别处理（例如预算耗尽时提示调大 `LLM_MAX_TURNS`），
//! 仅有一串文本则无法区分。

use std::time::Duration;

use crate::application::snapshot::StoreError;
use crate::domain::ids::SessionId;
use crate::domain::session::SessionError;
use crate::domain::task::TaskError;

/// 应用层错误。
#[derive(Debug, thiserror::Error)]
pub enum ApplicationError {
    /// 模型调用失败（网络、鉴权、限流、协议错误）。
    ///
    /// 只保留可读原因，不携带 SDK 错误类型：应用层不依赖模型客户端库，
    /// 底层错误由网关转成文本。
    #[error("模型调用失败: {0}")]
    Model(String),

    /// 单次模型调用超时未响应。与 [`ApplicationError::Model`] 分开报：
    /// 预算只在轮次之间检查，无响应的连接走不到检查点，只能由网关层
    /// 限制单次调用。
    #[error("模型调用超过 {seconds} 秒未响应，已中止；检查网络或服务状态后用 --resume 从断点继续")]
    ModelTimeout {
        /// 超时时限（秒）。
        seconds: u64,
    },

    /// 会话状态被破坏。正常流程不会出现，出现即编程错误。
    #[error("会话状态被破坏: {0}")]
    Session(#[from] SessionError),

    /// 达到最大轮数仍未得到最终答案。
    #[error("已达最大轮数 {max_turns}，任务未完成；请缩小任务范围或调大 LLM_MAX_TURNS")]
    BudgetExhausted {
        /// 配置的最大轮数。
        max_turns: u32,
    },

    /// 墙钟超时。与轮数耗尽分开报：对策不同，前者调大时限或拆小任务，
    /// 后者调大 LLM_MAX_TURNS。一个任务一个会话，编排模式下报的是
    /// 整个任务的耗时。
    #[error("耗时 {elapsed:?} 超过上限 {limit:?}，任务终止；请调大 QCH_MAX_WALL_CLOCK 或拆小任务")]
    WallClockExhausted {
        /// 实际耗时。
        elapsed: Duration,
        /// 配置的上限。
        limit: Duration,
    },

    /// 外部请求取消。取消是协作式的：不打断执行中的工具，在下一轮
    /// 调用模型前生效。历史完整保留，可从断点继续。
    #[error("运行被取消")]
    Cancelled,

    /// 模型返回了既无文本也无工具调用的回复。
    #[error("模型返回了空回复（既无文本也无工具调用），任务终止")]
    EmptyResponse,

    /// 会话历史为空，无法构造请求。
    #[error("对话历史为空，无法构造请求")]
    EmptyHistory,

    /// 内部状态错误：三个"正在处理本轮调用"的状态下必须存在待配平的轮次。
    ///
    /// 正常流程不会出现；出现说明 `RunState` 被外部篡改或状态机被跳过。
    #[error("内部状态错误：没有待配平的轮次")]
    NoPendingRound,

    /// 快照读写失败。恢复路径上出现即终止：快照损坏后继续运行，
    /// 会在未知历史上做决策。
    #[error("快照存储失败: {0}")]
    Snapshot(#[from] StoreError),

    /// 任务状态机拒绝。编排器驱动 `Task` 时才会遇到它。
    #[error("任务状态错误: {0}")]
    Task(#[from] TaskError),

    /// 工作区自快照以来被改动过，恢复前必须由人确认。
    ///
    /// 该错误可恢复：改动可能来自上一个 agent 的半成品，用户也可能
    /// 清楚自己在改什么。但必须有人确认，因为静默继续会让恢复的模型
    /// 拿着过期计划继续修改，覆盖用户刚做的改动。
    ///
    /// `changes` 列出差异，用户确认前能看到具体改动。
    #[error("工作区在任务中断后被改动过，恢复前需要确认：\n{}", format_changes(changes))]
    WorktreeChanged {
        /// 差异清单，每条一行。
        changes: Vec<String>,
    },

    /// 角色没有档案，该步的 runner 无法装配。花名册是装配的必要输入，
    /// 缺失时显式失败。静默退回默认角色会让模型使用错误的提示词与
    /// 工具集。
    #[error("第 {step} 步的角色 {role} 没有档案，无法装配")]
    ProfileMissing {
        /// 失败的步骤（1-based，给人看）。
        step: usize,
        /// 缺档案的角色标识。
        role: String,
    },

    /// 编排中某一步失败。`message` 保留原错误与对策指引，外层标明
    /// 失败的步骤，调用方据此定位流程断点。会话 ID 与已完成步数一并
    /// 带出，失败时即可按 ID 查证据或 `--resume`，无需再执行
    /// `--list-sessions`。
    #[error("{message}")]
    StepFailed {
        /// 失败的步骤（1-based，给人看）。
        step: usize,
        /// 失败原因，含原错误信息。
        message: String,
        /// 本次任务的会话 ID。一个任务只有一个会话，证据文件与快照
        /// 都按它命名，失败的那一步也写在同一文件。
        session_id: SessionId,
        /// 本次运行中已完成的步骤数（失败的那步不算）。
        steps_completed: usize,
    },

    /// 证据文件初始化失败。与快照错误同理：没有证据的运行无法复盘，
    /// 宁可让这一步失败，也不静默丢弃审计。
    #[error("证据初始化失败: {0}")]
    EvidenceInit(String),

    /// 历史摘要生成失败。摘要只是优化：调用方应降级为占位说明并继续，
    /// 不要把这个错误抛给任务。
    #[error("历史摘要生成失败: {0}")]
    SummaryFailed(String),
}

/// 把工作区差异清单排成多行文本。
///
/// 每条一行并缩进。差异可能有几十条，单行显示会让用户看不全，
/// 也就无法据以确认。
fn format_changes(changes: &[String]) -> String {
    changes
        .iter()
        .map(|change| format!("  - {change}"))
        .collect::<Vec<_>>()
        .join("\n")
}

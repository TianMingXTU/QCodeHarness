//! 应用层错误类型。
//!
//! 统一用 `thiserror` 而不是 `anyhow`：这些错误要穿过 `AgentRunner::run`
//! 的边界给 CLI 打印，调用方需要按种类区分处理（例如预算耗尽提示调大
//! `LLM_MAX_TURNS`），而不是只拿到一串字符串。

use std::time::Duration;

use rig::error::ProviderError;

use crate::application::snapshot::StoreError;
use crate::domain::ids::SessionId;
use crate::domain::session::SessionError;
use crate::domain::task::TaskError;

/// 应用层错误。
#[derive(Debug, thiserror::Error)]
pub enum ApplicationError {
    /// 模型调用失败（网络、鉴权、限流、协议错误）。
    #[error("模型调用失败: {0}")]
    Model(#[from] ProviderError),

    /// 单次模型调用超过时限仍未响应。与 [`ApplicationError::Model`] 分开报：
    /// 预算只在轮与轮之间检查，挂死的连接永远走不到检查点，必须在
    /// 网关层把单次调用兜住。
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

    /// 墙钟超时。与轮数耗尽分开报：两者的对策不同——前者调大时限
    /// 或拆小任务，后者调大 LLM_MAX_TURNS。P9-3 起一个任务一个会话，
    /// 编排模式下这里报的是整个任务的耗时。
    #[error("耗时 {elapsed:?} 超过上限 {limit:?}，任务终止；请调大 QCH_MAX_WALL_CLOCK 或拆小任务")]
    WallClockExhausted {
        /// 实际耗时。
        elapsed: Duration,
        /// 配置的上限。
        limit: Duration,
    },

    /// 外部请求取消。取消是协作式的：不打断执行中的工具，停点在下一轮
    /// 问模型之前；历史完整保留，可从断点继续。
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
    /// 正常流程不会出现；真出现说明 `RunState` 被外部篡改或状态机被跳过。
    #[error("内部状态错误：没有待配平的轮次")]
    NoPendingRound,

    /// 快照读写失败。恢复路径上出现即终止：快照坏了还继续跑，
    /// 等于拿着未知的历史做决策。
    #[error("快照存储失败: {0}")]
    Snapshot(#[from] StoreError),

    /// 任务状态机拒绝。编排器驱动 `Task` 时才会撞上它。
    #[error("任务状态错误: {0}")]
    Task(#[from] TaskError),

    /// 角色没有档案，该步的 runner 无法装配。花名册是装配的必要输入，
    /// 缺了就该显式失败——静默退回默认角色会让模型拿着错误的提示词
    /// 与工具集上场。
    #[error("第 {step} 步的角色 {role} 没有档案，无法装配")]
    ProfileMissing {
        /// 失败的步骤（1-based，给人看）。
        step: usize,
        /// 缺档案的角色标识。
        role: String,
    },

    /// 编排中某一步失败。原错误的对策指引在 `message` 里，
    /// 外层再标明是哪一步，调用方才知道断在流程的哪个位置。
    /// 会话 ID 与已完成步数一并带出：失败时刻就能按 ID 翻证据或
    /// `--resume`，不必再 `--list-sessions` 里找。
    #[error("{message}")]
    StepFailed {
        /// 失败的步骤（1-based，给人看）。
        step: usize,
        /// 失败原因，含原错误信息。
        message: String,
        /// 本次任务的会话 ID。P9-3 起一个任务只有一个会话，证据文件与
        /// 快照都按它命名——失败的那一步也在同一个文件里。
        session_id: SessionId,
        /// 本次运行中已完成的步骤数（失败的那步不算）。
        steps_completed: usize,
    },

    /// 证据文件初始化失败。与快照错误同理：没有证据的运行不可复盘，
    /// 宁可让这一步失败也不要静默丢审计。
    #[error("证据初始化失败: {0}")]
    EvidenceInit(String),

    /// 历史摘要生成失败。摘要只是优化：调用方应降级为占位说明并继续，
    /// 而不是把这个错误抛给任务。
    #[error("历史摘要生成失败: {0}")]
    SummaryFailed(String),
}

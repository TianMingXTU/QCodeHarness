//! 应用层错误类型。
//!
//! 统一用 `thiserror` 而不是 `anyhow`：这些错误要穿过 `AgentRunner::run`
//! 的边界给 CLI 打印，调用方需要按种类区分处理（例如预算耗尽提示调大
//! `LLM_MAX_TURNS`），而不是只拿到一串字符串。

use std::time::Duration;

use rig::error::ProviderError;

use crate::domain::session::SessionError;

/// 应用层错误。
#[derive(Debug, thiserror::Error)]
pub enum ApplicationError {
    /// 模型调用失败（网络、鉴权、限流、协议错误）。
    #[error("模型调用失败: {0}")]
    Model(#[from] ProviderError),

    /// 会话状态被破坏。正常流程不会出现，出现即编程错误。
    #[error("会话状态被破坏: {0}")]
    Session(#[from] SessionError),

    /// 达到最大轮数仍未得到最终答案。
    #[error("已达最大轮数 {max_turns}，任务未完成；请缩小任务范围或调大 LLM_MAX_TURNS")]
    BudgetExhausted {
        /// 配置的最大轮数。
        max_turns: u32,
    },

    /// 整体墙钟超时。与轮数耗尽分开报：两者的对策不同——前者调大时限
    /// 或拆小任务，后者调大 LLM_MAX_TURNS。
    #[error("整体耗时 {elapsed:?} 超过上限 {limit:?}，任务终止；请调大 QCH_MAX_WALL_CLOCK 或拆小任务")]
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
}

//! 证据抽象：可审计的操作记录。
//!
//! 每次思考、调用、观察都留下证据，供后续审查或回放。P0 只落在内存里，
//! P2 换成 JSONL 持久化——因为事件类型在领域层定好了，
//! 换存储不用改产生事件的代码。

use std::time::SystemTime;

use super::tool::{ToolCall, ToolOutcome};

/// 一次模型调用的 token 用量。
///
/// 从 rig 的 `Usage` 平移而来。目前只取输入/输出两个计数；
/// rig 还算 cost 等派生值，等真要按成本设预算时再补。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenUsage {
    /// 输入 token 数（含系统提示与历史）。
    pub input_tokens: u64,
    /// 输出 token 数。
    pub output_tokens: u64,
}

/// 可审计事件。每条都带轮次号，便于和日志、会话历史对齐。
#[derive(Debug, Clone, PartialEq)]
pub enum AuditEvent {
    /// 模型产出了一轮思考。文本可能为空（模型直接给调用）。
    Thought {
        /// 轮次号，从 1 开始。
        turn: u32,
        /// 模型输出的文本。
        text: String,
        /// 本轮 token 用量。
        usage: TokenUsage,
    },
    /// 模型提出了工具调用意图，尚未执行。
    ToolRequested {
        /// 轮次号。
        turn: u32,
        /// 调用意图。
        call: ToolCall,
    },
    /// 工具执行完毕，得到观察结果。
    ToolObserved {
        /// 轮次号。
        turn: u32,
        /// 被执行的调用。
        call: ToolCall,
        /// 观察结果。
        outcome: ToolOutcome,
    },
    /// 审批结论。P0 恒为放行，但记录本身是审计链的一环。
    ApprovalDecided {
        /// 轮次号。
        turn: u32,
        /// 被审批的工具名。
        tool: String,
        /// 是否放行。
        allowed: bool,
        /// 否决时的原因。
        reason: Option<String>,
    },
}

impl AuditEvent {
    /// 事件所属的轮次号，从 1 开始。
    pub fn turn(&self) -> u32 {
        match self {
            Self::Thought { turn, .. }
            | Self::ToolRequested { turn, .. }
            | Self::ToolObserved { turn, .. }
            | Self::ApprovalDecided { turn, .. } => *turn,
        }
    }
}

/// 一条带序号和时间戳的证据记录。
///
/// 序号由存储方分配，保证同一 sink 内严格递增；时间戳取事件发生时刻
/// 而非落库时刻，所以由产生事件的一方传入。
#[derive(Debug, Clone, PartialEq)]
pub struct Evidence {
    /// sink 内的递增序号，从 1 开始。
    pub sequence: u64,
    /// 事件发生时刻。
    pub recorded_at: SystemTime,
    /// 事件本体。
    pub event: AuditEvent,
}

//! 证据抽象：可审计的操作记录。
//!
//! 每次思考、调用、观察都留下证据，供后续审查或回放。P0 只落在内存里，
//! P2 换成 JSONL 持久化——因为事件类型在领域层定好了，
//! 换存储不用改产生事件的代码。

use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use super::tool::{ToolCall, ToolOutcome};

/// 一次模型调用的 token 用量。
///
/// 从 rig 的 `Usage` 平移而来。目前只取输入/输出两个计数；
/// rig 还算 cost 等派生值，等真要按成本设预算时再补。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    /// 输入 token 数（含系统提示与历史）。
    pub input_tokens: u64,
    /// 输出 token 数。
    pub output_tokens: u64,
}

/// 可审计事件。每条都带轮次号，便于和日志、会话历史对齐。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
    /// 策略引擎对一次调用给出了裁决。
    ///
    /// 与 `ApprovalDecided` 分开记：前者是"闸门问过了"，后者是
    /// "规则判过了"。被 `Deny` 的调用根本不会走到闸门，
    /// 只有这条事件能解释它为什么被拒。
    PolicyEvaluated {
        /// 轮次号。
        turn: u32,
        /// 被裁决的工具名。
        tool: String,
        /// 裁决结论。
        verdict: super::policy::PolicyVerdict,
        /// 命中的规则序号；`None` 表示走的是默认策略。
        rule_index: Option<usize>,
        /// 裁决结论的说明。
        reason: String,
    },
    /// 发给模型前按上下文预算裁剪了历史。
    ///
    /// 与 `Thought` 分开记：后者是"模型说了什么"，这条是"我们让它看到了
    /// 什么"。事后翻 JSONL 要能回答"第 5 轮发给模型的历史里有没有第 1 轮"，
    /// 靠的就是这条事件的 `dropped` 清单。
    ContextCompacted {
        /// 即将进入的轮次号。
        turn: u32,
        /// 裁剪前的完整轮数。
        rounds_before: usize,
        /// 裁剪后保留的轮数。
        rounds_after: usize,
        /// 被丢弃的轮次号。
        dropped: Vec<u32>,
    },
}

impl AuditEvent {
    /// 事件所属的轮次号，从 1 开始。
    pub fn turn(&self) -> u32 {
        match self {
            Self::Thought { turn, .. }
            | Self::ToolRequested { turn, .. }
            | Self::ToolObserved { turn, .. }
            | Self::ApprovalDecided { turn, .. }
            | Self::PolicyEvaluated { turn, .. }
            | Self::ContextCompacted { turn, .. } => *turn,
        }
    }
}

/// 一条带序号和时间戳的证据记录。
///
/// 序号由存储方分配，保证同一 sink 内严格递增；时间戳取事件发生时刻
/// 而非落库时刻，所以由产生事件的一方传入。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Evidence {
    /// sink 内的递增序号，从 1 开始。
    pub sequence: u64,
    /// 事件发生时刻。
    ///
    /// 用秒级 Unix 时间戳序列化，而不是 `SystemTime`——后者没有稳定的
    /// 跨平台序列化格式，而 JSONL 要的是人能读、能 diff 的东西。
    #[serde(with = "unix_seconds")]
    pub recorded_at: SystemTime,
    /// 事件本体。
    pub event: AuditEvent,
}

/// `SystemTime` 与秒级 Unix 时间戳之间的序列化桥。
///
/// 落盘格式用整数秒而不是 RFC 3339 字符串：事件之间的间隔通常远大于 1 秒，
/// 纳秒精度只会让日志更难读；真要亚秒级时序时再换成 chrono。
mod unix_seconds {
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(time: &SystemTime, serializer: S) -> Result<S::Ok, S::Error> {
        let secs = time
            .duration_since(UNIX_EPOCH)
            .unwrap_or(Duration::ZERO)
            .as_secs();
        serializer.serialize_u64(secs)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<SystemTime, D::Error> {
        let secs = u64::deserialize(deserializer)?;
        Ok(UNIX_EPOCH + Duration::from_secs(secs))
    }
}

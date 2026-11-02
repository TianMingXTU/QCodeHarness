//! 证据抽象：可审计的操作记录。
//!
//! 每次思考、调用、观察都留下证据，供后续审查或回放。P0 只落在内存里，
//! P2 换成 JSONL 持久化——因为事件类型在领域层定好了，
//! 换存储不用改产生事件的代码。

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use super::tool::{ToolCall, ToolOutcome};

/// 一次模型调用的 token 用量与成本。
///
/// 从 rig 的 `Usage` 平移而来。成本是 provider 报告的原值，不在领域层
/// 用单价表推算——推算值会随价目表过时而失真，报表宁可显示"未知"。
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct TokenUsage {
    /// 输入 token 数（含系统提示与历史）。
    pub input_tokens: u64,
    /// 输出 token 数。
    pub output_tokens: u64,
    /// 本次调用从 provider 缓存读回的输入 token 数。
    ///
    /// 命中缓存的输入按更低费率计费，也省掉对应的 prefill 时间——
    /// 这是"快"最直接的度量，所以必须单独记：把它并进
    /// `input_tokens` 就看不出缓存有没有生效。
    #[serde(default)]
    pub cached_input_tokens: u64,
    /// 本次调用写入 provider 缓存的输入 token 数。
    ///
    /// 写一次的费率高于普通输入，但后续请求读它更便宜。单独记才能
    /// 算清"这次多花的钱换来了多少次命中"。
    #[serde(default)]
    pub cache_creation_input_tokens: u64,
    /// 本次调用的成本（美元）。`None` 表示 provider 未报告且无法定价。
    ///
    /// 报表必须显示"未知"而不是 0：填 0 会让"免费"和"没查到"混为
    /// 一谈，部分已知的合计是一个被低估的数字，比"未知"更危险。
    #[serde(default)]
    pub cost_usd: Option<f64>,
}

/// 可审计事件。每条都带轮次号，便于和日志、会话历史对齐。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum AuditEvent {
    /// 一步开始执行，记录执行者的角色。
    ///
    /// 一个任务一个会话与证据文件，但 JSONL 里原本没有说明"这一步
    /// 是谁跑的"；
    /// 事后翻证据要能不借助快照就回答这个问题，靠的就是这条事件。
    StepStarted {
        /// 本步第一条模型回复将要占用的轮次号。
        ///
        /// P9-3 前每步一个会话，它恒为 1；单会话编排下一步的开始轮次
        /// 就是前面各步累计的轮数 + 1，所以它不再是常量。
        turn: u32,
        /// 执行这一步的角色。
        role: super::agent::AgentRole,
    },
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
    /// 为被裁历史额外花了一次模型调用生成摘要。
    ///
    /// 事后翻 JSONL 要能回答"这一轮为什么多了一次调用"——没有这条，
    /// token 账对不上；有了它，摘要的覆盖范围与用量都可查。
    SummaryGenerated {
        /// 即将进入的轮次号。
        turn: u32,
        /// 摘要覆盖的轮次范围（含）。
        covers: (u32, u32),
        /// 这次摘要调用的 token 用量。
        usage: TokenUsage,
    },
    /// 摘要调用失败，历史退回占位说明。
    ///
    /// 与 `SummaryGenerated` 成对：摘要是一次真实的模型调用，失败了也是
    /// 花掉的时间与配额——没有这条，JSONL 里只看到多等了一会儿却什么
    /// 都没发生，token 账与耗时都对不上。
    SummaryFailed {
        /// 即将进入的轮次号。
        turn: u32,
        /// 失败原因。
        reason: String,
    },
}

impl AuditEvent {
    /// 事件所属的轮次号，从 1 开始。
    pub fn turn(&self) -> u32 {
        match self {
            Self::StepStarted { turn, .. }
            | Self::Thought { turn, .. }
            | Self::ToolRequested { turn, .. }
            | Self::ToolObserved { turn, .. }
            | Self::ApprovalDecided { turn, .. }
            | Self::PolicyEvaluated { turn, .. }
            | Self::ContextCompacted { turn, .. }
            | Self::SummaryGenerated { turn, .. }
            | Self::SummaryFailed { turn, .. } => *turn,
        }
    }
}

/// 一条带序号和时间戳的证据记录。
///
/// 序号由存储方分配，保证同一 sink 内严格递增；时间戳取事件发生时刻
/// 而非落库时刻，所以由产生事件的一方传入。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Evidence {
    /// sink 内的递增序号，从 1 开始。
    pub sequence: u64,
    /// 本行的格式版本，见 [`EVIDENCE_FORMAT_MILLIS`]。
    ///
    /// 不带默认值：缺省（旧文件）按秒解释，这是 [`Evidence`] 的反序列化
    /// 实现自己处理的，字段本身不允许被写成别的数。
    #[serde(default = "legacy_format")]
    pub format: u32,
    /// 事件发生时刻。
    ///
    /// 精度由 [`Evidence::format`] 决定：缺省（0）是秒，[`EVIDENCE_FORMAT_MILLIS`]
    /// 是毫秒。序列化时始终写毫秒。
    #[serde(serialize_with = "unix_millis::serialize")]
    pub recorded_at: SystemTime,
    /// 事件本体。
    pub event: AuditEvent,
}

/// 旧证据行的格式号：`recorded_at` 是整秒。
///
/// P9 之前全部证据都是这个格式。读取时靠"字段缺失"识别——
/// 老文件里根本没有 `format` 这个键。
pub const EVIDENCE_FORMAT_SECONDS: u32 = 0;

/// 现行证据行的格式号：`recorded_at` 是毫秒。
///
/// P9-1 把精度从秒提到毫秒，因为"每轮模型调用花了多久"是本次唯一要
/// 优化的量，而一次调用动辄几秒、快的时候几百毫秒——秒级精度下
/// 快的那些轮次全被记成 0，延迟分布根本画不出来。
pub const EVIDENCE_FORMAT_MILLIS: u32 = 2;

/// 缺省格式号：老文件没有 `format` 键，按秒解释。
fn legacy_format() -> u32 {
    EVIDENCE_FORMAT_SECONDS
}

impl Evidence {
    /// 构造一条现行格式（毫秒）的证据记录。
    ///
    /// 新代码一律走这里，不直接拼结构体：格式号与精度必须一起给，
    /// 分开给就会出现"标着毫秒却写秒"或反之的错配。
    pub fn new(sequence: u64, recorded_at: SystemTime, event: AuditEvent) -> Self {
        Self {
            sequence,
            format: EVIDENCE_FORMAT_MILLIS,
            recorded_at,
            event,
        }
    }

    /// 事件发生时刻的毫秒表示。
    ///
    /// 只做一次换算，且**不再看格式号**：反序列化时秒级的旧文件已经乘过
    /// 1000 存进 `recorded_at`（见本文件里 `Deserialize` 的实现），这里
    /// 再按格式补一次就会把旧文件的耗时放大一千倍。而墙钟走另一条路
    /// （直接读 `recorded_at`），两列会因此互相矛盾：墙钟说 60 秒，
    /// 轮延迟说 6000 万毫秒。
    pub fn recorded_at_millis(&self) -> u64 {
        self.recorded_at
            .duration_since(UNIX_EPOCH)
            .unwrap_or(Duration::ZERO)
            .as_millis() as u64
    }
}

/// `Evidence` 的反序列化：按行自带的格式号解释 `recorded_at`。
///
/// 不手写这个实现而靠 `#[serde(with)]` 做不到：时间戳的解释依赖同一行
/// 里的另一个字段，派生宏看不到跨字段关系。这是本模块唯一手写
/// `Deserialize` 的原因。
///
/// 格式号不认识时返回错误而不是猜：reader 会跳过该行并计数。猜的代价
/// 是让所有耗时统计悄悄变成垃圾，而跳过至少能被 `skipped_lines`
/// 发现——与"末行截断跳过而非报错"是同一条降级哲学。
impl<'de> Deserialize<'de> for Evidence {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Raw {
            sequence: u64,
            #[serde(default = "legacy_format")]
            format: u32,
            recorded_at: u64,
            event: AuditEvent,
        }
        let raw = Raw::deserialize(deserializer)?;
        let millis = match raw.format {
            EVIDENCE_FORMAT_SECONDS => raw.recorded_at.saturating_mul(1000),
            EVIDENCE_FORMAT_MILLIS => raw.recorded_at,
            other => {
                return Err(serde::de::Error::custom(format!(
                    "不认识的证据格式号 {other}（已知 {} 秒 / {} 毫秒）",
                    EVIDENCE_FORMAT_SECONDS, EVIDENCE_FORMAT_MILLIS
                )));
            }
        };
        Ok(Self {
            sequence: raw.sequence,
            format: raw.format,
            recorded_at: UNIX_EPOCH + Duration::from_millis(millis),
            event: raw.event,
        })
    }
}

/// `SystemTime` 与毫秒 Unix 时间戳之间的序列化桥。
///
/// 落盘格式用整数毫秒而不是 RFC 3339 字符串：事件之间的间隔常常只有
/// 几百毫秒（一次快的模型调用），纳秒精度对 diff 没有意义，
/// 而毫秒足够画出每轮延迟分布。
mod unix_millis {
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use serde::Serializer;

    pub fn serialize<S: Serializer>(time: &SystemTime, serializer: S) -> Result<S::Ok, S::Error> {
        let millis = time
            .duration_since(UNIX_EPOCH)
            .unwrap_or(Duration::ZERO)
            .as_millis() as u64;
        serializer.serialize_u64(millis)
    }
}

//! 证据抽象：可审计的操作记录。
//!
//! 每次思考、调用与观察都留下证据，供审查或回放，以 JSONL 持久化。
//! 事件类型定义在领域层，更换存储不影响产生事件的代码。

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use super::ids::CallId;
use super::mcp::{McpCallFailure, McpLifecycleFact};
use super::tool::{ToolCall, ToolOutcome};

/// 一次模型调用的 token 用量与成本。
///
/// 来自 rig 的 `Usage`。成本使用 provider 报告的原值，不在领域层按
/// 单价表推算：推算值会随价目表过时，报表宁可显示"未知"。
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct TokenUsage {
    /// 输入 token 数（含系统提示与历史）。
    pub input_tokens: u64,
    /// 输出 token 数。
    pub output_tokens: u64,
    /// 本次调用从 provider 缓存读回的输入 token 数。
    ///
    /// 缓存命中按更低费率计费，并省掉对应的 prefill 时间，是衡量速度
    /// 的直接指标，必须单独记录。并入 `input_tokens` 后无法判断缓存
    /// 是否生效。
    #[serde(default)]
    pub cached_input_tokens: u64,
    /// 本次调用写入 provider 缓存的输入 token 数。
    ///
    /// 写入费率高于普通输入，后续读取更便宜。单独记录才能计算多花的
    /// 费用换来了多少次命中。
    #[serde(default)]
    pub cache_creation_input_tokens: u64,
    /// 本次调用的成本（美元）。`None` 表示 provider 未报告且无法定价。
    ///
    /// 报表显示"未知"，不填 0。填 0 会混淆"免费"与"未查到"，且部分
    /// 已知的合计会被低估，比"未知"更容易误导。
    #[serde(default)]
    pub cost_usd: Option<f64>,
}

/// 可审计事件。每条都带轮次号，便于和日志、会话历史对齐。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum AuditEvent {
    /// 一步开始执行，记录执行者的角色。
    ///
    /// 一个任务对应一个会话与一份证据文件。事后查看证据时无需借助
    /// 快照即可确认每一步的执行者，依据是这条事件。
    StepStarted {
        /// 本步第一条模型回复占用的轮次号。
        ///
        /// 单会话编排下为前面各步累计轮数 + 1，随步骤变化。
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
    /// 审批结论。记录本身是审计链的一环。
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
    /// 与 `ApprovalDecided` 分开记录：`ApprovalDecided` 记录审批闸门的
    /// 结论。被 `Deny` 的调用不会走到闸门，只有这条事件能说明拒绝原因。
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
    /// 与 `Thought` 分开记录：后者记录模型说了什么，这条记录模型看到
    /// 了什么。`dropped` 清单可用于事后确认每轮保留的历史范围。
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
    /// 按 token 预算裁剪历史的记录。
    ///
    /// 与 [`AuditEvent::ContextCompacted`] 分开记录：那条记录裁剪轮数，
    /// 这条记录节省的 token。轮数是手段，token 是实际成本。同样裁掉
    /// 3 轮，可能只省两百个 token，也可能省两万个。
    ContextBudgeted {
        /// 即将进入的轮次号。
        turn: u32,
        /// 裁剪前的历史 token 估算。
        tokens_before: u64,
        /// 裁剪后实际发出的 token 估算。
        tokens_after: u64,
        /// 净减掉的 token 数。
        dropped_tokens: u64,
    },
    /// 为被裁历史额外花了一次模型调用生成摘要。
    ///
    /// 没有这条事件，token 账目对不上多出的调用。摘要的覆盖范围与
    /// 用量记录在事件中，可事后核对。
    SummaryGenerated {
        /// 即将进入的轮次号。
        turn: u32,
        /// 摘要覆盖的轮次范围（含）。
        covers: (u32, u32),
        /// 这次摘要调用的 token 用量。
        usage: TokenUsage,
    },
    /// 恢复时提示模型：某个调用在中断前可能已经执行过。
    ///
    /// 与 `ToolObserved` 互补：后者记录已完成的调用与结果，这条表示
    /// 调用**是否**已执行无法确认、结果未知。
    ///
    /// 中断发生在调用发起之后、观察记录之前，副作用可能已经发生
    /// （文件已写入、命令已执行）且无法确认。恢复路径**不自动重跑**，
    /// 否则同一次副作用会执行两遍。同时不能忽略这一情况，因此把事实
    /// 告知模型，由模型决定是否重新调用。
    ToolCallReplayed {
        /// 轮次号。
        turn: u32,
        /// 是否执行过无法确认的工具名。
        tool: String,
        /// 为什么会有这条提示。
        reason: String,
        /// 同一调用（工具名与参数完全一致）此前执行过的结果。
        ///
        /// `None` 表示没有可参照的先例，或中断发生在执行期间、结果未知。
        previous_outcome: Option<ToolOutcome>,
    },
    /// 恢复时重新询问了一次审批。
    ///
    /// 审批请求本身不落进事件流，只有"询问"与"答复"两类事件。从快照
    /// 恢复时，上一次询问发生在**上一个进程**，当前证据文件里没有记录。
    /// 缺少这条事件，事后查看 JSONL 会认为用户只被询问过一次，实际是
    /// 两次，第一次未答复。
    ApprovalReasked {
        /// 轮次号。
        turn: u32,
        /// 被重新询问的工具名。
        tool: String,
        /// 重新询问的理由。
        reason: String,
    },
    /// 本地子进程或外部 MCP 调用的执行边界记录。
    ///
    /// `run_command`、`git_status`、`git_diff` 只在实际启动子进程后记录，
    /// 启动前的拒绝或失败不生成事件。MCP 工具记录将调用交给外部 Server
    /// 的尝试，不表示服务端已执行或调用成功；有进程沙箱的平台（Windows）
    /// 在握手前把该进程纳入同一个 Job（见 `active`）。
    ///
    /// 有进程沙箱的平台在 Job 创建失败时拒绝连接，不生成此事件。其他平台
    /// 没有进程沙箱，连接照常建立，本事件照发，`active` 为 false。
    ///
    /// 与 `ToolObserved` 分开记录：本条描述进程管理与调用前检查，工具
    /// 结果仍由 `ToolObserved` 表达。两者都不承诺文件系统隔离。
    SandboxApplied {
        /// 轮次号。
        turn: u32,
        /// 本次执行或尝试交给外部 Server 的调用。
        call: ToolCall,
        /// 执行边界标记：`"Windows JobObject"` / `"Linux"` / `"macOS"` /
        /// `"其他系统"` / `"MCP subprocess"`。
        ///
        /// 同一会话可以同时包含本地子进程与外部 MCP 调用，标签按调用记录。
        platform: String,
        /// 本 harness 的进程限额与进程树管理是否实际生效。
        ///
        /// 有进程沙箱的平台（Windows）：子进程成功纳入 Job 后为 true。
        /// 启动后 `assign` 失败仍记录本事件，值为 false；启动前 Job 创建
        /// 失败则拒绝执行，不生成事件。MCP 调用在握手前纳入 Job 后为
        /// true，此后该进程创建的子孙一并受管。
        ///
        /// 其他平台没有进程沙箱：命令照跑、事件照发，值恒为 false。PID
        /// 已失效或纳入失败时也是 false。
        active: bool,
        /// `run_command` 是否执行过命令字面路径检查。
        ///
        /// 不覆盖脚本运行时计算的路径，也不提供文件系统隔离。
        /// `git_status`、`git_diff` 与 MCP 调用为 false；文件类工具不
        /// 发送此事件。
        boundary_checked: bool,
    },
    /// 摘要调用失败，历史退回占位说明。
    ///
    /// 与 `SummaryGenerated` 成对。摘要是一次真实的模型调用，失败同样
    /// 消耗时间与配额。缺少这条事件，token 账目与耗时就对不上。
    SummaryFailed {
        /// 即将进入的轮次号。
        turn: u32,
        /// 失败原因。
        reason: String,
    },
    /// MCP Server 的一次连接级事实（建立、断开、恢复或关闭）。
    ///
    /// 与 `ToolObserved`、`SandboxApplied` 分开记录：启动失败没有工具
    /// 调用，不伪造观察或执行边界。同一连接的多个工具共享一个
    /// generation，断连只记一次。连接事件用 `turn: 0` 表示不属于任何
    /// 模型轮次。
    McpLifecycle {
        /// 恒为 0，见变体说明。
        turn: u32,
        /// 连接事实，只含 Server 标识、故障分类与真实进程保护状态。
        fact: McpLifecycleFact,
    },
    /// 一次 MCP 工具调用的结构化失败事实。
    ///
    /// 观察文本仍由 `ToolObserved` 承载。本条为审计和报表提供分类，
    /// 包括"请求已发出、远端结果未知"这种不能自动重放的状态。
    McpCallFailed {
        /// 轮次号。
        turn: u32,
        /// 触发故障的调用 ID，用于和 `ToolRequested` 关联。
        call_id: CallId,
        /// 被调工具名。
        tool: String,
        /// 执行层提供的故障事实。
        failure: McpCallFailure,
    },
}

impl AuditEvent {
    /// 事件所属的轮次号，从 1 开始；连接级 `McpLifecycle` 事件不属于任何
    /// 模型轮次，恒为 0。
    pub fn turn(&self) -> u32 {
        match self {
            Self::StepStarted { turn, .. }
            | Self::Thought { turn, .. }
            | Self::ToolRequested { turn, .. }
            | Self::ToolObserved { turn, .. }
            | Self::SandboxApplied { turn, .. }
            | Self::ToolCallReplayed { turn, .. }
            | Self::ApprovalReasked { turn, .. }
            | Self::ApprovalDecided { turn, .. }
            | Self::PolicyEvaluated { turn, .. }
            | Self::ContextCompacted { turn, .. }
            | Self::ContextBudgeted { turn, .. }
            | Self::SummaryGenerated { turn, .. }
            | Self::SummaryFailed { turn, .. }
            | Self::McpLifecycle { turn, .. }
            | Self::McpCallFailed { turn, .. } => *turn,
        }
    }
}

/// 一条带序号和时间戳的证据记录。
///
/// 序号由存储方分配，在同一 sink 内严格递增。时间戳取事件发生时刻
/// 而非写入时刻，由产生事件的一方传入。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Evidence {
    /// sink 内的递增序号，从 1 开始。
    pub sequence: u64,
    /// 本行的格式版本，见 [`EVIDENCE_FORMAT_MILLIS`]。
    ///
    /// 字段缺省（旧文件）时按秒解释，由 [`Evidence`] 的反序列化实现
    /// 处理。字段本身不接受未知取值。
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

/// 旧证据行的格式号：`recorded_at` 为整秒。
///
/// 读取时靠字段缺失识别：旧文件没有 `format` 键。
pub const EVIDENCE_FORMAT_SECONDS: u32 = 0;

/// 现行证据行的格式号：`recorded_at` 为毫秒。
///
/// 每轮模型调用耗时需要作为指标呈现：调用通常耗时几秒，快时只有几百
/// 毫秒。秒级精度下较快的轮次记为 0，无法画出延迟分布。
pub const EVIDENCE_FORMAT_MILLIS: u32 = 2;

/// 缺省格式号：老文件没有 `format` 键，按秒解释。
fn legacy_format() -> u32 {
    EVIDENCE_FORMAT_SECONDS
}

impl Evidence {
    /// 构造一条现行格式（毫秒）的证据记录。
    ///
    /// 新代码统一使用此构造函数，不直接拼结构体。格式号与精度必须
    /// 一起设置，分开设置会产生"标记毫秒、写入秒"之类的错配。
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
    /// 只做一次换算，**不再读取格式号**。反序列化时秒级的旧文件已经
    /// 乘 1000 存进 `recorded_at`（见下方 `Deserialize` 实现），这里再
    /// 按格式换算会放大一千倍。墙钟直接读 `recorded_at`，两列会互相
    /// 矛盾：墙钟 60 秒，轮延迟 6000 万毫秒。
    pub fn recorded_at_millis(&self) -> u64 {
        self.recorded_at
            .duration_since(UNIX_EPOCH)
            .unwrap_or(Duration::ZERO)
            .as_millis() as u64
    }
}

/// `Evidence` 的反序列化：按行内的格式号解释 `recorded_at`。
///
/// `#[serde(with)]` 无法表达这一逻辑：时间戳的解释依赖同一行的另一个
/// 字段，派生宏看不到跨字段关系。这是本模块唯一手写 `Deserialize`
/// 的原因。
///
/// 格式号未知时返回错误，不做猜测。读取方会跳过该行并计数，跳过至少
/// 能被 `skipped_lines` 发现；猜测会让所有耗时统计失真。这与"末行
/// 截断时跳过而非报错"是同一条降级策略。
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
/// 落盘格式使用整数毫秒，不用 RFC 3339 字符串。事件间隔常只有几百
/// 毫秒，毫秒精度足以分析每轮延迟分布，纳秒精度没有意义。
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

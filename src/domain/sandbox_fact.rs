//! R2 沙箱事实：用纯数据表达规则判定、分切片能力与降级证据。
//!
//! 本模块不读系统、不起进程、不访问文件。真实能力由 infrastructure 采集
//! 或施放后填进这里，调用方据此决定是否允许执行以及要写哪条证据。
//! 隔离能力没有证明生效时必须带 `SandboxDegradedFact`，不能只给 Allow。

use std::fmt::{self, Write as _};

use serde::{Deserialize, Serialize};

/// 证据账本中降级事件的稳定名称，供后续 `AuditEvent` 变体接线使用。
pub const SANDBOX_DEGRADED_EVENT: &str = "SandboxDegraded";
/// 降级事实的稳定码，避免调用方靠中文理由文本判断。
pub const SANDBOX_DEGRADED_STABLE_CODE: &str = "sandbox_degraded";

/// 一次沙箱判定的三种结局。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SandboxOutcome {
    /// 已证明隔离生效，可以进入后续审批与执行。
    Allow,
    /// 规则或策略拒绝，不允许执行。
    Deny,
    /// 隔离能力未生效或未实测，必须落降级证据，不允许按正常执行处理。
    Degrade,
}

impl SandboxOutcome {
    pub fn label(&self) -> &'static str {
        match *self {
            Self::Allow => "放行",
            Self::Deny => "拒绝",
            Self::Degrade => "降级",
        }
    }
}

/// 隔离能力切片。Windows R2 按文件、网络、进程树、MCP 子进程分别记账。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SandboxCapability {
    ProcessTreeKill,
    FileSystem,
    Network,
    McpSubprocess,
}

impl SandboxCapability {
    pub fn label(&self) -> &'static str {
        match *self {
            Self::ProcessTreeKill => "进程树终止",
            Self::FileSystem => "文件系统隔离",
            Self::Network => "网络隔离",
            Self::McpSubprocess => "MCP 子进程隔离",
        }
    }
}

/// 单个切片的结论。未实测与未生效分开，避免把本机没跑过的平台写成通过。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SandboxSliceStatus {
    Effective,
    Ineffective,
    Untested,
}

impl SandboxSliceStatus {
    pub fn label(&self) -> &'static str {
        match *self {
            Self::Effective => "生效",
            Self::Ineffective => "未生效",
            Self::Untested => "未实测",
        }
    }
}

/// 进程来源。`isolate_child_spec()` 后续用它区分内置命令与 MCP 子进程。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProcessOrigin {
    BuiltinCommand,
    McpSubprocess,
}

impl ProcessOrigin {
    pub fn label(&self) -> &'static str {
        match *self {
            Self::BuiltinCommand => "内置命令",
            Self::McpSubprocess => "MCP 子进程",
        }
    }
}

/// `SandboxEnforcer::apply` 的输入。字段是纯值，构造方负责把路径转成字符串。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessSpec {
    pub program: String,
    pub arguments: Vec<String>,
    pub workspace: String,
    pub command: String,
    pub origin: ProcessOrigin,
}

/// 一条命中的规则。规则号给调试输出与测试断言用，理由写给工程师看。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuleHit {
    pub rule_id: String,
    pub reason: String,
}

impl RuleHit {
    pub fn new(rule_id: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            rule_id: rule_id.into(),
            reason: reason.into(),
        }
    }
}

/// 对命令的判定。`matched_rules` 保留全部命中项，便于 `--sandbox-debug` 解释。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxDecision {
    pub outcome: SandboxOutcome,
    pub matched_rules: Vec<RuleHit>,
    pub reason: String,
    pub stable_code: Option<String>,
}

impl SandboxDecision {
    pub fn allow(matched_rules: Vec<RuleHit>, reason: impl Into<String>) -> Self {
        Self::new(SandboxOutcome::Allow, matched_rules, reason)
    }

    pub fn deny(matched_rules: Vec<RuleHit>, reason: impl Into<String>) -> Self {
        Self::new(SandboxOutcome::Deny, matched_rules, reason)
    }

    pub fn degrade(matched_rules: Vec<RuleHit>, reason: impl Into<String>) -> Self {
        Self::new(SandboxOutcome::Degrade, matched_rules, reason)
    }

    pub fn with_stable_code(mut self, stable_code: impl Into<String>) -> Self {
        self.stable_code = Some(stable_code.into());
        self
    }

    fn new(
        outcome: SandboxOutcome,
        matched_rules: Vec<RuleHit>,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            outcome,
            matched_rules,
            reason: reason.into(),
            stable_code: None,
        }
    }
}

/// 一个隔离切片的结构化事实。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxSliceFact {
    pub capability: SandboxCapability,
    pub status: SandboxSliceStatus,
    /// 本切片由什么平台机制承担，或为什么没有实测。
    pub backing: String,
    pub reason: String,
}

impl SandboxSliceFact {
    pub fn new(
        capability: SandboxCapability,
        status: SandboxSliceStatus,
        backing: impl Into<String>,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            capability,
            status,
            backing: backing.into(),
            reason: reason.into(),
        }
    }
}

/// 降级证据的纯数据结构。后续 `AuditEvent::SandboxDegraded` 直接承载它。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxDegradedFact {
    pub evidence_event: String,
    pub stable_code: String,
    pub reason: String,
    pub slices: Vec<SandboxSliceFact>,
}

impl SandboxDegradedFact {
    /// 由未生效或未实测的切片生成降级事实。调用方必须在 ToolGuard 链中
    /// 把返回值写进证据账本，再停止或走已审批的有限放行分支。
    pub fn from_slices(reason: impl Into<String>, slices: Vec<SandboxSliceFact>) -> Self {
        Self {
            evidence_event: SANDBOX_DEGRADED_EVENT.to_owned(),
            stable_code: SANDBOX_DEGRADED_STABLE_CODE.to_owned(),
            reason: reason.into(),
            slices,
        }
    }

    /// 列出未生效或未实测切片，供 --doctor 与 --report 如实呈现。
    pub fn ineffective_slices(&self) -> impl Iterator<Item = &SandboxSliceFact> {
        self.slices
            .iter()
            .filter(|slice| slice.status != SandboxSliceStatus::Effective)
    }
}

/// 一次执行或调试得到的完整沙箱事实。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxFact {
    pub platform: String,
    pub command: String,
    pub origin: ProcessOrigin,
    pub decision: SandboxDecision,
    pub slices: Vec<SandboxSliceFact>,
    /// 能力未生效、未实测或判定为 Degrade 时必填。
    pub degraded: Option<SandboxDegradedFact>,
}

impl SandboxFact {
    /// 调试路径的事实。调试不施放隔离，调用方只用它渲染报告。
    pub fn from_debug(
        platform: impl Into<String>,
        command: impl Into<String>,
        decision: SandboxDecision,
        slices: Vec<SandboxSliceFact>,
    ) -> Self {
        Self::from_facts(
            platform,
            command,
            ProcessOrigin::BuiltinCommand,
            decision,
            slices,
        )
    }

    /// 执行路径的事实。`origin` 标明内置命令还是 MCP 子进程。
    pub fn from_apply(
        platform: impl Into<String>,
        command: impl Into<String>,
        origin: ProcessOrigin,
        decision: SandboxDecision,
        slices: Vec<SandboxSliceFact>,
    ) -> Self {
        Self::from_facts(platform, command, origin, decision, slices)
    }

    /// 只有判定 Allow 才允许进入执行。降级事实不自动否决基线命令：既有
    /// 策略允许只读/测试基线在无完整隔离时继续运行，但必须写进证据账本；
    /// 判定 Degrade 或 Deny 时一律停止，避免把缺能力当正常放行。
    pub fn is_execution_permitted(&self) -> bool {
        self.decision.outcome == SandboxOutcome::Allow
    }

    /// 执行前显式检查。返回 Err 时调用方必须停止并记录 `degraded`。
    pub fn require_execution_permitted(&self) -> Result<(), String> {
        if self.is_execution_permitted() {
            return Ok(());
        }
        let reason = self
            .degraded
            .as_ref()
            .map(|fact| fact.reason.clone())
            .unwrap_or_else(|| format!("沙箱判定为 {}", self.decision.outcome.label()));
        Err(format!("沙箱拒绝按正常路径执行：{reason}"))
    }

    pub fn degraded_fact(&self) -> Option<&SandboxDegradedFact> {
        self.degraded.as_ref()
    }

    /// 渲染调试文本。规则、切片与降级原因都从同一份 fact 出发，避免显示与
    /// 判定各算一遍。
    pub fn render_debug(&self) -> String {
        let mut text = String::new();
        writeln!(&mut text, "平台: {}", self.platform).expect("写入 String 不会失败");
        writeln!(&mut text, "进程来源: {}", self.origin.label()).expect("写入 String 不会失败");
        writeln!(&mut text, "命令: {}", self.command).expect("写入 String 不会失败");
        writeln!(
            &mut text,
            "判定: {}{}",
            self.decision.outcome.label(),
            self.decision
                .stable_code
                .as_ref()
                .map(|code| format!(" ({code})"))
                .unwrap_or_default()
        )
        .expect("写入 String 不会失败");
        writeln!(&mut text, "说明: {}", self.decision.reason).expect("写入 String 不会失败");
        if self.decision.matched_rules.is_empty() {
            writeln!(&mut text, "命中规则: 无").expect("写入 String 不会失败");
        } else {
            writeln!(&mut text, "命中规则:").expect("写入 String 不会失败");
            for hit in &self.decision.matched_rules {
                writeln!(&mut text, "- {}: {}", hit.rule_id, hit.reason)
                    .expect("写入 String 不会失败");
            }
        }
        writeln!(&mut text, "隔离切片:").expect("写入 String 不会失败");
        for slice in &self.slices {
            writeln!(
                &mut text,
                "- {}: {}，{}",
                slice.capability.label(),
                slice.status.label(),
                slice.reason
            )
            .expect("写入 String 不会失败");
        }
        if let Some(degraded) = &self.degraded {
            writeln!(
                &mut text,
                "降级证据: {} ({})，{}",
                degraded.evidence_event, degraded.stable_code, degraded.reason
            )
            .expect("写入 String 不会失败");
        } else {
            writeln!(&mut text, "降级证据: 无").expect("写入 String 不会失败");
        }
        writeln!(&mut text, "执行: 否（--sandbox-debug 不启动进程）")
            .expect("写入 String 不会失败");
        text
    }

    fn from_facts(
        platform: impl Into<String>,
        command: impl Into<String>,
        origin: ProcessOrigin,
        decision: SandboxDecision,
        slices: Vec<SandboxSliceFact>,
    ) -> Self {
        let non_effective: Vec<SandboxSliceFact> = slices
            .iter()
            .filter(|slice| slice.status != SandboxSliceStatus::Effective)
            .cloned()
            .collect();
        let degraded = if !non_effective.is_empty() || decision.outcome == SandboxOutcome::Degrade {
            let slice_summary = non_effective
                .iter()
                .map(|slice| format!("{}={}", slice.capability.label(), slice.status.label()))
                .collect::<Vec<_>>()
                .join("、");
            let reason = if slice_summary.is_empty() {
                format!("判定要求显式降级：{}", decision.reason)
            } else {
                format!(
                    "隔离切片未生效或未实测：{slice_summary}；本事实必须写入证据账本。{}",
                    decision.reason
                )
            };
            Some(SandboxDegradedFact::from_slices(reason, non_effective))
        } else {
            None
        };
        Self {
            platform: platform.into(),
            command: command.into(),
            origin,
            decision,
            slices,
            degraded,
        }
    }
}

/// `--sandbox-debug` 的输出。`executed` 只由 `not_executed` 构造为 false。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxDebugReport {
    pub command: String,
    pub executed: bool,
    pub fact: SandboxFact,
    pub report: String,
}

impl SandboxDebugReport {
    pub fn not_executed(fact: SandboxFact) -> Self {
        let command = fact.command.clone();
        let report = fact.render_debug();
        Self {
            command,
            executed: false,
            fact,
            report,
        }
    }
}

impl fmt::Display for SandboxDebugReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.report)
    }
}

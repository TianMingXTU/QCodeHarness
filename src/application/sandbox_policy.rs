//! R2 沙箱策略端口：application 只定义判定与施放契约，不碰平台 IO。
//!
//! 真实平台能力探测、隔离施放和子进程包装放在 infrastructure。缺能力时
//! 一律返回 Degrade 并携带 `SandboxDegradedFact`，调用方必须走 ToolGuard
//! 链、审批闸门与证据账本，不能静默按 Allow 执行。

use crate::domain::sandbox_fact::{ProcessSpec, SandboxDebugReport, SandboxDecision, SandboxFact};

/// 命令规则端口。`inspect` 只做纯判定，调试入口也复用同一份规则。
pub trait SandboxPolicy: Send + Sync {
    /// 返回规则命中与 allow/deny/degrade 判定；不得启动进程。
    fn inspect(&self, command: &str) -> SandboxDecision;
}

/// 隔离施放端口。`apply` 在进程规格上挂接平台隔离并返回逐切片事实。
/// `debug` 只渲染将命中的规则与判定，绝不执行。
pub trait SandboxEnforcer: Send + Sync {
    fn apply(&self, process_spec: &ProcessSpec) -> SandboxFact;
    fn debug(&self, command: &str) -> SandboxDebugReport;
}

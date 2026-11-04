//! 非 Windows 平台尚未实现进程沙箱，调用方应声明降级后继续执行。

use crate::domain::sandbox::SandboxPolicy;

const UNSUPPORTED: &str = "沙箱不可用：本平台未实现（Windows Job Object 仅在 Windows 生效）";

/// 不持有平台资源的占位类型；当前平台无法创建实例。
pub struct JobGuard;

impl JobGuard {
    /// 返回平台不支持的说明，由调用方报告降级。
    pub fn create(_policy: &SandboxPolicy) -> Result<Self, String> {
        Err(UNSUPPORTED.to_owned())
    }

    /// 无平台资源需要终止。
    pub fn terminate_tree(&self) {}
}

/// 保持与 Windows 分支一致的调用接口，避免公共执行层引用原生句柄。
pub fn assign_child(_job: &JobGuard, _child: &tokio::process::Child) -> Result<(), String> {
    Err(UNSUPPORTED.to_owned())
}

/// 本平台无进程沙箱，无法把外部 PID 纳入任何边界；返回不支持，调用方按
/// `active = false` 降级并如实记证据。
pub fn protect_pid(_pid: u32, _policy: &SandboxPolicy) -> Result<JobGuard, String> {
    Err(UNSUPPORTED.to_owned())
}

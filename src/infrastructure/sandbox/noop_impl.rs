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

    /// 本平台无进程沙箱，无法把外部 PID 纳入任何边界。与 Windows 分支
    /// 一样是 `JobGuard` 上的方法，让调用方不必为平台差异写 cfg 分支；
    /// 实际上 `create` 在本平台恒失败，本方法没有可达的实例。
    pub fn assign_pid(&self, _pid: u32) -> Result<(), String> {
        Err(UNSUPPORTED.to_owned())
    }

    /// 无平台资源需要终止。
    pub fn terminate_tree(&self) {}
}

/// 保持与 Windows 分支一致的调用接口，避免公共执行层引用原生句柄。
pub fn assign_child(_job: &JobGuard, _child: &tokio::process::Child) -> Result<(), String> {
    Err(UNSUPPORTED.to_owned())
}

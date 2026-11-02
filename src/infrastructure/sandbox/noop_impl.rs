//! 非 Windows 平台的沙箱实现：不生效，但明确声明。
//!
//! 本仓库当前只在 Windows 上运行与验证；Linux 版（进程组 + rlimit）
//! 是后续工作。这里的实现保持与 Windows 版**同签名**——调用方代码
//! 在两个平台下完全一致，差异只在"是否真的有保护"。构造失败并附
//! 原因，命令照常降级执行：沙箱是纵深防御的一层，缺了它命令不该
//! 被拦死，但降级必须让模型与人都知情（`sandbox_active = false` +
//! 模型可见文本）。

use crate::domain::sandbox::SandboxPolicy;

/// 非 Windows 平台的"Job"占位：任何方法都不可用。
pub struct JobGuard {
    /// 记录创建请求，诊断信息里能说清"为什么没生效"。
    reason: String,
}

// 占位实现没有任何平台资源，同签名跨 await 携带是安全的。
unsafe impl Send for JobGuard {}

impl JobGuard {
    /// 永远失败：返回平台不支持的明确说明。
    ///
    /// # Errors
    ///
    /// 恒返回本平台未实现的说明文本。
    pub fn create(_policy: &SandboxPolicy) -> Result<Self, String> {
        Err("沙箱不可用：本平台未实现（Windows Job Object 仅在 Windows 生效）".into())
    }

    /// 永远失败：没有 Job 可 assign。
    ///
    /// # Errors
    ///
    /// 恒返回说明文本。
    pub fn assign(&self, _process_handle: isize) -> Result<(), String> {
        Err(self.reason.clone())
    }

    /// 无操作：没有 Job 可终止。
    pub fn terminate_tree(&self) {}
}

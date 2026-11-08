//! 进程沙箱：在平台层执行领域策略。
//!
//! Windows 使用 Job Object 限制资源并终止进程树。其他平台保留相同的
//! 调用接口并返回不支持，由执行层报告降级。原生进程句柄仅在 Windows
//! 分支访问，公共执行层不依赖平台 API。非 Windows 平台（Linux、macOS
//! 等）的进程沙箱尚未实现。

#[cfg(windows)]
mod windows_impl;
#[cfg(windows)]
pub use windows_impl::{assign_child, JobGuard};

#[cfg(not(windows))]
mod noop_impl;
#[cfg(not(windows))]
pub use noop_impl::{assign_child, JobGuard};

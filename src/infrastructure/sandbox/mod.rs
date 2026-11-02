//! 进程沙箱：把 [`SandboxPolicy`] 落到平台机制上。
//!
//! 平台分发在编译期完成：Windows 用 Job Object（[`windows`] 实现整树
//! 终止与资源上限），其他平台提供同签名的不生效实现——本仓库当前只在
//! Windows 上运行与验证，Linux 版（进程组 + rlimit）是后续工作。cfg
//! 隔离保证交叉编译不炸，也保证非 Windows 用户拿到的是明确的降级
//! 声明而不是假装的保护。
//!
//! 与 [`SandboxPolicy`] 的分工：领域类型只回答"约束是什么"，这里
//! 回答"怎么强制、强制失败怎么办"。强制失败的立场是**降级不阻断**：
//! 沙箱是纵深防御的第二层，创建失败（句柄受限等）不该把本来能完成的
//! 任务拦死，但降级必须让模型与人都知情（见 [`RawOutput::sandbox_active`]）。

#[cfg(windows)]
mod windows_impl;
#[cfg(windows)]
pub use windows_impl::{assign_child, JobGuard};

#[cfg(not(windows))]
mod noop_impl;
#[cfg(not(windows))]
pub use noop_impl::{assign_child, JobGuard};

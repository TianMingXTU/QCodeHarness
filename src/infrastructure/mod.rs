//! 基础设施层：与外部世界交互的具体实现。
//!
//! 负责实现领域层抽象：
//! - rig 客户端封装为 `ModelGateway`
//! - 文件操作封装为 `ToolInvoker`
//! - 审批、证据收集与时钟的具体实现

pub mod model;
pub mod mcp;
pub mod tool;
pub mod approval;
pub mod evidence;
pub mod policy;
pub mod clock;
pub mod session_store;
pub mod task_store;
pub mod worktree;
pub mod benchmark;
pub mod benchmark_runner;
pub mod instructions;
pub mod sandbox;
pub mod config;
pub mod git_worktree;
pub mod extensions;
pub mod delegation;

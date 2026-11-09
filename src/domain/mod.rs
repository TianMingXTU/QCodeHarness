//! 领域层：纯业务概念，零 IO、零框架依赖。
//!
//! 类型只表达业务规则与不变量，不涉及具体实现技术。所有 IO（文件、
//! 网络、数据库）通过 `application/port` 中的 trait 抽象。

pub mod ids;
pub mod platform;
pub mod workspace;
pub mod tool;
pub mod mcp;
pub mod agent;
pub mod agent_profile;
pub mod benchmark;
pub mod sandbox;
pub mod command;
pub mod session;
pub mod budget;
pub mod context;
pub mod evidence;
pub mod policy;
pub mod task;
pub mod worktree;
pub mod workflow;
pub mod config;
pub mod extension;

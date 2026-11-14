//! 领域层：纯业务概念，零 IO、零框架依赖。
//!
//! 类型只表达业务规则与不变量，不涉及具体实现技术。所有 IO（文件、
//! 网络、数据库）通过 `application/port` 中的 trait 抽象。

pub mod agent;
pub mod agent_profile;
pub mod benchmark;
pub mod budget;
pub mod command;
pub mod config;
pub mod context;
pub mod doctor;
pub mod evidence;
pub mod extension;
pub mod ids;
pub mod mcp;
pub mod platform;
pub mod policy;
pub mod sandbox;
pub mod service;
pub mod session;
pub mod task;
pub mod tool;
pub mod workflow;
pub mod workspace;
pub mod worktree;

pub mod memory;
pub mod permission_profile;
pub mod sandbox_fact;
pub mod todo;
pub mod web_policy;

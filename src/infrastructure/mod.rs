//! 基础设施层：与外部世界交互的具体实现。
//!
//! 负责实现领域层抽象：
//! - rig 客户端封装为 `ModelGateway`
//! - 文件操作封装为 `ToolInvoker`
//! - 审批、证据收集与时钟的具体实现

pub mod approval;
pub mod benchmark;
pub mod benchmark_runner;
pub mod clock;
pub mod config;
pub mod delegation;
pub mod doctor;
pub mod evidence;
pub mod extensions;
pub mod git_worktree;
pub mod instructions;
pub mod mcp;
pub mod model;
pub mod policy;
pub mod sandbox;
pub mod service_events;
pub mod service_factory;
pub mod service_report;
pub mod session_store;
pub mod task_service;
pub mod task_store;
pub mod tool;
pub mod worktree;

pub mod background_jobs;
pub mod code_navigation;
pub mod hooks_pretool;
pub mod mcp_http_sse;
pub mod memory;
pub mod nested_delegation;
pub mod notification;
pub mod todo;
pub mod web_tools;
pub mod web_ui;
pub mod skill_md;
pub mod acp;

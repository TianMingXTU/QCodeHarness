//! 应用层：协调领域对象，驱动业务流程。
//!
//! 核心是 `AgentRunner`，负责编排 ReAct 循环，不直接依赖 rig 或 tokio。
//! 外部依赖通过 `port` 中的 trait 注入，便于测试。

pub mod agent_roster;
pub mod agent_run;
pub mod context_policy;
pub mod control;
pub mod delegation;
pub mod doctor;
pub mod error;
pub mod extension;
pub mod isolation;
pub mod mcp_lifecycle;
pub mod memory_evidence;
pub mod orchestrator;
pub mod port;
pub mod provider_registry;
pub mod report;
pub mod service;
pub mod snapshot;
pub mod step_prompt;
pub mod summarizer;
pub mod task_resume;
pub mod tool_guard;
pub mod tool_registry;
pub mod worktree;

pub mod background_jobs;
pub mod code_navigation;
pub mod compact;
pub mod doctor_provider;
pub mod headless;
pub mod hooks;
pub mod hooks_compat;
pub mod mcp_http_sse;
pub mod memory;
pub mod nested_delegation;
pub mod notification;
pub mod permission_profile_guard;
pub mod plan_mode;
pub mod provider_auth;
pub mod sandbox_policy;
pub mod skill_md;
pub mod acp;
pub mod todo;
pub mod web_tools;
pub mod web_ui;

pub mod service_factory;

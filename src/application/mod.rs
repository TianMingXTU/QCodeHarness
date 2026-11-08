//! 应用层：协调领域对象，驱动业务流程。
//!
//! 核心是 `AgentRunner`，负责编排 ReAct 循环，不直接依赖 rig 或 tokio。
//! 外部依赖通过 `port` 中的 trait 注入，便于测试。

pub mod port;
pub mod agent_roster;
pub mod report;
pub mod agent_run;
pub mod context_policy;
pub mod mcp_lifecycle;
pub mod memory_evidence;
pub mod error;
pub mod orchestrator;
pub mod provider_registry;
pub mod snapshot;
pub mod step_prompt;
pub mod summarizer;
pub mod task_resume;
pub mod tool_registry;
pub mod tool_guard;
pub mod worktree;

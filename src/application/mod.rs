//! 应用层：协调领域对象、驱动业务流程。
//!
//! 这一层的核心是 `AgentRunner`，它编排 ReAct 循环，但不直接依赖 rig 或 tokio。
//! 对外依赖通过 `port` 中的 trait 注入，保持可测试性。

pub mod port;
pub mod agent_roster;
pub mod report;
pub mod benchmark;
pub mod agent_run;
pub mod mcp_lifecycle;
pub mod error;
pub mod orchestrator;
pub mod snapshot;
pub mod step_prompt;
pub mod summarizer;
pub mod task_resume;
pub mod tool_registry;
pub mod tool_guard;
pub mod worktree;

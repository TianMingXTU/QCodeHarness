//! 应用层：协调领域对象、驱动业务流程。
//!
//! 这一层的核心是 `AgentRunner`，它编排 ReAct 循环，但不直接依赖 rig 或 tokio。
//! 对外依赖通过 `port` 中的 trait 注入，保持可测试性。

pub mod port;
pub mod agent_run;
pub mod error;

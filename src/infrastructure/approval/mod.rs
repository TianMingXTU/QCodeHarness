//! 审批基础设施：交互式审批、AI 审查与自动审批。
//!
//! `cli` 是生产默认，遵循 fail-closed 并记住会话内的信任决定；
//! `ai_review` 先由模型筛选，只把拿不准的交给人；
//! `auto` 始终放行，用于测试和非交互场景（CI、脚本化运行）。

pub mod ai_review;
pub mod auto;
pub mod cli;

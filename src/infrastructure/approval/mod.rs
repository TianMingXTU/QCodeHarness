//! 审批基础设施：交互式审批、AI 审查与自动审批。
//!
//! `cli` 是生产默认——它引入 fail-closed 原则和会话内信任记忆；
//! `ai_review` 让模型先筛一遍，只把拿不准的交回人；
//! `auto` 恒放行，留给测试和非交互场景（CI、脚本化运行）。

pub mod ai_review;
pub mod auto;
pub mod cli;

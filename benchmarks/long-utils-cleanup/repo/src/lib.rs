//! bench-utils：一组被多个调用方依赖的小工具函数。
//!
//! 本 crate 是长任务基准的 fixture：五个模块之间存在真实的依赖关系
//! （`stats` 调用 `normalize`，`format` 调用 `stats`），agent 必须
//! 逐个模块读码、改动、验证——一个步骤装不下，工作流会把任务拆成
//! 多步完成。

pub mod format;
pub mod normalize;
pub mod parse;
pub mod stats;
pub mod validators;

/// 库版本，供下游打印诊断信息。
pub const VERSION: &str = "0.1.0";

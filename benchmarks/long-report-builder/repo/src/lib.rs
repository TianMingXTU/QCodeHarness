//! bench-report：把观测记录汇总成文本报表的库。
//!
//! 长任务基准的第二个 fixture：一条"读记录 → 聚合 → 排序 → 渲染"
//! 的处理链，四个环节各带一个缺陷，工作流至少四个步骤。

pub mod aggregate;
pub mod observations;
pub mod rank;
pub mod render;

/// 报表标题，渲染时带在首行。
pub const TITLE: &str = "观测报表";

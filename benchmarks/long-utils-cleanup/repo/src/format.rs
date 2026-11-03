//! 数值格式化（依赖统计模块）。

use crate::stats;

/// 汇总一行：中位数与规范化均值，固定两位小数。
///
/// 依赖 `stats::median` 与 `stats::normalized_mean`。两个统计量
/// 各自有缺陷时，本函数的输出只对得上其中一个——测试把它当作
/// 端到端的探针。
pub fn summary_line(values: &[f64]) -> String {
    format!(
        "median={:.2} mean={:.2}",
        stats::median(values),
        stats::normalized_mean(values)
    )
}

/// 百分比格式：把 0-1 的比例写成 "42.0%"。
pub fn percent(ratio: f64) -> String {
    // 缺陷：忘了乘 100，"0.42" 被格式化成 "0.4%"。
    format!("{ratio:.1}%")
}

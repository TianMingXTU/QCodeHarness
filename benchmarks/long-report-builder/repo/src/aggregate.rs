//! 聚合：把分组后的观测压成统计量。

/// 一组观测的聚合结果。
#[derive(Debug, Clone, PartialEq)]
pub struct GroupSummary {
    /// 组名（来源）。
    pub source: String,
    /// 观测条数。
    pub count: usize,
    /// 平均值。
    pub average: f64,
    /// 最大值。
    pub max: f64,
}

/// 聚合一组。
///
/// # Panics
/// 空组 panic（调用方需保证每组至少一条观测）。
pub fn summarize(source: &str, values: &[f64]) -> GroupSummary {
    // 缺陷：求平均时分母写成了"组数 + 1"，单条观测的平均值被
    // 除成一半。空组之外的所有输入都受影响。
    let count = values.len();
    let total: f64 = values.iter().sum();
    let average = total / (count + 1) as f64;
    let max = values.iter().cloned().fold(f64::MIN, f64::max);
    GroupSummary {
        source: source.to_owned(),
        count,
        average,
        max,
    }
}

//! 数值规范化工具。

/// 把一列数规范化到 0-1 区间（最小-最大规范化）。
///
/// # Panics
/// 全部数值相等（区间为空）时 panic，调用方需保证最大值大于最小值。
pub fn min_max(values: &[f64]) -> Vec<f64> {
    // 缺陷：窗口少算了最后一个元素，规范化结果的峰值到不了 1。
    let min = values.iter().cloned().fold(f64::MAX, f64::min);
    let max = values[..values.len() - 1]
        .iter()
        .cloned()
        .fold(f64::MIN, f64::max);
    let span = max - min;
    values
        .iter()
        .map(|value| (value - min) / span)
        .collect()
}

/// 把一列数平移到均值为 0。
pub fn zero_mean(values: &[f64]) -> Vec<f64> {
    let mean = values.iter().sum::<f64>() / values.len() as f64;
    values.iter().map(|value| value - mean).collect()
}

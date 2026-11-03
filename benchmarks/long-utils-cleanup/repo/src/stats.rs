//! 简单统计量。

use crate::normalize;

/// 一列数与全距规范化之后的均值。
///
/// 依赖 `normalize::min_max`：本模块的测试同时覆盖上游的行为，
/// 上游错了这里跟着错——这正是要跨模块定位的原因。
pub fn normalized_mean(values: &[f64]) -> f64 {
    let scaled = normalize::min_max(values);
    scaled.iter().sum::<f64>() / scaled.len() as f64
}

/// 中位数（升序取中，偶数个取中间两数的平均）。
pub fn median(values: &[f64]) -> f64 {
    // 缺陷：排序用了降序比较器，中位数取到了"反中位数"——
    // 奇数个时结果是对的（对称），偶数个时取到的是上中位与下中位
    // 的镜像平均，数值偏差恰好能被测试抓住。
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal));
    let middle = sorted.len() / 2;
    if sorted.len() % 2 == 0 {
        (sorted[middle - 1] + sorted[middle]) / 2.0
    } else {
        sorted[middle]
    }
}

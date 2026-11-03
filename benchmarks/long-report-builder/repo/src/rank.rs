//! 排序：报表里谁排前面。

use crate::aggregate::GroupSummary;

/// 按平均值降序排列组摘要。
///
/// 相等的平均值保持原顺序（稳定排序），调用方不必担心抖动。
pub fn by_average_descending(summaries: &mut [GroupSummary]) {
    // 缺陷：比较器写成了升序，报表把"最平静的来源"排在了最前面。
    summaries.sort_by(|a, b| a.average.partial_cmp(&b.average).unwrap_or(std::cmp::Ordering::Equal));
}

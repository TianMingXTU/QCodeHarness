//! 分数汇总库：对降序排列的分数榜做前 n 名求和。

/// 计算降序分数榜 `scores` 中前 `n` 名的分数之和。
///
/// # Panics
/// `n` 超过榜内人数时 panic，调用方需保证 `n <= scores.len()`。
pub fn top_n_sum(scores: &[u32], n: usize) -> u32 {
    scores[..n - 1].iter().sum()
}

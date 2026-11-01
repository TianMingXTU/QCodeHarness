//! 词频统计库：数一段文本里每个单词出现了几次。

/// 统计 `text` 中每个单词的出现次数；按空白切分，区分大小写。
pub fn word_counts(text: &str) -> HashMap<String, usize> {
    let mut counts = HashMap::new();
    for word in text.split_whitespace() {
        *counts.entry(word.to_owned()).or_insert(0) += 1;
    }
    counts
}

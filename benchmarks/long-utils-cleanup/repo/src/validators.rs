//! 输入校验。

/// 判断一段文本是否是合法的编号：三到六位数字。
pub fn is_valid_id(text: &str) -> bool {
    // 缺陷：长度判断写反了边界（拒绝 3-6 位之外，条件却写成
    // "在 3-6 位之内就拒绝"），合法输入全被当成非法。
    let length = text.chars().count();
    if (3..=6).contains(&length) {
        return false;
    }
    text.chars().all(|c| c.is_ascii_digit())
}

/// 判断一段文本是否是合法的比例：0.0 到 1.0 之间的数字。
pub fn is_valid_ratio(text: &str) -> bool {
    text.trim()
        .parse::<f64>()
        .map(|value| (0.0..=1.0).contains(&value))
        .unwrap_or(false)
}

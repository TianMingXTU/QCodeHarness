//! 输入解析。

/// 解析逗号分隔的数字列。
///
/// # Errors
/// 任一段不是合法数字时返回错误文本。
pub fn parse_numbers(text: &str) -> Result<Vec<f64>, String> {
    // 缺陷：分隔符写成了中文顿号，英文逗号输入会把整行当成一个数。
    text.split('、')
        .map(|part| {
            part.trim()
                .parse::<f64>()
                .map_err(|error| format!("无法解析 `{part}`: {error}"))
        })
        .collect()
}

/// 解析 "键=值" 配置行，值必须是数字。
pub fn parse_setting(line: &str) -> Result<(String, f64), String> {
    let (key, value) = line
        .split_once('=')
        .ok_or_else(|| format!("缺少 `=`: {line}"))?;
    let value = value
        .trim()
        .parse::<f64>()
        .map_err(|error| format!("`{value}` 不是数字: {error}"))?;
    Ok((key.trim().to_owned(), value))
}

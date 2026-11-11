/// 校验可安全用作目录名或工具名后缀的标识符。
///
/// 规则面向文件系统：只允许小写 ASCII 字母、数字、下划线与连字符，
/// 必须以字母或数字开头，并排除 Windows 保留设备名。`kind` 只用于
/// 错误信息，让调用方知道是哪类标识符不合法。
pub fn validate_safe_identifier(kind: &str, id: &str, max_bytes: usize) -> Result<(), String> {
    if id.is_empty()
        || id.len() > max_bytes
        || !id
            .bytes()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == b'_' || ch == b'-')
        || !id.as_bytes()[0].is_ascii_alphanumeric()
    {
        return Err(format!(
            "{kind} `{id}` 仅允许小写字母、数字、下划线与连字符，以字母或数字开头，最长 {max_bytes} 字节"
        ));
    }
    // Windows 的设备名即使带扩展名也不能作为普通文件使用。
    let upper = id.to_ascii_uppercase();
    let device = matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ["COM", "LPT"].iter().any(|prefix| {
            upper
                .strip_prefix(prefix)
                .is_some_and(|tail| tail.len() == 1 && matches!(tail.as_bytes()[0], b'1'..=b'9'))
        });
    if device {
        return Err(format!("{kind} `{id}` 是保留设备名"));
    }
    Ok(())
}

//! 仓内自写的标识符校验。
//!
//! 参考口径见 reference::validate_safe_identifier。缺陷：这份实现漏掉了
//! Windows 保留设备名的拦截（con / prn / aux / nul、com1..9、lpt1..9），
//! 于是对 "nul"、"com4" 这类输入会放行，与参考口径判定不一致。

/// 校验 `id` 能否安全用作目录名。
///
/// 返回 `Ok(())` 合法，`Err(说明)` 非法。
pub fn is_safe_dir_name(kind: &str, id: &str, max_bytes: usize) -> Result<(), String> {
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
    Ok(())
}

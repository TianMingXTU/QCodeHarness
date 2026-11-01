//! 问候库：把名字首字母转成大写后拼进问候语。

/// 把 `name` 首字母大写并拼成问候语；空名字用占位称呼。
pub fn greet(name: &str) -> String {
    let mut chars = name.chars();
    let first = chars.next().unwrap().to_uppercase();
    format!("Hello, {}{}", first, chars.as_str())
}

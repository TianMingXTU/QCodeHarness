//! 一个有 bug 的小库：`greet` 忘了把名字拼进问候语。

/// 向指定的人打招呼。
pub fn greet(name: &str) -> String {
    // BUG：名字没有拼进问候语，集成测试会抓住它。
    format!("Hello, !")
}

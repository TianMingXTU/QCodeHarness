//! 沙箱整树终止测试的受害进程，用于验证进程树的整树终止。
//!
//! `parent` 模式启动自身的 `child` 模式，两代进程各写各的日志（`<日志路径>.parent`
//! 与 `<日志路径>.child`），每 100ms 追加一行时间戳，持续 60 秒。测试在
//! 1.5 秒后终止进程树；两个日志都停止增长即证明 parent 与 child 都已结束。
//!
//! 两代分日志是必要的：只有直接子进程被终止时，父日志停止增长而子日志
//! 继续，两者混在一个文件里就分不出"整树终止"与"只杀了子进程"。
//!
//! 没有进程沙箱时，child 会成为孤儿继续写入。
//!
//! 用法：`sandbox-victim <日志路径>`（parent 模式）
//!       `sandbox-victim --child <日志路径>`（child 模式）

use std::io::Write;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("--child") => {
            let log = args.get(2).expect("child 需要日志路径");
            run_victim(&suffixed(log, ".child"));
        }
        Some(log) => {
            // parent：启动自己的 child，形成真实孙进程，然后共同写日志。
            let exe = std::env::current_exe().expect("当前可执行文件");
            let child = std::process::Command::new(exe)
                .arg("--child")
                .arg(log)
                .spawn()
                .expect("拉起 child 受害进程");
            // 故意不 wait：parent 结束后 child 成为孤儿并继续运行，这是旧实现
            // 的缺陷形态，child.kill() 无法终止它。
            println!("child pid: {}", child.id());
            run_victim(&suffixed(log, ".parent"));
        }
        other => panic!("用法: sandbox-victim <日志路径> 或 --child <日志路径>，实际 {other:?}"),
    }
}

/// 在路径末尾追加后缀。
fn suffixed(path: &str, suffix: &str) -> String {
    let mut text = path.to_owned();
    text.push_str(suffix);
    text
}

/// 每 100ms 追加一行时间戳，持续 60 秒，等待测试终止进程。
fn run_victim(log_path: &str) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(log_path) {
            let millis = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0);
            let _ = writeln!(file, "{millis}");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

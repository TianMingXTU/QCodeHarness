//! 沙箱整树终止测试的受害进程，用于验证 Job Object 的整树终止。
//!
//! `parent` 模式启动自身的 `child` 模式，两代进程每 100ms 向同一日志文件
//! 追加时间戳，持续 60 秒。测试在 1.5 秒后终止进程树；日志停止增长即证明
//! parent 与 child 都已结束。没有 Job Object 时，child 会成为孤儿继续写入。
//!
//! 用法：`sandbox-victim <日志路径>`（parent 模式）
//!       `sandbox-victim --child <日志路径>`（child 模式）

use std::io::Write;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("--child") => run_victim(args.get(2).expect("child 需要日志路径")),
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
            run_victim(log);
        }
        other => panic!("用法: sandbox-victim <日志路径> 或 --child <日志路径>，实际 {other:?}"),
    }
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

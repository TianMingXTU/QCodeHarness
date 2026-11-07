//! 演示 launcher：作为 harness 的直接子进程启动，再由它拉起真正的
//! MCP echo Server 作为**子进程**。这个"直接子进程 ≠ Server"的形状
//! 正是 MCP 沙箱收口要处理的场景（uvx / npx / pipx 都是它）。

use std::process::{Command, Stdio};

fn main() {
    let exe = std::env::args()
        .nth(1)
        .expect("用法: launcher <echo-server 路径>");
    let child = Command::new(exe)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("启动 echo server");
    let status = child.wait_with_output().expect("等待 echo server");
    std::process::exit(status.status.code().unwrap_or(0));
}

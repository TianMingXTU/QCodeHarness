//! 演示 launcher：harness 把它作为直接子进程启动，它再拉起真正的
//! MCP echo Server 作为子进程。直接子进程不是 Server，这是 MCP 沙箱
//! 需要处理的场景（uvx / npx / pipx 都这样）。

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

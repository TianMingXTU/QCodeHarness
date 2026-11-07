//! stdio MCP Server 的生命周期测试夹具：真实进程、协议与确定性副作用计数。
//!
//! 作为独立 bin 而不是 example：`cargo test` 编译它之后，测试可以用
//! `env!("CARGO_BIN_EXE_...")` 拿到确切路径。它手工应答 stdio JSON-RPC
//! 帧而不是走 rmcp Server 栈，行为完全由命令行模式决定——"先落盘计数
//! 再不回应"这类确定性故障，rmcp 的 handler 抽象反而不好表达。
//! 诊断写 stderr，不污染协议流。
//!
//! 确定性信号约定（均由第二个参数 counter_path 派生）：
//! - `counter_path`：每次 `tools/call` **先**追加一行计数再决定响应与否，
//!   副作用计数因此不受响应模式影响。计数跨进程续接：启动时先数一遍
//!   已有行数作为起点，`drop-after-N` 才能表达"全局恰好第 N 次调用退出"
//!   ——重连后的新进程接着数，测试才能区分"重放"与"新调用"；
//! - `counter_path` + `.pid`：启动时写入自身 PID，供"进程是否还活着"的
//!   有界轮询（握手超时走的是 rmcp 的 kill-on-drop，没有 EOF 可看，
//!   只能按 PID 查证清理真的发生）；
//! - `counter_path` + `.eof`：stdin 收到 EOF 时写入。父进程走完 transport
//!   优雅关闭（关 stdin）才会触发——它是"清理真的执行到了子进程"的凭据，
//!   不靠睡眠等进程消失；
//! - `counter_path` + `.child`：`with-child` 模式启动时写入孙进程 PID。

use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;

use serde_json::{Value, json};

fn main() {
    let mut args = std::env::args().skip(1);
    let mode = args.next().unwrap_or_else(|| "ok".to_owned());
    let counter_path = args.next().map(PathBuf::from);
    if let Some(path) = &counter_path {
        let pid_path = with_suffix(path, ".pid");
        if let Err(error) = std::fs::write(&pid_path, std::process::id().to_string()) {
            eprintln!("helper 写自身 PID 失败: {error}");
        }
    }
    if mode == "handshake-fail" {
        // 启动即退出：父进程应看到 spawn 成功但握手失败。
        std::process::exit(9);
    }
    if mode == "with-child" {
        // 整树清理的展示位：已纳入 Job 的进程所创建的子进程由系统自动
        // 继承成员资格。这里拉一个长命的 `ping` 并把它的 PID 落盘，
        // 测试观察"关闭之后它还活着吗"。
        spawn_grandchild(&counter_path);
    }
    // "no-init-reply"：收帧但对 initialize 永不回应——握手超时路径。
    let never_init_reply = mode == "no-init-reply";
    // "silent-after-N"：第 N 次调用前照常落盘计数，但不再回响应（连接保留）。
    // "drop-after-N"：第 N 次调用前落盘计数后直接退出（传输断开）。
    let (threshold, then_exit) = mode
        .strip_prefix("silent-after-")
        .and_then(|v| v.parse::<u64>().ok())
        .map(|n| (n, false))
        .or_else(|| {
            mode.strip_prefix("drop-after-")
                .and_then(|v| v.parse::<u64>().ok())
                .map(|n| (n, true))
        })
        .unwrap_or((u64::MAX, false));
    serve(counter_path, threshold, then_exit, never_init_reply);
}

fn spawn_grandchild(counter_path: &Option<PathBuf>) {
    let Some(path) = counter_path else { return };
    // `-n` 在 Windows 与 Linux 的 ping 里都是"次数"：这条测试只在 Windows
    // 跑，代码保持跨平台可编译。孙进程只需活得比一次测试长，120 次足够。
    let Ok(child) = std::process::Command::new("ping")
        .args(["-n", "120", "127.0.0.1"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    else {
        eprintln!("helper 无法拉起孙进程");
        return;
    };
    let pid_path = with_suffix(path, ".child");
    if let Err(error) = std::fs::write(&pid_path, child.id().to_string()) {
        eprintln!("helper 写孙进程 PID 失败: {error}");
    }
    // 故意不 wait 也不持有句柄：孙进程必须活过本夹具的退出，
    // 才能证明"父进程死了不等于整树死了"，Job 才是那条边界。
    std::mem::forget(child);
}

fn with_suffix(path: &PathBuf, suffix: &str) -> PathBuf {
    let mut text = path.as_os_str().to_os_string();
    text.push(suffix);
    PathBuf::from(text)
}

fn serve(
    counter_path: Option<PathBuf>,
    threshold: u64,
    then_exit: bool,
    never_init_reply: bool,
) {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    // 跨进程续接计数：重连拉起的新进程从"已有副作用数"之后接着数。
    // drop-after-N 因此是"全局恰好第 N 次调用退出"（单发），新进程不再
    // 退出；silent-after-N 保持"第 N 次起永远沉默"（持续）。两种语义的
    // 区别在于重连测试里第二个进程必须能正常应答，才能证明"恢复后调用可用"。
    let mut calls = counter_path
        .as_ref()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .map_or(0, |text| text.lines().count() as u64);
    for line in BufReader::new(stdin).lines() {
        let line = match line {
            Ok(line) => line,
            Err(error) => {
                eprintln!("helper stdin 读取失败: {error}");
                return;
            }
        };
        let Ok(request) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let id = request.get("id").cloned().unwrap_or(Value::Null);
        let method = request.get("method").and_then(Value::as_str).unwrap_or("");
        // 通知（无 id）不回响应；initialize 必答，否则握手过不去——
        // 正因如此 no-init-reply 模式靠"唯独不回它"来制造握手挂死。
        let response = match method {
            "initialize" if !never_init_reply => Some(json!({
                "jsonrpc":"2.0","id":id,
                "result":{"protocolVersion":"2025-06-18","capabilities":{"tools":{"listChanged":false}},"serverInfo":{"name":"qch-mcp-helper","version":"1"}}
            })),
            "tools/list" => Some(json!({
                "jsonrpc":"2.0","id":id,
                "result":{"tools":[{"name":"side_effect","description":"record one side effect","inputSchema":{"type":"object"}}]}
            })),
            "tools/call" => {
                calls += 1;
                if let Some(path) = &counter_path {
                    if let Err(error) = OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(path)
                        .and_then(|mut file| writeln!(file, "{calls}"))
                    {
                        eprintln!("helper 计数失败: {error}");
                    }
                }
                let triggered = if then_exit { calls == threshold } else { calls >= threshold };
                if triggered {
                    if then_exit {
                        std::process::exit(0);
                    }
                    continue; // silent：连接保留，但这个请求永远没有响应
                }
                Some(json!({
                    "jsonrpc":"2.0","id":id,
                    "result":{"content":[{"type":"text","text":format!("side-effect-{calls}")}]}
                }))
            }
            // notifications/initialized、no-init-reply 下的 initialize
            // 与其他未知方法：不回。
            _ => None,
        };
        if let Some(response) = response {
            if serde_json::to_writer(&mut stdout, &response).is_err()
                || writeln!(&mut stdout).is_err()
                || stdout.flush().is_err()
            {
                eprintln!("helper stdout 写失败：父进程可能已退出");
                return;
            }
        }
    }
    // stdin EOF：父进程关闭了写端。落一个标记文件，让"清理执行到了这里"
    // 变成可以断言的事实，而不是"过几秒看看进程还在不在"。
    if let Some(path) = &counter_path {
        let eof_path = with_suffix(path, ".eof");
        if let Err(error) = std::fs::write(&eof_path, "eof") {
            eprintln!("helper 写 EOF 标记失败: {error}");
        }
    }
}

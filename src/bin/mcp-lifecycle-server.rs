//! stdio MCP Server 的生命周期测试夹具：真实进程、真实协议、确定性副作用计数。
//!
//! 独立 bin：`cargo test` 编译后可用 `env!("CARGO_BIN_EXE_...")`
//! 取得确切路径。它手工应答 stdio JSON-RPC 帧，不走 rmcp Server 栈，
//! 行为完全由命令行模式决定。"先落盘计数再决定是否回应"这类确定性故障
//! 用 rmcp 的 handler 抽象不好表达。诊断写 stderr，不污染协议流。
//!
//! 确定性信号约定（均由第二个参数 counter_path 派生）：
//! - `counter_path`：每次 `tools/call` 先追加一行计数，再决定是否响应，
//!   副作用计数不受响应模式影响。计数跨进程续接：启动时数一遍已有行数
//!   作为起点，`drop-after-N` 因而表示"全局第 N 次调用时退出"。重连后的
//!   新进程接着数，测试可据此区分重放与新调用。
//! - `counter_path` + `.pid`：启动时写入自身 PID，供有界轮询判断进程
//!   是否存活。握手超时走 rmcp 的 kill-on-drop，没有 EOF 可看，只能按
//!   PID 查证清理是否发生。
//! - `counter_path` + `.eof`：stdin 收到 EOF 时写入。父进程完成 transport
//!   优雅关闭（关闭 stdin）后触发，是"清理确实到达子进程"的凭据，
//!   不依赖睡眠等待进程消失。
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
        // 整树清理的验证点：Job 中的进程创建的子进程自动继承成员资格。
        // 这里拉起长命的 `ping` 并写入其 PID，测试据此判断关闭后它是否
        // 仍然存活。
        spawn_grandchild(&counter_path);
    }
    // "no-init-reply"：收帧但对 initialize 永不回应，触发握手超时。
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
    // `ping -n` 是 Windows 的次数参数（Linux iputils 里 `-n` 表示数值
    // 输出）。这条测试只在 Windows 跑，代码保持跨平台可编译。孙进程只需
    // 活得比一次测试长，120 次足够。
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
    // 故意不 wait，也不持有句柄：孙进程必须活过夹具退出，才能证明
    // 父进程退出不会带走整棵树，Job 才是清理边界。
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
    // 跨进程续接计数：重连拉起的新进程从已有副作用数之后接着数。
    // drop-after-N 表示全局第 N 次调用时退出（单发），新进程不再退出；
    // silent-after-N 表示第 N 次起持续沉默。重连测试依赖两种语义的区别：
    // 第二个进程必须能正常应答，才能证明恢复后调用可用。
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
        // 通知（无 id）不回响应；initialize 必须应答，否则握手无法完成。
        // no-init-reply 模式只对 initialize 不回应，以此制造握手挂死。
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
    // stdin EOF：父进程关闭了写端。写一个标记文件，让"清理到达子进程"
    // 成为可断言的事实，无需等待数秒后再检查进程是否存在。
    if let Some(path) = &counter_path {
        let eof_path = with_suffix(path, ".eof");
        if let Err(error) = std::fs::write(&eof_path, "eof") {
            eprintln!("helper 写 EOF 标记失败: {error}");
        }
    }
}

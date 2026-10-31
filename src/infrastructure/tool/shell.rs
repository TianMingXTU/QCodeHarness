//! 命令执行工具：最危险的一个。
//!
//! 设计要点（每一条都对应一次真实事故形态）：
//! - **超时必须真杀子进程**：不杀的话 `cargo build` 之类的长命令会留下
//!   孤儿进程继续吃 CPU，超时只是让模型"以为"停了。
//! - **并发读 stdout/stderr**：先读完一个再读另一个，另一个的管道缓冲区
//!   （Windows 上约 4KB）写满后子进程会永久阻塞——这是经典的死锁形态。
//! - **环境脱敏**：dotenvy 把 `.env` 里的密钥放进了本进程环境，
//!   子进程默认全盘继承，不清洗等于任何命令都能 `env | grep KEY`。
//! - **退出码非 0 也是 failed 但输出照回**：测试失败的线索全在输出里，
//!   模型要靠它修 bug。

use serde::Deserialize;
use serde_json::json;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncReadExt;

use crate::application::port::{BoxFuture, ToolInvoker};
use crate::domain::tool::{ToolAnnotations, ToolCall, ToolOutcome, ToolSpec};
use crate::domain::workspace::WorkspaceRoot;

use super::parse_args;

const RUN_COMMAND: &str = "run_command";

/// 未指定时的超时秒数。
const DEFAULT_TIMEOUT_SECS: u64 = 30;
/// 超时硬上限：再长也不允许，失控命令最多拖 5 分钟。
const HARD_MAX_TIMEOUT_SECS: u64 = 300;
/// 合并输出的字符数上限，超出截断。
const MAX_OUTPUT_CHARS: usize = 8000;

/// 在 workspace 根下执行一条 shell 命令。
pub struct RunCommand {
    root: WorkspaceRoot,
}

impl RunCommand {
    /// 以 `root` 为命令的工作目录构造。
    pub fn new(root: WorkspaceRoot) -> Self {
        Self { root }
    }

    /// 工具规格。
    pub fn spec() -> ToolSpec {
        ToolSpec {
            name: RUN_COMMAND.to_owned(),
            description: "在 workspace 根目录下用系统 shell（Windows: cmd，其他: sh）执行一条命令，\
                          返回退出码、stdout 与 stderr。超时默认 30 秒、上限 300 秒，超时会终止进程。\
                          环境变量中疑似密钥的条目（名字含 key/token/secret/password/credential）不会传入子进程。"
                .to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "要执行的命令，例如 cargo test" },
                    "timeout_seconds": { "type": "integer", "description": "超时秒数，默认 30，上限 300" }
                },
                "required": ["command"]
            }),
            // shell 命令可以改任何东西、可以联网、毫无幂等可言。
            annotations: ToolAnnotations::new(false, true, false, true),
        }
    }
}

/// `run_command` 的参数。
#[derive(Debug, Deserialize)]
struct RunArgs {
    command: String,
    timeout_seconds: Option<u64>,
}

/// 一次命令执行的原始产出。
pub(crate) struct RawOutput {
    /// 退出码。进程被信号杀死等拿不到码的情况记为 -1。
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

/// 把输出文本截到上限内，超出时带说明。
///
/// 按字符而不是字节截：模型读的是文本，半个 UTF-8 序列只会变成乱码。
pub(crate) fn clamp_output(text: String, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text;
    }
    let head: String = text.chars().take(max_chars).collect();
    let total = text.chars().count();
    format!("{head}\n…（共 {total} 字符，已截断）")
}

/// 环境变量名是否疑似携带密钥。
///
/// 用"名字包含"而不是"名字等于"：密钥变量的命名五花八门
/// （`LLM_API_KEY`、`OPENAI_TOKEN`、`DB_PASSWORD`…），枚举不完；
/// 宁可错杀 `MONKEY_PATCH` 这类无辜变量，也不能漏放一个真密钥。
fn looks_like_secret(name_lowercase: &str) -> bool {
    ["key", "token", "secret", "password", "credential"]
        .iter()
        .any(|needle| name_lowercase.contains(needle))
}

/// 组装命令：Windows 用 cmd /C，其他平台用 sh -c。
///
/// `cfg!` 是编译期判断——本项目的目标平台在构建时已定，不需要运行时探测。
fn shell_program(command: &str) -> (String, Vec<String>) {
    if cfg!(windows) {
        ("cmd".to_owned(), vec!["/C".to_owned(), command.to_owned()])
    } else {
        ("sh".to_owned(), vec!["-c".to_owned(), command.to_owned()])
    }
}

/// 在 `root` 下执行命令，捕获输出。供本模块与 git 工具复用。
///
/// # Errors
///
/// 进程启动失败（命令不存在等）时返回说明文本。
pub(crate) async fn run_in_workspace(
    root: &WorkspaceRoot,
    command: &str,
    timeout: Duration,
) -> Result<RawOutput, String> {
    let (program, args) = shell_program(command);
    let mut cmd = tokio::process::Command::new(&program);
    cmd.args(&args)
        // 命令的 cwd 显式定为 workspace 根：进程 cwd 恰好也是它，但显式
        // 设置不依赖这个巧合（集成测试里 workspace 是临时目录）。
        .current_dir(root.as_path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    // 脱敏：从继承的环境里删掉疑似密钥的条目。
    // env_remove 用原始名字（大小写敏感的注册名），
    // 匹配用小写后的名字（Windows 环境变量名不区分大小写）。
    for (name, _) in std::env::vars() {
        if looks_like_secret(&name.to_lowercase()) {
            cmd.env_remove(&name);
        }
    }

    let mut child = cmd
        .spawn()
        .map_err(|error| format!("启动 `{program}` 失败: {error}"))?;

    // take() 之后管道归我们所有，读结束后子进程写端关闭会自然 EOF。
    let mut stdout_pipe = child
        .stdout
        .take()
        .ok_or_else(|| "无法捕获 stdout".to_owned())?;
    let mut stderr_pipe = child
        .stderr
        .take()
        .ok_or_else(|| "无法捕获 stderr".to_owned())?;

    // 三个 future 并发跑：两个管道同时排空（防死锁），wait 等退出。
    // timeout 包住整体——任何一个先卡住都不会拖过死线。
    let run = async {
        let read_stdout = async {
            let mut buf = Vec::new();
            let _ = stdout_pipe.read_to_end(&mut buf).await;
            buf
        };
        let read_stderr = async {
            let mut buf = Vec::new();
            let _ = stderr_pipe.read_to_end(&mut buf).await;
            buf
        };
        tokio::join!(read_stdout, read_stderr, child.wait())
    };

    match tokio::time::timeout(timeout, run).await {
        Ok((stdout, stderr, status)) => {
            let code = status.map(|status| status.code().unwrap_or(-1)).unwrap_or(-1);
            Ok(RawOutput {
                code,
                stdout: String::from_utf8_lossy(&stdout).into_owned(),
                stderr: String::from_utf8_lossy(&stderr).into_owned(),
            })
        }
        Err(_elapsed) => {
            // 显式杀进程再回收，不依赖 drop 语义：tokio 默认 kill_on_drop=false，
            // 且不 wait 会留下僵尸。杀完管道随进程关闭，资源彻底落定。
            let _ = child.kill().await;
            let _ = child.wait().await;
            Err(format!(
                "命令超时：{} 秒内未结束，进程已被终止。长任务请拆小，或调大 timeout_seconds（上限 {} 秒）",
                timeout.as_secs(),
                HARD_MAX_TIMEOUT_SECS
            ))
        }
    }
}

/// 把一次执行的产出渲染成给模型看的观察文本。
fn render_output(output: &RawOutput) -> String {
    let mut text = format!("退出码: {}\n", output.code);
    text.push_str("── stdout ──\n");
    text.push_str(output.stdout.trim_end());
    text.push_str("\n── stderr ──\n");
    text.push_str(output.stderr.trim_end());
    clamp_output(text, MAX_OUTPUT_CHARS)
}

impl ToolInvoker for RunCommand {
    fn invoke<'a>(&'a self, call: &'a ToolCall) -> BoxFuture<'a, ToolOutcome> {
        Box::pin(async move {
            let args: RunArgs = match parse_args(&call.arguments) {
                Ok(args) => args,
                Err(message) => return ToolOutcome::failed(message),
            };
            if args.command.trim().is_empty() {
                return ToolOutcome::failed("command 不能为空");
            }
            let timeout = Duration::from_secs(
                args.timeout_seconds
                    .unwrap_or(DEFAULT_TIMEOUT_SECS)
                    .clamp(1, HARD_MAX_TIMEOUT_SECS),
            );

            match run_in_workspace(&self.root, &args.command, timeout).await {
                Err(message) => ToolOutcome::failed(message),
                Ok(output) => {
                    let rendered = render_output(&output);
                    // 退出码非 0 也是 failed，但输出原样带回——模型要靠它修 bug。
                    if output.code == 0 {
                        ToolOutcome::success(rendered)
                    } else {
                        ToolOutcome::failed(rendered)
                    }
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::ids::CallId;
    use crate::domain::tool::OutcomeStatus;
    use serde_json::json;
    use std::path::PathBuf;

    fn temp_workspace(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("qch-shell-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建临时目录");
        dir
    }

    fn tool(root: &PathBuf) -> RunCommand {
        RunCommand::new(WorkspaceRoot::new(root))
    }

    fn call(arguments: serde_json::Value) -> ToolCall {
        ToolCall {
            id: CallId::new("call_run"),
            name: RUN_COMMAND.to_owned(),
            arguments,
        }
    }

    fn run_args(command: &str) -> serde_json::Value {
        json!({"command": command})
    }

    #[tokio::test]
    async fn captures_stdout_of_a_successful_command() {
        let root = temp_workspace("ok");
        let cmd = tool(&root);

        // Windows 与 Unix 各用一句最朴素的 echo。
        let command = if cfg!(windows) { "echo hello_from_shell" } else { "echo hello_from_shell" };
        let outcome = cmd.invoke(&call(run_args(command))).await;

        assert_eq!(outcome.status, OutcomeStatus::Success, "{}", outcome.content);
        assert!(outcome.content.contains("退出码: 0"), "{}", outcome.content);
        assert!(
            outcome.content.to_lowercase().contains("hello_from_shell"),
            "{}",
            outcome.content
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn command_runs_in_the_workspace_root() {
        let root = temp_workspace("cwd");
        let cmd = tool(&root);

        // 打印当前目录，确认子进程落在 workspace 里而不是进程 cwd。
        let command = if cfg!(windows) { "cd" } else { "pwd" };
        let outcome = cmd.invoke(&call(run_args(command))).await;

        assert_eq!(outcome.status, OutcomeStatus::Success, "{}", outcome.content);
        let canonical_root = root.canonicalize().unwrap().to_string_lossy().into_owned();
        let canonical_result = {
            // 输出里可能带引号或换行，宽松匹配路径的核心段。
            let lowered = outcome.content.to_lowercase();
            let root_lower = canonical_root.to_lowercase();
            root_lower.is_empty() || lowered.contains(&root_lower.replace("\\\\?\\", ""))
        };
        assert!(canonical_result, "输出应包含 workspace 路径: {}\nroot: {canonical_root}", outcome.content);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn nonzero_exit_is_failed_but_output_comes_back() {
        let root = temp_workspace("exitcode");
        let cmd = tool(&root);

        // 用一条退出码非 0 且带输出的命令。
        let command = if cfg!(windows) {
            "echo before_fail & exit /b 7"
        } else {
            "echo before_fail; exit 7"
        };
        let outcome = cmd.invoke(&call(run_args(command))).await;

        assert_eq!(outcome.status, OutcomeStatus::Failed);
        assert!(outcome.content.contains("退出码: 7"), "{}", outcome.content);
        // 失败的线索必须在输出里，模型靠它修 bug。
        assert!(
            outcome.content.contains("before_fail"),
            "stdout 应随观察回填: {}",
            outcome.content
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn empty_command_fails() {
        let root = temp_workspace("emptycmd");
        let cmd = tool(&root);

        for args in [json!({"command": ""}), json!({"command": "   "})] {
            let outcome = cmd.invoke(&call(args)).await;
            assert_eq!(outcome.status, OutcomeStatus::Failed);
            assert!(outcome.content.contains("不能为空"));
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn missing_binary_fails_without_panicking() {
        let root = temp_workspace("nobin");
        let cmd = tool(&root);

        let outcome = cmd
            .invoke(&call(run_args("qch_definitely_not_a_real_binary_9x7y2")))
            .await;

        assert_eq!(outcome.status, OutcomeStatus::Failed);
        // Windows 上 cmd /C 对不存在的命令：cmd 本身启动成功（spawn 不报错），
        // 错误以"退出码非 0 + stderr 报错文本"回来——这正好符合
        // "非 0 也算 failed 但输出必须回填"的约定，模型能读到原因。
        assert!(outcome.content.contains("退出码: 1"), "{}", outcome.content);
        assert!(!outcome.content.trim().ends_with("── stderr ──"), "stderr 应有报错文本");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn timeout_kills_the_child_process() {
        let root = temp_workspace("kill");
        let cmd = tool(&root);

        // 命令睡 3 秒后写一个标记文件。若超时只是"返回错误"而进程活着，
        // cmd 会活到 3 秒结束把标记写出来；真的被杀，标记永远不会出现。
        let command = if cfg!(windows) {
            "ping -n 3 127.0.0.1 >nul & echo done > killed_marker.txt"
        } else {
            "sleep 3 && echo done > killed_marker.txt"
        };
        let outcome = cmd
            .invoke(&call(json!({"command": command, "timeout_seconds": 1})))
            .await;

        assert_eq!(outcome.status, OutcomeStatus::Failed);
        assert!(outcome.content.contains("超时"), "{}", outcome.content);

        // 给"假死但没被杀"的进程留足完成时间，再验证标记不存在。
        tokio::time::sleep(Duration::from_millis(4500)).await;
        assert!(
            !root.join("killed_marker.txt").exists(),
            "子进程被超时终止后不应再写出标记文件"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn command_within_timeout_completes_normally() {
        let root = temp_workspace("in-time");
        let cmd = tool(&root);

        // 对照组：同样的"睡后写标记"，但给足超时——证明标记机制本身可靠，
        // 上一个测试里标记缺席确实归因于"被杀"而不是"命令从没跑起来"。
        let command = if cfg!(windows) {
            "ping -n 2 127.0.0.1 >nul & echo done > alive_marker.txt"
        } else {
            "sleep 1 && echo done > alive_marker.txt"
        };
        let outcome = cmd
            .invoke(&call(json!({"command": command, "timeout_seconds": 30})))
            .await;

        assert_eq!(outcome.status, OutcomeStatus::Success, "{}", outcome.content);
        assert!(root.join("alive_marker.txt").exists(), "正常完成的命令应写出标记");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn secret_environment_variables_are_stripped() {
        let root = temp_workspace("env");
        let cmd = tool(&root);

        // 测试进程自己把两个变量放进环境：一个疑似密钥，一个普通变量。
        // edition 2024 里 set_var 是 unsafe：测试进程独占启动，无并发读。
        unsafe {
            std::env::set_var("LLM_API_KEY", "super-secret-value-123");
            std::env::set_var("QCH_PLAIN_VAR", "visible-plain-value");
        }

        // 一条命令同时回显两个变量。
        let command = if cfg!(windows) {
            "echo KEY=%LLM_API_KEY% PLAIN=%QCH_PLAIN_VAR%"
        } else {
            "echo KEY=$LLM_API_KEY PLAIN=$QCH_PLAIN_VAR"
        };
        let outcome = cmd.invoke(&call(run_args(command))).await;

        assert_eq!(outcome.status, OutcomeStatus::Success, "{}", outcome.content);
        assert!(
            !outcome.content.contains("super-secret-value-123"),
            "密钥必须被脱敏: {}",
            outcome.content
        );
        assert!(
            outcome.content.contains("visible-plain-value"),
            "普通变量应正常传入（证明命令确实跑起来了）: {}",
            outcome.content
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn looks_like_secret_covers_the_documented_needles() {
        assert!(looks_like_secret("llm_api_key"));
        assert!(looks_like_secret("openai_token"));
        assert!(looks_like_secret("db_password"));
        assert!(looks_like_secret("client_secret"));
        assert!(looks_like_secret("aws_credentials"));
        assert!(!looks_like_secret("path"));
        assert!(!looks_like_secret("home"));
    }

    #[test]
    fn clamp_output_truncates_by_chars_with_a_note() {
        let long = "字".repeat(9000);
        let clamped = clamp_output(long, 8000);
        assert!(clamped.contains("已截断"));
        assert!(clamped.contains("共 9000 字符"));

        let short = "短文本".to_owned();
        assert_eq!(clamp_output(short, 8000), "短文本");
    }

    #[test]
    fn shell_program_picks_per_platform() {
        let (program, args) = shell_program("cargo test");
        if cfg!(windows) {
            assert_eq!(program, "cmd");
            assert_eq!(args, vec!["/C", "cargo test"]);
        } else {
            assert_eq!(program, "sh");
            assert_eq!(args, vec!["-c", "cargo test"]);
        }
    }
}

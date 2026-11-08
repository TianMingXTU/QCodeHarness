//! 命令执行工具。
//!
//! 设计要点（每一条对应一种真实故障）：
//! - **超时必须终止子进程**：否则 `cargo build` 等长命令会留下孤儿进程
//!   继续占用 CPU，超时只让模型以为进程已停止。
//! - **并发读取 stdout / stderr**：先读完一个再读另一个时，另一个的管道
//!   缓冲区（Windows 上约 4KB）写满会导致子进程永久阻塞。
//! - **环境脱敏**：dotenvy 把 `.env` 中的密钥放进了本进程环境，子进程
//!   默认继承全部变量；不清洗时任何命令都能通过 `env | grep KEY` 读取。
//! - **退出码非 0 记为 failed，但输出照常返回**：测试失败的线索在输出中，
//!   模型需要依靠它修复问题。
//! - **Windows 沙箱创建失败时拒绝执行**（fail-closed）：JobObject 是最后
//!   一道机械防线，创建失败后让命令裸跑会掩盖"有沙箱"的错误假设。只有
//!   平台本来未实现沙箱（非 Windows）和纳沙箱失败（子进程已启动的边界
//!   情况）允许降级。
//! - **文件系统边界检查**：命令参数中出现指向 workspace 外的绝对路径、
//!   UNC 路径或 `..` 穿越时**直接拒绝**，不进入审批升级。策略层的升级
//!   流程仍作为纵深保留，这里的拒绝是最后一道。

use serde::Deserialize;
use serde_json::json;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncReadExt;

use crate::application::port::{BoxFuture, ToolInvoker};
use crate::domain::sandbox::{
    write_escape_beyond, ExecutionBoundary, PlatformCapabilities, SandboxPolicy,
    SANDBOX_DOWNGRADE_NOTICE,
};
use crate::domain::tool::{ToolAnnotations, ToolCall, ToolOutcome, ToolSpec};
use crate::domain::workspace::WorkspaceRoot;
use crate::infrastructure::sandbox::{assign_child, JobGuard};

use super::parse_args;

const RUN_COMMAND: &str = "run_command";

/// 未指定时的超时秒数。
const DEFAULT_TIMEOUT_SECS: u64 = 30;
/// 超时硬上限。失控命令最长运行 5 分钟。
const HARD_MAX_TIMEOUT_SECS: u64 = 300;
/// 合并输出的字符数上限，超出截断。
const MAX_OUTPUT_CHARS: usize = 8000;

/// 在 workspace 根下执行一条 shell 命令。
pub struct RunCommand {
    root: WorkspaceRoot,
    // 生产入口使用默认策略；测试可在实例内注入失败策略，不修改全局状态。
    policy: SandboxPolicy,
}

impl RunCommand {
    /// 以 `root` 作为命令的工作目录构造。
    pub fn new(root: WorkspaceRoot) -> Self {
        Self { root, policy: SandboxPolicy::default() }
    }

    /// 工具规格。
    pub fn spec() -> ToolSpec {
        ToolSpec {
            name: RUN_COMMAND.to_owned(),
            description: "在 workspace 根目录下用系统 shell（Windows: cmd，其他: sh）执行一条命令，\
                          返回退出码、stdout 与 stderr。命令在受控进程组内执行：整树内存与进程数受限，\
                          超时终止全部后代进程（不只是直接子进程），不要依赖超限资源或遗留后台进程。\
                          超时默认 30 秒、上限 300 秒。环境变量中疑似密钥的条目\
                          （名字含 key/token/secret/password/credential）不会传入子进程。"
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

/// 一次命令执行的原始输出。
pub(crate) struct RawOutput {
    /// 退出码。进程被信号杀死等拿不到码的情况记为 -1。
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
    /// 沙箱是否实际生效。false 时调用方需在模型可见文本前加说明，让模型
    /// 知道自己运行在无沙箱环境中并采取更保守的行为。该文本随
    /// `ToolObserved` 进入证据，保持审计链完整。
    pub sandbox_active: bool,
}

/// 把输出截到上限内，超出时附加说明。
///
/// 按字符截断：模型读取文本，截断在 UTF-8 序列中间会产生乱码。
pub(crate) fn clamp_output(text: String, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text;
    }
    let head: String = text.chars().take(max_chars).collect();
    let total = text.chars().count();
    format!("{head}\n…（共 {total} 字符，已截断）")
}

/// 判断环境变量名是否疑似携带密钥。
///
/// 按名字包含匹配，不要求完全相等：密钥变量命名多样（`LLM_API_KEY`、
/// `OPENAI_TOKEN`、`DB_PASSWORD` 等），无法穷举。宁可多移除
/// `MONKEY_PATCH` 这类变量，也不能漏掉真实密钥。
fn looks_like_secret(name_lowercase: &str) -> bool {
    ["key", "token", "secret", "password", "credential"]
        .iter()
        .any(|needle| name_lowercase.contains(needle))
}

/// 组装命令：Windows 使用 cmd /C，其他平台使用 sh -c。
///
/// `cfg!` 是编译期判断，目标平台在构建时已确定，无需运行时探测。
fn shell_program(command: &str) -> (String, Vec<String>) {
    if cfg!(windows) {
        ("cmd".to_owned(), vec!["/C".to_owned(), command.to_owned()])
    } else {
        ("sh".to_owned(), vec!["-c".to_owned(), command.to_owned()])
    }
}

/// 区分启动前拒绝、启动失败和启动后失败，供进程工具共用。
#[derive(Debug, thiserror::Error)]
pub(crate) enum RunError {
    #[error("{0}")]
    Refused(String),
    #[error("{0}")]
    NotStarted(String),
    #[error("{message}")]
    Failed {
        message: String,
        sandbox_active: bool,
    },
}

impl RunError {
    pub(crate) fn into_outcome(self, boundary_checked: bool) -> ToolOutcome {
        match self {
            Self::Refused(message) => ToolOutcome::refused(message),
            Self::NotStarted(message) => ToolOutcome::failed(message),
            Self::Failed { message, sandbox_active } => with_process_boundary(
                ToolOutcome::failed(message), sandbox_active, boundary_checked,
            ),
        }
    }
}

impl RawOutput {
    /// 成功与非零退出共用边界交接，避免错误分支遗漏降级说明。
    pub(crate) fn into_outcome(self, body: String, boundary_checked: bool) -> ToolOutcome {
        let outcome = if self.code == 0 {
            ToolOutcome::success(body)
        } else {
            ToolOutcome::failed(body)
        };
        with_process_boundary(outcome, self.sandbox_active, boundary_checked)
    }
}

fn with_process_boundary(
    mut outcome: ToolOutcome,
    sandbox_active: bool,
    boundary_checked: bool,
) -> ToolOutcome {
    if !sandbox_active {
        outcome.content = format!("{SANDBOX_DOWNGRADE_NOTICE}\n{}", outcome.content);
    }
    outcome.with_execution(ExecutionBoundary::process(sandbox_active, boundary_checked))
}

/// 在 `root` 下执行命令并捕获输出，供命令工具与 git 工具复用。
///
/// 命令在进程沙箱（Windows Job Object，见 `infrastructure::sandbox`）中
/// 执行：整树内存与进程数受限，超时或句柄关闭时终止整树。
///
/// 沙箱不可用时的处理：
/// - 平台未实现（非 Windows）或 assign 失败：降级执行，不阻断命令。
///   `sandbox_active` 返回 false，调用方在模型可见文本中声明未受保护。
/// - Windows 上创建失败：不执行命令，返回 [`RunError::Refused`]。
///
/// # Errors
///
/// 启动前拒绝返回 `Refused`，启动失败返回 `NotStarted`。启动后的超时
/// 或 I/O 错误返回 `Failed`，其中携带实际沙箱状态。
pub(crate) async fn run_in_workspace(
    root: &WorkspaceRoot,
    command: &str,
    timeout: Duration,
) -> Result<RawOutput, RunError> {
    run_with_policy(root, command, timeout, &SandboxPolicy::default()).await
}

pub(crate) async fn run_with_policy(
    root: &WorkspaceRoot,
    command: &str,
    timeout: Duration,
    policy: &SandboxPolicy,
) -> Result<RawOutput, RunError> {
    let (program, args) = shell_program(command);
    let mut cmd = tokio::process::Command::new(&program);
    cmd.args(&args)
        // 显式设置 cwd 为 workspace 根。进程 cwd 恰好相同时也不依赖这个
        // 巧合；集成测试中 workspace 是临时目录。
        .current_dir(root.as_path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    // 从继承的环境中移除疑似密钥的变量。
    // env_remove 使用原始名字（注册名大小写敏感），匹配时使用小写名字
    // （Windows 环境变量名不区分大小写）。
    for (name, _) in std::env::vars() {
        if looks_like_secret(&name.to_lowercase()) {
            cmd.env_remove(&name);
        }
    }

    // 必须先创建 Job 再 spawn，否则拒绝执行时无法撤回副作用，还可能遗留孙进程。
    let sandbox = match JobGuard::create(policy) {
        Ok(job) => Some(job),
        Err(error) if !PlatformCapabilities::detect().has_process_sandbox => {
            eprintln!("警告：{error}");
            None
        }
        Err(error) => return Err(RunError::Refused(format!(
            "沙箱创建失败: {error}（fail-closed：拒绝执行）"
        ))),
    };
    let mut child = cmd.spawn().map_err(|error| {
        RunError::NotStarted(format!("启动 `{program}` 失败: {error}"))
    })?;

    // 纳沙箱失败时按降级策略执行，所有退出路径都返回该事实。
    // spawn 与 assign 之间存在窗口，这里不承诺 OS 级隔离。
    let sandbox = sandbox.and_then(|job| match assign_child(&job, &child) {
        Ok(()) => Some(job),
        Err(error) => {
            eprintln!("警告：进程纳入沙箱失败: {error}");
            None
        }
    });
    let sandbox_active = sandbox.is_some();

    // take() 之后管道归我们所有，读结束后子进程写端关闭会自然 EOF。
    let mut stdout_pipe = child.stdout.take().expect("stdout 已配置为 piped");
    let mut stderr_pipe = child.stderr.take().expect("stderr 已配置为 piped");

    // 三个 future 并发执行：两个管道同时排空以防死锁，wait 等待退出。
    // timeout 包住整体，避免任一环节卡住超过时限。
    let run = async {
        let read_stdout = async {
            let mut buf = Vec::new();
            stdout_pipe.read_to_end(&mut buf).await.map(|_| buf)
        };
        let read_stderr = async {
            let mut buf = Vec::new();
            stderr_pipe.read_to_end(&mut buf).await.map(|_| buf)
        };
        tokio::join!(read_stdout, read_stderr, child.wait())
    };

    match tokio::time::timeout(timeout, run).await {
        Ok((stdout, stderr, status)) => {
            let execution_error = |error| RunError::Failed {
                message: format!("等待命令结果失败: {error}"),
                sandbox_active,
            };
            let stdout = stdout.map_err(execution_error)?;
            let stderr = stderr.map_err(execution_error)?;
            let code = status.map_err(execution_error)?.code().unwrap_or(-1);
            // 关闭 Job 清理残留后代。RawOutput 只保留执行时的边界事实。
            drop(sandbox);
            Ok(RawOutput {
                code,
                stdout: String::from_utf8_lossy(&stdout).into_owned(),
                stderr: String::from_utf8_lossy(&stderr).into_owned(),
                sandbox_active,
            })
        }
        Err(_elapsed) => {
            // 超时终止整树：先在 Job 层终止，覆盖全部后代；再用 child.kill()
            // 兜底直接子进程，沙箱降级时这是唯一手段。随后 wait 回收，
            // 管道随进程关闭。
            if let Some(job) = &sandbox {
                job.terminate_tree();
            }
            let _ = child.kill().await;
            let _ = child.wait().await;
            drop(sandbox);
            Err(RunError::Failed {
                message: format!(
                    "命令超时：{} 秒内未结束，已请求终止进程。长任务请拆小，或调大 timeout_seconds（上限 {} 秒）",
                    timeout.as_secs(),
                    HARD_MAX_TIMEOUT_SECS
                ),
                sandbox_active,
            })
        }
    }
}

/// 把执行产出渲染成模型可见的观察文本。
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
            // 命令参数指向 workspace 外时直接拒绝。
            //
            // 使用**根感知**判据 `write_escape_beyond`，不用
            // `write_escape_suspect`。模型常写 `type D:\repo\src\main.rs`
            // 这类根内绝对路径，无根判据会把正常命令一并拒绝。
            // 策略层的升级审批把决定权交给用户；这里是最后一道机械防线，
            // 即使人已批准、策略已放行，越界仍会被拦。
            if let Some(violation) = write_escape_beyond(&args.command, self.root.as_path()) {
                return ToolOutcome::refused(format!(
                    "命令越出 workspace 边界：{violation}。当前 workspace 根：{}。\
                     这一层是硬拒绝，审批与策略都不能覆盖；\
                     需要跨目录操作请把 workspace 换到目标目录。",
                    self.root.as_path().display()
                ));
            }
            let timeout = Duration::from_secs(
                args.timeout_seconds
                    .unwrap_or(DEFAULT_TIMEOUT_SECS)
                    .clamp(1, HARD_MAX_TIMEOUT_SECS),
            );

            match run_with_policy(&self.root, &args.command, timeout, &self.policy).await {
                Err(error) => error.into_outcome(true),
                Ok(output) => {
                    let rendered = render_output(&output);
                    output.into_outcome(rendered, true)
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
    async fn successful_command_runs_with_the_sandbox_active() {
        // 回归保护：沙箱接入不能破坏正常命令，成功路径必须返回沙箱已生效
        // 的事实，且不带未生效说明。
        let root = temp_workspace("sandbox-ok");
        let cmd = tool(&root);

        let outcome = cmd.invoke(&call(run_args("echo ok"))).await;

        assert_eq!(outcome.status, OutcomeStatus::Success, "{}", outcome.content);
        let raw = run_in_workspace(&WorkspaceRoot::new(root.clone()), "echo ok", Duration::from_secs(30))
            .await
            .expect("执行成功");
        assert!(raw.sandbox_active, "Windows 上沙箱应实际生效");
        assert!(
            !outcome.content.contains("进程沙箱未生效"),
            "沙箱生效时不应带未生效说明: {}",
            outcome.content
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn timeout_kills_the_whole_process_tree() {
        // 核心验收：整树终止。受害进程 parent 再创建 child，两代都写
        // 日志；1.5 秒超时后日志必须停止增长。没有 Job 时 child 会成为
        // 孤儿继续写，这是旧实现的缺陷。
        // 探针为 examples/sandbox-victim.rs（无参数表示 parent 加 child）。
        let root = temp_workspace("killtree");
        let log = root.join("victim.log");

        let outcome = run_in_workspace(
            &WorkspaceRoot::new(root.clone()),
            &format!("{} {}", victim_exe().display(), log.display()),
            Duration::from_millis(1500),
        )
        .await;
        assert!(
            outcome.is_err() && outcome.err().unwrap().to_string().contains("超时"),
            "受害进程必须被超时收口"
        );

        // 给未被终止的进程留出两轮写入时间，再验证日志停止增长。
        tokio::time::sleep(Duration::from_millis(700)).await;
        let lines_after_kill = count_lines(&log);
        assert!(lines_after_kill > 0, "超时前 victim 应已写过日志");
        tokio::time::sleep(Duration::from_millis(700)).await;
        assert_eq!(
            count_lines(&log),
            lines_after_kill,
            "超时后日志必须停止增长：父与孙进程都被整树终止"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 受害探针的可执行文件路径。examples 与 lib 位于同一 target 的兄弟
    /// 目录，测试前由构建系统编译（examples 属于 dev target）。
    fn victim_exe() -> std::path::PathBuf {
        let path = std::env::current_exe()
            .expect("测试进程路径")
            .parent()
            .and_then(|p| p.parent())
            .map(|p| p.join("examples").join("sandbox-victim.exe"))
            .expect("定位探针");
        assert!(path.exists(), "探针未编译（cargo test 会编 examples）: {}", path.display());
        path
    }

    fn count_lines(path: &std::path::Path) -> usize {
        std::fs::read_to_string(path)
            .map(|text| text.lines().count())
            .unwrap_or(0)
    }

    /// Windows：沙箱创建失败时拒绝执行（fail-closed）。
    ///
    /// 使用明确未实现的 `cpu_time` 限额稳定触发创建失败，不依赖宿主机
    /// 权限或资源耗尽。降级裸跑会让模型和策略误以为有沙箱，因此 Windows
    /// 上无法创建沙箱就不执行；非 Windows 平台未实现时才降级。
    #[cfg(windows)]
    #[tokio::test]
    async fn sandbox_creation_failure_refuses_the_command() {
        let root = temp_workspace("sandbox-failclosed");
        let mut cmd = tool(&root);
        cmd.policy.cpu_time = Some(Duration::from_secs(1));

        let outcome = cmd
            .invoke(&call(run_args("echo should_not_run > ran.txt")))
            .await;

        // 拒绝与失败是两种结局：命令没有运行，失败表示跑过但未成功。
        // 证据必须能分辨；`SandboxApplied` 只为实际执行的命令记录。
        assert_eq!(outcome.status, OutcomeStatus::Refused, "{}", outcome.content);
        assert!(outcome.content.contains("沙箱创建失败"), "{}", outcome.content);
        assert!(outcome.content.contains("fail-closed"), "{}", outcome.content);
        assert!(outcome.execution.is_none(), "没启动进程就不能发边界证据");
        assert!(!root.join("ran.txt").exists(), "创建失败时命令不得执行");
        // 拒绝执行与降级执行不能同时出现：前者表示没跑，后者表示跑过但无保护。
        assert!(
            !outcome.content.contains("进程沙箱未生效"),
            "拒绝执行时不该再给降级说明: {}",
            outcome.content
        );

        let error = match run_with_policy(&cmd.root, "echo ok", Duration::from_secs(30), &cmd.policy)
            .await
        {
            Ok(_) => panic!("沙箱创建失败必须向上传播错误而不是降级"),
            Err(RunError::Refused(message)) => message,
            Err(other) => panic!("创建失败必须是拒绝: {other}"),
        };
        assert!(error.contains("沙箱创建失败"), "{error}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 非 Windows：平台未实现进程沙箱时降级执行，并在输出开头说明。
    ///
    /// "平台本来没有能力"与"有能力但创建失败"必须走不同分支。前者若拒绝
    /// 执行，该平台将完全无法运行命令；后者拒绝执行才是正确行为。
    #[cfg(not(windows))]
    #[tokio::test]
    async fn unsupported_platform_degrades_with_a_visible_notice() {
        let root = temp_workspace("sandbox-downgrade");
        let cmd = tool(&root);

        let outcome = cmd
            .invoke(&call(run_args("echo downgrade_ok > ran.txt")))
            .await;

        assert_eq!(outcome.status, OutcomeStatus::Success, "{}", outcome.content);
        assert!(
            outcome.content.starts_with("⚠️ 进程沙箱未生效"),
            "{}",
            outcome.content
        );
        assert_eq!(
            std::fs::read_to_string(root.join("ran.txt")).unwrap().trim(),
            "downgrade_ok"
        );

        let raw = run_in_workspace(&cmd.root, "echo ok", Duration::from_secs(30))
            .await
            .expect("不支持沙箱不该阻断命令");
        assert!(!raw.sandbox_active);
        assert_eq!(raw.code, 0);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 边界检查：越出 workspace 的命令在执行层就被拒。
    #[tokio::test]
    async fn command_reaching_outside_the_workspace_is_refused() {
        let root = temp_workspace("boundary");
        std::fs::write(root.join("inside.txt"), "内容").expect("写待读文件");
        let cmd = tool(&root);

        let escaping = [
            // 重定向到别的盘。
            r"echo x > C:\Windows\temp\y",
            // 读取越界同样拦截：外传与读取都依赖这一道机械防线。
            r"type ..\outside",
            // 基线放行的 `git diff *` 可通过 --output 把内容写到外部。
            r"git diff --output=D:\x",
            // UNC。
            r"copy inside.txt \\server\share\x",
        ];
        for command in escaping {
            let outcome = cmd.invoke(&call(run_args(command))).await;
            assert_eq!(
                outcome.status,
                OutcomeStatus::Refused,
                "`{command}` 应被执行层拒绝，实际: {}",
                outcome.content
            );
            assert!(
                outcome.content.contains("越出 workspace 边界"),
                "`{command}` 的拒绝理由要说明边界: {}",
                outcome.content
            );
        }

        // 对照：workspace 内的读写照旧放行，否则这条检查就成了禁用 run_command。
        let outcome = cmd.invoke(&call(run_args("type inside.txt"))).await;
        assert_eq!(outcome.status, OutcomeStatus::Success, "{}", outcome.content);
        assert!(outcome.execution.is_some(), "真启动的命令要带边界事实");

        // 根内绝对路径同样放行。模型常写 `type D:\repo\src\main.rs`，
        // 硬拒绝必须识别出该路径位于根内。
        let absolute = root.join("inside.txt");
        let outcome = cmd
            .invoke(&call(run_args(&format!("type {}", absolute.display()))))
            .await;
        assert_eq!(
            outcome.status,
            OutcomeStatus::Success,
            "{}",
            outcome.content
        );

        // 分段后的收窄与放行：越界藏在第二段仍会被拒绝，且理由要点名
        // 该段。整串扫描也能发现越界，但理由中的动词与段号必须准确。
        let chained = format!("type inside.txt && echo x > {}", outside_target());
        let outcome = cmd.invoke(&call(run_args(&chained))).await;
        assert_eq!(
            outcome.status,
            OutcomeStatus::Refused,
            "{}",
            outcome.content
        );
        assert!(
            outcome.content.contains("第 2 段"),
            "多段理由要点名越界段: {}",
            outcome.content
        );
        assert!(
            !std::env::temp_dir()
                .join("qch-shell-slice4-out.txt")
                .exists(),
            "被拒的命令不得在外部落盘"
        );

        // 两个重定向目标都在根内照常放行；任何一个越界都会被逐段收齐拦下。
        let outcome = cmd
            .invoke(&call(run_args("echo hi > out.txt 2> out.err")))
            .await;
        assert_eq!(
            outcome.status,
            OutcomeStatus::Success,
            "{}",
            outcome.content
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 通用越界目标，不依赖具体 workspace 根。它位于系统临时目录，
    /// 拒绝路径不会真正写入。
    fn outside_target() -> String {
        std::env::temp_dir()
            .join("qch-shell-slice4-out.txt")
            .to_string_lossy()
            .into_owned()
    }

    /// 拒绝发生在执行之前，越界命令不应产生任何落盘结果。
    ///
    /// 这是拦截与报错的差别：先执行再判失败时，副作用已经发生。
    #[tokio::test]
    async fn refused_command_leaves_no_side_effect() {
        let root = temp_workspace("boundary-noop");
        let outside = std::env::temp_dir().join(format!(
            "qch-boundary-outside-{}.txt",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&outside);
        let cmd = tool(&root);

        let outcome = cmd
            .invoke(&call(run_args(&format!(
                "echo leaked > {}",
                outside.display()
            ))))
            .await;

        assert_eq!(outcome.status, OutcomeStatus::Refused, "{}", outcome.content);
        assert!(!outside.exists(), "被拒的命令不该在 workspace 外留下文件");
        let _ = std::fs::remove_dir_all(&root);
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

        // 打印当前目录，确认子进程的 cwd 是 workspace 根。
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
        // 失败线索必须在输出中，模型依靠它修复问题。
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
            assert!(outcome.execution.is_none(), "没启动进程不能伪造边界证据");
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
        // Windows 上 cmd /C 执行不存在的命令时，cmd 本身启动成功，
        // 错误以非 0 退出码和 stderr 文本返回。这符合"非 0 记为 failed
        // 但必须返回输出"的约定，模型能读到原因。
        assert!(outcome.content.contains("退出码: 1"), "{}", outcome.content);
        assert!(!outcome.content.trim().ends_with("── stderr ──"), "stderr 应有报错文本");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn timeout_kills_the_child_process() {
        let root = temp_workspace("kill");
        let cmd = tool(&root);

        // 命令休眠 3 秒后写标记文件。若超时只返回错误而进程仍存活，
        // 标记会在 3 秒后写出；进程被终止时标记不会出现。
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
        assert!(
            outcome.execution.is_some_and(|boundary| boundary.boundary_checked),
            "超时前已启动进程，必须留下真实执行边界"
        );

        // 给未被终止的进程留出完成时间，再验证标记不存在。
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

        // 对照组：同样休眠后写标记，但给足超时。这证明标记机制本身可靠，
        // 上一个测试中标记缺席是因为进程被终止；命令已经启动。
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

        // 测试进程自行设置两个环境变量：一个疑似密钥，一个普通变量。
        // edition 2024 中 set_var 是 unsafe；测试进程独占启动，无并发读取。
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

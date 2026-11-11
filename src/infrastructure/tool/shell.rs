//! 命令执行工具。
//!
//! 设计要点（每一条对应一种真实故障）：
//! - **超时必须终止子进程**：否则 `cargo build` 等长命令会留下孤儿进程
//!   继续占用 CPU，超时只让模型以为进程已停止。
//! - **并发读取 stdout / stderr**：先读完一个再读另一个时，另一个的管道
//!   缓冲区写满会导致子进程永久阻塞。缓冲区大小因平台而异（Windows
//!   上约 4KB，POSIX 上默认 64 KiB），两个都得防。
//! - **环境脱敏**：dotenvy 把 `.env` 中的密钥放进了本进程环境，子进程
//!   默认继承全部变量；不清洗时任何命令都能通过 `env | grep KEY` 读取。
//! - **退出码非 0 记为 failed，但输出照常返回**：测试失败的线索在输出中，
//!   模型需要依靠它修复问题。
//! - **Windows 沙箱创建失败时拒绝执行**（fail-closed）：JobObject 是最后
//!   一道机械防线，创建失败后让命令裸跑会掩盖"有沙箱"的错误假设。只有
//!   平台本来未实现沙箱（Linux、macOS 等）和纳沙箱失败（子进程已启动的
//!   边界情况）允许降级。
//! - **平台差异集中检测**：当前平台由 [`Platform::current`] 判定，shell
//!   选择、进程沙箱是否存在、给模型看的保护范围说明都跟着它走。不同系统
//!   的处理方式不同，写死在各自逻辑里必然漏一处。
//! - **文件系统边界检查**：命令参数中出现指向 workspace 外的绝对路径、
//!   UNC 路径或 `..` 穿越时**直接拒绝**，不进入审批升级。策略层的升级
//!   流程仍作为纵深保留，这里的拒绝是最后一道。

use serde::Deserialize;
use serde_json::json;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncReadExt;

use crate::application::port::{BoxFuture, ToolInvoker};
use crate::domain::command::parse_command;
use crate::domain::platform::Platform;
use crate::domain::sandbox::{
    write_escape_beyond, ALLOW_UNISOLATED_COMMANDS_ENV, ExecutionBoundary,
    PlatformCapabilities, SandboxPolicy, SANDBOX_DOWNGRADE_NOTICE,
    unisolated_decision, UnisolatedDecision,
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
    // 宿主是否显式放行开放命令（QCH_ALLOW_UNISOLATED_COMMANDS=1）。
    // 构造时读一次环境并缓存：同一次执行内所有命令用同一口径，测试
    // 直接改字段而不动全局环境。
    host_allows_unisolated: bool,
}

impl RunCommand {
    /// 以 `root` 作为命令的工作目录构造。
    ///
    /// 宿主放行开关在构造时读入并缓存：同一次执行内所有命令用同一口径，
    /// 测试直接改 `host_allows_unisolated` 字段而不改全局环境。`QCH_ALLOW_
    /// UNISOLATED_COMMANDS=1` 时开放命令可无完整隔离执行（未生效声明照常
    /// 附带）；未设置时，缺文件系统与网络隔离的平台只放行基线形状命令。
    pub fn new(root: WorkspaceRoot) -> Self {
        Self {
            root,
            policy: SandboxPolicy::default(),
            host_allows_unisolated: host_allows_unisolated_commands(),
        }
    }

    /// 工具规格。
    ///
    /// 描述按当前平台生成：整树终止与资源上限是 Windows Job Object 的
    /// 能力，写成无条件事实会让模型误以为其他平台也有。
    pub fn spec() -> ToolSpec {
        let capabilities = PlatformCapabilities::detect();
        let (shell, args) = capabilities.platform.shell();
        let shell_args = args.join(" ");
        ToolSpec {
            name: RUN_COMMAND.to_owned(),
            description: format!(
                "在 workspace 根目录下用系统 shell（{shell} {shell_args}）执行一条命令，\
                 返回退出码、stdout 与 stderr。{}\
                 超时默认 30 秒、上限 300 秒。环境变量中疑似密钥的条目\
                 （名字含 key/token/secret/password/credential）不会传入子进程。",
                capabilities.process_guard_summary()
            ),
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

/// 宿主是否显式放行无完整隔离的开放命令。只认 `1`，其余值（含未设置）
/// 视为未放行。shell 与 git 工具共用这一口径。
pub(crate) fn host_allows_unisolated_commands() -> bool {
    std::env::var(ALLOW_UNISOLATED_COMMANDS_ENV).is_ok_and(|value| value == "1")
}

/// 无完整隔离时对一条命令的开放命令收口裁决。返回 `Some(理由)` 即拒绝
/// 执行，`None` 放行。
///
/// 供 shell 工具与 git 工具共用，避免两套口径。基于实时检测的平台能力
/// 与实时读入的宿主开关（`host_allows_unisolated_commands`），不依赖
/// `RunCommand` 实例。`run_command` 走缓存的实例字段版本以便测试注入；
/// `git_status`/`git_diff` 走这里。
pub(crate) fn unisolated_open_command_refusal(command: &str) -> Option<String> {
    if let UnisolatedDecision::Refused { reason, .. } = unisolated_decision(
        &PlatformCapabilities::detect(),
        &parse_command(command),
        host_allows_unisolated_commands(),
    ) {
        return Some(reason);
    }
    None
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

/// 组装命令：按当前平台选 shell。
///
/// shell 的选择由 [`Platform::shell`] 给出，与二进制编译目标一致。
fn shell_program(command: &str) -> (String, Vec<String>) {
    let (program, args) = Platform::current().shell();
    let mut full = args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>();
    full.push(command.to_owned());
    (program.to_owned(), full)
}

/// Windows 下命令进程的创建标志。
///
/// 命令必须运行在独立的隐藏控制台里。模型脚本常以
/// `subprocess.send_signal(CTRL_BREAK_EVENT)` 收尾，实测（实验脚本 repro/
/// 归档）该事件会广播到整个**共享控制台**，跨进程组、pid 参数无效：共享
/// 框架控制台时框架进程会被一并终止（真实故障，任务无终态）。只有独立
/// 控制台能隔离；新进程组让 CTRL_C 定向到组内。取消路径依赖 Job Object
/// 整树终止，不依赖控制台信号，两者都不影响取消语义。
#[cfg(windows)]
pub(crate) fn command_creation_flags() -> u32 {
    windows::Win32::System::Threading::CREATE_NEW_PROCESS_GROUP.0
        | windows::Win32::System::Threading::CREATE_NO_WINDOW.0
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
    // 两条独立声明：进程沙箱未生效（平台没有或创建失败）与文件/网络隔离
    // 未生效（任何平台都没有实现，Windows 有 Job 也缺这两项）。按能力分别
    // 生成，不虚报。降级说明在前，隔离说明在后，保持既有"降级开头"口径。
    let mut prefix = String::new();
    if !sandbox_active {
        prefix.push_str(SANDBOX_DOWNGRADE_NOTICE);
        prefix.push('\n');
    }
    if let Some(statement) = PlatformCapabilities::detect().unisolation_statement() {
        prefix.push_str(statement);
        prefix.push('\n');
    }
    if !prefix.is_empty() {
        outcome.content = format!("{prefix}{}", outcome.content);
    }
    outcome.with_execution(ExecutionBoundary::process(sandbox_active, boundary_checked))
}

/// 在 `root` 下执行命令并捕获输出。
///
/// 供基准运行器与本模块的测试复用；命令工具与 git 工具走
/// [`run_with_policy`]，各自带自己的策略。
///
/// 命令在进程沙箱中执行。沙箱能力按当前平台判定（见
/// [`PlatformCapabilities::detect`]）：Windows 上是 Job Object，整树内存
/// 与进程数受限，超时或句柄关闭时终止整树；其余平台没有进程沙箱，
/// 超时只终止直接子进程。
///
/// 沙箱不可用时的处理：
/// - 平台未实现（Linux、macOS 等）或 assign 失败：降级执行，不阻断命令。
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

    // Windows 上命令进程用独立的隐藏控制台和新进程组。模型脚本常以
    // `subprocess.send_signal(CTRL_BREAK_EVENT)` 收尾，而该事件会广播到
    // **整个共享控制台**（跨进程组，pid 参数被忽略）：不隔离时框架进程
    // 会被一并终止（真实故障：任务无终态，调用方只看到 0xC000013A）。
    // CREATE_NO_WINDOW 给命令一个独立控制台，广播只波及命令自身；同时
    // 新进程组让 CTRL_C 定向到组内。取消路径依赖 Job Object 整树终止，
    // 不依赖控制台信号，该组合不影响取消语义。
    #[cfg(windows)]
    cmd.creation_flags(command_creation_flags());

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
            let _ = sandbox;
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
            let _ = sandbox;
            // 收口范围按平台能力措辞。没有整树终止能力时说"已终止进程"
            // 会让模型以为后台进程也被收掉了。
            let killed = if PlatformCapabilities::detect().has_tree_kill {
                "已终止整个进程树"
            } else {
                "已终止直接子进程，后代进程可能仍在运行，需要的话请自行清理"
            };
            Err(RunError::Failed {
                message: format!(
                    "命令超时：{} 秒内未结束，{killed}。长任务请拆小，或调大 timeout_seconds（上限 {} 秒）",
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
            // 缺少文件系统与网络隔离时收口开放命令：基线形状（各语言测试
            // 命令与只读 git）放行，开放命令默认拒绝，宿主显式设置
            // QCH_ALLOW_UNISOLATED_COMMANDS=1 才放行。拒绝发生在启动进程
            // 之前，不产生 SandboxApplied 证据；放行的开放命令仍走
            // 降级执行，边界证据 active=false，模型可见文本带未生效声明。
            if let UnisolatedDecision::Refused { reason, .. } = unisolated_decision(
                &PlatformCapabilities::detect(),
                &parse_command(&args.command),
                self.host_allows_unisolated,
            ) {
                return ToolOutcome::refused(reason);
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
    use std::path::{Path, PathBuf};

    fn temp_workspace(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("qch-shell-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建临时目录");
        dir
    }

    /// 从渲染后的观察文本里取出 stdout 段。
    ///
    /// 输出是"退出码 / ── stdout ── / 内容 / ── stderr ── / 内容"四段，
    /// 整段拿去当路径用会把格式噪声一起比进去。
    fn stdout_section(content: &str) -> &str {
        const MARKER: &str = "── stdout ──";
        let start = content
            .find(MARKER)
            .map(|index| index + MARKER.len())
            .unwrap_or(0);
        let rest = &content[start..];
        let end = rest.find("\n── stderr ──").unwrap_or(rest.len());
        rest[..end].trim()
    }

    fn tool(root: &PathBuf) -> RunCommand {
        // 测试命令大多是 `echo` 这类开放命令形状；沙箱收口只放行基线形状，
        // 因此宿主放行开关按测试环境固定开启，未生效声明照常附带。
        let mut command = RunCommand::new(WorkspaceRoot::new(root));
        command.host_allows_unisolated = true;
        command
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

    /// 正常命令必须成功，沙箱状态与当前平台一致。
    ///
    /// 沙箱是否生效由平台决定：Windows 上 Job Object 可用，其他平台没有
    /// 进程沙箱，命令仍要跑通，只是没有保护。
    #[tokio::test]
    async fn successful_command_runs_and_reports_the_platform_sandbox_state() {
        // 回归保护：沙箱接入不能破坏正常命令，成功路径必须返回真实的
        // 边界事实，且不带未生效说明（生效时）。
        let root = temp_workspace("sandbox-ok");
        let cmd = tool(&root);
        let capabilities = PlatformCapabilities::detect();

        let outcome = cmd.invoke(&call(run_args("echo ok"))).await;

        assert_eq!(outcome.status, OutcomeStatus::Success, "{}", outcome.content);
        let raw = run_in_workspace(&WorkspaceRoot::new(root.clone()), "echo ok", Duration::from_secs(30))
            .await
            .expect("执行成功");
        assert_eq!(
            raw.sandbox_active,
            capabilities.has_process_sandbox,
            "沙箱状态必须与平台能力一致"
        );
        if capabilities.has_process_sandbox {
            assert!(
                !outcome.content.contains("进程沙箱未生效"),
                "沙箱生效时不应带未生效说明: {}",
                outcome.content
            );
        } else {
            assert!(
                outcome.content.starts_with("⚠️ 进程沙箱未生效"),
                "没有进程沙箱的平台必须声明未受保护: {}",
                outcome.content
            );
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 超时必须终止整棵进程树。
    ///
    /// 受害进程 parent 再创建 child，两代各写各的日志：`<日志>.parent` 是
    /// harness 直接子进程写的，`<日志>.child` 是孙进程写的。1.5 秒超时后：
    ///
    /// - parent 日志必须停止增长。直接子进程在任何平台都要被收住，这是
    ///   超时处理的底线。
    /// - 有整树终止能力的平台，child 日志也必须停止增长。
    /// - 没有整树终止能力的平台，child 会成为孤儿继续写。这是已知边界，
    ///   不断言它停止，但要记在案里。
    ///
    /// POSIX 上命令带 `exec`：shell 顶替成受害进程，它才是 harness 的直接
    /// 子进程，超时收住直接子进程时必然跟着死。不带 exec 时 shell 会
    /// fork，受害进程成了后代，只有整树终止能力才能收住它——那是
    /// Windows 的语义，不是 POSIX 的。
    ///
    /// 两代必须分文件：混在一个文件里，"直接子进程已死、孙进程还活着"
    /// 与"两代都活着"看不出区别，断言会退化成恒真。
    ///
    /// 探针为 examples/sandbox-victim.rs。整树终止依赖平台能力，由
    /// [`PlatformCapabilities::detect`] 判定。
    #[cfg(windows)]
    #[test]
    fn windows_commands_run_in_an_isolated_console() {
        // 回归锚点：真实故障（L3 实验任务）里模型脚本发出的
        // CTRL_BREAK 广播杀死了与命令共享控制台的框架进程。CTRL_BREAK 的
        // 广播跨进程组，只有 CREATE_NO_WINDOW 的独立控制台能隔离。
        // cargo test 的测试进程没有控制台（AllocConsole 失败、独立控制台
        // 替身也不触发广播），无法在本环境复现广播行为；修复对真实故障
        // 形态的有效性由实验脚本（无修复调用链死亡、带修复存活，同一
        // smoke_test 脚本对照）独立验证。这里锚定 flags 组合本身，防止
        // 无意的回退。
        assert_eq!(
            command_creation_flags(),
            windows::Win32::System::Threading::CREATE_NEW_PROCESS_GROUP.0
                | windows::Win32::System::Threading::CREATE_NO_WINDOW.0,
            "命令进程必须在独立隐藏控制台和新进程组中运行"
        );
    }

    #[tokio::test]
    async fn timeout_kills_the_whole_process_tree() {
        let root = temp_workspace("killtree");
        let log = root.join("victim.log");

        let command = if Platform::current() == Platform::Windows {
            format!("{} {}", victim_exe().display(), log.display())
        } else {
            format!("exec {} {}", victim_exe().display(), log.display())
        };
        let outcome = run_in_workspace(
            &WorkspaceRoot::new(root.clone()),
            &command,
            Duration::from_millis(1500),
        )
        .await;
        assert!(
            outcome.is_err() && outcome.err().unwrap().to_string().contains("超时"),
            "受害进程必须被超时收口"
        );

        // 给未被终止的进程留出两轮写入时间，再验证日志停止增长。
        tokio::time::sleep(Duration::from_millis(700)).await;
        let parent_after_kill = count_lines(&suffixed(&log, ".parent"));
        let child_after_kill = count_lines(&suffixed(&log, ".child"));
        assert!(parent_after_kill > 0, "超时前 victim 应已写过日志");
        assert!(child_after_kill > 0, "孙进程应已建立自己的日志");
        tokio::time::sleep(Duration::from_millis(700)).await;

        let parent_now = count_lines(&suffixed(&log, ".parent"));
        assert_eq!(
            parent_now, parent_after_kill,
            "直接子进程必须已终止，否则连它都没收住"
        );

        let child_now = count_lines(&suffixed(&log, ".child"));
        if PlatformCapabilities::detect().has_tree_kill {
            assert_eq!(
                child_now, child_after_kill,
                "有整树终止能力时孙进程也必须停止写入"
            );
        } else {
            // 没有整树终止能力时，孙进程成为孤儿继续写是已知边界。不断言
            // 它停止，但要确认它确实还活着，否则这条分支就退化成恒真。
            assert!(
                child_now > child_after_kill,
                "没有整树终止能力的平台上，孙进程应当继续写入（已知边界）"
            );
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 受害探针的可执行文件路径。examples 与 lib 位于同一 target 的兄弟
    /// 目录，测试前由构建系统编译（examples 属于 dev target）。
    fn victim_exe() -> std::path::PathBuf {
        let path = std::env::current_exe()
            .expect("测试进程路径")
            .parent()
            .and_then(|p| p.parent())
            .map(|p| {
                p.join("examples")
                    .join(format!("sandbox-victim{}", std::env::consts::EXE_SUFFIX))
            })
            .expect("定位探针");
        assert!(path.exists(), "探针未编译（cargo test 会编 examples）: {}", path.display());
        path
    }

    fn count_lines(path: &std::path::Path) -> usize {
        std::fs::read_to_string(path)
            .map(|text| text.lines().count())
            .unwrap_or(0)
    }

    /// 给路径追加后缀，与受害探针的分日志约定保持一致。
    fn suffixed(path: &std::path::Path, suffix: &str) -> PathBuf {
        let mut text = path.as_os_str().to_owned();
        text.push(suffix);
        PathBuf::from(text)
    }

    /// 沙箱不可用时的两条路径由平台能力决定，走不同结局。
    ///
    /// - 平台有进程沙箱（Windows）：创建失败时拒绝执行（fail-closed）。
    ///   用明确未实现的 `cpu_time` 限额稳定触发创建失败，不依赖宿主机权限
    ///   或资源耗尽。降级裸跑会让模型和策略误以为有沙箱，因此无法创建
    ///   沙箱就不执行。
    /// - 平台没有进程沙箱（Linux、macOS 等）：降级执行，并在输出开头
    ///   说明。"平台本来没有能力"与"有能力但创建失败"必须走不同分支。
    ///   前者若拒绝执行，该平台将完全无法运行命令；后者拒绝执行才是
    ///   正确行为。
    ///
    /// 两条断言都在每个平台真实执行，不靠编译期门控跳过。
    #[tokio::test]
    async fn sandbox_availability_follows_the_detected_platform() {
        let root = temp_workspace("sandbox-policy");
        let mut cmd = tool(&root);
        cmd.policy.cpu_time = Some(Duration::from_secs(1));

        if PlatformCapabilities::detect().has_process_sandbox {
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
        } else {
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
        }
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
        // `type` 是 cmd 的内置命令，POSIX 的 `type` 语义不同（查命令而非
        // 读文件），读文件用 `cat`。
        let read_file = if Platform::current() == Platform::Windows {
            "type inside.txt"
        } else {
            "cat inside.txt"
        };
        let outcome = cmd.invoke(&call(run_args(read_file))).await;
        assert_eq!(outcome.status, OutcomeStatus::Success, "{}", outcome.content);
        assert!(outcome.execution.is_some(), "真启动的命令要带边界事实");

        // 根内绝对路径同样放行。模型常写 `type D:\repo\src\main.rs`，
        // 硬拒绝必须识别出该路径位于根内。
        let absolute = root.join("inside.txt");
        let read_absolute = if Platform::current() == Platform::Windows {
            format!("type {}", absolute.display())
        } else {
            format!("cat {}", absolute.display())
        };
        let outcome = cmd.invoke(&call(run_args(&read_absolute))).await;
        assert_eq!(
            outcome.status,
            OutcomeStatus::Success,
            "{}",
            outcome.content
        );

        // 分段后的收窄与放行：越界藏在第二段仍会被拒绝，且理由要点名
        // 该段。整串扫描也能发现越界，但理由中的动词与段号必须准确。
        let first_segment = if Platform::current() == Platform::Windows {
            "type inside.txt"
        } else {
            "cat inside.txt"
        };
        let chained = format!("{first_segment} && echo x > {}", outside_target());
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

        // echo 在 cmd 与 sh 下都是同一条命令，无需分平台。
        let outcome = cmd.invoke(&call(run_args("echo hello_from_shell"))).await;

        assert_eq!(outcome.status, OutcomeStatus::Success, "{}", outcome.content);
        assert!(outcome.content.contains("退出码: 0"), "{}", outcome.content);
        assert!(
            outcome.content.to_lowercase().contains("hello_from_shell"),
            "{}",
            outcome.content
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 给模型看的描述必须与当前平台的实际能力一致。
    ///
    /// 描述是模型唯一的平台信息来源：它据此判断超时会不会留下后台进程。
    /// 平台没有进程沙箱却声称"超时终止全部后代进程"，模型会放心地留下
    /// 不再等待的进程，孤儿进程在宿主机上继续跑。
    #[test]
    fn the_model_facing_description_matches_the_platform_capabilities() {
        let capabilities = PlatformCapabilities::detect();
        let description = RunCommand::spec().description;

        let (shell, args) = capabilities.platform.shell();
        assert!(
            description.contains(&format!("{shell} {}", args.join(" "))),
            "描述必须写明本平台用的 shell: {description}"
        );
        if capabilities.has_process_sandbox {
            assert!(
                description.contains("整树内存与进程数受限"),
                "有进程沙箱的平台应说明资源上限: {description}"
            );
            assert!(
                description.contains("超时终止全部后代进程"),
                "有整树终止能力时应说明超时收得干净: {description}"
            );
        } else {
            assert!(
                description.contains("没有进程沙箱"),
                "没有进程沙箱的平台必须说明这一点: {description}"
            );
            assert!(
                description.contains("超时只终止直接子进程"),
                "没有整树终止能力时必须说明残留风险: {description}"
            );
            assert!(
                !description.contains("超时终止全部后代进程"),
                "不能声称本平台做不到的事: {description}"
            );
        }
    }

    #[tokio::test]
    async fn command_runs_in_the_workspace_root() {
        let root = temp_workspace("cwd");
        let cmd = tool(&root);

        // 打印当前目录，确认子进程的 cwd 是 workspace 根。
        let command = if Platform::current() == Platform::Windows {
            "cd"
        } else {
            "pwd"
        };
        let outcome = cmd.invoke(&call(run_args(command))).await;

        assert_eq!(outcome.status, OutcomeStatus::Success, "{}", outcome.content);
        // 比的是"两条路径指向同一个目录"，不是两串文本：Windows 上 `cd`
        // 可能打印 8.3 短名（RUNNER~1），macOS 上 /tmp 是 /private/tmp 的
        // 软链。两侧都 canonicalize 之后才是同一串可比的路径。
        let printed = stdout_section(&outcome.content);
        let actual = Path::new(printed)
            .canonicalize()
            .unwrap_or_else(|error| panic!("输出应是可解析的当前目录: {printed} ({error})"))
            .to_string_lossy()
            .into_owned();
        let expected = root
            .canonicalize()
            .expect("workspace 根应存在")
            .to_string_lossy()
            .into_owned();
        // 比较口径跟平台的文件系统语义走：Windows 与 macOS 不区分大小写，
        // Linux 区分。全平台都折小写会让 Linux 上的断言变松。
        let fold = |text: &str| {
            if Platform::current().has_case_insensitive_paths() {
                text.to_lowercase()
            } else {
                text.to_owned()
            }
        };
        assert_eq!(
            fold(&actual),
            fold(&expected),
            "输出应指向 workspace 根: {}\nroot: {expected}",
            outcome.content
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn nonzero_exit_is_failed_but_output_comes_back() {
        let root = temp_workspace("exitcode");
        let cmd = tool(&root);

        // 用一条退出码非 0 且带输出的命令。
        let command = if Platform::current() == Platform::Windows {
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
        // shell 本身启动成功，报错以非 0 退出码和 stderr 文本返回。这符合
        // "非 0 记为 failed 但必须返回输出"的约定，模型能读到原因。
        //
        // 退出码按 shell 而定：`cmd /C` 找不到命令返回 1，`sh -c` 返回
        // 127。这里断言"非 0"而不是某个具体数字，否则测试在另一个平台上
        // 必红。
        assert!(
            outcome.content.contains("退出码: 1") || outcome.content.contains("退出码: 127"),
            "{}",
            outcome.content
        );
        assert!(!outcome.content.trim().ends_with("── stderr ──"), "stderr 应有报错文本");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn timeout_kills_the_child_process() {
        let root = temp_workspace("kill");
        let cmd = tool(&root);

        // 命令休眠 3 秒后写标记文件。若超时只返回错误而进程仍存活，
        // 标记会在 3 秒后写出；进程被终止时标记不会出现。
        let command = if Platform::current() == Platform::Windows {
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
        let command = if Platform::current() == Platform::Windows {
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
        let command = if Platform::current() == Platform::Windows {
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

    /// shell 的选择必须与平台检测一致。
    ///
    /// 判据不重复 `Platform::current` 的实现，直接比对该平台给出的 shell，
    /// 否则实现写错时测试会跟着错。
    #[test]
    fn shell_program_picks_per_platform() {
        let (program, args) = shell_program("cargo test");
        let (expected_program, expected_args) = Platform::current().shell();
        assert_eq!(program, expected_program);
        assert_eq!(
            args,
            expected_args
                .iter()
                .map(|arg| (*arg).to_owned())
                .chain(std::iter::once("cargo test".to_owned()))
                .collect::<Vec<_>>()
        );
    }

    // ── 开放命令收口在 run_command 执行路径上的接线 ────────────────

    /// 生产默认口径下，开放命令在启动进程之前被拒绝，不产生执行边界证据，
    /// 也不启动任何子进程。拒绝理由带稳定码与开关建议。
    #[tokio::test]
    async fn invoke_refuses_open_commands_by_default_without_spawn() {
        let root = temp_workspace("gate-open");
        let mut cmd = RunCommand::new(WorkspaceRoot::new(root.clone()));
        cmd.host_allows_unisolated = false;

        let outcome = cmd.invoke(&call(run_args("echo x > open_default.txt"))).await;

        assert_eq!(
            outcome.status,
            OutcomeStatus::Refused,
            "开放命令默认应被收口拒绝: {}",
            outcome.content
        );
        assert!(
            outcome.content.contains(crate::domain::sandbox::REFUSAL_CODE_UNISOLATED),
            "拒绝理由带稳定码: {}",
            outcome.content
        );
        assert!(
            outcome.content.contains(ALLOW_UNISOLATED_COMMANDS_ENV),
            "拒绝理由给出口令建议: {}",
            outcome.content
        );
        assert!(
            outcome.execution.is_none(),
            "拒绝发生在启动进程之前，不能伪造执行边界"
        );
        assert!(
            !root.join("open_default.txt").exists(),
            "被拒的开放命令不得执行重定向，落盘文件不应出现"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 基线形状在生产默认口径下照常放行：正常编码任务不被误阻断。这里
    /// 真跑一条基线命令（`cargo check` 太重，用 git 只读形态），验证
    /// 收口不会把基线命令挡掉。git 不在 PATH 时以非 0 退出码返回，不
    /// 影响"未拒绝"的判定。
    #[tokio::test]
    async fn invoke_allows_baseline_shapes_by_default() {
        let root = temp_workspace("gate-baseline");
        let mut cmd = RunCommand::new(WorkspaceRoot::new(root.clone()));
        cmd.host_allows_unisolated = false;

        let outcome = cmd.invoke(&call(run_args("git status"))).await;
        assert!(
            outcome.status != OutcomeStatus::Refused,
            "基线形状 git status 不应被收口拒绝: {}",
            outcome.content
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 基线命令接输出观察器在生产默认口径下照常放行：真实运行里
    /// `node --test 2>&1 | tail -30` 这类形状是最普通的验证方式，收口不能
    /// 把它整条拒掉。这里用 git 只读形态接 `more` 验证接线：git 在临时
    /// 目录里必然报「不是仓库」，但进程确实起来了，判定只看「没被收口
    /// 拒绝、真的执行了」。
    #[tokio::test]
    async fn invoke_allows_baseline_with_output_observers_by_default() {
        let root = temp_workspace("gate-observer");
        let mut cmd = RunCommand::new(WorkspaceRoot::new(root.clone()));
        cmd.host_allows_unisolated = false;

        let outcome = cmd
            .invoke(&call(run_args("git status 2>&1 | more")))
            .await;

        assert!(
            outcome.status != OutcomeStatus::Refused,
            "基线加输出观察器不应被收口拒绝: {}",
            outcome.content
        );
        assert!(
            outcome.execution.is_some(),
            "放行后应真的启动进程并留下执行边界"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 宿主显式放行后开放命令执行，模型可见文本仍带未生效声明：证据如实
    /// 反映「执行但无完整隔离」，未生效声明不因放行而省略。
    #[tokio::test]
    async fn host_opt_in_runs_open_commands_with_unisolation_statement() {
        let root = temp_workspace("gate-optin");
        let mut cmd = RunCommand::new(WorkspaceRoot::new(root.clone()));
        cmd.host_allows_unisolated = true;

        let outcome = cmd.invoke(&call(run_args("echo optin_ran"))).await;

        assert_eq!(
            outcome.status,
            OutcomeStatus::Success,
            "宿主放行后开放命令应执行: {}",
            outcome.content
        );
        // 未生效声明按平台能力生成：缺文件与网络隔离时必然附带。
        if PlatformCapabilities::detect().unisolation_statement().is_some() {
            assert!(
                outcome.content.contains("未生效"),
                "放行执行仍要声明隔离未生效: {}",
                outcome.content
            );
        }
        let boundary = outcome.execution.expect("真启动的命令要带执行边界");
        // 执行边界如实反映进程沙箱状态，不虚报：有进程沙箱的平台 active
        // 为 true，没有的平台为 false。文件与网络隔离由模型可见文本的
        // 未生效声明承担，不塞进执行边界字段。
        assert_eq!(
            boundary.active,
            PlatformCapabilities::detect().has_process_sandbox,
            "进程沙箱状态必须与平台能力一致"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}

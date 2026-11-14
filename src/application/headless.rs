//! `exec` 无头模式的入口契约与结果转换。
//!
//! 这一层不接触模型、文件系统或进程退出：命令行参数、单次执行抽象和
//! 退出码/JSON 转换都是纯逻辑，便于脚本与 CI 用固定输出结构验收。真实
//! `AgentRunner` 装配交给调用方注入，避免无头路径偷偷打开交互闸门。

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::domain::benchmark::BenchmarkResult;
use crate::domain::permission_profile::{ActionScope, PermissionProfile};

/// 成功。
pub const EXIT_SUCCESS: i32 = 0;
/// 模型、工具或装配等其他运行失败。
pub const EXIT_ERROR: i32 = 1;
/// 命令行参数错误。
pub const EXIT_INVALID_ARGS: i32 = 2;
/// 审批拒绝；无头模式不等待人工答复。
pub const EXIT_APPROVAL_REFUSED: i32 = 3;
/// 运行超时。
pub const EXIT_TIMEOUT: i32 = 4;

/// `exec` 解析后的运行参数。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeadlessArgs {
    /// 单次任务的文本。
    pub task: String,
    /// 审批档位；`None` 表示采用最严格的只读默认。
    pub profile: Option<PermissionProfile>,
    /// stdout 是否输出基准结果结构。
    pub json: bool,
    /// 墙钟超时秒数。
    pub timeout_secs: Option<u64>,
    /// 工作区路径。
    pub workspace: PathBuf,
}

impl HeadlessArgs {
    /// 实际生效的档位：未指定时按只读处理。
    fn effective_profile(&self) -> PermissionProfile {
        self.profile.unwrap_or(PermissionProfile::ReadOnly)
    }
}

/// 参数错误，统一映射为退出码 2。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeadlessError(pub String);

impl std::fmt::Display for HeadlessError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for HeadlessError {}

/// 解析 `exec` 子命令的 argv，不包含程序名。
///
/// CLI 直接转发原始 argv 时会带上子命令字面量 `exec`，库内调用也可以只传
/// 选项和任务；两种写法指向同一份参数。只接受一个位置任务，`--json`、
/// `--permissions`、`--timeout` 为显式选项。`--permissions` 的既有扩展工具
/// 名列表由 CLI 层先行过滤，本函数只认三档 profile 值，未知值按参数错误返回。
pub fn parse_exec(argv: &[String]) -> Result<HeadlessArgs, HeadlessError> {
    let mut task = None;
    let mut profile = None;
    let mut json = false;
    let mut timeout_secs = None;
    let mut workspace = None;
    let mut iter = argv.iter();
    if argv.first().map(String::as_str) == Some("exec") {
        iter.next();
    }

    while let Some(raw) = iter.next() {
        match raw.as_str() {
            "--json" => json = true,
            "--permissions" => {
                let value = iter
                    .next()
                    .ok_or_else(|| HeadlessError("--permissions 缺少档位".to_owned()))?;
                profile = Some(
                    PermissionProfile::parse(value)
                        .ok_or_else(|| HeadlessError(format!("未知权限档 `{value}`")))?,
                );
            }
            "--timeout" => {
                let value = iter
                    .next()
                    .ok_or_else(|| HeadlessError("--timeout 缺少秒数".to_owned()))?;
                let seconds = value.parse::<u64>().map_err(|_| {
                    HeadlessError(format!("--timeout 需要正整数秒数，收到 `{value}`"))
                })?;
                if seconds == 0 {
                    return Err(HeadlessError("--timeout 必须大于 0".to_owned()));
                }
                timeout_secs = Some(seconds);
            }
            "--workspace" => {
                let value = iter
                    .next()
                    .ok_or_else(|| HeadlessError("--workspace 缺少路径".to_owned()))?;
                workspace = Some(PathBuf::from(value));
            }
            option if option.starts_with('-') => {
                return Err(HeadlessError(format!("exec 不支持选项 `{option}`")));
            }
            _ => {
                if task.is_some() {
                    return Err(HeadlessError("exec 只接受一个任务文本".to_owned()));
                }
                task = Some(raw.clone());
            }
        }
    }

    Ok(HeadlessArgs {
        task: task.ok_or_else(|| HeadlessError("exec 缺少任务文本".to_owned()))?,
        profile,
        json,
        timeout_secs,
        workspace: workspace.unwrap_or_else(|| PathBuf::from(".")),
    })
}

/// 单次执行请求，传给宿主注入的运行器。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeadlessRunRequest {
    /// 任务文本。
    pub task: String,
    /// 生效档位。
    pub profile: PermissionProfile,
    /// 墙钟超时秒数。
    pub timeout_secs: Option<u64>,
    /// 工作区路径。
    pub workspace: PathBuf,
}

/// 单次执行失败的分类；每一类对应一个可脚本判断的退出码。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeadlessRunError {
    /// 审批被拒或无头模式未获授权。
    ApprovalRefused(String),
    /// 墙钟/模型超时。
    TimedOut(u64),
    /// 模型、工具、装配等其他失败。
    Failed(String),
}

/// 扩展点：宿主注入一次完整运行，测试可替换为确定性脚本。
pub trait RunOnce: Send + Sync {
    /// 运行一次任务；成功返回最终答案。
    fn run_once(&self, request: &HeadlessRunRequest) -> Result<String, HeadlessRunError>;
}

/// 无头运行的用户可见结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeadlessOutcome {
    /// 参数错误。
    InvalidArguments(String),
    /// 成功，`String` 是最终答案。
    Success(String),
    /// 审批拒绝，理由可用于脚本日志。
    ApprovalRefused(String),
    /// 超时，携带超时秒数。
    TimedOut(u64),
    /// 其他失败。
    Failed(String),
}

impl HeadlessOutcome {
    /// 映射为进程退出码。
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::InvalidArguments(_) => EXIT_INVALID_ARGS,
            Self::Success(_) => EXIT_SUCCESS,
            Self::ApprovalRefused(_) => EXIT_APPROVAL_REFUSED,
            Self::TimedOut(_) => EXIT_TIMEOUT,
            Self::Failed(_) => EXIT_ERROR,
        }
    }

    /// 转成基准评测的 JSON 结果数组。
    ///
    /// `--json` 与 `--benchmark` 共用 `Vec<BenchmarkResult>`：一条 exec
    /// 任务就是单项结果，未知 token 与成本保持 `None`，不能补 0。
    pub fn json_results(&self) -> Vec<BenchmarkResult> {
        let (passed, failures) = match self {
            Self::Success(_) => (true, vec![]),
            HeadlessOutcome::ApprovalRefused(reason) => (false, vec![reason.clone()]),
            other => (false, vec![failure_text(other)]),
        };
        vec![BenchmarkResult {
            task_id: "exec".to_owned(),
            language: None,
            session_id: None,
            passed,
            failures,
            steps_completed: usize::from(passed),
            turns_used: u32::from(passed),
            wall_clock_ms: 0,
            approvals: usize::from(matches!(self, Self::ApprovalRefused(_))),
            cache_hit_ratio: None,
            cost_usd: None,
            metrics: Default::default(),
        }]
    }
}

/// 失败原因的文本；`success` 不产生失败项。
fn failure_text(outcome: &HeadlessOutcome) -> String {
    match outcome {
        HeadlessOutcome::InvalidArguments(reason) => format!("参数错误：{reason}"),
        HeadlessOutcome::ApprovalRefused(reason) => format!("审批拒绝：{reason}"),
        HeadlessOutcome::TimedOut(seconds) => format!("运行超时：{seconds} 秒"),
        HeadlessOutcome::Failed(reason) => format!("运行失败：{reason}"),
        HeadlessOutcome::Success(_) => String::new(),
    }
}

/// 无头运行器：参数校验、单次执行、结果分类集中在一处。
pub struct HeadlessRunner {
    run_once: Box<dyn RunOnce>,
    enforce_default_read_only: bool,
}

impl HeadlessRunner {
    /// 注入一次运行的执行器。
    ///
    /// 测试与库内组合可执行器通过 `RunOnce` 决定结果；面向真实 CLI 的
    /// `DefaultRunOnce` 才负责"无 profile 默认只读"的启动前收口，以免把
    /// 可注入运行的错误分类测试短路成默认拒绝。
    pub fn new(run_once: Box<dyn RunOnce>) -> Self {
        Self {
            run_once,
            enforce_default_read_only: false,
        }
    }

    /// 打开默认只读收口；仅用于没有注入真实执行器的命令行默认路径。
    pub fn with_default_read_only(mut self) -> Self {
        self.enforce_default_read_only = true;
        self
    }

    /// 执行已解析参数，返回可映射退出码的结果。
    pub fn run(&self, args: HeadlessArgs) -> HeadlessOutcome {
        if self.enforce_default_read_only {
            if let Some(reason) = default_refusal(&args) {
                return HeadlessOutcome::ApprovalRefused(reason);
            }
        }
        let request = HeadlessRunRequest {
            task: args.task.clone(),
            profile: args.effective_profile(),
            timeout_secs: args.timeout_secs,
            workspace: args.workspace.clone(),
        };
        match self.run_once.run_once(&request) {
            Ok(answer) => HeadlessOutcome::Success(answer),
            Err(HeadlessRunError::ApprovalRefused(reason)) => {
                HeadlessOutcome::ApprovalRefused(reason)
            }
            Err(HeadlessRunError::TimedOut(seconds)) => HeadlessOutcome::TimedOut(seconds),
            Err(HeadlessRunError::Failed(reason)) => HeadlessOutcome::Failed(reason),
        }
    }
}

/// 无 profile 时按只读默认拒绝任务里的非只读动作。
///
/// 无头模式没有 stdin 可以追问，任务文本里显式出现写文件或开放命令时
/// 必须在启动模型前拒绝；未写动作的任务仍按只读档运行。
fn default_refusal(args: &HeadlessArgs) -> Option<String> {
    if args.profile.is_some() {
        return None;
    }
    let call = tool_call_from_task(&args.task)?;
    let scope = PermissionProfile::classify(&call);
    if scope == ActionScope::ReadOnly {
        return None;
    }
    Some(format!(
        "exec 未指定 --permissions，默认只读档拒绝{}动作",
        scope.as_str()
    ))
}

/// 供真实 CLI 在进入模型前调用同一份默认只读判据。
///
/// 真实执行路径不走 `HeadlessRunner`（那是同步桩路径），但启动前收口
/// 必须是同一份逻辑，否则"没给 profile 却要求写动作"在两条路径上会
/// 得到不同结论。
pub fn default_read_only_refusal(args: &HeadlessArgs) -> Option<String> {
    default_refusal(args)
}

/// 从任务文本里识别用户可见的写/开放动作。
///
/// 这是参数边界上的保守检查，不是命令解析器：只把明确写出的 shell 任务
/// 或写文件工具名映射到等价 `ToolCall`，让领域分类做一个来源的判断。
fn tool_call_from_task(task: &str) -> Option<crate::domain::tool::ToolCall> {
    let lowered = task.to_ascii_lowercase();
    if lowered.contains("write_file")
        || lowered.contains("edit_file")
        || lowered.contains("apply_patch")
        || lowered.contains("写文件")
        || lowered.contains("写入")
        || lowered.contains("修改文件")
    {
        return Some(call("write_file", Value::Null));
    }
    if let Some(command) = lowered.strip_prefix("run:") {
        return Some(call(
            "run_command",
            serde_json::json!({ "command": command.trim() }),
        ));
    }
    if lowered.contains("curl ")
        || lowered.contains("rm ")
        || lowered.contains("git push")
        || lowered.contains("开放命令")
    {
        return Some(call("run_command", serde_json::json!({ "command": task })));
    }
    None
}

/// 造一次等价工具调用，仅用于参数边界分类。
fn call(name: &str, arguments: Value) -> crate::domain::tool::ToolCall {
    crate::domain::tool::ToolCall {
        id: crate::domain::ids::CallId::new("exec-default-check"),
        name: name.to_owned(),
        arguments,
    }
}

/// 格式化 stdout：文本模式给最终答案或失败原因，JSON 模式给基准结果数组。
pub fn format_output(outcome: &HeadlessOutcome, json: bool) -> String {
    if json {
        return serde_json::to_string_pretty(&outcome.json_results())
            .expect("BenchmarkResult 总能序列化");
    }
    match outcome {
        HeadlessOutcome::Success(answer) => answer.clone(),
        other => failure_text(other),
    }
}

/// 在指定工作区按 argv 运行一次，返回不含副作用的结果。
///
/// 宿主注入真实 `RunOnce` 后即可接到 `main.rs`；当前默认路径启用只读
/// 收口，让"没给 profile 却要求写动作"在脚本里稳定得到退出码 3。
pub fn execute_in_workspace(
    argv: &[String],
    workspace: &Path,
    profile: Option<PermissionProfile>,
    timeout_secs: Option<u64>,
) -> HeadlessOutcome {
    let args = match parse_exec(argv) {
        Ok(args) => args,
        Err(error) => return HeadlessOutcome::InvalidArguments(error.0),
    };
    let args = HeadlessArgs {
        profile: profile.or(args.profile),
        timeout_secs: timeout_secs.or(args.timeout_secs),
        workspace: workspace.to_path_buf(),
        ..args
    };
    HeadlessRunner::new(Box::new(DefaultRunOnce))
        .with_default_read_only()
        .run(args)
}

/// 默认单次执行器：真实模型未注入时，只读任务返回明确答复。
struct DefaultRunOnce;

impl RunOnce for DefaultRunOnce {
    fn run_once(&self, request: &HeadlessRunRequest) -> Result<String, HeadlessRunError> {
        Ok(format!(
            "无头任务已接收：{}（{}）",
            request.task,
            request.profile.as_str()
        ))
    }
}

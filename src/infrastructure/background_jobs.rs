//! 后台作业注册器与 `job_list` 只读工具。
//!
//! 子进程与前台 `run_command` 的环境脱敏、shell 选择、Job Object 整树终止
//! 语义保持一致。持久终端（PTY）是后继切片，本模块不实现会话复用或交互输入。

use std::collections::BTreeMap;
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use serde::Deserialize;
use serde_json::{Value, json};

use crate::application::background_jobs::{
    BackgroundJobRegistry, BackgroundJobSpec, JobId, JobRecord, JobStatus,
};
use crate::application::port::{BoxFuture, ToolInvoker};
use crate::application::tool_registry::{ToolEntry, ToolSource};
use crate::domain::platform::Platform;
use crate::domain::sandbox::{PlatformCapabilities, SandboxPolicy};
use crate::domain::tool::{ToolAnnotations, ToolCall, ToolOutcome, ToolSpec};
use crate::infrastructure::sandbox::{JobGuard, assign_child};

/// `run_command` 是否转入后台；前台路径由现有 shell 工具继续执行。
pub const RUN_BACKGROUND_ARG: &str = "background";
/// `job_list` 工具名。
pub const JOB_LIST_TOOL: &str = "job_list";

struct JobState {
    command: String,
    status: JobStatus,
    note: String,
}

/// 进程内作业表。状态只有一个 writer：等待子进程的独立任务。
#[derive(Default)]
pub struct ProcessJobRegistry {
    next: Mutex<u64>,
    jobs: Arc<Mutex<BTreeMap<u64, JobState>>>,
}

impl ProcessJobRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    fn update(&self, id: u64, status: JobStatus, note: &str) {
        let mut jobs = self.jobs.lock().expect("作业表锁不应中毒");
        if let Some(job) = jobs.get_mut(&id) {
            job.status = status;
            job.note.push_str(note);
        }
    }
}

impl BackgroundJobRegistry for ProcessJobRegistry {
    fn spawn(&self, spec: BackgroundJobSpec) -> BoxFuture<'_, Result<JobId, String>> {
        Box::pin(async move {
            if spec.command.trim().is_empty() {
                return Err("后台命令不能为空".to_owned());
            }
            if spec.timeout.is_zero() {
                return Err("后台命令超时必须大于 0".to_owned());
            }
            let id = {
                let mut next = self.next.lock().expect("作业 ID 锁不应中毒");
                *next += 1;
                *next
            };
            let command = spec.command.clone();
            self.jobs.lock().expect("作业表锁不应中毒").insert(
                id,
                JobState {
                    command: command.clone(),
                    status: JobStatus::Running,
                    note: String::new(),
                },
            );
            match start_child(&spec) {
                Ok((child, job_guard, active)) => {
                    if !active {
                        self.update(id, JobStatus::Running, "进程沙箱未生效；");
                    }
                    let registry = Arc::new(RegistrySnapshot {
                        jobs: Arc::clone(&self.jobs),
                    });
                    tokio::spawn(wait_job(
                        registry,
                        id,
                        child,
                        job_guard,
                        spec.timeout,
                        active,
                    ));
                    Ok(JobId(id))
                }
                Err(error) => {
                    self.jobs.lock().expect("作业表锁不应中毒").remove(&id);
                    Err(error)
                }
            }
        })
    }

    fn list(&self) -> BoxFuture<'_, Result<Vec<JobRecord>, String>> {
        Box::pin(async move {
            let jobs = self.jobs.lock().expect("作业表锁不应中毒");
            Ok(jobs
                .iter()
                .map(|(id, job)| JobRecord {
                    id: JobId(*id),
                    command: job.command.clone(),
                    status: job.status,
                    note: job.note.clone(),
                })
                .collect())
        })
    }
}

/// 等待任务只持作业表，避免把运行时句柄泄漏到查询接口。
struct RegistrySnapshot {
    jobs: Arc<Mutex<BTreeMap<u64, JobState>>>,
}

impl RegistrySnapshot {
    fn finish(&self, id: u64, status: JobStatus, note: &str) {
        let mut jobs = self.jobs.lock().expect("作业表锁不应中毒");
        if let Some(job) = jobs.get_mut(&id) {
            job.status = status;
            job.note.push_str(note);
        }
    }
}

/// 启动后台子进程。Windows 先建 Job 再 spawn，让后代进程纳入整树范围。
fn start_child(
    spec: &BackgroundJobSpec,
) -> Result<(tokio::process::Child, Option<JobGuard>, bool), String> {
    let (program, args) = Platform::current().shell();
    let mut full = args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>();
    full.push(spec.command.clone());

    let mut cmd = tokio::process::Command::new(program);
    cmd.args(&full)
        .current_dir(&spec.root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    cmd.creation_flags(
        windows::Win32::System::Threading::CREATE_NEW_PROCESS_GROUP.0
            | windows::Win32::System::Threading::CREATE_NO_WINDOW.0,
    );
    for (name, _) in std::env::vars() {
        let lower = name.to_lowercase();
        if ["key", "token", "secret", "password", "credential"]
            .iter()
            .any(|needle| lower.contains(needle))
        {
            cmd.env_remove(&name);
        }
    }

    let capabilities = PlatformCapabilities::detect();
    let job = match JobGuard::create(&SandboxPolicy::default()) {
        Ok(job) => Some(job),
        Err(error) if !capabilities.has_process_sandbox => {
            eprintln!("警告：{error}");
            None
        }
        Err(error) => {
            return Err(format!("沙箱创建失败: {error}（fail-closed：拒绝执行）"));
        }
    };
    let child = cmd
        .spawn()
        .map_err(|error| format!("启动后台命令失败: {error}"))?;
    let job = job.and_then(|guard| match assign_child(&guard, &child) {
        Ok(()) => Some(guard),
        Err(error) => {
            eprintln!("警告：后台进程纳入沙箱失败: {error}");
            None
        }
    });
    let active = job.is_some();
    Ok((child, job, active))
}

/// 等待进程结束，超时显式终止整树并回收直接子进程；异常不静默吞掉。
async fn wait_job(
    registry: Arc<RegistrySnapshot>,
    id: u64,
    mut child: tokio::process::Child,
    job: Option<JobGuard>,
    timeout: std::time::Duration,
    sandbox_active: bool,
) {
    let note = match tokio::time::timeout(timeout, child.wait()).await {
        Ok(Ok(status)) => (
            JobStatus::Exited {
                code: status.code().unwrap_or(-1),
            },
            String::new(),
        ),
        Ok(Err(error)) => (
            JobStatus::Exited { code: -1 },
            format!("等待进程失败: {error}；"),
        ),
        Err(_) => {
            if let Some(job) = &job {
                job.terminate_tree();
            }
            if let Err(error) = child.kill().await {
                registry.finish(
                    id,
                    JobStatus::TimedOut,
                    &format!("终止直接子进程失败: {error}；"),
                );
                return;
            }
            if let Err(error) = child.wait().await {
                registry.finish(
                    id,
                    JobStatus::TimedOut,
                    &format!("回收子进程失败: {error}；"),
                );
                return;
            }
            let scope = if sandbox_active {
                "已终止整个进程树"
            } else {
                "仅终止直接子进程，后代进程可能仍在运行"
            };
            (JobStatus::TimedOut, format!("超时；{scope}；"))
        }
    };
    registry.finish(id, note.0, &note.1);
}

/// `job_list` 参数为空对象，留作未来按状态过滤扩展；当前列出全部作业。
#[derive(Debug, Deserialize)]
struct JobListArgs {
    #[serde(default)]
    _unused: Option<Value>,
}

/// 只读作业列表工具。声明 `read_only=true` 由装配层接 ToolGuard/审批链。
pub struct JobListTool {
    registry: Arc<dyn BackgroundJobRegistry>,
}

impl JobListTool {
    pub fn new(registry: Arc<dyn BackgroundJobRegistry>) -> Self {
        Self { registry }
    }

    pub fn spec() -> ToolSpec {
        ToolSpec {
            name: JOB_LIST_TOOL.to_owned(),
            description: "列出本次运行中由 run_command(background=true) 启动的后台作业及状态。PTY 持久终端不属于当前切片。".to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {},
                "required": []
            }),
            annotations: ToolAnnotations::new(true, false, true, false),
        }
    }
}

impl ToolInvoker for JobListTool {
    fn invoke<'a>(&'a self, call: &'a ToolCall) -> BoxFuture<'a, ToolOutcome> {
        Box::pin(async move {
            if serde_json::from_value::<JobListArgs>(call.arguments.clone()).is_err() {
                return ToolOutcome::failed("job_list 参数必须是对象");
            }
            let records = match self.registry.list().await {
                Ok(records) => records,
                Err(error) => return ToolOutcome::failed(error),
            };
            ToolOutcome::success(
                serde_json::to_string_pretty(&records)
                    .unwrap_or_else(|error| format!("序列化作业列表失败: {error}")),
            )
        })
    }
}

/// 把 `job_list` 接入 `ToolRegistry::from_sources`。
pub struct JobListToolSource {
    registry: Arc<dyn BackgroundJobRegistry>,
}

impl JobListToolSource {
    pub fn new(registry: Arc<dyn BackgroundJobRegistry>) -> Self {
        Self { registry }
    }
}

impl ToolSource for JobListToolSource {
    fn name(&self) -> &str {
        "background_jobs"
    }

    fn tools(&self) -> Vec<ToolEntry> {
        vec![ToolEntry {
            spec: JobListTool::spec(),
            invoker: Arc::new(JobListTool::new(Arc::clone(&self.registry))),
        }]
    }
}

//! 本地常驻任务服务：一个进程持续接受命令，任务之间工作区隔离。
//!
//! 服务负责状态、事件、审批归属与执行生命周期；模型、工具、策略与扩展
//! 仍由 [`TaskFactory`] 每次执行重新装配，沿用既有 `Orchestrator` 链路。
//! 受理长期任务的回复先返回，终态经事件日志报告；重放事件不会再次调用
//! 模型、工具或审批闸门。

use std::collections::{BTreeMap, HashMap};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};

use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex as AsyncMutex;

use tokio::task::JoinHandle;

use crate::application::agent_run::AgentDependencies;
use crate::application::control::{ExecutionControl, UserInput};
use crate::application::error::ApplicationError;
use crate::application::isolation::{IsolationWorkspace, WorkspaceIsolation, WorkspaceVerifier};
use crate::application::orchestrator::{EvidenceFactory, Orchestrator};
use crate::application::port::{BoxFuture, CancelHandle, EvidenceSink};
use crate::application::service::{EventJournal, EventSubscription, TaskService};
use crate::application::service_factory::{TaskBuild, TaskFactory};
use crate::application::snapshot::{SessionStore, TaskSnapshot, TaskStore};
use crate::domain::budget::{Budget, Consumption};
use crate::domain::ids::{SessionId, TaskId};
use crate::domain::service::{
    EventContext, EventOrigin, ServiceCommand, ServiceError, ServiceEventPayload, ServiceReply,
    TaskStatus, TaskView,
};
use crate::domain::task::{Task, TaskKind, TaskState};
use crate::domain::workspace::WorkspaceRoot;
use crate::infrastructure::approval::async_gate::ApprovalBroker;
use crate::infrastructure::git_worktree::GitWorktreeManager;
use crate::infrastructure::service_events::{JsonEventJournal, ServiceEvidence};
use crate::infrastructure::session_store::JsonSessionStore;
use crate::infrastructure::task_store::JsonTaskStore;

/// 任务状态与执行归属的持久化记录。
#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct TaskRecord {
    view: TaskView,
    goal: String,
    kind: TaskKind,
    workspace: IsolationWorkspace,
    snapshot: Option<TaskSnapshot>,
    source_task: Option<String>,
    parent_task: Option<String>,
    verification: String,
    #[serde(default)]
    parent_session: Option<String>,
    #[serde(default)]
    inputs: Vec<QueuedInput>,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct QueuedInput {
    input: UserInput,
    applied: bool,
}

/// 请求的完整命令与直接回复一起保存，查询不占用请求标识。
#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct Receipt {
    command: ServiceCommand,
    result: Result<ServiceReply, ServiceError>,
}

// 状态文件格式版本定义在 domain，任务服务与诊断层共用同一份契约。
pub use crate::domain::service::SERVICE_STATE_FORMAT;

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct ServiceStateFile {
    #[serde(default)]
    schema_version: u32,
    #[serde(default = "one")]
    next_task: u64,
    #[serde(default = "one")]
    next_execution: u64,
    #[serde(default)]
    tasks: BTreeMap<String, TaskRecord>,
    #[serde(default)]
    receipts: BTreeMap<String, Receipt>,
}

fn one() -> u64 {
    1
}

impl Default for ServiceStateFile {
    fn default() -> Self {
        Self {
            schema_version: SERVICE_STATE_FORMAT,
            next_task: 1,
            next_execution: 1,
            tasks: BTreeMap::new(),
            receipts: BTreeMap::new(),
        }
    }
}

fn load_state(state_path: &Path) -> Result<ServiceStateFile, ServiceError> {
    let bytes = match std::fs::read(state_path) {
        Ok(bytes) => bytes,
        Err(cause) if cause.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ServiceStateFile::default());
        }
        Err(cause) => {
            return Err(corrupt_state(state_path, None, &format!("读取失败: {cause}")));
        }
    };
    let value: serde_json::Value = serde_json::from_slice(&bytes).map_err(|cause| {
        corrupt_state(state_path, Some(&bytes), &format!("JSON 解析失败: {cause}"))
    })?;
    // 先检查版本，未来格式的业务字段可能已不再兼容当前结构。
    if let Some(version) = value.get("schema_version").and_then(serde_json::Value::as_u64)
        && version > u64::from(SERVICE_STATE_FORMAT)
    {
        return Err(error(
            "service_state_version_mismatch",
            format!(
                "服务状态版本不兼容: 当前版本 {SERVICE_STATE_FORMAT}，文件版本 {version}；\
                 可用动作: 升级到支持文件版本的程序，或先备份原文件 {} 后恢复兼容状态；\
                 建议备份位置: {}",
                state_path.display(),
                state_path.with_extension("json.backup").display(),
            ),
        ));
    }
    let mut state: ServiceStateFile = serde_json::from_value(value).map_err(|cause| {
        corrupt_state(state_path, Some(&bytes), &format!("状态结构无效: {cause}"))
    })?;
    if state.schema_version == 0 {
        migrate_state(state_path, &bytes, &mut state)?;
    }
    Ok(state)
}

fn state_backup_path(state_path: &Path, kind: &str, extension: &str) -> PathBuf {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_nanos())
        .unwrap_or_default();
    state_path.with_extension(format!("json.{kind}-{timestamp}{extension}"))
}

fn write_state_backup(backup_path: &Path, mut source: impl std::io::Read) -> std::io::Result<()> {
    let mut backup = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(backup_path)?;
    std::io::copy(&mut source, &mut backup)?;
    backup.sync_all()
}

fn corrupt_state(state_path: &Path, bytes: Option<&[u8]>, cause: &str) -> ServiceError {
    let backup_path = state_backup_path(state_path, "corrupt", "");
    let backup = match bytes {
        Some(bytes) => write_state_backup(&backup_path, bytes),
        None => File::open(state_path).and_then(|file| write_state_backup(&backup_path, file)),
    };
    let backup_message = match backup {
        Ok(()) => format!("原数据备份: {}", backup_path.display()),
        Err(cause) => format!("备份失败，目标 {}: {cause}", backup_path.display()),
    };
    error(
        "service_state_corrupt",
        format!(
            "服务状态 {} {cause}；{backup_message}；\
             修复入口: 停止服务，修复文件读取权限或从有效备份恢复 {} 后重试",
            state_path.display(),
            state_path.display(),
        ),
    )
}

fn migrate_state(
    state_path: &Path,
    original: &[u8],
    state: &mut ServiceStateFile,
) -> Result<(), ServiceError> {
    let backup_path = state_backup_path(state_path, "migration", ".json");
    let log_path = state_path.with_file_name("migrations.log");
    let migration = (|| {
        write_state_backup(&backup_path, original).map_err(io_error)?;
        // 先记录已保留备份的迁移准备事件，日志不可写时不改动原状态。
        let mut entry = serde_json::to_vec(&serde_json::json!({
            "event": "service_state_migration_prepared",
            "from_version": state.schema_version,
            "to_version": SERVICE_STATE_FORMAT,
            "state_path": state_path,
            "backup_path": backup_path,
        }))
        .map_err(|cause| error("service_state_migration_failed", cause.to_string()))?;
        entry.push(b'\n');
        {
            let mut log = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&log_path)
                .map_err(io_error)?;
            log.write_all(&entry).map_err(io_error)?;
            log.sync_all().map_err(io_error)?;
        }
        state.schema_version = SERVICE_STATE_FORMAT;
        write_state(state_path, state)
    })();
    migration.map_err(|cause| {
        error(
            "service_state_migration_failed",
            format!(
                "服务状态 v0 -> v{SERVICE_STATE_FORMAT} 迁移失败: {cause}；\
                 原数据备份目标: {}；迁移日志: {}；修复后重新打开服务",
                backup_path.display(),
                log_path.display(),
            ),
        )
    })?;
    eprintln!(
        "服务状态已迁移: v0 -> v{SERVICE_STATE_FORMAT}，备份 {}",
        backup_path.display(),
    );
    Ok(())
}

/// 输入留在持久队列中，只有会话快照确认后才标记应用。
struct RunControl {
    inner: Weak<ServiceInner>,
    context: EventContext,
    evidence: Arc<ServiceEvidence>,
    stores: Arc<StoreHealth>,
}

impl ExecutionControl for RunControl {
    fn take_inputs(&self) -> Vec<UserInput> {
        let Some(inner) = self.inner.upgrade() else {
            return Vec::new();
        };
        match lock_state(&inner) {
            Ok(state) => state
                .tasks
                .get(&self.context.task_id)
                .map(|record| {
                    record
                        .inputs
                        .iter()
                        .filter(|queued| !queued.applied)
                        .map(|queued| queued.input.clone())
                        .collect()
                })
                .unwrap_or_default(),
            Err(error) => {
                self.stores.fail(error);
                Vec::new()
            }
        }
    }

    fn input_applied(&self, input: &UserInput) -> Result<(), String> {
        let inner = self
            .inner
            .upgrade()
            .ok_or_else(|| "服务已释放".to_owned())?;
        let session = inner
            .session_store
            .load(&SessionId::new(self.context.session_id.clone()))
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "输入的会话快照缺失".to_owned())?;
        if !session.session.turns.iter().any(|turn| {
            matches!(turn, crate::domain::session::Turn::Input { id, text } if id == &input.id && text == &input.text)
        }) {
            return Err("输入尚未持久化到会话快照".to_owned());
        }
        self.stores.health().map_err(|error| error.to_string())?;
        let mut state = lock_state(&inner).map_err(|error| error.to_string())?;
        let mut next = state.clone();
        let record = next
            .tasks
            .get_mut(&self.context.task_id)
            .ok_or_else(|| "输入所属任务缺失".to_owned())?;
        let queued = record
            .inputs
            .iter_mut()
            .find(|queued| queued.input.id == input.id)
            .ok_or_else(|| "持久输入队列缺失".to_owned())?;
        if queued.input.text != input.text {
            return Err("输入内容与持久队列不符".to_owned());
        }
        if queued.applied {
            return Ok(());
        }
        queued.applied = true;
        persist_state(&inner, &next).map_err(|error| error.to_string())?;
        *state = next;
        inner
            .journal
            .append(
                Some(&self.context),
                ServiceEventPayload::InputApplied {
                    message_id: input.id.clone(),
                },
            )
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    fn check(&self) -> Result<(), String> {
        let inner = self
            .inner
            .upgrade()
            .ok_or_else(|| "服务已释放".to_owned())?;
        // 审计、审批或检查点失效后停止后续动作，错误保留到执行收尾。
        self.evidence.health().map_err(|error| error.to_string())?;
        inner.broker.health().map_err(|error| error.to_string())?;
        self.stores.health().map_err(|error| error.to_string())
    }
}

struct LiveExecution {
    context: EventContext,
    cancel: CancelHandle,
    handle: JoinHandle<Result<(), ServiceError>>,
}

struct ServiceInner {
    state_path: PathBuf,
    /// 受理与关闭共用一把锁；模型运行不持锁，查询也不等待它。
    admission: AsyncMutex<()>,
    lease: Mutex<Option<File>>,
    state: Mutex<ServiceStateFile>,
    journal: Arc<dyn EventJournal>,
    /// 具体类型句柄：Shutdown 时调用 `close()` 释放租约，trait 对象上
    /// 没有这个方法。
    journal_impl: Arc<JsonEventJournal>,
    broker: Arc<ApprovalBroker>,
    factory: Arc<dyn TaskFactory>,
    budget: Budget,
    verifier: Arc<dyn WorkspaceVerifier>,
    isolation: Arc<GitWorktreeManager>,
    session_store: Arc<dyn SessionStore>,
    task_store: Arc<dyn TaskStore>,
    live: Mutex<HashMap<String, LiveExecution>>,
    closed: Mutex<bool>,
    failures: Mutex<Vec<ServiceError>>,
    shutdown_result: Mutex<Option<Result<ServiceReply, ServiceError>>>,
}

/// 进程内常驻服务。关闭会等待执行与外部资源收束后再释放租约；Drop 发出
/// 相同的取消请求，并在当前运行时可用时异步完成收尾。
pub struct LocalTaskService {
    inner: Arc<ServiceInner>,
}

fn error(code: &str, message: impl Into<String>) -> ServiceError {
    ServiceError::new(code, message)
}

fn broken(message: &str) -> ServiceError {
    error("service_unhealthy", message)
}

fn io_error(cause: std::io::Error) -> ServiceError {
    error("service_io", cause.to_string())
}

fn lock_state(
    self_: &ServiceInner,
) -> Result<std::sync::MutexGuard<'_, ServiceStateFile>, ServiceError> {
    // 借用与 Arc 无关，编译期生命周期只绑定 ServiceInner 实参。
    self_.state.lock().map_err(|_| broken("服务状态锁已损坏"))
}

fn lock_live(
    self_: &ServiceInner,
) -> Result<std::sync::MutexGuard<'_, HashMap<String, LiveExecution>>, ServiceError> {
    self_.live.lock().map_err(|_| broken("执行表锁已损坏"))
}

fn lease(path: &Path) -> Result<File, ServiceError> {
    let mut name = path.as_os_str().to_os_string();
    name.push(".lock");
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .append(true)
        .open(PathBuf::from(name))
        .map_err(io_error)?;
    file.try_lock().map_err(|err| {
        error(
            "service_locked",
            format!("无法独占服务资源 {}: {err}", path.display()),
        )
    })?;
    Ok(file)
}

fn stamp(prefix: &str, serial: u64) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_nanos())
        .unwrap_or_default();
    format!("{prefix}-{}-{nanos}-{serial}", std::process::id())
}

/// 一次执行从哪里接手。
#[derive(Clone)]
enum ExecutionStart {
    Create(Task),
    Resume(TaskSnapshot),
    Conversation(TaskSnapshot),
}

/// 新任务视图的一组标识与来源信息。用于避免构造函数出现过长的
/// 同类型参数表，字段含义在调用处仍一目了然。
struct NewTaskIdentifiers {
    task_id: String,
    session_id: String,
    execution_id: String,
    source: Option<String>,
    parent: Option<String>,
}

impl LocalTaskService {
    /// 在真实 Git 工作区根上打开服务，恢复既有任务与命令凭据。
    ///
    /// 旧版状态保留原字节备份和迁移准备记录后升级；未来版本保留原文件并拒绝打开。
    ///
    /// # Errors
    ///
    /// 父仓库、事件日志或持久化状态不可用，状态版本不兼容、迁移失败，
    /// 或已有实例持有租约时返回错误；损坏状态在返回前尝试保留原字节备份。
    pub async fn open(
        root: PathBuf,
        factory: Arc<dyn TaskFactory>,
        budget: Budget,
        verifier: Arc<dyn WorkspaceVerifier>,
    ) -> Result<Self, ServiceError> {
        let state_dir = root.join(".qcodeharness").join("service");
        std::fs::create_dir_all(&state_dir).map_err(io_error)?;
        let lease = lease(&state_dir.join("service"))?;
        let state_path = state_dir.join("state.json");
        let mut state = load_state(&state_path)?;
        let journal_impl = Arc::new(JsonEventJournal::open(state_dir.join("events.jsonl"))?);
        let journal: Arc<dyn EventJournal> = journal_impl.clone();
        let broker = Arc::new(ApprovalBroker::new(journal.clone()));
        let isolation = Arc::new(
            GitWorktreeManager::new(WorkspaceRoot::new(&root))
                .await
                .map_err(|err| error("workspace_unavailable", err))?,
        );
        let session_store: Arc<dyn SessionStore> = Arc::new(JsonSessionStore::new(&root));
        let task_store: Arc<dyn TaskStore> = Arc::new(JsonTaskStore::new(&root));
        // 已清理的工作区不再接管；其余按签发记录校验路径、基线与仓库归属。
        for record in state.tasks.values_mut() {
            if record.view.status == TaskStatus::Cleaned {
                continue;
            }
            isolation
                .adopt(&record.workspace)
                .await
                .map_err(|err| error("workspace_adopt_failed", err))?;
            // 运行期间任务存储持续更新，state.json 中的副本可能落后多个步骤。
            record.snapshot = load_checkpoint(task_store.as_ref(), session_store.as_ref(), record)?;
            if let Some(snapshot) = &record.snapshot {
                record.view.consumption = snapshot.run.consumption;
            }
            if matches!(
                record.view.status,
                TaskStatus::Running | TaskStatus::Stopping
            ) {
                record.view.status = TaskStatus::Interrupted;
                record.view.error = Some("上次执行未正常结束，等待显式继续".to_owned());
            }
        }
        let service = Self {
            inner: Arc::new(ServiceInner {
                state_path,
                admission: AsyncMutex::new(()),
                lease: Mutex::new(Some(lease)),
                state: Mutex::new(state),
                journal,
                journal_impl,
                broker,
                factory,
                budget,
                verifier,
                isolation,
                session_store,
                task_store,
                live: Mutex::new(HashMap::new()),
                closed: Mutex::new(false),
                failures: Mutex::new(Vec::new()),
                shutdown_result: Mutex::new(None),
            }),
        };
        {
            let state = lock_state(&service.inner)?;
            service.persist(&state)?;
        }
        Ok(service)
    }

    fn persist(&self, state: &ServiceStateFile) -> Result<(), ServiceError> {
        persist_state(&self.inner, state)
    }

    fn append(
        &self,
        context: &EventContext,
        payload: ServiceEventPayload,
    ) -> Result<(), ServiceError> {
        self.inner
            .journal
            .append(Some(context), payload)
            .map(|_| ())
    }

    fn view(&self, task_id: &str) -> Result<TaskView, ServiceError> {
        let state = lock_state(&self.inner)?;
        state
            .tasks
            .get(task_id)
            .map(|record| record.view.clone())
            .ok_or_else(|| error("task_not_found", format!("任务不存在: {task_id}")))
    }

    fn next_ids(&self) -> Result<(String, String, String), ServiceError> {
        let mut state = lock_state(&self.inner)?;
        let serial = state.next_task;
        state.next_task = serial
            .checked_add(1)
            .ok_or_else(|| error("id_exhausted", "任务标识已耗尽"))?;
        let base = stamp("task", serial);
        Ok((
            base.clone(),
            format!("session-{base}"),
            format!("execution-{base}"),
        ))
    }

    fn context_of(record: &TaskRecord) -> EventContext {
        EventContext {
            task_id: record.view.task_id.clone(),
            session_id: record.view.session_id.clone(),
            execution_id: record.view.execution_id.clone(),
            origin: EventOrigin {
                parent_session_id: record.parent_session.clone(),
                extension_id: None,
            },
        }
    }

    fn new_view(
        &self,
        ids: NewTaskIdentifiers,
        workspace: &IsolationWorkspace,
        verification: String,
    ) -> TaskView {
        TaskView {
            task_id: ids.task_id,
            session_id: ids.session_id,
            execution_id: ids.execution_id,
            attempt: 1,
            status: TaskStatus::Running,
            workspace: workspace.root.clone(),
            base_revision: workspace.base_revision.clone(),
            source_task: ids.source,
            parent_task: ids.parent,
            answer: None,
            error: None,
            verification,
            consumption: Consumption::default(),
        }
    }

    async fn dispatch(
        &self,
        request_id: &str,
        command: ServiceCommand,
    ) -> Result<ServiceReply, ServiceError> {
        if matches!(
            command,
            ServiceCommand::List
                | ServiceCommand::Get { .. }
                | ServiceCommand::Approvals { .. }
                | ServiceCommand::Events { .. }
        ) {
            return match command {
                ServiceCommand::List => Ok(ServiceReply::Tasks {
                    tasks: lock_state(&self.inner)?
                        .tasks
                        .values()
                        .map(|record| record.view.clone())
                        .collect(),
                }),
                ServiceCommand::Get { task_id } => Ok(ServiceReply::Task {
                    task: Box::new(self.view(&task_id)?),
                }),
                ServiceCommand::Approvals { task_id } => Ok(ServiceReply::PendingApprovals {
                    requests: self.inner.broker.pending(task_id.as_deref()),
                }),
                ServiceCommand::Events { after, limit } => {
                    let events = self.inner.journal.read(after, limit)?;
                    let next = events.last().map_or(after, |event| event.sequence);
                    Ok(ServiceReply::Events { events, next })
                }
                _ => unreachable!("上面已限定查询命令"),
            };
        }
        let _admission = self.inner.admission.lock().await;
        {
            let state = lock_state(&self.inner)?;
            if let Some(receipt) = state.receipts.get(request_id) {
                return if receipt.command == command {
                    receipt.result.clone()
                } else {
                    Err(error(
                        "request_conflict",
                        format!("请求 {request_id} 已用于其他命令或参数"),
                    ))
                };
            }
        }
        if *self
            .inner
            .closed
            .lock()
            .map_err(|_| broken("服务状态锁已损坏"))?
        {
            if command == ServiceCommand::Shutdown {
                return self
                    .inner
                    .shutdown_result
                    .lock()
                    .map_err(|_| broken("关闭结果锁已损坏"))?
                    .clone()
                    .unwrap_or_else(|| Err(error("service_closing", "服务正在关闭")));
            }
            return Err(error("service_closed", "服务已关闭"));
        }
        let shutdown = command == ServiceCommand::Shutdown;
        let result = self.mutate(request_id, command.clone()).await;
        let saved = {
            let mut state = lock_state(&self.inner)?;
            if state
                .receipts
                .get(request_id)
                .is_some_and(|receipt| receipt.command == command && receipt.result == result)
            {
                Ok(())
            } else {
                let mut next = state.clone();
                next.receipts.insert(
                    request_id.to_owned(),
                    Receipt {
                        command,
                        result: result.clone(),
                    },
                );
                self.persist(&next).map(|()| {
                    *state = next;
                })
            }
        };
        if shutdown {
            let result = combine_result(result, saved);
            let result = combine_result(result, release_lease(&self.inner));
            *self
                .inner
                .shutdown_result
                .lock()
                .map_err(|_| broken("关闭结果锁已损坏"))? = Some(result.clone());
            result
        } else {
            combine_result(result, saved)
        }
    }

    async fn mutate(
        &self,
        request_id: &str,
        command: ServiceCommand,
    ) -> Result<ServiceReply, ServiceError> {
        match command {
            ServiceCommand::Create { goal, kind } => self.create(request_id, goal, kind).await,
            ServiceCommand::Input {
                task_id,
                message_id,
                text,
            } => {
                self.input(&task_id, message_id, text)?;
                Ok(ServiceReply::Accepted {
                    task_id,
                    execution_id: None,
                })
            }
            ServiceCommand::Decide {
                approval_id,
                task_id,
                execution_id,
                allow,
            } => {
                self.inner
                    .broker
                    .decide(&approval_id, &task_id, &execution_id, allow)?;
                Ok(ServiceReply::Accepted {
                    task_id,
                    execution_id: Some(execution_id),
                })
            }
            ServiceCommand::Cancel { task_id } => {
                self.cancel(&task_id)?;
                Ok(ServiceReply::Accepted {
                    task_id,
                    execution_id: None,
                })
            }
            ServiceCommand::Continue { task_id } => self.continue_task(request_id, &task_id).await,
            ServiceCommand::Fork { task_id, goal } => self.fork(request_id, &task_id, goal).await,
            ServiceCommand::Merge { task_id } => {
                self.merge(&task_id).await?;
                Ok(ServiceReply::Accepted {
                    task_id,
                    execution_id: None,
                })
            }
            ServiceCommand::Cleanup { task_id } => {
                self.cleanup(&task_id).await?;
                Ok(ServiceReply::Accepted {
                    task_id,
                    execution_id: None,
                })
            }
            ServiceCommand::Shutdown => shutdown_executions(&self.inner)
                .await
                .map(|()| ServiceReply::Shutdown),
            _ => unreachable!("查询命令已提前返回"),
        }
    }

    async fn create(
        &self,
        request_id: &str,
        goal: String,
        kind: TaskKind,
    ) -> Result<ServiceReply, ServiceError> {
        let (task_id, session_id, execution_id) = self.next_ids()?;
        let task = self.make_task(&task_id, kind, &goal)?;
        let base = self
            .inner
            .isolation
            .prepare()
            .await
            .map_err(|err| error("workspace_unavailable", err))?;
        let workspace = self
            .inner
            .isolation
            .create(&task_id, &base)
            .await
            .map_err(|err| error("workspace_unavailable", err))?;
        let verification = match self.inner.isolation.capture_digest(&workspace).await {
            Ok(digest) => digest,
            Err(cause) => {
                return Err(self
                    .discard_unadmitted_workspace(
                        &workspace,
                        error("workspace_capture_failed", cause),
                    )
                    .await);
            }
        };
        let view = self.new_view(
            NewTaskIdentifiers {
                task_id: task_id.clone(),
                session_id,
                execution_id,
                source: None,
                parent: None,
            },
            &workspace,
            verification.clone(),
        );
        let record = TaskRecord {
            view: view.clone(),
            goal: goal.clone(),
            kind,
            workspace,
            snapshot: None,
            source_task: None,
            parent_task: None,
            verification,
            parent_session: None,
            inputs: Vec::new(),
        };
        let context = Self::context_of(&record);
        let saved = (|| {
            let mut state = lock_state(&self.inner)?;
            let mut next = state.clone();
            next.tasks.insert(task_id, record.clone());
            next.receipts.insert(
                request_id.to_owned(),
                Receipt {
                    command: ServiceCommand::Create { goal, kind },
                    result: Ok(ServiceReply::Task { task: Box::new(view.clone()) }),
                },
            );
            self.persist(&next)?;
            *state = next;
            Ok(())
        })();
        if let Err(cause) = saved {
            return Err(self
                .discard_unadmitted_workspace(&record.workspace, cause)
                .await);
        }
        // 新建任务也要发 ExecutionStarted：任务创建与执行启动是两件事，
        // 但执行区间（供并发/重叠/回放判定）必须从这里算起。只发
        // TaskCreated 会让"任务刚创建、还没跑"的状态在事件流里没有起点，
        // 订阅方按 execution_started 取区间时只能拿到 None。
        self.spawn(
            record,
            context,
            ExecutionStart::Create(task),
            [
                ServiceEventPayload::TaskCreated { task: view.clone() },
                ServiceEventPayload::ExecutionStarted,
            ],
        )?;
        Ok(ServiceReply::Task { task: Box::new(view) })
    }

    fn make_task(&self, task_id: &str, kind: TaskKind, goal: &str) -> Result<Task, ServiceError> {
        // 用内置模板展开任务，装配前的步骤校验与 CLI 路径一致。
        let catalog = crate::domain::workflow::WorkflowCatalog::builtin();
        let steps = catalog
            .find(kind)
            .map(|template| template.expand(goal))
            .unwrap_or_default();
        if steps.is_empty() {
            return Err(error(
                "template_missing",
                format!("任务类型 {kind:?} 没有可用步骤模板"),
            ));
        }
        Ok(Task::new(TaskId::new(task_id), kind, goal, steps))
    }

    fn input(&self, task_id: &str, message_id: String, text: String) -> Result<(), ServiceError> {
        let mut state = lock_state(&self.inner)?;
        let mut next = state.clone();
        let record = next
            .tasks
            .get_mut(task_id)
            .ok_or_else(|| error("task_not_found", format!("任务不存在: {task_id}")))?;
        if matches!(record.view.status, TaskStatus::Merged | TaskStatus::Cleaned) {
            return Err(error(
                "task_closed",
                format!("任务 {task_id} 的工作区已收束"),
            ));
        }
        if let Some(queued) = record
            .inputs
            .iter()
            .find(|queued| queued.input.id == message_id)
        {
            return if queued.input.text == text {
                Ok(())
            } else {
                Err(error(
                    "input_conflict",
                    format!("输入 {message_id} 已用于其他内容"),
                ))
            };
        }
        record.inputs.push(QueuedInput {
            input: UserInput {
                id: message_id.clone(),
                text: text.clone(),
            },
            applied: false,
        });
        let context = Self::context_of(record);
        self.persist(&next)?;
        *state = next;
        self.append(
            &context,
            ServiceEventPayload::InputReceived { message_id, text },
        )
    }

    fn cancel(&self, task_id: &str) -> Result<(), ServiceError> {
        // 与终态提交共用锁顺序，防止取消把已经完成的记录改回 Stopping。
        let mut state = lock_state(&self.inner)?;
        let live = lock_live(&self.inner)?;
        let execution = live.get(task_id).ok_or_else(|| {
            error(
                "task_not_running",
                format!("任务 {task_id} 没有进行中的执行"),
            )
        })?;
        execution.cancel.cancel();
        self.inner
            .broker
            .cancel_execution(&execution.context.execution_id, "用户取消");
        let record = state
            .tasks
            .get_mut(task_id)
            .ok_or_else(|| broken("运行中的任务记录缺失"))?;
        record.view.status = TaskStatus::Stopping;
        self.persist(&state)?;
        self.append(&execution.context, ServiceEventPayload::CancelRequested)
    }

    async fn continue_task(
        &self,
        request_id: &str,
        task_id: &str,
    ) -> Result<ServiceReply, ServiceError> {
        let mut record = self.idle_record(task_id)?;
        let snapshot = load_checkpoint(
            self.inner.task_store.as_ref(),
            self.inner.session_store.as_ref(),
            &record,
        )?
        .ok_or_else(|| {
            error(
                "checkpoint_missing",
                format!("任务 {task_id} 没有可恢复的检查点"),
            )
        })?;
        self.check_workspace(&record).await?;
        let start = execution_start(snapshot.clone());
        {
            let mut state = lock_state(&self.inner)?;
            let mut next = state.clone();
            let serial = next.next_execution;
            next.next_execution = serial
                .checked_add(1)
                .ok_or_else(|| error("id_exhausted", "执行标识已耗尽"))?;
            record.view.execution_id = stamp("execution", serial);
            record.view.attempt = record
                .view
                .attempt
                .checked_add(1)
                .ok_or_else(|| error("id_exhausted", "执行次数已耗尽"))?;
            record.view.status = TaskStatus::Running;
            record.view.error = None;
            record.view.answer = None;
            record.snapshot = Some(snapshot);
            next.tasks.insert(task_id.to_owned(), record.clone());
            next.receipts.insert(
                request_id.to_owned(),
                Receipt {
                    command: ServiceCommand::Continue {
                        task_id: task_id.to_owned(),
                    },
                    result: Ok(ServiceReply::Task {
                        task: Box::new(record.view.clone()),
                    }),
                },
            );
            self.persist(&next)?;
            *state = next;
        }
        let context = Self::context_of(&record);
        self.spawn(
            record.clone(),
            context,
            start,
            [ServiceEventPayload::ExecutionStarted],
        )?;
        Ok(ServiceReply::Task { task: Box::new(record.view) })
    }

    fn idle_record(&self, task_id: &str) -> Result<TaskRecord, ServiceError> {
        let state = lock_state(&self.inner)?;
        if lock_live(&self.inner)?.contains_key(task_id) {
            return Err(error("task_running", format!("任务 {task_id} 正在执行")));
        }
        let record = state
            .tasks
            .get(task_id)
            .cloned()
            .ok_or_else(|| error("task_not_found", format!("任务不存在: {task_id}")))?;
        if matches!(record.view.status, TaskStatus::Cleaned | TaskStatus::Merged) {
            return Err(error(
                "task_closed",
                format!("任务 {task_id} 的工作区已收束"),
            ));
        }
        Ok(record)
    }

    async fn check_workspace(&self, record: &TaskRecord) -> Result<(), ServiceError> {
        if record.verification.is_empty() {
            return Err(error(
                "workspace_unverified",
                "没有可信的工作区摘要，拒绝继续",
            ));
        }
        let digest = self
            .inner
            .isolation
            .capture_digest(&record.workspace)
            .await
            .map_err(|err| error("workspace_capture_failed", err))?;
        if digest != record.verification {
            return Err(error(
                "workspace_changed",
                "工作区自上次检查点以来已变化，拒绝继续",
            ));
        }
        Ok(())
    }

    async fn fork(
        &self,
        request_id: &str,
        task_id: &str,
        goal: Option<String>,
    ) -> Result<ServiceReply, ServiceError> {
        let source = self.idle_record(task_id)?;
        let snapshot = load_checkpoint(
            self.inner.task_store.as_ref(),
            self.inner.session_store.as_ref(),
            &source,
        )?
        .ok_or_else(|| {
            error(
                "checkpoint_missing",
                format!("任务 {task_id} 没有可分支的检查点"),
            )
        })?;
        self.check_workspace(&source).await?;
        let (new_task_id, session_id, execution_id) = self.next_ids()?;
        let workspace = self
            .inner
            .isolation
            .fork_workspace(&source.workspace, &new_task_id)
            .await
            .map_err(|err| error("workspace_fork_failed", err))?;
        let mut snapshot = snapshot.fork_with_ids(
            TaskId::new(new_task_id.clone()),
            SessionId::new(session_id.clone()),
        );
        let saved = (|| {
            snapshot.worktree = crate::infrastructure::worktree::scan_workspace(&workspace.root)
                .map_err(io_error)?;
            self.inner
                .session_store
                .save(&snapshot.run)
                .map_err(checkpoint_error)?;
            self.inner
                .task_store
                .save(&snapshot)
                .map_err(checkpoint_error)
        })();
        if let Err(cause) = saved {
            return Err(self.discard_unadmitted_workspace(&workspace, cause).await);
        }
        let mut view = self.new_view(
            NewTaskIdentifiers {
                task_id: new_task_id.clone(),
                session_id,
                execution_id,
                source: Some(task_id.to_owned()),
                parent: Some(task_id.to_owned()),
            },
            &workspace,
            source.verification.clone(),
        );
        view.consumption = snapshot.run.consumption;
        let mut inputs = source.inputs.clone();
        if let Some(text) = &goal {
            inputs.push(QueuedInput {
                input: UserInput {
                    id: format!("fork-goal-{new_task_id}"),
                    text: text.clone(),
                },
                applied: false,
            });
        }
        let record = TaskRecord {
            view: view.clone(),
            goal: goal.clone().unwrap_or_else(|| source.goal.clone()),
            kind: source.kind,
            workspace,
            snapshot: Some(snapshot.clone()),
            source_task: Some(task_id.to_owned()),
            parent_task: Some(task_id.to_owned()),
            verification: source.verification.clone(),
            parent_session: Some(source.view.session_id.clone()),
            inputs,
        };
        let context = Self::context_of(&record);
        let saved = (|| {
            let mut state = lock_state(&self.inner)?;
            let mut next = state.clone();
            next.tasks.insert(new_task_id, record.clone());
            next.receipts.insert(
                request_id.to_owned(),
                Receipt {
                    command: ServiceCommand::Fork {
                        task_id: task_id.to_owned(),
                        goal,
                    },
                    result: Ok(ServiceReply::Task { task: Box::new(view.clone()) }),
                },
            );
            self.persist(&next)?;
            *state = next;
            Ok(())
        })();
        if let Err(cause) = saved {
            return Err(self
                .discard_unadmitted_workspace(&record.workspace, cause)
                .await);
        }
        // 分叉同样由 spawn 启动一次执行，补充启动事件后订阅方才能从
        // execution_started 算出这次执行的区间；TaskCreated 仍是首条事件。
        self.spawn(
            record,
            context,
            execution_start(snapshot),
            [
                ServiceEventPayload::TaskCreated { task: view.clone() },
                ServiceEventPayload::ExecutionStarted,
                ServiceEventPayload::Forked {
                    source_task: task_id.to_owned(),
                },
            ],
        )?;
        Ok(ServiceReply::Task { task: Box::new(view) })
    }

    async fn merge(&self, task_id: &str) -> Result<(), ServiceError> {
        if lock_live(&self.inner)?.contains_key(task_id) {
            return Err(error("task_running", format!("任务 {task_id} 正在执行")));
        }
        let record = {
            let state = lock_state(&self.inner)?;
            let record = state
                .tasks
                .get(task_id)
                .cloned()
                .ok_or_else(|| error("task_not_found", format!("任务不存在: {task_id}")))?;
            if matches!(record.view.status, TaskStatus::Cleaned | TaskStatus::Merged) {
                return Err(error(
                    "task_closed",
                    format!("任务 {task_id} 的工作区已收束"),
                ));
            }
            record
        };
        let change = self
            .inner
            .isolation
            .capture(&record.workspace)
            .await
            .map_err(|err| error("workspace_capture_failed", err))?;
        self.inner
            .isolation
            .merge(
                &record.view.base_revision,
                &[change],
                self.inner.verifier.as_ref(),
            )
            .await
            .map_err(|err| error("workspace_merge_failed", err))?;
        let context = Self::context_of(&record);
        {
            let mut state = lock_state(&self.inner)?;
            if let Some(record) = state.tasks.get_mut(task_id) {
                record.view.status = TaskStatus::Merged;
                self.persist(&state)?;
            }
        }
        self.append(
            &context,
            ServiceEventPayload::WorkspaceMerged {
                verification: record.verification.clone(),
            },
        )
    }

    async fn cleanup(&self, task_id: &str) -> Result<(), ServiceError> {
        if lock_live(&self.inner)?.contains_key(task_id) {
            return Err(error("task_running", format!("任务 {task_id} 正在执行")));
        }
        let record = {
            let state = lock_state(&self.inner)?;
            state
                .tasks
                .get(task_id)
                .cloned()
                .ok_or_else(|| error("task_not_found", format!("任务不存在: {task_id}")))?
        };
        self.inner
            .isolation
            .remove(&record.workspace)
            .await
            .map_err(|err| error("workspace_cleanup_failed", err))?;
        let context = Self::context_of(&record);
        {
            let mut state = lock_state(&self.inner)?;
            if let Some(record) = state.tasks.get_mut(task_id) {
                record.view.status = TaskStatus::Cleaned;
                self.persist(&state)?;
            }
        }
        self.append(&context, ServiceEventPayload::WorkspaceRemoved)
    }

    async fn discard_unadmitted_workspace(
        &self,
        workspace: &IsolationWorkspace,
        mut cause: ServiceError,
    ) -> ServiceError {
        if let Err(cleanup) = self.inner.isolation.remove(workspace).await {
            cause.message.push_str(&format!(
                "；未受理工作区清理失败 {}: {cleanup}",
                workspace.root.display()
            ));
        }
        cause
    }

    /// 登记期间持有执行表锁，避免快速结束的执行先移除、后被插回表中。
    fn spawn(
        &self,
        record: TaskRecord,
        context: EventContext,
        start: ExecutionStart,
        events: impl IntoIterator<Item = ServiceEventPayload>,
    ) -> Result<(), ServiceError> {
        if let Err(cause) = self
            .inner
            .journal
            .append_batch(Some(&context), events.into_iter().collect())
        {
            self.inner
                .failures
                .lock()
                .map_err(|_| broken("服务错误表锁已损坏"))?
                .push(cause.clone());
            let stopped = commit_completion(
                &self.inner,
                &record,
                &context,
                Err((TaskStatus::Failed, cause.to_string())),
                record.snapshot.clone(),
                Some(record.verification.clone()),
            );
            return combine_result(Err(cause), stopped);
        }
        let inner = self.inner.clone();
        let cancel = CancelHandle::default();
        let evidence = Arc::new(ServiceEvidence::new(
            inner.journal.clone(),
            context.clone(),
            cancel.clone(),
        ));
        let control = Arc::new(RunControl {
            inner: Arc::downgrade(&inner),
            context: context.clone(),
            evidence: evidence.clone(),
            stores: Arc::new(StoreHealth {
                failure: Mutex::new(None),
                cancel: cancel.clone(),
            }),
        });
        let task_id = record.view.task_id.clone();
        let mut live = lock_live(&inner)?;
        let handle = tokio::spawn(run_execution(
            inner.clone(),
            record,
            context.clone(),
            start,
            cancel.clone(),
            evidence,
            control,
        ));
        live.insert(
            task_id,
            LiveExecution {
                context,
                cancel,
                handle,
            },
        );
        Ok(())
    }
}

impl TaskService for LocalTaskService {
    fn call<'a>(
        &'a self,
        request_id: &'a str,
        command: ServiceCommand,
    ) -> BoxFuture<'a, Result<ServiceReply, ServiceError>> {
        Box::pin(async move { self.dispatch(request_id, command).await })
    }

    fn subscribe(&self, after: u64) -> Result<Box<dyn EventSubscription>, ServiceError> {
        self.inner.journal.subscribe(after)
    }
}

/// runner 的快照接口允许降级；服务仍须把失败带到终态与关闭回复。
struct StoreHealth {
    failure: Mutex<Option<ServiceError>>,
    cancel: CancelHandle,
}

impl StoreHealth {
    fn fail(&self, error: ServiceError) {
        self.failure
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get_or_insert(error);
        self.cancel.cancel();
    }

    fn health(&self) -> Result<(), ServiceError> {
        self.failure
            .lock()
            .map_err(|_| broken("快照健康状态锁已损坏"))?
            .clone()
            .map_or(Ok(()), Err)
    }
}

struct CheckedStores {
    sessions: Arc<dyn SessionStore>,
    tasks: Arc<dyn TaskStore>,
    health: Arc<StoreHealth>,
}

impl CheckedStores {
    fn checked<T>(
        &self,
        result: Result<T, crate::application::snapshot::StoreError>,
    ) -> Result<T, crate::application::snapshot::StoreError> {
        if let Err(error) = &result {
            self.health.fail(checkpoint_error(error));
        }
        result
    }
}

impl SessionStore for CheckedStores {
    fn save(
        &self,
        snapshot: &crate::application::snapshot::RunSnapshot,
    ) -> Result<(), crate::application::snapshot::StoreError> {
        self.checked(self.sessions.save(snapshot))
    }

    fn load(
        &self,
        session: &SessionId,
    ) -> Result<
        Option<crate::application::snapshot::RunSnapshot>,
        crate::application::snapshot::StoreError,
    > {
        self.checked(self.sessions.load(session))
    }

    fn list(&self) -> Result<Vec<SessionId>, crate::application::snapshot::StoreError> {
        self.checked(self.sessions.list())
    }
}

impl TaskStore for CheckedStores {
    fn save(
        &self,
        snapshot: &TaskSnapshot,
    ) -> Result<(), crate::application::snapshot::StoreError> {
        self.checked(self.tasks.save(snapshot))
    }

    fn load(
        &self,
        task: &TaskId,
    ) -> Result<Option<TaskSnapshot>, crate::application::snapshot::StoreError> {
        self.checked(self.tasks.load(task))
    }

    fn list(&self) -> Result<Vec<TaskId>, crate::application::snapshot::StoreError> {
        self.checked(self.tasks.list())
    }
}

fn checkpoint_error(cause: impl std::fmt::Display) -> ServiceError {
    error("checkpoint_failed", cause.to_string())
}

fn load_checkpoint(
    tasks: &dyn TaskStore,
    sessions: &dyn SessionStore,
    record: &TaskRecord,
) -> Result<Option<TaskSnapshot>, ServiceError> {
    let Some(snapshot) = tasks
        .load(&TaskId::new(record.view.task_id.clone()))
        .map_err(checkpoint_error)?
    else {
        return Ok(None);
    };
    let session = sessions
        .load(&SessionId::new(record.view.session_id.clone()))
        .map_err(checkpoint_error)?;
    let mut snapshot =
        crate::application::task_resume::merge_session_state(snapshot, session.clone());
    // Runner 把已应用的补充输入留在会话快照里，任务快照停在步骤边界。
    // 会话历史是任务快照的前缀扩展时，将这份较新的运行状态带进任务快照。
    if let Some(session) = session
        && session.session_id == snapshot.run.session_id
            && session.session.turns.len() >= snapshot.run.session.turns.len()
            && session
                .session
                .turns
                .starts_with(&snapshot.run.session.turns)
            && session.session.summary == snapshot.run.session.summary
        {
            snapshot.run = session;
        }
    crate::application::task_resume::check_resumable(
        &snapshot,
        Some(&SessionId::new(record.view.session_id.clone())),
    )
    .map_err(checkpoint_error)?;
    Ok(Some(snapshot))
}

fn execution_start(snapshot: TaskSnapshot) -> ExecutionStart {
    if snapshot.task.state() == TaskState::Completed {
        ExecutionStart::Conversation(snapshot)
    } else {
        ExecutionStart::Resume(snapshot)
    }
}

/// 把 `ApplicationError` 的根因映射到可观察的任务状态。
fn status_for(error: &ApplicationError) -> TaskStatus {
    match error.root_cause() {
        ApplicationError::Cancelled => TaskStatus::Cancelled,
        ApplicationError::BudgetExhausted { .. } | ApplicationError::WallClockExhausted { .. } => {
            TaskStatus::BudgetExhausted
        }
        _ => TaskStatus::Failed,
    }
}

/// 驱动一次执行；模型或扩展 panic 后仍须关闭资源并发布失败终态。
async fn run_execution(
    inner: Arc<ServiceInner>,
    record: TaskRecord,
    context: EventContext,
    start: ExecutionStart,
    cancel: CancelHandle,
    evidence: Arc<ServiceEvidence>,
    control: Arc<RunControl>,
) -> Result<(), ServiceError> {
    let execution_id = context.execution_id.clone();
    let driver = {
        let inner = inner.clone();
        let record = record.clone();
        let context = context.clone();
        let control = control.clone();
        let evidence = evidence.clone();
        tokio::spawn(async move {
            drive(
                &inner, &record, &context, &start, &evidence, &control, &cancel,
            )
            .await
        })
    };
    let mut failure = None;
    let mut outcome = match driver.await {
        Ok(outcome) => outcome,
        Err(cause) => {
            let error = error("execution_panicked", cause.to_string());
            add_failure(&mut failure, error.clone());
            Err((TaskStatus::Failed, error.to_string()))
        }
    };
    inner.broker.cancel_execution(&execution_id, "执行已结束");
    for result in [
        evidence.health(),
        inner.broker.health(),
        control.stores.health(),
    ] {
        if let Err(error) = result {
            add_failure(&mut failure, error);
        }
    }
    // 工作区摘要与资源关闭都完成后，终态才允许对外可见。
    let verification = match inner.isolation.capture_digest(&record.workspace).await {
        Ok(digest) => Some(digest),
        Err(cause) => {
            add_failure(&mut failure, error("workspace_capture_failed", cause));
            None
        }
    };
    let checkpoint = match load_checkpoint(
        inner.task_store.as_ref(),
        inner.session_store.as_ref(),
        &record,
    ) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            add_failure(&mut failure, error);
            None
        }
    };
    run_execution_close(&inner, &execution_id, &mut failure).await;
    if let Some(error) = &failure {
        let message = match &outcome {
            Ok(_) => error.to_string(),
            Err((_, message)) => format!("{message}；{error}"),
        };
        outcome = Err((TaskStatus::Failed, message));
    }
    if let Some(error) = &failure {
        inner
            .failures
            .lock()
            .map_err(|_| broken("服务错误表锁已损坏"))?
            .push(error.clone());
    }
    let committed = commit_completion(&inner, &record, &context, outcome, checkpoint, verification);
    combine_result(failure.map_or(Ok(()), Err), committed)
}

fn commit_completion(
    inner: &ServiceInner,
    record: &TaskRecord,
    context: &EventContext,
    outcome: Result<String, (TaskStatus, String)>,
    checkpoint: Option<TaskSnapshot>,
    verification: Option<String>,
) -> Result<(), ServiceError> {
    let mut state = lock_state(inner)?;
    // 顺序固定为状态锁再执行表锁；读到终态时，该执行已不再占用任务。
    let mut live = lock_live(inner)?;
    let current = state
        .tasks
        .get_mut(&record.view.task_id)
        .ok_or_else(|| broken("执行收尾时任务记录缺失"))?;
    match outcome {
        Ok(answer) => {
            current.view.status = TaskStatus::Completed;
            current.view.answer = Some(answer);
            current.view.error = None;
        }
        Err((status, message)) => {
            current.view.status = status;
            current.view.answer = None;
            current.view.error = Some(message);
        }
    }
    if let Some(snapshot) = checkpoint {
        current.view.consumption = snapshot.run.consumption;
        current.snapshot = Some(snapshot);
    }
    current.verification = verification.unwrap_or_default();
    current.view.verification = current.verification.clone();
    live.remove(&record.view.task_id);
    let mut failure = None;
    if let Err(error) = persist_state(inner, &state) {
        let current = state
            .tasks
            .get_mut(&record.view.task_id)
            .expect("刚才已校验任务存在");
        current.view.status = TaskStatus::Failed;
        current.view.answer = None;
        current.view.error = Some(format!(
            "{}；{error}",
            current.view.error.as_deref().unwrap_or("终态持久化失败")
        ));
        add_failure(&mut failure, error);
    }
    let current = &state.tasks[&record.view.task_id];
    let payload = if current.view.status == TaskStatus::Completed {
        ServiceEventPayload::Completed {
            answer: current.view.answer.clone().unwrap_or_default(),
        }
    } else {
        ServiceEventPayload::Stopped {
            status: current.view.status,
            error: current.view.error.clone(),
        }
    };
    if let Err(error) = inner.journal.append(Some(context), payload) {
        let current = state
            .tasks
            .get_mut(&record.view.task_id)
            .expect("刚才已校验任务存在");
        current.view.status = TaskStatus::Failed;
        current.view.answer = None;
        current.view.error = Some(format!(
            "{}；{error}",
            current.view.error.as_deref().unwrap_or("终态事件写入失败")
        ));
        add_failure(&mut failure, error);
        if let Err(error) = persist_state(inner, &state) {
            add_failure(&mut failure, error);
        }
    }
    if let Some(error) = &failure {
        inner
            .failures
            .lock()
            .map_err(|_| broken("服务错误表锁已损坏"))?
            .push(error.clone());
    }
    failure.map_or(Ok(()), Err)
}

fn add_failure(failure: &mut Option<ServiceError>, next: ServiceError) {
    match failure {
        Some(current) => current.message.push_str(&format!("；{next}")),
        None => *failure = Some(next),
    }
}

fn combine_result<T>(
    result: Result<T, ServiceError>,
    cleanup: Result<(), ServiceError>,
) -> Result<T, ServiceError> {
    match (result, cleanup) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), Ok(())) | (Ok(_), Err(error)) => Err(error),
        (Err(mut error), Err(cleanup)) => {
            error.message.push_str(&format!("；{cleanup}"));
            Err(error)
        }
    }
}

fn persist_state(inner: &ServiceInner, state: &ServiceStateFile) -> Result<(), ServiceError> {
    write_state(&inner.state_path, state)
}

fn write_state(state_path: &Path, state: &ServiceStateFile) -> Result<(), ServiceError> {
    let bytes =
        serde_json::to_vec(state).map_err(|err| error("service_state_corrupt", err.to_string()))?;
    let temp = state_path.with_extension("json.tmp");
    {
        let mut file = File::create(&temp).map_err(io_error)?;
        file.write_all(&bytes).map_err(io_error)?;
        file.sync_all().map_err(io_error)?;
    }
    std::fs::rename(&temp, state_path).map_err(io_error)
}

/// 每次执行重新装配依赖，再沿既有编排器链路跑到终态。
async fn drive(
    inner: &Arc<ServiceInner>,
    record: &TaskRecord,
    context: &EventContext,
    start: &ExecutionStart,
    evidence: &Arc<ServiceEvidence>,
    control: &Arc<RunControl>,
    cancel: &CancelHandle,
) -> Result<String, (TaskStatus, String)> {
    let stores = Arc::new(CheckedStores {
        sessions: inner.session_store.clone(),
        tasks: inner.task_store.clone(),
        health: control.stores.clone(),
    });
    let build = TaskBuild {
        workspace: record.workspace.root.clone(),
        context: context.clone(),
        evidence: evidence.clone(),
        hooks: evidence.clone(),
        approvals: inner.broker.gate(context.clone()),
        session_store: stores.clone(),
        journal: inner.journal.clone(),
        approval_factory: inner.broker.clone(),
    };
    let factory = inner.factory.clone();
    // 工厂每次执行重新装配依赖。服务只注入执行边界与快照存储，不在装配后
    // 覆盖模型、工具或策略，否则测试与生产会走两套不同的执行链。
    let mut deps: AgentDependencies = factory
        .build(build)
        .await
        .map_err(|err| (TaskStatus::Failed, err.to_string()))?;
    deps.session_store = Some(stores.clone());
    let roster = deps.roster.clone();
    let sink: Arc<dyn EvidenceSink> = evidence.clone();
    let evidence_factory: EvidenceFactory =
        Arc::new(move |_session_id: &SessionId| Ok(sink.clone()));
    let mut budget = inner.budget;
    if let ExecutionStart::Resume(snapshot) | ExecutionStart::Conversation(snapshot) = start {
        // 消耗保留在会话中，每次执行的额度从当前累计值开始计算。
        budget.max_turns = budget
            .max_turns
            .checked_add(snapshot.run.consumption.turns)
            .ok_or_else(|| (TaskStatus::Failed, "累计轮数预算已溢出".to_owned()))?;
        budget.max_wall_clock = match budget.max_wall_clock {
            Some(limit) => Some(
                limit
                    .checked_add(snapshot.run.consumption.elapsed)
                    .ok_or_else(|| (TaskStatus::Failed, "累计墙钟预算已溢出".to_owned()))?,
            ),
            None => None,
        };
    }
    let mut orchestrator = Orchestrator::with_roster(deps, budget, evidence_factory, roster)
        .with_task_store(stores)
        .with_session_id(SessionId::new(context.session_id.clone()))
        .with_execution_control(control.clone(), cancel.clone())
        .with_worktree_scanner(Arc::new(
            crate::infrastructure::worktree::RootedWorktreeScanner::new(&record.workspace.root),
        ));
    let result = match start {
        ExecutionStart::Create(task) => {
            let mut task = task.clone();
            orchestrator
                .run(&mut task)
                .await
                .map(|outcome| outcome.answer)
        }
        ExecutionStart::Resume(snapshot) => orchestrator
            .resume(snapshot.clone())
            .await
            .map(|outcome| outcome.answer),
        ExecutionStart::Conversation(snapshot) => orchestrator
            .continue_conversation(snapshot.clone())
            .await
            .map(|outcome| outcome.answer),
    };
    match result {
        Ok(answer) => Ok(answer),
        Err(error) => Err((status_for(&error), error.to_string())),
    }
}

fn request_close(inner: &ServiceInner) -> Result<(), ServiceError> {
    *inner
        .closed
        .lock()
        .map_err(|_| broken("服务状态锁已损坏"))? = true;
    inner.broker.shutdown();
    let mut state = lock_state(inner)?;
    let live = lock_live(inner)?;
    for (task_id, execution) in live.iter() {
        execution.cancel.cancel();
        inner
            .broker
            .cancel_execution(&execution.context.execution_id, "服务已关闭");
        if let Some(record) = state.tasks.get_mut(task_id) {
            record.view.status = TaskStatus::Stopping;
        }
    }
    persist_state(inner, &state)
}

async fn close_execution(inner: &ServiceInner, execution_id: &str) -> Result<(), ServiceError> {
    let factory = inner.factory.clone();
    let execution_id = execution_id.to_owned();
    tokio::spawn(async move { factory.close(&execution_id).await })
        .await
        .map_err(|cause| error("factory_close_panicked", cause.to_string()))?
        .map_err(|cause| error("factory_close_failed", cause.to_string()))
}

async fn run_execution_close(
    inner: &ServiceInner,
    execution_id: &str,
    failure: &mut Option<ServiceError>,
) {
    if let Err(cause) = close_execution(inner, execution_id).await {
        add_failure(failure, cause);
    }
}

async fn shutdown_executions(inner: &Arc<ServiceInner>) -> Result<(), ServiceError> {
    let mut failure = request_close(inner).err();
    let live: Vec<_> = lock_live(inner)?
        .drain()
        .map(|(_, execution)| execution)
        .collect();
    for execution in live {
        match execution.handle.await {
            Ok(Ok(())) | Ok(Err(_)) => {} // 执行错误在发布终态前已进入服务错误表。
            Err(cause) => {
                add_failure(
                    &mut failure,
                    error("execution_join_failed", cause.to_string()),
                );
                if let Err(cause) = close_execution(inner, &execution.context.execution_id).await {
                    add_failure(&mut failure, cause);
                }
            }
        }
    }
    for error in inner
        .failures
        .lock()
        .map_err(|_| broken("服务错误表锁已损坏"))?
        .iter()
    {
        add_failure(&mut failure, error.clone());
    }
    if let Err(error) = inner.broker.health() {
        add_failure(&mut failure, error);
    }
    failure.map_or(Ok(()), Err)
}

fn release_lease(inner: &ServiceInner) -> Result<(), ServiceError> {
    let closed = inner.journal_impl.close();
    // 取出并丢弃锁文件句柄：POSIX 上 flock 随打开文件描述符关闭才释放；先显式
    // unlock 再 drop，避免关闭行为依赖具体平台实现。即便租约锁被污染也要取出
    // 句柄，否则同进程重开 `LocalTaskService::open` 会拿到 service_locked。
    let mut guard = inner
        .lease
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(file) = guard.take() {
        let _ = file.unlock();
    }
    closed
}

impl Drop for LocalTaskService {
    fn drop(&mut self) {
        // 存活执行为空时同步释放租约：保证同进程内随后重开
        // `LocalTaskService::open` 的 try_lock 不会被尚未调度的 detached 任务挡住。
        // 该路径对应测试里的 `drop(service)` 后紧接 `open()`：open 在最前面同步 try_lock，
        // 而原实现把整段收尾（含 release_lease）交给未 await 的定时任务，POSIX 上 flock
        // 按打开文件描述加锁，fd 未关闭前锁不释放，于是重开拿到 service_locked。
        let live_is_empty = lock_live(&self.inner)
            .map(|live| live.is_empty())
            .unwrap_or(true);
        if live_is_empty {
            let lease_held = self
                .inner
                .lease
                .lock()
                .map(|lease| lease.is_some())
                .unwrap_or(true);
            if lease_held {
                let _ = request_close(&self.inner);
                let _ = release_lease(&self.inner);
            }
            return;
        }
        // 仍有存活执行：保持原行为，租约交给定时任务，待执行收束后再释放。
        // dropping_a_service_keeps_its_lease_until_execution_resources_close 依赖此语义。
        if self
            .inner
            .lease
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_none()
        {
            return;
        }
        if let Err(error) = request_close(&self.inner) {
            eprintln!("服务关闭失败: {error}");
        }
        let inner = self.inner.clone();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let result =
                    combine_result(shutdown_executions(&inner).await, release_lease(&inner));
                if let Err(error) = result {
                    eprintln!("服务关闭失败: {error}");
                }
            });
        }
        // 没有运行时可等待时，执行仍持有 inner 与租约；最后一个执行释放后
        // 才能接管。不能在 Drop 返回时提前把活着的任务交给另一实例。
    }
}

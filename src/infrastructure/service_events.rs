//! 持久化服务事件与运行适配器。写入完成后发布，订阅只交付已提交的事实。

use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::sync::broadcast;

use crate::application::port::{
    BoxFuture, CancelHandle, EvidenceSink, ModelReply, ModelRequest, RunHooks, Timestamp,
};
use crate::application::service::{EventJournal, EventSubscription};
use crate::domain::evidence::AuditEvent;
// 事件日志字节上限 `MAX_JOURNAL_BYTES` 定义在 domain/service。这里公开再导出，
// 让既有按 infrastructure 路径引用它的调用方与测试继续可用，也避免同一份
// 常量出现两个来源。
pub use crate::domain::service::MAX_JOURNAL_BYTES;
use crate::domain::service::{
    EventContext, ServiceError, ServiceEvent, ServiceEventPayload, PROTOCOL_VERSION,
};
use crate::domain::tool::ToolCall;

/// 实时订阅的缓冲上限；慢客户端通过持久日志补读，不无限占用内存。
pub const EVENT_QUEUE_CAPACITY: usize = 128;
/// 单条 JSONL 记录的字节上限，包含末尾换行。
pub const MAX_EVENT_BYTES: usize = 1024 * 1024;
// 事件日志字节上限 `MAX_JOURNAL_BYTES` 定义在 domain/service，这里通过
// use 引入供本模块与既有外部调用方使用，避免同一份常量出现两个来源。

/// 一次事件日志归档的结果；空跑不会生成归档或创建数据日志。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct RotateReport {
    /// 是否已归档非空日志。
    pub archived: bool,
    /// 原日志的保留位置；空跑时为 `None`。
    pub archive_path: Option<PathBuf>,
    /// 原日志不存在或没有新事件时为 `true`。
    pub empty_run: bool,
    /// 服务重启、游标与原数据恢复的建议。
    pub recommendation: String,
}

/// 取得日志写入租约后归档原始字节，再创建空日志；不解析或删除归档。
///
/// 日志不存在或长度为零时返回空跑结果。运行中的服务持有同一租约，
/// 因此必须先停止服务；归档后的新日志从序号 1 开始。
///
/// # Errors
///
/// 租约被占用、系统时间无效或文件操作失败时返回错误。原日志重命名后
/// 若无法创建新日志，错误会给出仍然保留原始数据的归档路径。
pub fn rotate_events(workspace: &Path) -> Result<RotateReport, ServiceError> {
    let service = workspace.join(".qcodeharness/service");
    let path = service.join("events.jsonl");
    let empty = RotateReport {
        archived: false,
        archive_path: None,
        empty_run: true,
        recommendation: "没有需要归档的新事件；可正常启动服务，已有归档保持保留".to_owned(),
    };
    if !service.try_exists().map_err(io_error)? {
        return Ok(empty);
    }
    let _lease = open_lease(&path)?;
    let metadata = match std::fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(empty),
        Err(error) => return Err(io_error(error)),
    };
    if !metadata.is_file() {
        return Err(ServiceError::new("journal_io", "事件日志必须是普通文件"));
    }
    if metadata.len() == 0 {
        return Ok(empty);
    }
    let mut millis = SystemTime::now().duration_since(UNIX_EPOCH).map_err(|error| {
        ServiceError::new("invalid_clock", error.to_string())
    })?.as_millis();
    // 同毫秒内再次归档或时钟回拨时也不能覆盖已有归档。
    let archive_path = loop {
        let candidate = service.join(format!("events.jsonl.{millis}.archive"));
        match std::fs::symlink_metadata(&candidate) {
            Ok(_) => millis += 1,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break candidate,
            Err(error) => return Err(io_error(error)),
        }
    };
    std::fs::rename(&path, &archive_path).map_err(io_error)?;
    OpenOptions::new().write(true).create_new(true).open(&path).map_err(|error| {
        ServiceError::new(
            "journal_io",
            format!(
                "原日志已保留在 {}，创建新日志失败: {error}；修复文件权限后重新启动服务，或停止服务后将归档复制回原路径",
                archive_path.display()
            ),
        )
    })?;
    Ok(RotateReport {
        archived: true,
        archive_path: Some(archive_path),
        empty_run: false,
        recommendation: "原数据已保留在归档中；重新启动服务后从游标 0 订阅。需要恢复旧日志时，先停止服务并备份新日志，再将归档复制回 events.jsonl".to_owned(),
    })
}

struct JournalState {
    file: File,
    /// 独占写入租约。数据文件保持可读，外部回放与归档不会被锁挡住。
    /// `close()` 必须显式解锁并交出句柄，另一个实例才能接管同一日志。
    lease: Option<File>,
    events: Vec<ServiceEvent>,
    bytes: u64,
    failure: Option<ServiceError>,
    publisher: Option<broadcast::Sender<ServiceEvent>>,
}

/// 有界 JSONL 日志。一个路径只允许一个写入实例，由调用方用 `Arc` 共享。
///
/// 独占租约放在独立的 `.lock` 文件上，覆盖实例整个生命周期；数据文件始终
/// 允许读者打开，供回放、对账与归档使用。重开时校验完整日志，拒绝未知版本、
/// 跳号、重号和截断；不从旧 evidence 文件推测服务序号。
pub struct JsonEventJournal {
    state: Mutex<JournalState>,
}

impl JsonEventJournal {
    /// 打开或创建日志，恢复最后提交的序号。父目录不存在时创建。
    ///
    /// 文件超过大小限制、已有其他写入者、格式损坏或 IO 失败均返回错误。
    /// 写入期间发生 IO 错误后本实例停止追加，避免在半行后继续产生记录。
    pub fn open(path: impl AsRef<Path>) -> Result<Self, ServiceError> {
        let path = path.as_ref();
        if let Some(parent) = path.parent().filter(|parent| !parent.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent).map_err(io_error)?;
        }
        // 先取得租约再读数据：两个实例同时打开时，第二个不会读到正在追加的半行。
        // 锁与数据分开，读者仍能按普通文件打开日志。
        let lease = open_lease(path)?;
        let mut file = OpenOptions::new().create(true).read(true).append(true).open(path).map_err(io_error)?;
        let bytes = file.metadata().map_err(io_error)?.len();
        if bytes > MAX_JOURNAL_BYTES {
            return Err(journal_too_large());
        }
        let events = load_events(&mut file)?;
        let (publisher, _) = broadcast::channel(EVENT_QUEUE_CAPACITY);
        Ok(Self { state: Mutex::new(JournalState {
            file, lease: Some(lease), events, bytes, failure: None, publisher: Some(publisher),
        }) })
    }

    /// 停止发布并释放独占租约，供服务关闭时移交日志给新实例。
    ///
    /// 已提交事件仍在数据文件中；关闭后本实例不再追加。重复关闭无副作用。
    pub fn close(&self) -> Result<(), ServiceError> {
        // 状态锁可能因执行线程 panic 而毒化。仍要取出租约并显式解锁：POSIX 的
        // flock 随打开文件描述符释放，服务持有的 Arc 在本方法返回后仍存活。
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.publisher.take();
        if let Some(lease) = state.lease.take() {
            let _ = lease.unlock();
        }
        if state.failure.is_none() {
            state.failure = Some(ServiceError::new("journal_closed", "事件日志已关闭"));
        }
        Ok(())
    }

    /// 返回状态锁；锁毒化时让调用方报出明确错误，不带着可疑状态继续追加。
    ///
    /// 关闭路径不经过这里：即使状态已损坏，也必须释放独占租约。
    fn lock(&self) -> Result<MutexGuard<'_, JournalState>, ServiceError> {
        self.state
            .lock()
            .map_err(|_| ServiceError::new("journal_unhealthy", "事件日志状态锁已损坏"))
    }
}

impl EventJournal for JsonEventJournal {
    fn append(
        &self,
        context: Option<&EventContext>,
        payload: ServiceEventPayload,
    ) -> Result<ServiceEvent, ServiceError> {
        let mut events = self.append_batch(context, vec![payload])?;
        Ok(events.pop().expect("单条批次必返回一条事件"))
    }

    fn append_batch(
        &self,
        context: Option<&EventContext>,
        payloads: Vec<ServiceEventPayload>,
    ) -> Result<Vec<ServiceEvent>, ServiceError> {
        let mut state = self.lock()?;
        if let Some(error) = &state.failure {
            return Err(error.clone());
        }
        if payloads.is_empty() {
            return Ok(Vec::new());
        }
        // 启动事件属于一次原子提交：先在内存里取号、编码并检查全部上限，
        // 再把整批字节一次写入磁盘。任何前置检查失败都不能留下半组事件。
        let millis = SystemTime::now().duration_since(UNIX_EPOCH).map_err(|error| {
            ServiceError::new("invalid_clock", error.to_string())
        })?.as_millis();
        let time_unix_ms = u64::try_from(millis).map_err(|_| ServiceError::new("invalid_clock", "毫秒时间戳越界"))?;
        let task_id = context.map(|context| context.task_id.clone());
        let session_id = context.map(|context| context.session_id.clone());
        let execution_id = context.map(|context| context.execution_id.clone());
        let origin = context.map(|context| context.origin.clone()).unwrap_or_default();
        let mut sequence = latest_sequence(&state.events);
        let mut events = Vec::with_capacity(payloads.len());
        let mut chunk = Vec::new();
        for payload in payloads {
            sequence = sequence.checked_add(1).ok_or_else(|| {
                ServiceError::new("sequence_exhausted", "服务事件序号已耗尽")
            })?;
            let event = ServiceEvent {
                version: PROTOCOL_VERSION,
                sequence,
                time_unix_ms,
                task_id: task_id.clone(),
                session_id: session_id.clone(),
                execution_id: execution_id.clone(),
                origin: origin.clone(),
                payload,
            };
            let mut line = serde_json::to_vec(&event).map_err(|error| {
                ServiceError::new("event_encoding", error.to_string())
            })?;
            line.push(b'\n');
            if line.len() > MAX_EVENT_BYTES {
                return Err(event_too_large());
            }
            if state.bytes + chunk.len() as u64 + line.len() as u64 > MAX_JOURNAL_BYTES {
                return Err(journal_too_large());
            }
            chunk.extend_from_slice(&line);
            events.push(event);
        }
        // 取号、写盘和发布持同一把锁，订阅者不会看到尚未确认落盘的事件。
        // 写盘失败时先尽力把文件截断回原长度；截断是否成功都返回原始错误，
        // 不让截断失败掩盖真正的写入原因（如实例外的文件修改）。
        if let Err(error) = write_committed(&mut state, &chunk) {
            let _ = state.file.set_len(state.bytes).map_err(io_error);
            state.failure = Some(error.clone());
            state.publisher.take();
            return Err(error);
        }
        state.bytes += chunk.len() as u64;
        for event in &events {
            state.events.push(event.clone());
            if let Some(publisher) = &state.publisher {
                // 没有订阅者是正常状态，持久日志仍是事实来源。
                let _ = publisher.send(event.clone());
            }
        }
        Ok(events)
    }

    fn read(&self, after: u64, limit: usize) -> Result<Vec<ServiceEvent>, ServiceError> {
        let state = self.lock()?;
        validate_cursor(after, &state.events)?;
        let start = state.events.partition_point(|event| event.sequence <= after);
        Ok(state.events.iter().skip(start).take(limit).cloned().collect())
    }

    fn subscribe(&self, after: u64) -> Result<Box<dyn EventSubscription>, ServiceError> {
        let state = self.lock()?;
        validate_cursor(after, &state.events)?;
        if let Some(error) = &state.failure {
            return Err(error.clone());
        }
        let publisher = state.publisher.as_ref().ok_or_else(|| {
            ServiceError::new("journal_closed", "事件日志已停止发布")
        })?;
        // 追加也需要此锁，因此快照与接上实时流之间没有提交窗口。
        let live = publisher.subscribe();
        let start = state.events.partition_point(|event| event.sequence <= after);
        let replay = state.events.iter().skip(start).cloned().collect();
        Ok(Box::new(JournalSubscription { replay, live, cursor: after, lagged: false }))
    }

    fn latest(&self) -> Result<u64, ServiceError> {
        Ok(latest_sequence(&self.lock()?.events))
    }
}

fn open_lease(path: &Path) -> Result<File, ServiceError> {
    let lease_path = lease_path(path);
    let lease = OpenOptions::new()
        .create(true)
        .read(true)
        .append(true)
        .open(&lease_path)
        .map_err(io_error)?;
    lease.try_lock().map_err(|error| ServiceError::new(
        "journal_locked",
        format!("无法独占事件日志 {}: {error}", path.display()),
    ))?;
    Ok(lease)
}

fn lease_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".lock");
    PathBuf::from(name)
}

fn write_committed(state: &mut JournalState, line: &[u8]) -> Result<(), ServiceError> {
    let size = state.file.metadata().map_err(io_error)?.len();
    if size != state.bytes {
        return Err(ServiceError::new("journal_changed", "事件日志在实例外被修改，拒绝继续追加"));
    }
    state.file.write_all(line).map_err(io_error)?;
    state.file.flush().map_err(io_error)?;
    state.file.sync_data().map_err(io_error)
}

fn load_events(file: &mut File) -> Result<Vec<ServiceEvent>, ServiceError> {
    let mut reader = BufReader::new(file);
    let mut events = Vec::new();
    let mut line = Vec::new();
    loop {
        line.clear();
        let read = reader.by_ref().take(MAX_EVENT_BYTES as u64 + 1).read_until(b'\n', &mut line).map_err(io_error)?;
        if read == 0 {
            break;
        }
        if line.len() > MAX_EVENT_BYTES {
            return Err(event_too_large());
        }
        if line.last() != Some(&b'\n') {
            return Err(ServiceError::new("journal_corrupt", "事件日志末行不完整，拒绝追加"));
        }
        let value: serde_json::Value = serde_json::from_slice(&line).map_err(|error| {
            ServiceError::new("journal_corrupt", format!("事件日志第 {} 行损坏: {error}", events.len() + 1))
        })?;
        if value.get("version").and_then(serde_json::Value::as_u64) != Some(PROTOCOL_VERSION as u64) {
            return Err(ServiceError::new("unsupported_version", "事件日志版本未知或缺失"));
        }
        let event: ServiceEvent = serde_json::from_value(value).map_err(|error| {
            ServiceError::new("journal_corrupt", format!("事件日志第 {} 行不符合协议: {error}", events.len() + 1))
        })?;
        let expected = latest_sequence(&events).checked_add(1).ok_or_else(|| {
            ServiceError::new("sequence_exhausted", "服务事件序号已耗尽")
        })?;
        if event.sequence != expected {
            return Err(ServiceError::new(
                "journal_sequence", format!("服务事件序号应为 {expected}，实际为 {}", event.sequence),
            ));
        }
        events.push(event);
    }
    Ok(events)
}

fn latest_sequence(events: &[ServiceEvent]) -> u64 {
    events.last().map_or(0, |event| event.sequence)
}

fn validate_cursor(after: u64, events: &[ServiceEvent]) -> Result<(), ServiceError> {
    let latest = latest_sequence(events);
    if after > latest {
        return Err(ServiceError::new("invalid_cursor", format!("游标 {after} 超过日志末尾 {latest}")));
    }
    Ok(())
}

fn io_error(error: std::io::Error) -> ServiceError {
    ServiceError::new("journal_io", error.to_string())
}

fn event_too_large() -> ServiceError {
    ServiceError::new("event_too_large", format!("单条事件超过 {MAX_EVENT_BYTES} 字节上限"))
}

fn journal_too_large() -> ServiceError {
    ServiceError::new(
        "journal_too_large",
        format!(
            "事件日志超过 {MAX_JOURNAL_BYTES} 字节上限；先停止服务，执行 `--rotate-events` 归档日志并保留原数据，再重新启动服务"
        ),
    )
}

struct JournalSubscription {
    replay: VecDeque<ServiceEvent>,
    live: broadcast::Receiver<ServiceEvent>,
    cursor: u64,
    lagged: bool,
}

impl EventSubscription for JournalSubscription {
    fn next(&mut self) -> BoxFuture<'_, Result<ServiceEvent, ServiceError>> {
        Box::pin(async move {
            if self.lagged {
                return Err(lagged(self.cursor));
            }
            if let Some(event) = self.replay.pop_front() {
                self.cursor = event.sequence;
                return Ok(event);
            }
            match self.live.recv().await {
                Ok(event) => {
                    self.cursor = event.sequence;
                    Ok(event)
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    self.lagged = true;
                    Err(lagged(self.cursor))
                }
                Err(broadcast::error::RecvError::Closed) => {
                    Err(ServiceError::new("journal_closed", "事件发布已关闭"))
                }
            }
        })
    }
}

fn lagged(cursor: u64) -> ServiceError {
    ServiceError::new("lagged", format!("订阅缓冲已溢出，请从游标 {cursor} 补读并重新订阅"))
}

/// 从扩展调用 ID 补充来源关联。前缀只提供审计线索，不授予任何权限。
pub fn context_for_call(context: &EventContext, call: &ToolCall) -> EventContext {
    context_for_call_id(context, call.id.as_str())
}

fn context_for_call_id(context: &EventContext, id: &str) -> EventContext {
    let mut context = context.clone();
    if let Some((extension, _)) = id.strip_prefix("extension__").and_then(|rest| rest.split_once("__"))
        && !extension.is_empty() {
            context.origin.extension_id = Some(extension.to_owned());
        }
    context
}

fn context_for_audit(context: &EventContext, event: &AuditEvent) -> EventContext {
    match event {
        AuditEvent::ToolRequested { call, .. }
        | AuditEvent::ToolObserved { call, .. }
        | AuditEvent::SandboxApplied { call, .. } => context_for_call(context, call),
        AuditEvent::McpCallFailed { call_id, .. } => context_for_call_id(context, call_id.as_str()),
        _ => context.clone(),
    }
}

/// 将真实审计事件和模型挂点写入服务日志。
///
/// `record` 的端口没有错误返回，调用方应在停止边界检查 `health`。写失败会
/// 保留首个错误并取消本次执行，不声称该条记录已持久化。审计只经 EvidenceSink
/// 写入，工具挂点不重复生成审计；事件时间取服务日志提交时刻。
pub struct ServiceEvidence {
    journal: Arc<dyn EventJournal>,
    context: EventContext,
    cancel: CancelHandle,
    failure: Mutex<Option<ServiceError>>,
}

impl ServiceEvidence {
    /// 为一次执行绑定日志、来源和独立的取消通道。
    pub fn new(journal: Arc<dyn EventJournal>, context: EventContext, cancel: CancelHandle) -> Self {
        Self { journal, context, cancel, failure: Mutex::new(None) }
    }

    /// 返回首个持久化错误；失败标记在本实例生命周期内不会自动清除。
    pub fn health(&self) -> Result<(), ServiceError> {
        match self.failure.lock() {
            Ok(failure) => failure.clone().map_or(Ok(()), Err),
            Err(_) => Err(ServiceError::new("evidence_unhealthy", "服务证据健康状态锁已损坏")),
        }
    }

    fn emit(&self, context: &EventContext, payload: ServiceEventPayload) {
        let Ok(mut failure) = self.failure.lock() else {
            self.cancel.cancel();
            return;
        };
        if failure.is_some() {
            self.cancel.cancel();
            return;
        }
        if let Err(error) = self.journal.append(Some(context), payload) {
            *failure = Some(error);
            self.cancel.cancel();
        }
    }
}

impl EvidenceSink for ServiceEvidence {
    fn record(&self, _at: Timestamp, event: AuditEvent) {
        let context = context_for_audit(&self.context, &event);
        self.emit(&context, ServiceEventPayload::Audit { event });
    }
}

impl RunHooks for ServiceEvidence {
    fn before_model_call<'a>(&'a self, request: &'a ModelRequest<'a>) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            self.emit(&self.context, ServiceEventPayload::ModelCallStarted { role: request.spec.role });
        })
    }

    fn after_model_reply<'a>(&'a self, reply: &'a ModelReply, retried: bool) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            self.emit(&self.context, ServiceEventPayload::ModelCallFinished {
                text: reply.turn.text.clone(), calls: reply.turn.calls.clone(), retried,
            });
        })
    }
}

#[cfg(test)]
mod rotate_tests {
    use super::*;

    #[test]
    fn an_empty_journal_still_requires_the_writers_lease() {
        let stamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let workspace = std::env::temp_dir().join(format!(
            "qch-rotate-empty-lease-{}-{stamp}", std::process::id()
        ));
        let path = workspace.join(".qcodeharness/service/events.jsonl");
        let journal = JsonEventJournal::open(&path).unwrap();

        assert_eq!(rotate_events(&workspace).unwrap_err().code, "journal_locked");
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 0);
        drop(journal);
        assert!(rotate_events(&workspace).unwrap().empty_run);
        std::fs::remove_dir_all(workspace).unwrap();
    }
}

#[cfg(test)]
mod close_tests {
    use super::*;

    /// 状态锁被污染时，关闭仍必须交出写入租约。
    ///
    /// POSIX 的 flock 随打开文件描述符释放。若 `close()` 因状态锁毒化提前
    /// 返回，旧句柄会持续占锁，同进程重开只能拿到 `journal_locked`。这里直接
    /// 断言关闭成功且租约可移交，不依赖不同平台的锁实现差异。
    #[test]
    fn a_poisoned_state_lock_still_releases_the_writer_lease_on_close() {
        let stamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let workspace = std::env::temp_dir().join(format!(
            "qch-close-poisoned-lease-{}-{stamp}",
            std::process::id()
        ));
        let path = workspace.join("events.jsonl");
        let journal = Arc::new(JsonEventJournal::open(&path).unwrap());
        let poisoner = journal.clone();
        let _ = std::thread::spawn(move || {
            let _guard = poisoner.state.lock().unwrap();
            panic!("模拟持锁线程崩溃");
        })
        .join();

        assert!(
            journal.close().is_ok(),
            "关闭不应因状态锁毒化而跳过租约释放"
        );
        let reopened = JsonEventJournal::open(&path).unwrap();
        assert!(reopened.close().is_ok());
        drop(journal);
        drop(reopened);
        std::fs::remove_dir_all(workspace).unwrap();
    }
}

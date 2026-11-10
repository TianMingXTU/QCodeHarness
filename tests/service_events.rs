//! 服务事件与异步审批的外部契约测试，不连接模型或执行工具。

use std::fs::{self, OpenOptions};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use qcode_harness::application::port::{
    ApprovalGate, ApprovalRequest, CancelHandle, EvidenceSink, ModelReply, ModelRequest, RunHooks,
};
use qcode_harness::application::service::{EventJournal, EventSubscription};
use qcode_harness::domain::agent::{AgentRole, AgentSpec, AgentTurn};
use qcode_harness::domain::evidence::{AuditEvent, TokenUsage};
use qcode_harness::domain::ids::CallId;
use qcode_harness::domain::service::{
    EventContext, EventOrigin, ServiceCommand, ServiceError, ServiceEvent, ServiceEventPayload,
    PROTOCOL_VERSION,
};
use qcode_harness::domain::tool::{ToolAnnotations, ToolCall};
use qcode_harness::infrastructure::approval::async_gate::ApprovalBroker;
use qcode_harness::infrastructure::service_events::{
    JsonEventJournal, ServiceEvidence, EVENT_QUEUE_CAPACITY, MAX_EVENT_BYTES, MAX_JOURNAL_BYTES,
};
use serde_json::json;
use tokio::time::timeout;

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        let stamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let serial = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "qcode-service-events-{}-{stamp}-{serial}", std::process::id()
        ));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn log(&self) -> PathBuf {
        self.0.join("events.jsonl")
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

fn context(task: &str, execution: &str) -> EventContext {
    EventContext {
        task_id: task.into(),
        session_id: format!("session-{task}"),
        execution_id: execution.into(),
        origin: EventOrigin { parent_session_id: Some("parent".into()), extension_id: None },
    }
}

fn request(id: &str) -> ApprovalRequest {
    ApprovalRequest {
        turn: 3,
        call: ToolCall {
            id: CallId::new(id),
            name: "write_file".into(),
            arguments: json!({"path": "a.txt", "content": "hello"}),
        },
        annotations: ToolAnnotations::new(false, true, false, false),
        reason: Some("疑似越界写入，需要人工复核".into()),
        resumed: true,
    }
}

async fn next(subscription: &mut dyn EventSubscription) -> ServiceEvent {
    timeout(Duration::from_secs(3), subscription.next()).await.unwrap().unwrap()
}

#[test]
fn commands_use_the_public_operation_tag() {
    let command: ServiceCommand = serde_json::from_value(json!({
        "op": "decide", "approval_id": "a1", "task_id": "t1", "execution_id": "e1", "allow": true
    })).unwrap();
    assert!(matches!(command, ServiceCommand::Decide { allow: true, .. }));
    assert_eq!(serde_json::to_value(ServiceCommand::Shutdown).unwrap(), json!({"op": "shutdown"}));
}

#[test]
fn restart_continues_the_service_sequence_and_preserves_context() {
    let dir = TestDirectory::new();
    let ctx = context("t1", "e1");
    {
        let journal = JsonEventJournal::open(dir.log()).unwrap();
        let first = journal.append(Some(&ctx), ServiceEventPayload::ExecutionStarted).unwrap();
        assert_eq!(first.version, PROTOCOL_VERSION);
        assert_eq!(first.sequence, 1);
        assert_eq!(first.origin.parent_session_id.as_deref(), Some("parent"));
        assert_eq!(first.execution_id.as_deref(), Some("e1"));
    }
    let journal = JsonEventJournal::open(dir.log()).unwrap();
    assert_eq!(journal.latest().unwrap(), 1);
    let second = journal.append(None, ServiceEventPayload::WorkspaceRemoved).unwrap();
    assert_eq!(second.sequence, 2);
    assert_eq!(journal.read(0, 10).unwrap().len(), 2);
    assert_eq!(journal.read(1, 10).unwrap()[0], second);
}

#[tokio::test]
async fn replay_is_read_only_and_the_replay_to_live_boundary_has_no_gap() {
    let dir = TestDirectory::new();
    let journal = JsonEventJournal::open(dir.log()).unwrap();
    journal.append(None, ServiceEventPayload::ExecutionStarted).unwrap();
    let mut subscription = journal.subscribe(0).unwrap();
    journal.append(None, ServiceEventPayload::CancelRequested).unwrap();
    assert_eq!(next(subscription.as_mut()).await.sequence, 1);
    assert_eq!(next(subscription.as_mut()).await.sequence, 2);
    journal.append(None, ServiceEventPayload::WorkspaceRemoved).unwrap();
    assert_eq!(next(subscription.as_mut()).await.sequence, 3);
    assert_eq!(journal.read(0, 100).unwrap().len(), 3);
    assert_eq!(fs::read_to_string(dir.log()).unwrap().lines().count(), 3);
    assert!(journal.read(4, 1).is_err());
    assert!(journal.subscribe(4).is_err());
}

#[tokio::test]
async fn a_slow_client_is_told_to_recover_from_its_cursor() {
    let dir = TestDirectory::new();
    let journal = JsonEventJournal::open(dir.log()).unwrap();
    let mut slow = journal.subscribe(0).unwrap();
    for _ in 0..EVENT_QUEUE_CAPACITY + 1 {
        journal.append(None, ServiceEventPayload::ExecutionStarted).unwrap();
    }
    let error = slow.next().await.unwrap_err();
    assert_eq!(error.code, "lagged");
    let replay = journal.read(0, EVENT_QUEUE_CAPACITY + 1).unwrap();
    assert_eq!(replay.len(), EVENT_QUEUE_CAPACITY + 1);
    let cursor = replay.last().unwrap().sequence;
    let mut recovered = journal.subscribe(cursor).unwrap();
    let live = journal.append(None, ServiceEventPayload::WorkspaceRemoved).unwrap();
    assert_eq!(next(recovered.as_mut()).await, live);
}

#[test]
fn corrupt_unknown_duplicate_and_out_of_order_logs_are_rejected() {
    for defect in ["version", "duplicate", "order", "truncated", "evidence"] {
        let dir = TestDirectory::new();
        let journal = JsonEventJournal::open(dir.log()).unwrap();
        let event = journal.append(None, ServiceEventPayload::ExecutionStarted).unwrap();
        drop(journal);
        let mut value = serde_json::to_value(&event).unwrap();
        let contents = match defect {
            "version" => { value["version"] = json!(999); format!("{value}\n") }
            "duplicate" => format!("{value}\n{value}\n"),
            "order" => { value["sequence"] = json!(2); format!("{value}\n") }
            "truncated" => "{\"version\":1".into(),
            "evidence" => "{\"sequence\":900,\"event\":{\"StepStarted\":{\"turn\":1,\"role\":\"Coder\"}}}\n".into(),
            _ => unreachable!(),
        };
        fs::write(dir.log(), contents).unwrap();
        assert!(JsonEventJournal::open(dir.log()).is_err(), "应拒绝 {defect}");
    }
}

#[test]
fn journal_and_event_size_limits_are_explicit() {
    let dir = TestDirectory::new();
    let journal = JsonEventJournal::open(dir.log()).unwrap();
    let error = journal.append(None, ServiceEventPayload::Completed {
        answer: "x".repeat(MAX_EVENT_BYTES),
    }).unwrap_err();
    assert_eq!(error.code, "event_too_large");
    assert_eq!(journal.latest().unwrap(), 0);
    drop(journal);
    OpenOptions::new().write(true).open(dir.log()).unwrap().set_len(MAX_JOURNAL_BYTES + 1).unwrap();
    let error = match JsonEventJournal::open(dir.log()) { Ok(_) => panic!("超大文件必须拒绝"), Err(error) => error };
    assert_eq!(error.code, "journal_too_large");
}

#[test]
fn the_same_journal_cannot_have_two_writers() {
    let dir = TestDirectory::new();
    let first = JsonEventJournal::open(dir.log()).unwrap();
    assert!(JsonEventJournal::open(dir.log()).is_err());
    drop(first);
    assert!(JsonEventJournal::open(dir.log()).is_ok());
}

struct SwitchJournal {
    journal: JsonEventJournal,
    fail: AtomicBool,
}

impl EventJournal for SwitchJournal {
    fn append(&self, context: Option<&EventContext>, payload: ServiceEventPayload) -> Result<ServiceEvent, ServiceError> {
        if self.fail.load(Ordering::SeqCst) {
            return Err(ServiceError::new("journal_io", "测试模拟写入失败"));
        }
        self.journal.append(context, payload)
    }
    fn read(&self, after: u64, limit: usize) -> Result<Vec<ServiceEvent>, ServiceError> { self.journal.read(after, limit) }
    fn subscribe(&self, after: u64) -> Result<Box<dyn EventSubscription>, ServiceError> { self.journal.subscribe(after) }
    fn latest(&self) -> Result<u64, ServiceError> { self.journal.latest() }
}

#[tokio::test]
async fn evidence_and_model_hooks_preserve_origins_without_duplicate_audits() {
    let dir = TestDirectory::new();
    let journal = Arc::new(JsonEventJournal::open(dir.log()).unwrap());
    let evidence = ServiceEvidence::new(journal.clone(), context("t1", "e1"), CancelHandle::default());
    let call = request("extension__sample__call1").call;
    evidence.record(SystemTime::now(), AuditEvent::ToolRequested { turn: 1, call: call.clone() });
    evidence.before_tool_call(&call).await;
    evidence.after_tool_result(&call, &qcode_harness::domain::tool::ToolOutcome::success("done")).await;
    let spec = AgentSpec::new(AgentRole::Coder, "test");
    evidence.before_model_call(&ModelRequest { history: vec![], spec: &spec, tools: &[], cache_key: None }).await;
    evidence.after_model_reply(&ModelReply {
        turn: AgentTurn { text: "answer".into(), calls: vec![call] }, usage: TokenUsage::default(),
    }, true).await;
    let events = journal.read(0, 10).unwrap();
    assert_eq!(events.len(), 3);
    assert_eq!(events[0].origin.extension_id.as_deref(), Some("sample"));
    assert_eq!(events[0].origin.parent_session_id.as_deref(), Some("parent"));
    assert!(matches!(&events[0].payload, ServiceEventPayload::Audit { event: AuditEvent::ToolRequested { .. } }));
    assert!(matches!(&events[1].payload, ServiceEventPayload::ModelCallStarted { role: AgentRole::Coder }));
    assert!(matches!(&events[2].payload, ServiceEventPayload::ModelCallFinished { retried: true, .. }));
    assert!(evidence.health().is_ok());
}

#[test]
fn a_failed_evidence_sink_reports_unhealthy_and_requests_cancellation() {
    let dir = TestDirectory::new();
    let journal = Arc::new(SwitchJournal { journal: JsonEventJournal::open(dir.log()).unwrap(), fail: AtomicBool::new(true) });
    let cancel = CancelHandle::default();
    let evidence = ServiceEvidence::new(journal.clone(), context("t1", "e1"), cancel.clone());
    evidence.record(SystemTime::now(), AuditEvent::StepStarted { turn: 1, role: AgentRole::Coder });
    assert!(cancel.is_cancelled());
    assert_eq!(evidence.health().unwrap_err().code, "journal_io");
    assert_eq!(journal.latest().unwrap(), 0);
}

#[tokio::test]
async fn approval_decisions_are_owned_durable_and_cannot_be_reused() {
    let dir = TestDirectory::new();
    let journal = Arc::new(JsonEventJournal::open(dir.log()).unwrap());
    let broker = ApprovalBroker::new(journal.clone());
    let gate = broker.gate(context("t1", "e1"));
    let mut subscription = journal.subscribe(0).unwrap();
    let waiter = tokio::spawn(async move { gate.request(&request("extension__sample__call1")).await });
    let waiting = next(subscription.as_mut()).await;
    let ServiceEventPayload::ApprovalWaiting { request: pending } = waiting.payload else { panic!("必须先发审批等待事件") };
    assert!(pending.resumed);
    assert_eq!(pending.reason, request("call").reason);
    assert_eq!(pending.origin.extension_id.as_deref(), Some("sample"));
    assert!(broker.pending(Some("other")).is_empty());
    assert_eq!(broker.pending(Some("t1")).len(), 1);
    assert!(broker.decide(&pending.approval_id, "other", "e1", true).is_err());
    assert!(broker.decide(&pending.approval_id, "t1", "other", true).is_err());
    assert!(!waiter.is_finished());
    broker.decide(&pending.approval_id, "t1", "e1", true).unwrap();
    assert!(waiter.await.unwrap().is_ok());
    assert!(broker.pending(None).is_empty());
    assert!(broker.decide(&pending.approval_id, "t1", "e1", true).is_err());
    assert!(matches!(next(subscription.as_mut()).await.payload, ServiceEventPayload::ApprovalResolved { allowed: true, .. }));
    let persisted = fs::read_to_string(dir.log()).unwrap();
    assert_eq!(persisted.lines().count(), 2);
    assert!(persisted.contains("approval_resolved"));
}

#[tokio::test]
async fn independent_approvals_can_resolve_out_of_order() {
    let dir = TestDirectory::new();
    let journal = Arc::new(JsonEventJournal::open(dir.log()).unwrap());
    let broker = ApprovalBroker::new(journal.clone());
    let mut subscription = journal.subscribe(0).unwrap();
    let first = broker.gate(context("t1", "e1"));
    let second = broker.gate(context("t2", "e2"));
    let one = tokio::spawn(async move { first.request(&request("call1")).await });
    let two = tokio::spawn(async move { second.request(&request("call2")).await });
    next(subscription.as_mut()).await;
    next(subscription.as_mut()).await;
    let requests = broker.pending(None);
    let second_request = requests.iter().find(|request| request.task_id == "t2").unwrap();
    broker.decide(&second_request.approval_id, "t2", "e2", true).unwrap();
    assert!(timeout(Duration::from_secs(2), two).await.unwrap().unwrap().is_ok());
    assert!(!one.is_finished());
    broker.cancel_execution("e1", "任务已取消");
    assert!(timeout(Duration::from_secs(2), one).await.unwrap().unwrap().is_err());
}

#[tokio::test]
async fn failed_waiting_or_resolution_writes_never_authorize_the_tool() {
    let dir = TestDirectory::new();
    let journal = Arc::new(SwitchJournal { journal: JsonEventJournal::open(dir.log()).unwrap(), fail: AtomicBool::new(true) });
    let broker = ApprovalBroker::new(journal.clone());
    let gate = broker.gate(context("t1", "e1"));
    assert!(gate.request(&request("call1")).await.is_err());
    assert!(broker.pending(None).is_empty());
    journal.fail.store(false, Ordering::SeqCst);
    let gate = broker.gate(context("t2", "e2"));
    let mut subscription = journal.subscribe(0).unwrap();
    let waiter = tokio::spawn(async move { gate.request(&request("call2")).await });
    next(subscription.as_mut()).await;
    let pending = broker.pending(None).pop().unwrap();
    journal.fail.store(true, Ordering::SeqCst);
    assert!(broker.decide(&pending.approval_id, "t2", "e2", true).is_err());
    assert!(timeout(Duration::from_secs(2), waiter).await.unwrap().unwrap().is_err());
    assert!(broker.pending(None).is_empty());
}

#[tokio::test]
async fn cancellation_and_shutdown_release_waiters_and_bind_cancel_can_be_replaced() {
    let dir = TestDirectory::new();
    let journal = Arc::new(JsonEventJournal::open(dir.log()).unwrap());
    let broker = ApprovalBroker::new(journal.clone());
    let gate = broker.gate(context("t1", "e1"));
    let stale = CancelHandle::default();
    stale.cancel();
    gate.bind_cancel(stale);
    let fresh = CancelHandle::default();
    gate.bind_cancel(fresh.clone());
    let mut subscription = journal.subscribe(0).unwrap();
    let waiter = tokio::spawn(async move { gate.request(&request("call1")).await });
    next(subscription.as_mut()).await;
    let pending = broker.pending(None).pop().unwrap();
    fresh.cancel();
    assert!(broker.decide(&pending.approval_id, "t1", "e1", true).is_err());
    assert!(timeout(Duration::from_secs(2), waiter).await.unwrap().unwrap().is_err());
    assert!(broker.pending(None).is_empty());
    let second = broker.gate(context("t2", "e2"));
    let mut subscription = journal.subscribe(journal.latest().unwrap()).unwrap();
    let waiter = tokio::spawn(async move { second.request(&request("call2")).await });
    next(subscription.as_mut()).await;
    broker.shutdown();
    assert!(timeout(Duration::from_secs(2), waiter).await.unwrap().unwrap().is_err());
    assert!(broker.pending(None).is_empty());
    assert!(broker.gate(context("t3", "e3")).request(&request("call3")).await.is_err());
}

#[tokio::test]
async fn polling_a_cancel_handle_and_dropping_a_request_both_remove_pending_approvals() {
    let dir = TestDirectory::new();
    let journal = Arc::new(JsonEventJournal::open(dir.log()).unwrap());
    let broker = ApprovalBroker::new(journal.clone());
    let gate = broker.gate(context("t1", "e1"));
    let cancel = CancelHandle::default();
    gate.bind_cancel(cancel.clone());
    let mut subscription = journal.subscribe(0).unwrap();
    let waiter = tokio::spawn(async move { gate.request(&request("call1")).await });
    next(subscription.as_mut()).await;
    cancel.cancel();
    assert!(timeout(Duration::from_secs(2), waiter).await.unwrap().unwrap().is_err());
    assert!(broker.pending(None).is_empty());
    let gate = broker.gate(context("t2", "e2"));
    let mut subscription = journal.subscribe(journal.latest().unwrap()).unwrap();
    let waiter = tokio::spawn(async move { gate.request(&request("call2")).await });
    next(subscription.as_mut()).await;
    waiter.abort();
    assert!(waiter.await.unwrap_err().is_cancelled());
    assert!(broker.pending(None).is_empty());
}

//! 委派工具经真实 AgentRunner 执行，隔离端替身只控制文件生命周期与故障点。

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use qcode_harness::application::agent_roster::AgentRoster;
use qcode_harness::application::agent_run::AgentDependencies;
use qcode_harness::application::context_policy::DefaultContextPolicy;
use qcode_harness::application::delegation::{ChildAgentFactory, DelegationResult, SubtaskStatus};
use qcode_harness::application::error::ApplicationError;
use qcode_harness::application::isolation::{
    ChangeSet, IsolationWorkspace, WorkspaceIsolation, WorkspaceVerifier,
};
use qcode_harness::application::port::{
    ApprovalGate, ApprovalRequest, BoxFuture, CancelHandle, Decision, EvidenceSink, ModelGateway,
    ModelReply, ModelRequest, NoopHooks, Timestamp, ToolInvoker,
};
use qcode_harness::application::snapshot::{RunSnapshot, SessionStore, StoreError};
use qcode_harness::application::tool_registry::{ToolRegistry, ToolSource};
use qcode_harness::domain::agent::{AgentRole, AgentTurn};
use qcode_harness::domain::budget::Budget;
use qcode_harness::domain::context::ContextBudget;
use qcode_harness::domain::evidence::{AuditEvent, TokenUsage};
use qcode_harness::domain::ids::{CallId, SessionId};
use qcode_harness::domain::policy::{PolicyEngine, PolicyRule, PolicyVerdict};
use qcode_harness::domain::tool::{OutcomeStatus, ToolCall, ToolOutcome};
use qcode_harness::domain::workspace::WorkspaceRoot;
use qcode_harness::infrastructure::clock::SystemClock;
use qcode_harness::infrastructure::delegation::SubAgentToolSource;
use qcode_harness::infrastructure::tool::BuiltinToolSource;
use serde_json::{Value, json};
use tokio::sync::{Barrier, Notify};

struct TempRoot(PathBuf);

impl TempRoot {
    fn new(label: &str) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "qch-delegation-{}-{label}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

struct FakeIsolation {
    root: TempRoot,
    prepared: AtomicUsize,
    created: Mutex<Vec<IsolationWorkspace>>,
    removed: Mutex<Vec<PathBuf>>,
    merged: AtomicUsize,
    fail_prepare: AtomicBool,
    fail_create: AtomicBool,
    fail_capture: AtomicBool,
    fail_merge: AtomicBool,
    fail_remove: AtomicBool,
}

impl FakeIsolation {
    fn new() -> Self {
        let root = TempRoot::new("fake");
        fs::create_dir(root.0.join("parent")).unwrap();
        Self {
            root,
            prepared: AtomicUsize::new(0),
            created: Mutex::new(Vec::new()),
            removed: Mutex::new(Vec::new()),
            merged: AtomicUsize::new(0),
            fail_prepare: AtomicBool::new(false),
            fail_create: AtomicBool::new(false),
            fail_capture: AtomicBool::new(false),
            fail_merge: AtomicBool::new(false),
            fail_remove: AtomicBool::new(false),
        }
    }

    fn assert_cleaned(&self) {
        let created = self.created.lock().unwrap();
        let removed = self.removed.lock().unwrap();
        assert!(!created.is_empty(), "必须真正创建过子工作区");
        assert_eq!(created.len(), removed.len());
        assert!(created.iter().all(|workspace| !workspace.root.exists()));
    }
}

impl WorkspaceIsolation for FakeIsolation {
    fn prepare(&self) -> BoxFuture<'_, Result<String, String>> {
        Box::pin(async move {
            self.prepared.fetch_add(1, Ordering::SeqCst);
            if self.fail_prepare.load(Ordering::SeqCst) {
                Err("dirty parent".to_owned())
            } else {
                Ok("a".repeat(40))
            }
        })
    }

    fn create<'a>(&'a self, id: &'a str, base: &'a str) -> BoxFuture<'a, Result<IsolationWorkspace, String>> {
        Box::pin(async move {
            if self.fail_create.load(Ordering::SeqCst) {
                return Err("create failed".to_owned());
            }
            let workspace = IsolationWorkspace {
                id: id.to_owned(), root: self.root.0.join(id), base_revision: base.to_owned(),
            };
            fs::create_dir(&workspace.root).unwrap();
            fs::create_dir(workspace.root.join("src")).unwrap();
            self.created.lock().unwrap().push(workspace.clone());
            Ok(workspace)
        })
    }

    fn capture<'a>(&'a self, workspace: &'a IsolationWorkspace) -> BoxFuture<'a, Result<ChangeSet, String>> {
        Box::pin(async move {
            if self.fail_capture.load(Ordering::SeqCst) {
                return Err("capture failed".to_owned());
            }
            let mut files = BTreeMap::new();
            for entry in fs::read_dir(&workspace.root).unwrap() {
                let entry = entry.unwrap();
                if entry.file_name().to_string_lossy().starts_with("output-") {
                    files.insert(entry.file_name().to_string_lossy().into_owned(), fs::read(entry.path()).unwrap());
                }
            }
            Ok(ChangeSet { id: workspace.id.clone(), patch: serde_json::to_vec(&files).unwrap() })
        })
    }

    fn merge<'a>(
        &'a self,
        _base: &'a str,
        changes: &'a [ChangeSet],
        verifier: &'a dyn WorkspaceVerifier,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            self.merged.fetch_add(1, Ordering::SeqCst);
            if self.fail_merge.load(Ordering::SeqCst) {
                return Err("merge conflict".to_owned());
            }
            let integration = self.root.0.join("integration");
            fs::create_dir(&integration).unwrap();
            for change in changes {
                let files: BTreeMap<String, Vec<u8>> = serde_json::from_slice(&change.patch).unwrap();
                for (name, contents) in files {
                    fs::write(integration.join(name), contents).unwrap();
                }
            }
            let verdict = verifier.verify(&integration).await;
            if verdict.is_ok() {
                for entry in fs::read_dir(&integration).unwrap() {
                    let entry = entry.unwrap();
                    fs::copy(entry.path(), self.root.0.join("parent").join(entry.file_name())).unwrap();
                }
            }
            fs::remove_dir_all(&integration).unwrap();
            verdict
        })
    }

    fn remove<'a>(&'a self, workspace: &'a IsolationWorkspace) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            if self.fail_remove.load(Ordering::SeqCst) {
                return Err("remove failed".to_owned());
            }
            fs::remove_dir_all(&workspace.root).unwrap();
            self.removed.lock().unwrap().push(workspace.root.clone());
            Ok(())
        })
    }
}

#[derive(Default)]
struct Verifier {
    calls: Mutex<Vec<PathBuf>>,
    reject_children: bool,
    reject_integration: bool,
}

impl WorkspaceVerifier for Verifier {
    fn verify<'a>(&'a self, root: &'a Path) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            self.calls.lock().unwrap().push(root.to_path_buf());
            if (root.ends_with("integration") && self.reject_integration)
                || (!root.ends_with("integration") && self.reject_children)
            {
                Err("external validation failed".to_owned())
            } else {
                Ok(())
            }
        })
    }
}

#[derive(Default)]
struct Events(Mutex<Vec<AuditEvent>>);

impl EvidenceSink for Events {
    fn record(&self, _at: Timestamp, event: AuditEvent) {
        self.0.lock().unwrap().push(event);
    }
}

#[derive(Default)]
struct Store(Mutex<Option<RunSnapshot>>);

impl SessionStore for Store {
    fn save(&self, snapshot: &RunSnapshot) -> Result<(), StoreError> {
        *self.0.lock().unwrap() = Some(snapshot.clone());
        Ok(())
    }
    fn load(&self, id: &SessionId) -> Result<Option<RunSnapshot>, StoreError> {
        Ok(self.0.lock().unwrap().clone().filter(|snapshot| &snapshot.session_id == id))
    }
    fn list(&self) -> Result<Vec<SessionId>, StoreError> {
        Ok(self.0.lock().unwrap().iter().map(|snapshot| snapshot.session_id.clone()).collect())
    }
}

#[derive(Default)]
struct Gate {
    deny: bool,
    cancel: Mutex<Option<CancelHandle>>,
}

impl ApprovalGate for Gate {
    fn request<'a>(&'a self, _request: &'a ApprovalRequest) -> BoxFuture<'a, Decision> {
        Box::pin(async move { if self.deny { Err("child write denied".to_owned()) } else { Ok(()) } })
    }
    fn bind_cancel(&self, cancel: CancelHandle) {
        *self.cancel.lock().unwrap() = Some(cancel);
    }
}

#[derive(Default)]
struct Probe {
    active: AtomicUsize,
    peak: AtomicUsize,
    model_calls: AtomicUsize,
    first_arrived: Notify,
    barrier: Option<Barrier>,
}

struct Active<'a>(&'a Probe);

impl Drop for Active<'_> {
    fn drop(&mut self) { self.0.active.fetch_sub(1, Ordering::SeqCst); }
}

struct Model {
    ordinal: usize,
    first: AtomicBool,
    probe: Arc<Probe>,
    fail: bool,
    hang: bool,
}

impl ModelGateway for Model {
    fn complete<'a>(&'a self, request: ModelRequest<'a>) -> BoxFuture<'a, Result<ModelReply, ApplicationError>> {
        Box::pin(async move {
            assert!(request.cache_key.is_some());
            assert!(!request.tools.iter().any(|tool| tool.name == "delegate_subtasks"));
            self.probe.model_calls.fetch_add(1, Ordering::SeqCst);
            let first = self.first.swap(false, Ordering::SeqCst);
            let turn = if first {
                let active = self.probe.active.fetch_add(1, Ordering::SeqCst) + 1;
                self.probe.peak.fetch_max(active, Ordering::SeqCst);
                let _active = Active(&self.probe);
                self.probe.first_arrived.notify_one();
                if let Some(barrier) = &self.probe.barrier { barrier.wait().await; }
                if self.hang { std::future::pending::<()>().await; }
                if self.fail { return Err(ApplicationError::Model("scripted model failure".to_owned())); }
                AgentTurn {
                    text: "写入独立子工作区".to_owned(),
                    calls: vec![ToolCall {
                        id: CallId::new(format!("write-{}", self.ordinal)),
                        name: "write_file".to_owned(),
                        arguments: json!({
                            "path": format!("output-{}.txt", self.ordinal),
                            "content": format!("child {}\n", self.ordinal)
                        }),
                    }],
                }
            } else {
                AgentTurn { text: format!("child {} finished", self.ordinal), calls: Vec::new() }
            };
            Ok(ModelReply { turn, usage: TokenUsage::default() })
        })
    }
}

#[derive(Clone)]
struct ChildRecord {
    root: PathBuf,
    role: AgentRole,
    scope: PathBuf,
    session_id: SessionId,
    events: Arc<Events>,
    store: Arc<Store>,
    gate: Arc<Gate>,
}

#[derive(Default)]
struct Factory {
    probe: Arc<Probe>,
    children: Mutex<Vec<ChildRecord>>,
    fail_factory: Option<usize>,
    fail_model: Option<usize>,
    deny_write: bool,
    hang_model: bool,
}

impl ChildAgentFactory for Factory {
    fn dependencies(
        &self,
        workspace: &IsolationWorkspace,
        role: AgentRole,
        session_id: &SessionId,
        scope: &Path,
    ) -> Result<AgentDependencies, String> {
        let mut children = self.children.lock().unwrap();
        let ordinal = children.len() + 1;
        if self.fail_factory == Some(ordinal) {
            return Err("factory failed".to_owned());
        }
        let events = Arc::new(Events::default());
        let store = Arc::new(Store::default());
        let gate = Arc::new(Gate {
            deny: self.deny_write,
            cancel: Mutex::new(None),
        });
        children.push(ChildRecord {
            root: workspace.root.clone(),
            role,
            scope: scope.to_path_buf(),
            session_id: session_id.clone(),
            events: events.clone(),
            store: store.clone(),
            gate: gate.clone(),
        });
        Ok(AgentDependencies {
            model: Arc::new(Model {
                ordinal,
                first: AtomicBool::new(true),
                probe: self.probe.clone(),
                fail: self.fail_model == Some(ordinal),
                hang: self.hang_model,
            }),
            tools: ToolRegistry::from_sources(&[Arc::new(BuiltinToolSource::new(
                WorkspaceRoot::new(&workspace.root),
            ))]),
            approvals: gate,
            evidence: events,
            clock: Arc::new(SystemClock),
            policy: PolicyEngine::with_rules(vec![PolicyRule {
                tool: None,
                scope: None,
                verdict: PolicyVerdict::RequireApproval,
                description: "保留子审批".to_owned(),
            }]),
            additional_guards: Vec::new(),
            roster: AgentRoster::builtin().with_project_instructions(&format!(
                "child root={} scope={}",
                workspace.root.display(),
                scope.display()
            )),
            context_budget: ContextBudget::default(),
            context_policy: Arc::new(DefaultContextPolicy),
            session_store: Some(store),
            hooks: Arc::new(NoopHooks),
        })
    }
}

fn call(tasks: Value) -> ToolCall {
    ToolCall {
        id: CallId::new("delegate"),
        name: "delegate_subtasks".to_owned(),
        arguments: json!({"tasks": tasks}),
    }
}

fn tasks(count: usize) -> Value {
    Value::Array(
        (0..count)
            .map(|index| json!({"id": format!("task-{index}"), "goal": "write an independent file"}))
            .collect(),
    )
}

fn tool(
    isolation: Arc<dyn WorkspaceIsolation>,
    factory: Arc<dyn ChildAgentFactory>,
    verifier: Arc<dyn WorkspaceVerifier>,
    max: usize,
) -> Arc<dyn ToolInvoker> {
    SubAgentToolSource::new(
        isolation,
        factory,
        verifier,
        Budget::new(4).with_wall_clock(Duration::from_secs(10)),
        max,
    )
    .tools()
    .remove(0)
    .invoker
}

fn result(outcome: &ToolOutcome) -> DelegationResult {
    serde_json::from_str(&outcome.content)
        .unwrap_or_else(|error| panic!("必须返回整批结构化结果: {error}: {}", outcome.content))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_children_overlap_and_config_bounds_concurrency() {
    let isolation = Arc::new(FakeIsolation::new());
    let factory = Arc::new(Factory {
        probe: Arc::new(Probe {
            barrier: Some(Barrier::new(2)),
            ..Probe::default()
        }),
        ..Factory::default()
    });
    let verifier = Arc::new(Verifier::default());
    let invoker = tool(isolation.clone(), factory.clone(), verifier.clone(), 2);

    let outcome = tokio::time::timeout(Duration::from_secs(5), invoker.invoke(&call(tasks(4))))
        .await
        .expect("并发屏障必须由两个独立子运行同时抵达");

    assert_eq!(outcome.status, OutcomeStatus::Success, "{}", outcome.content);
    let batch = result(&outcome);
    assert!(batch.merged);
    assert_eq!(factory.probe.peak.load(Ordering::SeqCst), 2);
    assert_eq!(
        verifier.calls.lock().unwrap().len(),
        5,
        "每个子结果及最终组合都要外部验证"
    );
    assert_eq!(
        batch
            .tasks
            .iter()
            .map(|task| task.id.as_str())
            .collect::<Vec<_>>(),
        vec!["task-0", "task-1", "task-2", "task-3"]
    );
    assert!(batch.tasks.iter().all(|task| {
        task.status == SubtaskStatus::Completed && task.verified && task.workspace_removed
    }));
    assert_eq!(fs::read_dir(isolation.root.0.join("parent")).unwrap().count(), 4);
    isolation.assert_cleaned();
}

#[tokio::test]
async fn invalid_inputs_are_rejected_before_prepare_or_workspace_creation() {
    let isolation = Arc::new(FakeIsolation::new());
    let invoker = tool(isolation.clone(), Arc::new(Factory::default()), Arc::new(Verifier::default()), 2);
    let mut invalid = vec![
        json!({"tasks": []}),
        json!({"tasks": [{"id": "same", "goal": "one"}, {"id": "same", "goal": "two"}]}),
        json!({"tasks": [{"id": "../escape", "goal": "one"}]}),
        json!({"tasks": [{"id": "ok", "goal": "  "}]}),
        json!({"tasks": [{"id": "ok", "goal": "one", "role": "manager"}]}),
        json!({"tasks": [{"id": "ok", "goal": "one", "scope": "../escape"}]}),
        json!({"tasks": [{"id": "ok", "goal": "one", "scope": "/escape"}]}),
        json!({"tasks": [{"id": "ok", "goal": "one", "scope": ".git"}]}),
        json!({"tasks": [{"id": "ok", "goal": "one", "scope": ""}]}),
        json!({"tasks": tasks(17)}),
        json!({"tasks": tasks(1), "verify_command": "skip validation"}),
    ];
    for id in ["", ".", "-flag", "a/b", "a\\b", "CON", "nul", "white space"] {
        invalid.push(json!({"tasks": [{"id": id, "goal": "one"}]}));
    }
    for arguments in invalid {
        let mut request = call(Value::Null);
        request.arguments = arguments.clone();
        let outcome = invoker.invoke(&request).await;
        assert_eq!(outcome.status, OutcomeStatus::Refused, "{arguments}: {}", outcome.content);
    }
    assert_eq!(isolation.prepared.load(Ordering::SeqCst), 0);
    assert!(isolation.created.lock().unwrap().is_empty());
}

#[tokio::test]
async fn disabled_or_invalid_parallelism_never_prepares_a_workspace() {
    let isolation = Arc::new(FakeIsolation::new());
    for max in [0, 17, usize::MAX] {
        let invoker = tool(isolation.clone(), Arc::new(Factory::default()), Arc::new(Verifier::default()), max);
        assert_eq!(invoker.invoke(&call(tasks(1))).await.status, OutcomeStatus::Refused);
    }
    assert_eq!(isolation.prepared.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn child_sessions_roots_audits_stores_and_cancel_gates_are_independent() {
    let isolation = Arc::new(FakeIsolation::new());
    let factory = Arc::new(Factory::default());
    let invoker = tool(isolation.clone(), factory.clone(), Arc::new(Verifier::default()), 1);
    let outcome = invoker.invoke(&call(json!([
        {"id": "one", "goal": "one", "role": "coder", "scope": "src"},
        {"id": "two", "goal": "two"}
    ]))).await;
    assert_eq!(outcome.status, OutcomeStatus::Success, "{}", outcome.content);
    let batch = result(&outcome);
    let children = factory.children.lock().unwrap();
    assert_eq!(children.len(), 2);
    assert_ne!(children[0].root, children[1].root);
    assert_ne!(children[0].session_id, children[1].session_id);
    assert_eq!(children[0].scope, Path::new("src"));
    assert_eq!(children[1].scope, Path::new("."));
    for (task, child) in batch.tasks.iter().zip(children.iter()) {
        assert_eq!(task.evidence_id.as_ref(), Some(&child.session_id));
        assert_eq!(task.session_id, child.session_id);
        assert_eq!(child.role, AgentRole::Coder);
        let snapshot = child.store.0.lock().unwrap().clone().expect("子快照必须由真实 runner 落盘");
        assert_eq!(snapshot.session_id, child.session_id);
        assert!(snapshot.spec.system_prompt.contains(&child.root.display().to_string()));
        let events = child.events.0.lock().unwrap();
        assert!(events.iter().any(|event| matches!(
            event,
            AuditEvent::StepStarted {
                role: AgentRole::Coder,
                ..
            }
        )));
        assert!(events
            .iter()
            .any(|event| matches!(event, AuditEvent::Thought { .. })));
        assert!(events
            .iter()
            .any(|event| matches!(event, AuditEvent::ToolRequested { .. })));
        assert!(events.iter().any(|event| matches!(
            event,
            AuditEvent::PolicyEvaluated {
                verdict: PolicyVerdict::RequireApproval,
                ..
            }
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            AuditEvent::ApprovalDecided { allowed: true, .. }
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            AuditEvent::ToolObserved { outcome, .. } if outcome.status == OutcomeStatus::Success
        )));
    }
    children[0].gate.cancel.lock().unwrap().as_ref().unwrap().cancel();
    assert!(!children[1].gate.cancel.lock().unwrap().as_ref().unwrap().is_cancelled());
    isolation.assert_cleaned();
}

#[tokio::test]
async fn model_failure_settles_other_running_children_prevents_merge_and_cleans_all_roots() {
    let isolation = Arc::new(FakeIsolation::new());
    let factory = Arc::new(Factory {
        fail_model: Some(1),
        probe: Arc::new(Probe {
            barrier: Some(Barrier::new(2)),
            ..Probe::default()
        }),
        ..Factory::default()
    });
    let invoker = tool(isolation.clone(), factory.clone(), Arc::new(Verifier::default()), 2);
    let outcome = tokio::time::timeout(Duration::from_secs(5), invoker.invoke(&call(tasks(3))))
        .await
        .unwrap();
    assert_eq!(outcome.status, OutcomeStatus::Failed);
    let batch = result(&outcome);
    assert!(!batch.merged);
    assert_eq!(batch.tasks.len(), 3);
    assert!(batch.tasks[0].error.as_ref().unwrap().contains("scripted model failure"));
    assert_eq!(factory.probe.active.load(Ordering::SeqCst), 0);
    assert_eq!(isolation.merged.load(Ordering::SeqCst), 0);
    assert_eq!(fs::read_dir(isolation.root.0.join("parent")).unwrap().count(), 0);
    isolation.assert_cleaned();
}

#[tokio::test]
async fn child_verifier_failure_is_reported_without_any_merge() {
    let isolation = Arc::new(FakeIsolation::new());
    let verifier = Arc::new(Verifier { reject_children: true, ..Verifier::default() });
    let invoker = tool(isolation.clone(), Arc::new(Factory::default()), verifier, 2);
    let outcome = invoker.invoke(&call(tasks(2))).await;
    assert_eq!(outcome.status, OutcomeStatus::Failed);
    let batch = result(&outcome);
    assert!(!batch.merged);
    assert!(batch.tasks.iter().any(|task| task
        .error
        .as_deref()
        .is_some_and(|reason| reason.contains("external validation failed"))));
    assert_eq!(isolation.merged.load(Ordering::SeqCst), 0);
    isolation.assert_cleaned();
}

#[tokio::test]
async fn combined_verifier_failure_keeps_parent_files_unchanged() {
    let isolation = Arc::new(FakeIsolation::new());
    let verifier = Arc::new(Verifier { reject_integration: true, ..Verifier::default() });
    let invoker = tool(isolation.clone(), Arc::new(Factory::default()), verifier, 2);
    let outcome = invoker.invoke(&call(tasks(2))).await;
    assert_eq!(outcome.status, OutcomeStatus::Failed);
    assert!(!result(&outcome).merged);
    assert!(outcome.content.contains("external validation failed"));
    assert_eq!(fs::read_dir(isolation.root.0.join("parent")).unwrap().count(), 0);
    isolation.assert_cleaned();
}

#[tokio::test]
async fn factory_capture_and_merge_failures_clean_every_created_workspace() {
    for failure in ["factory", "capture", "merge"] {
        let isolation = Arc::new(FakeIsolation::new());
        isolation.fail_capture.store(failure == "capture", Ordering::SeqCst);
        isolation.fail_merge.store(failure == "merge", Ordering::SeqCst);
        let factory = Arc::new(Factory {
            fail_factory: (failure == "factory").then_some(2),
            ..Factory::default()
        });
        let invoker = tool(isolation.clone(), factory, Arc::new(Verifier::default()), 2);
        let outcome = invoker.invoke(&call(tasks(3))).await;
        assert_eq!(outcome.status, OutcomeStatus::Failed);
        let batch = result(&outcome);
        assert!(!batch.merged);
        assert_eq!(batch.tasks.len(), 3);
        let expected = format!(
            "{failure} {}",
            if failure == "merge" {
                "conflict"
            } else {
                "failed"
            }
        );
        assert!(outcome.content.contains(&expected));
        isolation.assert_cleaned();
    }
}

#[tokio::test]
async fn prepare_and_create_failures_return_batch_reasons_without_starting_models() {
    for prepare in [false, true] {
        let isolation = Arc::new(FakeIsolation::new());
        isolation.fail_prepare.store(prepare, Ordering::SeqCst);
        isolation.fail_create.store(!prepare, Ordering::SeqCst);
        let factory = Arc::new(Factory::default());
        let invoker = tool(isolation.clone(), factory.clone(), Arc::new(Verifier::default()), 2);
        let outcome = invoker.invoke(&call(tasks(2))).await;
        assert_eq!(outcome.status, OutcomeStatus::Failed);
        assert!(!result(&outcome).merged);
        assert!(outcome.content.contains(if prepare { "dirty parent" } else { "create failed" }));
        assert_eq!(factory.probe.model_calls.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn cleanup_failure_preserves_recovery_paths_and_does_not_claim_removal() {
    let isolation = Arc::new(FakeIsolation::new());
    isolation.fail_remove.store(true, Ordering::SeqCst);
    let invoker = tool(isolation.clone(), Arc::new(Factory::default()), Arc::new(Verifier::default()), 2);
    let outcome = invoker.invoke(&call(tasks(2))).await;
    assert_eq!(outcome.status, OutcomeStatus::Failed);
    let batch = result(&outcome);
    assert!(batch.merged, "合并已发生，清理错误不能改写这个事实");
    assert_eq!(batch.cleanup_failures.len(), 2);
    for failure in &batch.cleanup_failures {
        assert!(failure.path.exists());
    }
    assert!(batch.tasks.iter().all(|task| !task.workspace_removed));
}

#[tokio::test]
async fn denied_child_writes_do_not_run_and_keep_the_approval_audit() {
    let isolation = Arc::new(FakeIsolation::new());
    let factory = Arc::new(Factory { deny_write: true, ..Factory::default() });
    let invoker = tool(isolation.clone(), factory.clone(), Arc::new(Verifier::default()), 2);
    let outcome = invoker.invoke(&call(tasks(2))).await;
    assert_eq!(outcome.status, OutcomeStatus::Success, "{}", outcome.content);
    assert_eq!(fs::read_dir(isolation.root.0.join("parent")).unwrap().count(), 0);
    for child in factory.children.lock().unwrap().iter() {
        let events = child.events.0.lock().unwrap();
        assert!(events.iter().any(|event| matches!(
            event,
            AuditEvent::ApprovalDecided { allowed: false, .. }
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            AuditEvent::ToolObserved { outcome, .. } if outcome.status == OutcomeStatus::Refused
        )));
    }
    isolation.assert_cleaned();
}

#[tokio::test]
async fn all_stable_role_names_are_accepted_and_reviewer_cannot_write() {
    let isolation = Arc::new(FakeIsolation::new());
    let factory = Arc::new(Factory::default());
    let invoker = tool(isolation.clone(), factory.clone(), Arc::new(Verifier::default()), 4);
    let request = call(Value::Array(
        AgentRole::all()
            .iter()
            .map(|role| json!({"id": role.name(), "goal": "inspect or change", "role": role.name()}))
            .collect(),
    ));
    let outcome = invoker.invoke(&request).await;
    assert_eq!(outcome.status, OutcomeStatus::Success, "{}", outcome.content);
    for (child, role) in factory.children.lock().unwrap().iter().zip(AgentRole::all()) {
        assert_eq!(child.role, role);
        if matches!(role, AgentRole::Planner | AgentRole::Reviewer | AgentRole::Tester) {
            let events = child.events.0.lock().unwrap();
            assert!(events.iter().any(|event| matches!(
                event,
                AuditEvent::ToolObserved { outcome, .. }
                    if outcome.status == OutcomeStatus::Refused
            )));
        }
    }
    assert_eq!(fs::read_dir(isolation.root.0.join("parent")).unwrap().count(), 1);
}

#[tokio::test]
async fn repeated_calls_generate_distinct_worktree_and_session_ids() {
    let isolation = Arc::new(FakeIsolation::new());
    let factory = Arc::new(Factory::default());
    let invoker = tool(isolation.clone(), factory, Arc::new(Verifier::default()), 1);
    let first = result(&invoker.invoke(&call(tasks(1))).await);
    let second = result(&invoker.invoke(&call(tasks(1))).await);
    assert!(first.merged && second.merged);
    assert_ne!(first.invocation_id, second.invocation_id);
    assert_ne!(first.tasks[0].session_id, second.tasks[0].session_id);
    assert_ne!(first.tasks[0].workspace_path, second.tasks[0].workspace_path);
    isolation.assert_cleaned();
}

#[tokio::test]
async fn hanging_model_is_bounded_without_dropping_an_inflight_command() {
    let isolation = Arc::new(FakeIsolation::new());
    let factory = Arc::new(Factory { hang_model: true, ..Factory::default() });
    let source = SubAgentToolSource::new(
        isolation.clone(),
        factory,
        Arc::new(Verifier::default()),
        Budget::new(4).with_wall_clock(Duration::from_millis(30)),
        1,
    );
    let invoker = source.tools().remove(0).invoker;
    let outcome = tokio::time::timeout(Duration::from_secs(3), invoker.invoke(&call(tasks(1))))
        .await
        .expect("无响应模型必须由内部时限收口");
    assert_eq!(outcome.status, OutcomeStatus::Failed);
    assert!(!result(&outcome).merged);
    isolation.assert_cleaned();
}

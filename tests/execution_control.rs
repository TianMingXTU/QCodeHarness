//! 常驻执行控制的行为约束：输入只在完整轮次间应用，取消不丢弃在途工具。

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use qcode_harness::application::agent_roster::AgentRoster;
use qcode_harness::application::agent_run::{AgentDependencies, AgentRunner, RunState};
use qcode_harness::application::context_policy::DefaultContextPolicy;
use qcode_harness::application::control::{ExecutionControl, UserInput};
use qcode_harness::application::error::ApplicationError;
use qcode_harness::application::memory_evidence::MemoryEvidenceSink;
use qcode_harness::application::orchestrator::{EvidenceFactory, Orchestrator};
use qcode_harness::application::port::{
    BoxFuture, CancelHandle, ModelGateway, ModelReply, ModelRequest, NoopHooks, ToolInvoker,
};
use qcode_harness::application::snapshot::{
    RUN_FORMAT, RunSnapshot, SessionStore, StoreError, TaskSnapshot, TaskStore,
};
use qcode_harness::application::task_resume;
use qcode_harness::application::tool_registry::{ToolEntry, ToolRegistry};
use qcode_harness::domain::agent::{AgentRole, AgentSpec, AgentTurn};
use qcode_harness::domain::budget::Budget;
use qcode_harness::domain::evidence::TokenUsage;
use qcode_harness::domain::ids::{CallId, SessionId, TaskId};
use qcode_harness::domain::policy::PolicyEngine;
use qcode_harness::domain::session::{ContextSlice, Turn};
use qcode_harness::domain::task::{TaskKind, TaskState};
use qcode_harness::domain::tool::{ToolAnnotations, ToolCall, ToolOutcome, ToolSpec};
use qcode_harness::infrastructure::approval::auto::AutoApproveGate;
use qcode_harness::infrastructure::clock::SystemClock;
use serde_json::json;
use tokio::sync::Notify;

#[derive(Default)]
struct Sessions(Mutex<Option<RunSnapshot>>);

impl SessionStore for Sessions {
    fn save(&self, snapshot: &RunSnapshot) -> Result<(), StoreError> {
        *self.0.lock().unwrap() = Some(snapshot.clone());
        Ok(())
    }

    fn load(&self, _: &SessionId) -> Result<Option<RunSnapshot>, StoreError> {
        Ok(self.0.lock().unwrap().clone())
    }

    fn list(&self) -> Result<Vec<SessionId>, StoreError> {
        Ok(self.0.lock().unwrap().iter().map(|run| run.session_id.clone()).collect())
    }
}

#[derive(Default)]
struct Tasks(Mutex<Option<TaskSnapshot>>);

impl TaskStore for Tasks {
    fn save(&self, snapshot: &TaskSnapshot) -> Result<(), StoreError> {
        *self.0.lock().unwrap() = Some(snapshot.clone());
        Ok(())
    }

    fn load(&self, _: &TaskId) -> Result<Option<TaskSnapshot>, StoreError> {
        Ok(self.0.lock().unwrap().clone())
    }

    fn list(&self) -> Result<Vec<TaskId>, StoreError> {
        Ok(self.0.lock().unwrap().iter().map(|task| task.task_id().clone()).collect())
    }
}

#[derive(Default)]
struct Control {
    inputs: Mutex<Vec<UserInput>>,
    applied: Mutex<Vec<String>>,
    broken: AtomicBool,
    sessions: Option<Arc<Sessions>>,
}

impl Control {
    fn push(&self, id: &str, text: &str) {
        self.inputs.lock().unwrap().push(UserInput { id: id.to_owned(), text: text.to_owned() });
    }
}

impl ExecutionControl for Control {
    fn take_inputs(&self) -> Vec<UserInput> {
        std::mem::take(&mut *self.inputs.lock().unwrap())
    }

    fn input_applied(&self, input: &UserInput) -> Result<(), String> {
        if let Some(sessions) = &self.sessions {
            let saved = sessions.0.lock().unwrap();
            assert!(saved.as_ref().unwrap().session.turns.iter().any(|turn| {
                matches!(turn, Turn::Input { id, text } if id == &input.id && text == &input.text)
            }), "通知已应用前必须保存输入");
        }
        self.applied.lock().unwrap().push(input.id.clone());
        Ok(())
    }

    fn check(&self) -> Result<(), String> {
        if self.broken.load(Ordering::SeqCst) { Err("审计不可用".to_owned()) } else { Ok(()) }
    }
}

struct Model {
    turns: Mutex<VecDeque<AgentTurn>>,
    calls: AtomicUsize,
    histories: Mutex<Vec<Vec<String>>>,
    after_reply: Option<Box<dyn Fn(usize) + Send + Sync>>,
}

impl Model {
    fn new(turns: Vec<AgentTurn>) -> Self {
        Self { turns: Mutex::new(turns.into()), calls: AtomicUsize::new(0), histories: Mutex::new(Vec::new()), after_reply: None }
    }
}

impl ModelGateway for Model {
    fn complete<'a>(&'a self, request: ModelRequest<'a>) -> BoxFuture<'a, Result<ModelReply, ApplicationError>> {
        Box::pin(async move {
            self.histories.lock().unwrap().push(request.history.iter().map(|slice| match slice {
                ContextSlice::User(text) => text.to_string(),
                ContextSlice::Round { thought, .. } => thought.to_string(),
            }).collect());
            let index = self.calls.fetch_add(1, Ordering::SeqCst);
            let turn = self.turns.lock().unwrap().pop_front().expect("模型脚本未耗尽");
            if let Some(after_reply) = &self.after_reply { after_reply(index); }
            Ok(ModelReply { turn, usage: TokenUsage::default() })
        })
    }
}

fn answer(text: &str) -> AgentTurn {
    AgentTurn { text: text.to_owned(), calls: Vec::new() }
}

fn tool_turn() -> AgentTurn {
    AgentTurn { text: "读取两个文件".to_owned(), calls: ["a", "b"].into_iter().map(|path| ToolCall {
        id: CallId::new(path), name: "read_file".to_owned(), arguments: json!({"path": path}),
    }).collect() }
}

struct Tools {
    calls: AtomicUsize,
    on_call: Option<Box<dyn Fn(usize) + Send + Sync>>,
    entered: Option<Arc<Notify>>,
    release: Option<Arc<Notify>>,
}

impl Default for Tools {
    fn default() -> Self {
        Self { calls: AtomicUsize::new(0), on_call: None, entered: None, release: None }
    }
}

impl ToolInvoker for Tools {
    fn invoke<'a>(&'a self, _: &'a ToolCall) -> BoxFuture<'a, ToolOutcome> {
        Box::pin(async move {
            let index = self.calls.fetch_add(1, Ordering::SeqCst);
            if let Some(callback) = &self.on_call { callback(index); }
            if let Some(entered) = &self.entered { entered.notify_one(); }
            if let Some(release) = &self.release { release.notified().await; }
            ToolOutcome::success("文件内容")
        })
    }
}

fn deps(model: Arc<Model>, tools: Arc<Tools>, store: Option<Arc<dyn SessionStore>>) -> AgentDependencies {
    AgentDependencies {
        model,
        tools: ToolRegistry::new(vec![ToolEntry { spec: ToolSpec {
            name: "read_file".to_owned(), description: "读文件".to_owned(), parameters: json!({"type": "object"}),
            annotations: ToolAnnotations::new(true, false, true, false),
        }, invoker: tools }]),
        approvals: Arc::new(AutoApproveGate), evidence: Arc::new(MemoryEvidenceSink::new()),
        clock: Arc::new(SystemClock), policy: PolicyEngine::new(), roster: AgentRoster::builtin(),
        context_budget: Default::default(), context_policy: Arc::new(DefaultContextPolicy),
        session_store: store, hooks: Arc::new(NoopHooks), additional_guards: Vec::new(),
    }
}

fn runner(deps: AgentDependencies) -> AgentRunner {
    AgentRunner::new(AgentSpec::new(AgentRole::Coder, "控制测试"), SessionId::new("controlled"), "初始任务", deps)
}

fn factory() -> EvidenceFactory {
    Arc::new(|_| Ok(Arc::new(MemoryEvidenceSink::new())))
}

#[tokio::test]
async fn tool_round_is_paired_before_a_persisted_input_is_acknowledged() {
    let sessions = Arc::new(Sessions::default());
    let control = Arc::new(Control { sessions: Some(sessions.clone()), ..Default::default() });
    let queue = control.clone();
    let tools = Arc::new(Tools { on_call: Some(Box::new(move |index| {
        if index == 0 { queue.push("i1", "补充约束"); }
    })), ..Default::default() });
    let model = Arc::new(Model::new(vec![tool_turn(), answer("完成")]));
    let mut run = runner(deps(model.clone(), tools.clone(), Some(sessions)))
        .with_execution_control(control.clone(), CancelHandle::default());
    run.run(&Budget::new(10)).await.unwrap();
    let turns = &run.snapshot().session.turns;
    assert!(matches!(turns[1], Turn::Agent(_)));
    assert!(matches!(&turns[2], Turn::Observation(results) if results.len() == 2));
    assert!(matches!(&turns[3], Turn::Input { id, .. } if id == "i1"));
    assert_eq!(*control.applied.lock().unwrap(), vec!["i1"]);
    assert_eq!(tools.calls.load(Ordering::SeqCst), 2);
    assert_eq!(run.session().started_steps(), 1);
    assert!(model.histories.lock().unwrap()[1].contains(&"补充约束".to_owned()));
}

#[tokio::test]
async fn failed_input_checkpoint_is_not_acknowledged_or_sent_to_the_model() {
    struct FailingStore;
    impl SessionStore for FailingStore {
        fn save(&self, _: &RunSnapshot) -> Result<(), StoreError> {
            Err(StoreError::Io(std::io::Error::other("磁盘不可写")))
        }
        fn load(&self, _: &SessionId) -> Result<Option<RunSnapshot>, StoreError> { Ok(None) }
        fn list(&self) -> Result<Vec<SessionId>, StoreError> { Ok(Vec::new()) }
    }
    let model = Arc::new(Model::new(vec![answer("不应调用")]));
    let control = Arc::new(Control::default());
    control.push("durable-1", "先保存这条输入");
    let mut runner = runner(deps(model.clone(), Arc::new(Tools::default()), Some(Arc::new(FailingStore))))
        .with_execution_control(control.clone(), CancelHandle::default());
    assert!(matches!(runner.run(&Budget::new(10)).await, Err(ApplicationError::Snapshot(_))));
    assert!(control.applied.lock().unwrap().is_empty());
    assert_eq!(model.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn cancellation_after_model_reply_pairs_and_refuses_every_unstarted_tool() {
    let cancel = CancelHandle::default();
    let trigger = cancel.clone();
    let mut model = Model::new(vec![tool_turn()]);
    model.after_reply = Some(Box::new(move |_| trigger.cancel()));
    let tools = Arc::new(Tools::default());
    let mut run = runner(deps(Arc::new(model), tools.clone(), None))
        .with_execution_control(Arc::new(Control::default()), cancel);
    let error = run.run(&Budget::new(10)).await.unwrap_err();
    assert!(matches!(error, ApplicationError::Cancelled));
    assert_eq!(tools.calls.load(Ordering::SeqCst), 0);
    assert!(run.snapshot().pending.is_none());
    let snapshot = run.snapshot();
    let Turn::Observation(outcomes) = &snapshot.session.turns[2] else { panic!("缺少取消观察") };
    assert_eq!(outcomes.len(), 2);
    assert!(outcomes.iter().all(|outcome| outcome.content.contains("取消")));
    assert_eq!(snapshot.state, RunState::AwaitingModel);
}

#[tokio::test]
async fn cancellation_waits_for_the_started_tool_and_refuses_the_next_one() {
    let cancel = CancelHandle::default();
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let tools = Arc::new(Tools { entered: Some(entered.clone()), release: Some(release.clone()), ..Default::default() });
    let mut run = runner(deps(Arc::new(Model::new(vec![tool_turn()])), tools.clone(), None))
        .with_execution_control(Arc::new(Control::default()), cancel.clone());
    let task = tokio::spawn(async move {
        let result = run.run(&Budget::new(10)).await;
        (run.snapshot(), result)
    });
    entered.notified().await;
    cancel.cancel();
    assert!(!task.is_finished(), "执行中的工具必须等待结果");
    release.notify_one();
    let (snapshot, result) = task.await.unwrap();
    assert!(matches!(result, Err(ApplicationError::Cancelled)));
    assert_eq!(tools.calls.load(Ordering::SeqCst), 1);
    let Turn::Observation(outcomes) = &snapshot.session.turns[2] else { panic!("缺少观察") };
    assert_eq!(outcomes[0].content, "文件内容");
    assert!(outcomes[1].content.contains("取消"));
}

#[tokio::test]
async fn failed_execution_control_stops_before_the_next_tool() {
    let control = Arc::new(Control::default());
    let broken = control.clone();
    let tools = Arc::new(Tools { on_call: Some(Box::new(move |_| {
        broken.broken.store(true, Ordering::SeqCst);
    })), ..Default::default() });
    let mut run = runner(deps(Arc::new(Model::new(vec![tool_turn()])), tools.clone(), None))
        .with_execution_control(control, CancelHandle::default());
    let error = run.run(&Budget::new(10)).await.unwrap_err();
    assert!(matches!(error.root_cause(), ApplicationError::ExecutionControl { .. }));
    assert_eq!(tools.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn template_has_a_checkpoint_before_its_first_model_call_and_preserves_inputs_on_resume() {
    let sessions = Arc::new(Sessions::default());
    let tasks = Arc::new(Tasks::default());
    let control = Arc::new(Control { sessions: Some(sessions.clone()), ..Default::default() });
    control.push("i1", "必须覆盖错误路径");
    let saved_tasks = tasks.clone();
    let mut model = Model::new(vec![answer("第一步完成")]);
    model.after_reply = Some(Box::new(move |_| {
        let snapshot = saved_tasks.0.lock().unwrap().clone().expect("首次调用模型前应有任务快照");
        assert_eq!(snapshot.task.current_index(), 0);
        assert_eq!(snapshot.run.session_id.as_str(), "preassigned");
    }));
    let mut orchestrator = Orchestrator::new(deps(Arc::new(model), Arc::new(Tools::default()), Some(sessions.clone())), Budget::new(1), factory())
        .with_task_store(tasks.clone()).with_session_id(SessionId::new("preassigned"))
        .with_execution_control(control.clone(), CancelHandle::default());
    let mut task = orchestrator.create_task(TaskId::new("task-input"), TaskKind::BugFix, "修复解析");
    let error = orchestrator.run(&mut task).await.unwrap_err();
    assert!(matches!(error.root_cause(), ApplicationError::BudgetExhausted { .. }));
    let snapshot = tasks.0.lock().unwrap().clone().unwrap();
    task_resume::check_resumable(&snapshot, None).unwrap();
    assert_eq!(snapshot.run.session.started_steps(), 2);
    let model = Arc::new(Model::new(vec![answer("第二步"), answer("第三步"), answer("第四步")]));
    let mut restored = Orchestrator::new(deps(model.clone(), Arc::new(Tools::default()), Some(sessions)), Budget::new(10), factory())
        .with_task_store(tasks).with_execution_control(control, CancelHandle::default());
    assert_eq!(restored.resume(snapshot).await.unwrap().steps_completed, 3);
    assert_eq!(model.calls.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn completed_task_continues_without_replaying_steps_or_duplicating_answers() {
    let sessions = Arc::new(Sessions::default());
    let tasks = Arc::new(Tasks::default());
    let model = Arc::new(Model::new(["一", "二", "三", "原答案"].into_iter().map(answer).collect()));
    let mut original = Orchestrator::new(deps(model, Arc::new(Tools::default()), Some(sessions.clone())), Budget::new(10), factory())
        .with_task_store(tasks.clone());
    let mut task = original.create_task(TaskId::new("conversation"), TaskKind::BugFix, "修复问题");
    original.run(&mut task).await.unwrap();
    for (id, text, reply) in [("next-1", "再解释一次", "新答案"), ("next-2", "给出示例", "末次答案")] {
        let snapshot = tasks.0.lock().unwrap().clone().unwrap();
        let control = Arc::new(Control { sessions: Some(sessions.clone()), ..Default::default() });
        control.push(id, text);
        let model = Arc::new(Model::new(vec![answer(reply)]));
        let mut continued = Orchestrator::new(deps(model.clone(), Arc::new(Tools::default()), Some(sessions.clone())), Budget::new(20), factory())
            .with_task_store(tasks.clone()).with_execution_control(control, CancelHandle::default());
        let outcome = continued.continue_conversation(snapshot).await.unwrap();
        assert_eq!(outcome.answer, reply);
        assert_eq!(model.calls.load(Ordering::SeqCst), 1);
        let checkpoint = tasks.0.lock().unwrap().clone().unwrap();
        assert_eq!(checkpoint.task.state(), TaskState::Completed);
        assert_eq!(checkpoint.run.session.started_steps(), 4);
        task_resume::check_resumable(&checkpoint, None).unwrap();
    }
    let snapshot = tasks.0.lock().unwrap().clone().unwrap();
    for text in ["原答案", "新答案"] {
        assert_eq!(snapshot.run.session.turns.iter().filter(|turn| {
            matches!(turn, Turn::Agent(agent) if agent.text == text)
        }).count(), 1, "历史答案应恰好保留一次");
    }
    assert_eq!(snapshot.run.consumption.turns, 6);
}

#[tokio::test]
async fn continuing_a_completed_task_resumes_its_interrupted_tool_round() {
    let sessions = Arc::new(Sessions::default());
    let tasks = Arc::new(Tasks::default());
    let mut original = Orchestrator::new(deps(Arc::new(Model::new(
        ["一", "二", "三", "原答案"].into_iter().map(answer).collect(),
    )), Arc::new(Tools::default()), Some(sessions.clone())), Budget::new(10), factory())
        .with_task_store(tasks.clone());
    let mut task = original.create_task(TaskId::new("continued-round"), TaskKind::BugFix, "修复问题");
    original.run(&mut task).await.unwrap();
    let mut snapshot = tasks.0.lock().unwrap().clone().unwrap();
    let tools = Arc::new(Tools::default());
    let model = Arc::new(Model::new(vec![tool_turn(), answer("补充结论")]));
    let mut interrupted = AgentRunner::resume(snapshot.run.clone(), deps(model.clone(), tools.clone(), Some(sessions.clone()))).unwrap();
    interrupted.start_conversation().unwrap();
    assert!(matches!(interrupted.step().await.unwrap(), RunState::Evaluating(_)));
    snapshot.run = interrupted.snapshot();
    let mut continued = Orchestrator::new(deps(model, tools.clone(), Some(sessions)), Budget::new(20), factory())
        .with_task_store(tasks);
    let outcome = continued.continue_conversation(snapshot).await.unwrap();
    assert_eq!(outcome.answer, "补充结论");
    assert_eq!(tools.calls.load(Ordering::SeqCst), 2, "任务已完成也不能丢掉补充对话的在途工具轮次");
}

#[tokio::test]
async fn version_three_snapshots_are_explicitly_rejected() {
    let mut snapshot = runner(deps(Arc::new(Model::new(vec![])), Arc::new(Tools::default()), None)).snapshot();
    snapshot.format_version = 3;
    let error = AgentRunner::resume(snapshot, deps(Arc::new(Model::new(vec![])), Arc::new(Tools::default()), None)).err().expect("v3 应拒绝");
    assert!(matches!(error, ApplicationError::Snapshot(StoreError::VersionMismatch { found: 3, expected: RUN_FORMAT })));
}

#[test]
fn a_fork_keeps_progress_and_inputs_but_uses_a_new_task_id() {
    let orchestrator = Orchestrator::new(deps(Arc::new(Model::new(vec![])), Arc::new(Tools::default()), None), Budget::new(10), factory());
    let mut task = orchestrator.create_task(TaskId::new("parent"), TaskKind::BugFix, "原目标");
    task.start().unwrap();
    task.advance().unwrap();
    let fork = task.fork_with_id(TaskId::new("fork"));
    assert_eq!(fork.id().as_str(), "fork");
    assert_eq!(fork.progress(), task.progress());
    assert_eq!(fork.steps(), task.steps());
    assert_eq!(task.id().as_str(), "parent");
}

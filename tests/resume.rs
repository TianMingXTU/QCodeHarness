//! 会话恢复的集成验收。
//!
//! 不走网络：模型用 `MockCompletionModel` 脚本化。重点不是"快照能序列化"，
//! 而是恢复后的行为语义——半轮不重跑已执行的调用、审批请求重新递给闸门、
//! 取消不跨恢复传递、保存失败不拖垮任务。

use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use qcode_harness::application::agent_run::{
    AgentDependencies, AgentRunner, PendingRound, RunState,
};
use qcode_harness::application::agent_roster::AgentRoster;
use qcode_harness::application::context_policy::DefaultContextPolicy;
use qcode_harness::application::error::ApplicationError;
use qcode_harness::application::port::{
    ApprovalGate, ApprovalRequest, BoxFuture, Decision, NoopHooks, ToolInvoker,
};
use qcode_harness::application::snapshot::{RunSnapshot, RUN_FORMAT, SessionStore, StoreError};
use qcode_harness::application::tool_registry::{ToolEntry, ToolRegistry};
use qcode_harness::domain::agent::{AgentRole, AgentSpec, AgentTurn};
use qcode_harness::domain::budget::{Budget, Consumption};
use qcode_harness::domain::ids::{CallId, SessionId};
use qcode_harness::domain::policy::{PolicyEngine, PolicyRule, PolicyVerdict};
use qcode_harness::domain::session::{Session, Turn};
use qcode_harness::domain::tool::{ToolAnnotations, ToolCall, ToolOutcome, ToolSpec};
use qcode_harness::domain::workspace::WorkspaceRoot;
use qcode_harness::infrastructure::approval::auto::AutoApproveGate;
use qcode_harness::infrastructure::clock::SystemClock;
use qcode_harness::infrastructure::evidence::memory::MemoryEvidenceSink;
use qcode_harness::infrastructure::model::rig_chat::RigChatGateway;
use qcode_harness::infrastructure::session_store::JsonSessionStore;
use qcode_harness::infrastructure::tool::all_tools;

use rig::test_utils::{MockCompletionModel, MockTurn};
use serde_json::json;

fn spec() -> AgentSpec {
    AgentSpec {
        role: AgentRole::Coder,
        system_prompt: "恢复测试".to_owned(),
        model: None,
        effort: None,
        verbosity: None,
    }
}

fn read_file_spec() -> ToolSpec {
    ToolSpec {
        name: "read_file".to_owned(),
        description: "读文件".to_owned(),
        parameters: json!({"type": "object"}),
        annotations: ToolAnnotations::new(true, false, true, false),
    }
}

fn call(id: &str, path: &str) -> ToolCall {
    ToolCall {
        id: CallId::new(id),
        name: "read_file".to_owned(),
        arguments: json!({"path": path}),
    }
}

fn registry() -> ToolRegistry {
    ToolRegistry::new(vec![ToolEntry {
        spec: read_file_spec(),
        invoker: Arc::new(RecordingInvoker::default()),
    }])
}

fn deps_with(
    mock: &MockCompletionModel,
    tools: ToolRegistry,
    approvals: Arc<dyn ApprovalGate>,
    policy: PolicyEngine,
    session_store: Option<Arc<dyn SessionStore>>,
) -> AgentDependencies {
    AgentDependencies {
        model: Arc::new(RigChatGateway::new(mock.clone().erase())),
        tools,
        approvals,
        evidence: Arc::new(MemoryEvidenceSink::new()),
        clock: Arc::new(SystemClock),
        policy,
        roster: AgentRoster::builtin(),
        context_budget: Default::default(),
        session_store,
        hooks: Arc::new(NoopHooks),
        context_policy: Arc::new(DefaultContextPolicy),
        additional_guards: Vec::new(),
    }
}

/// 强制 read_file 也要审批的策略：把运行停在闸门前用。
fn approval_policy() -> PolicyEngine {
    PolicyEngine::with_rules(vec![PolicyRule {
        tool: Some("read_file".to_owned()),
        scope: None,
        verdict: PolicyVerdict::RequireApproval,
        description: "测试：read_file 一律要审批".to_owned(),
    }])
}

/// 自动放行并记录被询问次数的闸门。
#[derive(Default)]
struct CountingGate {
    asked: Mutex<usize>,
}

impl ApprovalGate for CountingGate {
    fn request<'a>(&'a self, _request: &'a ApprovalRequest) -> BoxFuture<'a, Decision> {
        *self.asked.lock().unwrap() += 1;
        Box::pin(async { Ok(()) })
    }
}

/// 记录"真的执行了哪些调用"的假执行器：同名调用靠参数区分。
#[derive(Default)]
struct RecordingInvoker {
    executed: Mutex<Vec<String>>,
}

impl ToolInvoker for RecordingInvoker {
    fn invoke<'a>(&'a self, call: &'a ToolCall) -> BoxFuture<'a, ToolOutcome> {
        self.executed.lock().unwrap().push(call.arguments.to_string());
        Box::pin(async { ToolOutcome::success("文件内容") })
    }
}

/// save 永远失败的存储：模拟磁盘满、目录不可写。
struct FailingStore;

impl SessionStore for FailingStore {
    fn save(&self, _snapshot: &RunSnapshot) -> Result<(), StoreError> {
        Err(StoreError::Io(std::io::Error::other("磁盘已满（模拟）")))
    }

    fn load(&self, _session_id: &SessionId) -> Result<Option<RunSnapshot>, StoreError> {
        Ok(None)
    }

    fn list(&self) -> Result<Vec<SessionId>, StoreError> {
        Ok(Vec::new())
    }
}

/// 每次测试用独立的临时目录，避免并行测试互相踩。
fn temp_workspace(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("qch-resume-{}-{}", name, std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    dir
}

/// 一份合法的"停在正常点"的最小快照，供损坏文件测试打底。
fn minimal_snapshot(id: &str) -> RunSnapshot {
    RunSnapshot {
        format_version: RUN_FORMAT,
        session_id: SessionId::new(id),
        spec: spec(),
        session: Session::new(SessionId::new(id), "最小快照").snapshot(),
        state: RunState::AwaitingModel,
        consumption: Consumption::default(),
        pending: None,
    }
}

#[tokio::test]
async fn budget_stop_then_resume_finishes_the_remaining_rounds() {
    let mock1 = MockCompletionModel::from_turns(vec![
        MockTurn::tool_call("call_1", "read_file", json!({"path": "a"})),
        MockTurn::tool_call("call_2", "read_file", json!({"path": "b"})),
    ]);
    let mut runner = AgentRunner::new(
        spec(),
        SessionId::new("resume-budget"),
        "读完两个文件再总结",
        deps_with(&mock1, registry(), Arc::new(AutoApproveGate), PolicyEngine::new(), None),
    );

    // 只给 2 轮预算：两轮工具调用跑完，第 3 次问模型之前被预算拦下。
    let error = runner.run(&Budget::new(2)).await.unwrap_err();
    assert!(matches!(error, ApplicationError::BudgetExhausted { max_turns: 2 }), "{error}");
    assert_eq!(runner.session().rounds(), 2);
    let snapshot = runner.snapshot();

    // 换一个接续脚本的模型：第 3、4 轮 + 最终答案。
    let mock2 = MockCompletionModel::from_turns(vec![
        MockTurn::tool_call("call_3", "read_file", json!({"path": "c"})),
        MockTurn::tool_call("call_4", "read_file", json!({"path": "d"})),
        MockTurn::text("四个文件都读完了"),
    ]);
    let mut runner2 = AgentRunner::resume(
        snapshot,
        deps_with(&mock2, registry(), Arc::new(AutoApproveGate), PolicyEngine::new(), None),
    )
    .unwrap();
    let outcome = runner2.run(&Budget::new(10)).await.unwrap();

    assert_eq!(outcome.answer, "四个文件都读完了");
    assert_eq!(outcome.turns_used, 5, "恢复前的 2 轮要计入总量");
    assert_eq!(runner2.session().rounds(), 4);
}

#[tokio::test]
async fn awaiting_approval_snapshot_reasks_the_gate_on_resume() {
    let mock1 = MockCompletionModel::from_turns(vec![
        MockTurn::tool_call("call_1", "read_file", json!({"path": "a"})),
    ]);
    let mut runner = AgentRunner::new(
        spec(),
        SessionId::new("resume-gate"),
        "要审批的任务",
        deps_with(&mock1, registry(), Arc::new(AutoApproveGate), approval_policy(), None),
    );

    // 手动推进：第 1 步给出调用（Evaluating），第 2 步停在闸门前。
    let state = runner.step().await.unwrap();
    assert!(matches!(state, RunState::Evaluating(_)), "{state:?}");
    let state = runner.step().await.unwrap();
    let RunState::AwaitingApproval(request) = &state else {
        panic!("应停在审批前，实际 {state:?}")
    };
    assert_eq!(request.call.id.as_str(), "call_1");
    let snapshot = runner.snapshot();
    assert_eq!(snapshot.state, state, "快照要原样记住停在哪");

    // 恢复：闸门换成一个会计数的——验证恢复后同一个请求被重新递给闸门。
    let gate = Arc::new(CountingGate::default());
    let mock2 = MockCompletionModel::from_turns(vec![MockTurn::text("审批后完成")]);
    let mut runner2 = AgentRunner::resume(
        snapshot,
        deps_with(&mock2, registry(), gate.clone(), approval_policy(), None),
    )
    .unwrap();
    let outcome = runner2.run(&Budget::new(5)).await.unwrap();

    assert_eq!(outcome.answer, "审批后完成");
    assert_eq!(*gate.asked.lock().unwrap(), 1, "恢复后应把同一请求重新递给闸门恰好一次");
    assert_eq!(runner2.session().rounds(), 1);
    // 闸门放行后模型才被再次询问，且只问了这一次（终答案）。
    assert_eq!(mock2.requests().len(), 1);
}

#[tokio::test]
async fn half_dispatched_snapshot_does_not_rerun_the_call_that_was_in_flight() {
    // 手工构造"第二条调用执行前"的快照：两条调用，第一条已有观察。
    let call_1 = call("call_1", "a");
    let call_2 = call("call_2", "b");
    let snapshot = RunSnapshot {
        format_version: RUN_FORMAT,
        session_id: SessionId::new("resume-half"),
        spec: spec(),
        session: Session::new(SessionId::new("resume-half"), "半轮恢复").snapshot(),
        state: RunState::Dispatching(vec![call_1.clone(), call_2.clone()]),
        consumption: Consumption {
            turns: 1,
            elapsed: Duration::from_millis(120),
        },
        pending: Some(PendingRound {
            turn: AgentTurn {
                text: "先读 a 再读 b".to_owned(),
                calls: vec![call_1, call_2],
            },
            outcomes: vec![ToolOutcome::success("a 的内容")],
        }),
    };

    let invoker = Arc::new(RecordingInvoker::default());
    let registry = ToolRegistry::new(vec![ToolEntry {
        spec: read_file_spec(),
        invoker: invoker.clone(),
    }]);
    let mock = MockCompletionModel::from_turns(vec![MockTurn::text("半轮续跑完成")]);
    let mut runner = AgentRunner::resume(
        snapshot,
        deps_with(&mock, registry, Arc::new(AutoApproveGate), PolicyEngine::new(), None),
    )
    .unwrap();
    let outcome = runner.run(&Budget::new(5)).await.unwrap();

    assert_eq!(outcome.answer, "半轮续跑完成");
    // 两条都不重跑：第一条已有观察（重跑就是白读一遍），第二条是中断时
    // 正在执行的那一条——它可能已经产生了副作用，恢复不替模型重跑一次。
    assert!(
        invoker.executed.lock().unwrap().is_empty(),
        "已执行的与在途的调用都不得被自动重跑"
    );
    assert_eq!(runner.session().rounds(), 1, "半轮应配平并提交成完整一轮");
    // 提交的那轮带两条观察：一条来自快照，一条是重放提示。
    let turns = runner.session().snapshot().turns;
    let Some(Turn::Observation(outcomes)) = turns.last() else {
        panic!("最后一轮应是观察，实际 {:?}", turns.last());
    };
    assert_eq!(outcomes.len(), 2);
    assert_eq!(outcomes[0].content, "a 的内容");
    assert!(
        outcomes[1].content.contains("中断恢复"),
        "在途的那条要收到重放提示: {}",
        outcomes[1].content
    );
    assert!(
        outcomes[1].content.contains("read_file"),
        "重放提示要点名工具: {}",
        outcomes[1].content
    );
}

/// 恢复路径也必须过角色边界。
///
/// P9-3 把角色白名单从"装配时过滤注册表"移到"调用时校验"，注册表因此
/// 对每个角色都是全量的。这条测试守的是那次迁移唯一真实的坑：快照可以
/// 直接停在 `Dispatching`，恢复后 `Evaluating` 的检查被整段跳过。如果
/// 只在 `Evaluating` 查一次，一个停在 `Dispatching(write_file)` 的受限
/// 角色快照就能把写工具放行——白名单名义上还在，实际上少了一半入口。
#[tokio::test]
async fn dispatching_snapshot_of_a_restricted_role_still_refuses_the_write() {
    let workspace = std::env::temp_dir().join(format!("qch-resume-role-{}", std::process::id()));
    fs::create_dir_all(&workspace).expect("建临时工作区");
    let target = workspace.join("keep.txt");
    fs::write(&target, "原文").expect("写初始内容");

    let write_call = ToolCall {
        id: CallId::new("call_w"),
        name: "write_file".to_owned(),
        arguments: json!({"path": "keep.txt", "content": "被改"}),
    };
    let reviewer = AgentSpec {
        role: AgentRole::Reviewer,
        system_prompt: "恢复测试：评审者".to_owned(),
        model: None,
        effort: None,
        verbosity: None,
    };
    let snapshot = RunSnapshot {
        format_version: RUN_FORMAT,
        session_id: SessionId::new("resume-role"),
        spec: reviewer,
        session: Session::new(SessionId::new("resume-role"), "越权写入").snapshot(),
        state: RunState::Dispatching(vec![write_call.clone()]),
        consumption: Consumption {
            turns: 1,
            elapsed: Duration::from_millis(10),
        },
        pending: Some(PendingRound {
            turn: AgentTurn {
                text: "把结论写进文件".to_owned(),
                calls: vec![write_call],
            },
            outcomes: Vec::new(),
        }),
    };

    // 真实工具、全量注册表——与 P9-3 之后生产路径给 runner 的一致。
    let registry = ToolRegistry::new(all_tools(WorkspaceRoot::new(workspace.clone())));
    let mock = MockCompletionModel::from_turns(vec![MockTurn::text("改用文字给出结论")]);
    let mut runner = AgentRunner::resume(
        snapshot,
        deps_with(&mock, registry, Arc::new(AutoApproveGate), PolicyEngine::new(), None),
    )
    .unwrap();
    let outcome = runner.run(&Budget::new(5)).await.unwrap();

    assert_eq!(outcome.answer, "改用文字给出结论");
    // 文件一个字节都没动：恢复路径上的角色边界与正常调用链是同一条。
    assert_eq!(
        fs::read_to_string(&target).expect("读回文件"),
        "原文",
        "恢复后停在 Dispatching 的越权调用必须被拒"
    );
    // 拒绝观察如实回给模型，指明是角色权限问题。
    let turns = runner.session().snapshot().turns;
    let Some(Turn::Observation(outcomes)) = turns.last() else {
        panic!("最后一轮应是观察，实际 {:?}", turns.last());
    };
    assert_eq!(outcomes.len(), 1);
    assert!(
        outcomes[0].content.contains("角色 `reviewer` 无权调用工具 `write_file`"),
        "{}",
        outcomes[0].content
    );

    let _ = fs::remove_dir_all(&workspace);
}

#[tokio::test]
async fn normal_stop_with_pending_is_rejected_on_resume() {
    let snapshot = RunSnapshot {
        format_version: RUN_FORMAT,
        session_id: SessionId::new("resume-ghost"),
        spec: spec(),
        session: Session::new(SessionId::new("resume-ghost"), "幽灵轮次").snapshot(),
        state: RunState::AwaitingModel,
        consumption: Consumption::default(),
        pending: Some(PendingRound {
            turn: AgentTurn {
                text: "来路不明的轮次".to_owned(),
                calls: vec![call("call_x", "x")],
            },
            outcomes: vec![],
        }),
    };

    let error = AgentRunner::resume(
        snapshot,
        deps_with(
            &MockCompletionModel::from_turns(vec![]),
            registry(),
            Arc::new(AutoApproveGate),
            PolicyEngine::new(),
            None,
        ),
    )
    .err()
    .expect("应报快照损坏");

    let ApplicationError::Snapshot(StoreError::Corrupt(reason)) = &error else {
        panic!("应报快照损坏，实际 {error}")
    };
    assert!(reason.contains("正常点"), "{reason}");
}

#[tokio::test]
async fn mismatched_call_list_is_rejected_on_resume() {
    // 状态机记的调用清单与 pending 里的清单不一致：step() 按前者取
    // 下一个调用、按后者收观察，错位会让调用与观察配错对。
    let snapshot = RunSnapshot {
        format_version: RUN_FORMAT,
        session_id: SessionId::new("resume-mismatch"),
        spec: spec(),
        session: Session::new(SessionId::new("resume-mismatch"), "清单错位").snapshot(),
        state: RunState::Dispatching(vec![call("call_1", "a")]),
        consumption: Consumption {
            turns: 1,
            elapsed: Duration::ZERO,
        },
        pending: Some(PendingRound {
            turn: AgentTurn {
                text: "读 a".to_owned(),
                calls: vec![call("call_2", "b")],
            },
            outcomes: vec![],
        }),
    };

    let error = AgentRunner::resume(
        snapshot,
        deps_with(
            &MockCompletionModel::from_turns(vec![]),
            registry(),
            Arc::new(AutoApproveGate),
            PolicyEngine::new(),
            None,
        ),
    )
    .err()
    .expect("应报快照损坏");

    let ApplicationError::Snapshot(StoreError::Corrupt(reason)) = &error else {
        panic!("应报快照损坏，实际 {error}")
    };
    assert!(reason.contains("调用清单"), "{reason}");
}

#[tokio::test]
async fn snapshot_save_failure_degrades_without_breaking_the_run() {
    let mock = MockCompletionModel::from_turns(vec![
        MockTurn::tool_call("call_1", "read_file", json!({"path": "a"})),
        MockTurn::text("保存坏了也能跑完"),
    ]);
    let mut runner = AgentRunner::new(
        spec(),
        SessionId::new("resume-failing-store"),
        "保存会失败的任务",
        deps_with(
            &mock,
            registry(),
            Arc::new(AutoApproveGate),
            PolicyEngine::new(),
            Some(Arc::new(FailingStore)),
        ),
    );

    // 每个保存点都会失败（stderr 警告），但运行必须照常完成。
    let outcome = runner.run(&Budget::new(5)).await.unwrap();

    assert_eq!(outcome.answer, "保存坏了也能跑完");
    assert_eq!(runner.session().rounds(), 1);
}

#[tokio::test]
async fn cancelled_state_does_not_survive_resume() {
    let mock1 = MockCompletionModel::from_turns(vec![]);
    let mut runner = AgentRunner::new(
        spec(),
        SessionId::new("resume-cancel"),
        "会被取消的任务",
        deps_with(&mock1, registry(), Arc::new(AutoApproveGate), PolicyEngine::new(), None),
    );
    runner.cancel_handle().cancel();
    let error = runner.run(&Budget::new(5)).await.unwrap_err();
    assert!(matches!(error, ApplicationError::Cancelled), "{error}");

    // 恢复是一次新的"继续"意图：取消标记不跟着快照走。
    let mock2 = MockCompletionModel::from_turns(vec![MockTurn::text("恢复后完成")]);
    let mut runner2 = AgentRunner::resume(
        runner.snapshot(),
        deps_with(&mock2, registry(), Arc::new(AutoApproveGate), PolicyEngine::new(), None),
    )
    .unwrap();
    let outcome = runner2.run(&Budget::new(5)).await.unwrap();
    assert_eq!(outcome.answer, "恢复后完成");
}

#[tokio::test]
async fn awaiting_approval_autosaves_and_resumes_from_disk() {
    let workspace = temp_workspace("autosave");
    let store: Arc<dyn SessionStore> = Arc::new(JsonSessionStore::new(&workspace));
    let mock1 = MockCompletionModel::from_turns(vec![
        MockTurn::tool_call("call_1", "read_file", json!({"path": "a"})),
    ]);
    let mut runner = AgentRunner::new(
        spec(),
        SessionId::new("resume-disk"),
        "落盘审批",
        deps_with(&mock1, registry(), Arc::new(AutoApproveGate), approval_policy(), Some(store.clone())),
    );

    // 走到审批前——这是自动保存点之一。
    runner.step().await.unwrap();
    let state = runner.step().await.unwrap();
    assert!(matches!(state, RunState::AwaitingApproval(_)), "{state:?}");

    // 快照应已自动落盘，且停在审批前。
    let snapshot = store
        .load(&SessionId::new("resume-disk"))
        .unwrap()
        .expect("审批前的快照应自动落盘");
    assert_eq!(snapshot.state, state);

    // 从磁盘快照恢复并跑完。
    let mock2 = MockCompletionModel::from_turns(vec![MockTurn::text("从磁盘恢复后完成")]);
    let mut runner2 = AgentRunner::resume(
        snapshot,
        deps_with(&mock2, registry(), Arc::new(AutoApproveGate), approval_policy(), Some(store)),
    )
    .unwrap();
    let outcome = runner2.run(&Budget::new(5)).await.unwrap();
    assert_eq!(outcome.answer, "从磁盘恢复后完成");
    let _ = fs::remove_dir_all(&workspace);
}

#[test]
fn corrupt_snapshot_file_is_rejected() {
    let workspace = temp_workspace("corrupt");
    let store = JsonSessionStore::new(&workspace);
    let id = SessionId::new("broken");

    store.save(&minimal_snapshot("broken")).unwrap();
    let path = workspace
        .join(".qcodeharness")
        .join("snapshots")
        .join("broken.json");
    fs::write(&path, "{ 不是 JSON").unwrap();

    let error = store.load(&id).unwrap_err();
    assert!(matches!(error, StoreError::Parse(_)), "{error}");
    let _ = fs::remove_dir_all(&workspace);
}

//! PreToolUse 阻断语义的集成测试。
//!
//! 外部命令 exit code 2 必须并入现有唯一 RunHooks 链：拒绝时不调用执行器，
//! 且证据账本出现 ApprovalDecided(false) 与 ToolObserved(Refused)。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use qcode_harness::application::agent_roster::AgentRoster;
use qcode_harness::application::agent_run::{AgentDependencies, AgentRunner};
use qcode_harness::application::context_policy::DefaultContextPolicy;
use qcode_harness::application::hooks::{PreToolUseHook, PreToolUseHooks, PreToolUseVerdict};
use qcode_harness::application::port::{
    ApprovalGate, ApprovalRequest, BoxFuture, Decision, EvidenceSink, NoopHooks, Timestamp,
    ToolInvoker,
};
use qcode_harness::application::tool_registry::{ToolEntry, ToolRegistry};
use qcode_harness::domain::agent::{AgentRole, AgentSpec};
use qcode_harness::domain::budget::Budget;
use qcode_harness::domain::context::ContextBudget;
use qcode_harness::domain::evidence::AuditEvent;
use qcode_harness::domain::ids::{CallId, SessionId};
use qcode_harness::domain::policy::PolicyEngine;
use qcode_harness::domain::tool::{OutcomeStatus, ToolAnnotations, ToolCall, ToolOutcome, ToolSpec};
use qcode_harness::infrastructure::clock::SystemClock;
use qcode_harness::infrastructure::hooks_pretool::CommandPreToolUseHook;
use qcode_harness::infrastructure::model::rig_chat::RigChatGateway;
use rig::test_utils::{MockCompletionModel, MockTurn};
use serde_json::json;

/// 记录证据事件的测试替身；集成测试不能访问库内 cfg(test) 的证据实现。
#[derive(Default)]
struct RecordingEvidence {
    events: Mutex<Vec<AuditEvent>>,
}

impl EvidenceSink for RecordingEvidence {
    fn record(&self, _at: Timestamp, event: AuditEvent) {
        self.events.lock().expect("证据锁不应中毒").push(event);
    }
}

/// 记录工具执行次数，用于断言阻断时执行器零调用。
#[derive(Default)]
struct CountingInvoker {
    calls: Mutex<Vec<String>>,
}

impl ToolInvoker for CountingInvoker {
    fn invoke<'a>(&'a self, call: &'a ToolCall) -> BoxFuture<'a, ToolOutcome> {
        Box::pin(async move {
            self.calls
                .lock()
                .expect("执行器锁不应中毒")
                .push(call.name.clone());
            ToolOutcome::success("不应执行")
        })
    }
}

/// 自动放行的审批闸门；本用例的工具是只读，正常路径不会询问。
struct AllowGate;

impl ApprovalGate for AllowGate {
    fn request<'a>(&'a self, _request: &'a ApprovalRequest) -> BoxFuture<'a, Decision> {
        Box::pin(async { Ok(()) })
    }
}

static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

/// 独立临时目录，避免测试之间共享命令脚本或写进真实工作区。
fn temp_dir() -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!(
        "qch-hooks-pretool-{}-{}",
        std::process::id(),
        NEXT_DIR.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&path).expect("应创建独立临时目录");
    path
}

fn tool_call() -> ToolCall {
    ToolCall {
        id: CallId::new("call_1"),
        name: "read_file".to_owned(),
        arguments: json!({"path": "Cargo.toml"}),
    }
}

fn tool_spec() -> ToolSpec {
    ToolSpec {
        name: "read_file".to_owned(),
        description: "读文件".to_owned(),
        parameters: json!({"type": "object"}),
        annotations: ToolAnnotations::new(true, false, true, false),
    }
}

fn runner(
    invoker: Arc<CountingInvoker>,
    evidence: Arc<RecordingEvidence>,
    hooks: Arc<PreToolUseHooks>,
) -> AgentRunner {
    let mock = MockCompletionModel::from_turns(vec![
        MockTurn::tool_call("call_1", "read_file", json!({"path": "Cargo.toml"})),
        MockTurn::text("读完了。"),
    ]);
    AgentRunner::new(
        AgentSpec {
            role: AgentRole::Coder,
            system_prompt: "系统提示".to_owned(),
            model: None,
            effort: None,
            verbosity: None,
        },
        SessionId::new("hooks-pretool-test"),
        "读一下 Cargo.toml",
        AgentDependencies {
            model: Arc::new(RigChatGateway::new(mock.clone().erase())),
            tools: ToolRegistry::new(vec![ToolEntry {
                spec: tool_spec(),
                invoker,
            }]),
            approvals: Arc::new(AllowGate),
            evidence,
            clock: Arc::new(SystemClock),
            policy: PolicyEngine::new(),
            additional_guards: Vec::new(),
            roster: AgentRoster::builtin(),
            context_budget: ContextBudget::default(),
            context_policy: Arc::new(DefaultContextPolicy),
            session_store: None,
            hooks,
        },
    )
}

#[tokio::test]
async fn external_command_exit_2_denies_tool_without_invoking_executor() {
    let temp = temp_dir();
    let script = temp.join("deny.cmd");
    #[cfg(windows)]
    std::fs::write(&script, "@echo off\r\nexit /b 2\r\n").expect("应写入拒绝脚本");
    #[cfg(not(windows))]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(&script, "#!/bin/sh\nexit 2\n").expect("应写入拒绝脚本");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
            .expect("应设置可执行位");
    }

    let invoker = Arc::new(CountingInvoker::default());
    let evidence = Arc::new(RecordingEvidence::default());
    let hook = Arc::new(CommandPreToolUseHook::new(vec![
        script.to_string_lossy().into_owned()
    ]));
    let hooks = PreToolUseHooks::new(Arc::new(NoopHooks), vec![hook.clone()]);
    let mut runner = runner(invoker.clone(), evidence.clone(), hooks);

    let verdict = hook.decide(&tool_call()).await;
    assert_eq!(
        verdict,
        PreToolUseVerdict::Deny("命令以退出码 2 拒绝工具调用".to_owned())
    );

    let outcome = runner
        .run(&Budget::new(4))
        .await
        .expect("拒绝观察应回填给模型并正常结束");
    assert_eq!(outcome.answer, "读完了。");
    assert!(
        invoker.calls.lock().expect("执行器锁不应中毒").is_empty(),
        "PreToolUse 拒绝时执行器必须零调用"
    );

    let events = evidence.events.lock().expect("证据锁不应中毒").clone();
    assert!(
        events.iter().any(
            |event| matches!(event, AuditEvent::ApprovalDecided { allowed: false, .. })
        ),
        "拒绝应留下 ApprovalDecided(false)，实际事件：{events:?}"
    );
    assert!(
        events.iter().any(|event| matches!(
            event,
            AuditEvent::ToolObserved {
                outcome: ToolOutcome {
                    status: OutcomeStatus::Refused,
                    ..
                },
                ..
            }
        )),
        "拒绝应留下 ToolObserved(Refused)，实际事件：{events:?}"
    );
    std::fs::remove_dir_all(temp).expect("应清理独立临时目录");
}

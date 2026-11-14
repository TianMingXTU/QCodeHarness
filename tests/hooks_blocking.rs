//! 外部 PreToolUse 配置到唯一 RunHooks 链的装配测试。
//!
//! 配置映射只产出一条 AgentDependencies.hooks 链：exit 2 拒绝并留证据，
//! exit 0 放行，既有观察挂点恰好收到一次 before_tool_call。

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use qcode_harness::application::agent_roster::AgentRoster;
use qcode_harness::application::agent_run::{AgentDependencies, AgentRunner};
use qcode_harness::application::context_policy::DefaultContextPolicy;
use qcode_harness::application::hooks::command_hooks_from_report;
use qcode_harness::application::hooks_compat::{HookConfigFormat, parse_hook_config};
use qcode_harness::application::port::{
    ApprovalGate, ApprovalRequest, BoxFuture, Decision, EvidenceSink, NoopHooks, RunHooks,
    Timestamp, ToolInvoker,
};
use qcode_harness::application::tool_registry::{ToolEntry, ToolRegistry};
use qcode_harness::domain::agent::{AgentRole, AgentSpec};
use qcode_harness::domain::budget::Budget;
use qcode_harness::domain::context::ContextBudget;
use qcode_harness::domain::evidence::AuditEvent;
use qcode_harness::domain::ids::SessionId;
use qcode_harness::domain::policy::PolicyEngine;
use qcode_harness::domain::tool::{
    OutcomeStatus, ToolAnnotations, ToolCall, ToolOutcome, ToolSpec,
};
use qcode_harness::infrastructure::clock::SystemClock;
use qcode_harness::infrastructure::model::rig_chat::RigChatGateway;
use rig::test_utils::{MockCompletionModel, MockTurn};
use serde_json::json;

/// 独立临时目录；外部命令脚本不能共享，否则并发测试会互相改写退出码。
fn temp_dir(label: &str) -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("系统时钟应可用")
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "qch-hooks-blocking-{label}-{}-{stamp}",
        std::process::id()
    ));
    std::fs::create_dir_all(&path).expect("应创建独立临时目录");
    path
}

/// 记录观察调用次数，用于证明配置映射后没有并行挂点链。
#[derive(Default)]
struct CountingObserver {
    before: AtomicUsize,
}

impl RunHooks for CountingObserver {
    fn before_tool_call<'a>(&'a self, _call: &'a ToolCall) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            self.before.fetch_add(1, Ordering::Relaxed);
        })
    }
}

#[derive(Default)]
struct RecordingEvidence {
    events: std::sync::Mutex<Vec<AuditEvent>>,
}

impl EvidenceSink for RecordingEvidence {
    fn record(&self, _at: Timestamp, event: AuditEvent) {
        self.events.lock().expect("证据锁不应中毒").push(event);
    }
}

#[derive(Default)]
struct CountingInvoker {
    calls: AtomicUsize,
}

impl ToolInvoker for CountingInvoker {
    fn invoke<'a>(&'a self, _call: &'a ToolCall) -> BoxFuture<'a, ToolOutcome> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::Relaxed);
            ToolOutcome::success("已执行")
        })
    }
}

struct AllowGate;

impl ApprovalGate for AllowGate {
    fn request<'a>(&'a self, _request: &'a ApprovalRequest) -> BoxFuture<'a, Decision> {
        Box::pin(async { Ok(()) })
    }
}

#[cfg(unix)]
fn tool_call() -> ToolCall {
    ToolCall {
        id: CallId::new("call_1"),
        name: "read_file".to_owned(),
        arguments: json!({"path": "Cargo.toml"}),
    }
}

#[cfg(windows)]
fn command_script(directory: &PathBuf, exit_code: i32) -> String {
    let script = directory.join(format!("hook-{exit_code}.cmd"));
    std::fs::write(
        &script,
        format!("@echo off\r\nexit /b {exit_code}\r\n"),
    )
    .expect("应写入命令脚本");
    script.to_string_lossy().into_owned()
}

#[cfg(not(windows))]
fn command_script(directory: &PathBuf, exit_code: i32) -> String {
    use std::os::unix::fs::PermissionsExt;

    let script = directory.join(format!("hook-{exit_code}.sh"));
    std::fs::write(&script, format!("#!/bin/sh\nexit {exit_code}\n"))
        .expect("应写入命令脚本");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
        .expect("应设置可执行位");
    script.to_string_lossy().into_owned()
}

fn claude_settings(command: &str) -> String {
    // 兼容范围：Claude Code settings.json 的 PreToolUse command hook 最小字段。
    let escaped = command.replace('\\', "\\\\").replace('"', "\\\"");
    format!(
        r#"{{"hooks":{{"PreToolUse":[{{"matcher":"Bash","hooks":[{{"type":"command","command":"{escaped}"}}]}}]}}}}"#
    )
}

fn runner(
    hooks: Arc<dyn RunHooks>,
    invoker: Arc<CountingInvoker>,
    evidence: Arc<RecordingEvidence>,
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
        SessionId::new("hooks-blocking-test"),
        "读一下 Cargo.toml",
        AgentDependencies {
            model: Arc::new(RigChatGateway::new(mock.clone().erase())),
            tools: ToolRegistry::new(vec![ToolEntry {
                spec: ToolSpec {
                    name: "read_file".to_owned(),
                    description: "读文件".to_owned(),
                    parameters: json!({"type": "object"}),
                    annotations: ToolAnnotations::new(true, false, true, false),
                },
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

/// 从兼容报告装配出唯一的 RunHooks 链；测试与生产共用同一入口。
fn mapped_hooks(
    settings: &str,
    observer: Arc<dyn RunHooks>,
) -> Arc<qcode_harness::application::hooks::PreToolUseHooks> {
    let report = parse_hook_config(HookConfigFormat::ClaudeCode, settings)
        .expect("最小已知 Claude Code 字段应解析成功");
    assert!(report.unmapped.is_empty(), "兼容范围内不得有未映射项");
    command_hooks_from_report(&report, observer, None)
        .expect("command hook 应能显式映射")
}

#[tokio::test]
async fn configured_exit_2_denies_with_evidence_and_exit_0_allows_on_one_chain() {
    for (label, exit_code, expected_calls) in [("deny", 2, 0), ("allow", 0, 1)] {
        let directory = temp_dir(label);
        let counting = Arc::new(CountingObserver::default());
        let observer_arc: Arc<dyn RunHooks> = counting.clone();
        let hooks = mapped_hooks(
            &claude_settings(&command_script(&directory, exit_code)),
            observer_arc,
        );
        let observer = counting;
        let invoker = Arc::new(CountingInvoker::default());
        let evidence = Arc::new(RecordingEvidence::default());
        let mut agent = runner(hooks.clone(), invoker.clone(), evidence.clone());

        let outcome = agent
            .run(&Budget::new(4))
            .await
            .expect("拒绝观察应回填给模型并正常结束");
        assert_eq!(outcome.answer, "读完了。");
        assert_eq!(invoker.calls.load(Ordering::Relaxed), expected_calls);
        assert_eq!(
            observer.before.load(Ordering::Relaxed),
            1,
            "配置映射后必须只有一条 RunHooks 链，观察挂点恰好触发一次"
        );

        let events = evidence.events.lock().expect("证据锁不应中毒").clone();
        let denied = events.iter().any(
            |event| matches!(event, AuditEvent::ApprovalDecided { allowed: false, .. }),
        );
        let refused = events.iter().any(|event| matches!(
            event,
            AuditEvent::ToolObserved {
                outcome: ToolOutcome { status: OutcomeStatus::Refused, .. },
                ..
            }
        ));
        if exit_code == 2 {
            assert!(denied, "exit 2 应留下拒绝决定：{events:?}");
            assert!(refused, "exit 2 应留下 Refused 观察：{events:?}");
        } else {
            assert!(!denied, "exit 0 不应留下拒绝决定：{events:?}");
            assert!(!refused, "exit 0 不应留下 Refused 观察：{events:?}");
        }
        std::fs::remove_dir_all(directory).expect("应清理独立临时目录");
    }
}

#[test]
fn unsupported_mapping_is_explicitly_rejected() {
    let settings = r#"{"hooks":{"PreToolUse":[{"hooks":[{"type":"prompt","prompt":"allow?"}]}]}}"#;
    let report = parse_hook_config(HookConfigFormat::ClaudeCode, settings)
        .expect("解析本身成功，但映射范围应显式报告");
    let error = command_hooks_from_report(&report, Arc::new(NoopHooks), None)
        .err()
        .expect("未映射配置不能静默变成放行");

    assert!(report.unmapped.iter().any(|item| item.reason.contains("hook type")));
    assert!(
        error.contains("未映射") || error.contains("hook type"),
        "错误必须带出未映射原因，实际：{error}"
    );
}

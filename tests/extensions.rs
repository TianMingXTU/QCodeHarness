//! 扩展通过真实清单、共享注册表和既有工具端口运行。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use qcode_harness::application::agent_roster::AgentRoster;
use qcode_harness::application::agent_run::AgentDependencies;
use qcode_harness::application::context_policy::DefaultContextPolicy;
use qcode_harness::application::extension::{
    ExtensionRegistry, ExtensionSelection, Hook, HookEvent, Plugin, Skill,
};
use qcode_harness::application::port::{
    ApprovalGate, ApprovalRequest, BoxFuture, Decision, EvidenceSink, ModelGateway, ModelReply,
    ModelRequest, NoopHooks, RunHooks, Timestamp, ToolInvoker,
};
use qcode_harness::application::tool_guard::{GuardContext, GuardVerdict, ToolGuard};
use qcode_harness::application::tool_registry::{ToolEntry, ToolRegistry};
use qcode_harness::domain::agent::{AgentRole, AgentSpec};
use qcode_harness::domain::context::ContextBudget;
use qcode_harness::domain::evidence::AuditEvent;
use qcode_harness::domain::extension::ExtensionDescriptor;
use qcode_harness::domain::ids::CallId;
use qcode_harness::domain::policy::{PolicyEngine, PolicyRule, PolicyVerdict};
use qcode_harness::domain::sandbox::ExecutionBoundary;
use qcode_harness::domain::tool::{
    OutcomeStatus, ToolAnnotations, ToolCall, ToolOutcome, ToolSpec,
};
use qcode_harness::domain::workspace::WorkspaceRoot;
use qcode_harness::infrastructure::approval::auto::AutoApproveGate;
use qcode_harness::infrastructure::clock::{SystemClock, TokioTimeLimiter};
use qcode_harness::infrastructure::extensions::LocalExtensionStore;
use qcode_harness::infrastructure::tool::BuiltinToolSource;
use serde_json::json;

static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

/// 扩展动作时限；测试默认给足时间，只有超时用例才收窄。
fn limiter() -> Arc<dyn qcode_harness::application::port::TimeLimiter> {
    limiter_with(Duration::from_secs(1))
}

fn limiter_with(limit: Duration) -> Arc<dyn qcode_harness::application::port::TimeLimiter> {
    Arc::new(TokioTimeLimiter::new(limit).expect("时限大于零"))
}

struct Workspace(PathBuf);

impl Workspace {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "qch-extensions-{}-{}",
            std::process::id(),
            NEXT_DIR.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&root).unwrap();
        Self(root)
    }

    fn file(&self, name: &str, text: &str) -> PathBuf {
        let path = self.0.join(name);
        std::fs::write(&path, text).unwrap();
        path
    }

    fn dir(&self, name: &str) -> PathBuf {
        let path = self.0.join(name);
        std::fs::create_dir(&path).unwrap();
        path
    }

    /// 授权状态放在工作区外的独立目录，测试之间互不影响，也不会写进
    /// 真实用户配置目录。
    fn store(&self) -> LocalExtensionStore {
        LocalExtensionStore::with_state_directory(&self.0, Some(self.0.join("user-config")))
    }

    fn state_path(&self) -> PathBuf {
        let path = self
            .store()
            .state_path()
            .expect("测试指定了状态目录")
            .to_path_buf();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        path
    }
}

#[test]
fn a_repository_carried_state_file_grants_nothing() {
    let workspace = Workspace::new();
    let source = workspace.file("source.toml", MANIFEST);
    let tools = fake_tools(Arc::new(Recorder::default()));
    let store = workspace.store();
    let registry = store.load(&tools).unwrap();
    store.install(&registry, &source).unwrap();

    // 仓库里自带的"已批准"不再被读取：授权只在用户配置目录。
    let in_repo = workspace.0.join(".qcodeharness/extensions/state.toml");
    std::fs::write(
        &in_repo,
        "[[extensions]]\nid='demo'\nenabled=true\npermissions=['write_file']\n",
    )
    .unwrap();
    let reloaded = store.load(&tools).unwrap();
    let listed = reloaded.list().unwrap();
    assert!(!listed[0].enabled, "克隆来的仓库不能自带启用状态");
    assert!(listed[0].approved_permissions.is_empty());

    // 清单仍然被识别，安装项不因授权文件位置变化而丢失。
    assert_eq!(listed[0].descriptor.id, "demo");
}

#[test]
fn a_workspace_under_a_linked_parent_still_loads() {
    let workspace = Workspace::new();
    let target = workspace.dir("real");
    let link = workspace.0.join("linked");
    if let Err(error) = link_dir(&target, &link) {
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        return;
    }
    // 工作区自身走链接（macOS 的 /var 就是这样）：祖先链接不能拦，
    // 工作区内部的链接仍然要拦。
    let store = LocalExtensionStore::with_state_directory(&link, Some(workspace.0.join("cfg")));
    let registry = store.load(&fake_tools(Arc::new(Recorder::default()))).unwrap();
    assert!(registry.list().unwrap().is_empty());

    // 工作区内部的链接仍要拒绝：把扩展目录所在的那一级做成链接。
    let elsewhere = workspace.dir("elsewhere");
    std::fs::create_dir_all(elsewhere.join("extensions")).unwrap();
    let internal = link.join(".qcodeharness");
    if link_dir(&elsewhere, &internal).is_ok() {
        assert!(
            store
                .load(&fake_tools(Arc::new(Recorder::default())))
                .is_err(),
            "工作区内部的链接仍要拒绝"
        );
    }
}

fn link_dir(target: &Path, link: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link)
    }
    #[cfg(windows)]
    {
        std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .output()
            .and_then(|output| {
                if output.status.success() {
                    Ok(())
                } else {
                    Err(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        String::from_utf8_lossy(&output.stderr).into_owned(),
                    ))
                }
            })
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).unwrap();
    }
}

#[derive(Default)]
struct Events(Mutex<Vec<AuditEvent>>);

impl EvidenceSink for Events {
    fn record(&self, _at: Timestamp, event: AuditEvent) {
        self.0.lock().unwrap().push(event);
    }
}

struct NoModel;

impl ModelGateway for NoModel {
    fn complete<'a>(
        &'a self,
        _request: ModelRequest<'a>,
    ) -> BoxFuture<'a, Result<ModelReply, qcode_harness::application::error::ApplicationError>>
    {
        Box::pin(async { panic!("适配器不得调用模型") })
    }
}

fn dependencies(tools: ToolRegistry, evidence: Arc<Events>) -> AgentDependencies {
    AgentDependencies {
        model: Arc::new(NoModel),
        tools,
        approvals: Arc::new(AutoApproveGate),
        evidence,
        clock: Arc::new(SystemClock),
        policy: policy(PolicyVerdict::Allow),
        additional_guards: Vec::new(),
        roster: AgentRoster::builtin(),
        context_budget: ContextBudget::default(),
        context_policy: Arc::new(DefaultContextPolicy),
        session_store: None,
        hooks: Arc::new(NoopHooks),
    }
}

fn policy(verdict: PolicyVerdict) -> PolicyEngine {
    PolicyEngine::with_rules(vec![PolicyRule {
        tool: None,
        scope: None,
        verdict,
        description: "测试规则".to_owned(),
    }])
}

fn descriptor(id: &str, permissions: &[&str]) -> ExtensionDescriptor {
    ExtensionDescriptor {
        id: id.to_owned(),
        description: "测试扩展".to_owned(),
        permissions: permissions.iter().map(|name| (*name).to_owned()).collect(),
    }
}

struct Prompt(ExtensionDescriptor);

impl Skill for Prompt {
    fn descriptor(&self) -> ExtensionDescriptor {
        self.0.clone()
    }

    fn instructions(&self) -> &str {
        "先读文件，再作判断。"
    }
}

fn install(registry: &ExtensionRegistry, id: &str, permissions: &[&str]) {
    registry
        .install_skill(Arc::new(Prompt(descriptor(id, permissions))))
        .unwrap();
}

fn call(name: &str, arguments: serde_json::Value) -> ToolCall {
    ToolCall {
        id: CallId::new("outer"),
        name: name.to_owned(),
        arguments,
    }
}

fn action(tool: &str) -> ToolCall {
    call("skill__demo", json!({"tool": tool, "arguments": {}}))
}

#[derive(Default)]
struct Recorder(Mutex<Vec<String>>);

impl ToolInvoker for Recorder {
    fn invoke<'a>(&'a self, call: &'a ToolCall) -> BoxFuture<'a, ToolOutcome> {
        Box::pin(async move {
            self.0.lock().unwrap().push(call.name.clone());
            ToolOutcome::success("已执行").with_execution(ExecutionBoundary::process(false, true))
        })
    }
}

fn fake_tools(invoker: Arc<dyn ToolInvoker>) -> ToolRegistry {
    ToolRegistry::new(
        ["read_file", "write_file", "run_command"]
            .into_iter()
            .map(|name| ToolEntry {
                spec: ToolSpec {
                    name: name.to_owned(),
                    description: name.to_owned(),
                    parameters: json!({"type": "object"}),
                    annotations: ToolAnnotations::new(name == "read_file", false, false, false),
                },
                invoker: invoker.clone(),
            })
            .collect(),
    )
}

const MANIFEST: &str = r#"
id = "demo"
description = "本地代码阅读规范"
instructions = "先读文件，再作判断。"
permissions = ["read_file", "write_file"]
"#;

#[tokio::test]
async fn real_manifest_install_grants_reload_and_stale_handle_disable() {
    let workspace = Workspace::new();
    workspace.file("input.txt", "实际文件内容");
    let source = workspace.file("source.toml", MANIFEST);
    let backing = ToolRegistry::from_sources(&[Arc::new(BuiltinToolSource::new(
        WorkspaceRoot::new(&workspace.0),
    ))]);
    let store = workspace.store();
    let registry = store.load(&backing).unwrap();
    store.install(&registry, &source).unwrap();
    assert!(!registry.list().unwrap()[0].enabled);
    assert!(
        store.install(&registry, &source).is_err(),
        "不得覆盖已安装清单"
    );
    let deps = dependencies(backing.clone(), Arc::new(Events::default()));
    let disabled = registry
        .adapters(
            &deps,
            AgentRole::Coder,
            ExtensionSelection::all(),
            limiter(),
        )
        .unwrap();
    assert!(disabled.source.tools().is_empty());

    store
        .enable(&registry, "demo", &["read_file".to_owned()])
        .unwrap();
    let restored = store.load(&backing).unwrap();
    assert_eq!(
        restored.list().unwrap()[0].approved_permissions,
        ["read_file"]
    );
    let selected = restored.select(&["demo".to_owned()], &[]).unwrap();
    assert!(
        restored
            .enabled_instructions(&selected)
            .unwrap()
            .contains("先读文件")
    );
    let adapters = restored
        .adapters(&deps, AgentRole::Coder, selected, limiter())
        .unwrap();
    let tools = ToolRegistry::from_sources(std::slice::from_ref(&adapters.source));
    assert_eq!(tools.specs()[0].name, "skill__demo");
    assert!(!tools.specs()[0].annotations.read_only);
    assert!(
        tools
            .invoke(&call("skill__demo", json!({})))
            .await
            .content
            .contains("先读文件")
    );
    let read = call(
        "skill__demo",
        json!({"tool": "read_file", "arguments": {"path": "input.txt"}}),
    );
    assert!(tools.invoke(&read).await.content.contains("实际文件内容"));
    let write = call(
        "skill__demo",
        json!({"tool": "write_file", "arguments": {"path": "forbidden.txt", "content": "越权"}}),
    );
    assert_eq!(tools.invoke(&write).await.status, OutcomeStatus::Refused);
    assert!(!workspace.0.join("forbidden.txt").exists());
    store.disable(&restored, "demo").unwrap();
    assert_eq!(tools.invoke(&read).await.status, OutcomeStatus::Refused);
    assert!(adapters.source.tools().is_empty());
    assert!(!store.load(&backing).unwrap().list().unwrap()[0].enabled);
}

#[tokio::test]
async fn exact_permissions_cannot_escalate_with_policy_allow_or_autoapprove() {
    let recorder = Arc::new(Recorder::default());
    let tools = fake_tools(recorder.clone());
    let registry = Arc::new(ExtensionRegistry::new(&tools));
    install(&registry, "demo", &["read_file", "write_file"]);
    assert!(
        registry
            .enable("demo", &["run_command".to_owned()])
            .is_err()
    );
    registry.enable("demo", &["read_file".to_owned()]).unwrap();
    let adapters = registry
        .adapters(
            &dependencies(tools, Arc::new(Events::default())),
            AgentRole::Coder,
            ExtensionSelection::all(),
            limiter(),
        )
        .unwrap();
    let assembled = ToolRegistry::from_sources(&[adapters.source]);
    for name in ["write_file", "run_command", "skill__demo"] {
        assert_eq!(
            assembled.invoke(&action(name)).await.status,
            OutcomeStatus::Refused
        );
    }
    assert!(recorder.0.lock().unwrap().is_empty());
    assert_eq!(
        assembled.invoke(&action("read_file")).await.status,
        OutcomeStatus::Success
    );
    registry.revoke("demo", "read_file").unwrap();
    assert_eq!(
        assembled.invoke(&action("read_file")).await.status,
        OutcomeStatus::Refused
    );
    assert_eq!(*recorder.0.lock().unwrap(), ["read_file"]);
}

#[test]
fn duplicate_unknown_and_colliding_ids_are_rejected() {
    let tools = fake_tools(Arc::new(Recorder::default()));
    let registry = ExtensionRegistry::new(&tools);
    install(&registry, "demo", &[]);
    assert!(
        registry
            .install_skill(Arc::new(Prompt(descriptor("demo", &[]))))
            .is_err()
    );
    assert!(registry.enable("missing", &[]).is_err());
    assert!(registry.disable("missing").is_err());
    assert!(registry.select(&["missing".to_owned()], &[]).is_err());
    assert!(
        registry
            .select(&["demo".to_owned(), "demo".to_owned()], &[])
            .is_err(),
        "配置重复 ID 必须报错"
    );
    assert!(registry.select(&[], &["demo".to_owned()]).is_err());
    assert!(
        registry
            .install_skill(Arc::new(Prompt(descriptor("../escape", &[]))))
            .is_err()
    );
    assert!(
        registry
            .install_skill(Arc::new(Prompt(descriptor("read_file", &[]))))
            .is_err()
    );
    assert!(
        registry
            .install_skill(Arc::new(Prompt(descriptor(
                "unknown-permission",
                &["invented"]
            ))))
            .is_err()
    );
    let collision = ToolRegistry::new(vec![ToolEntry {
        spec: ToolSpec {
            name: "skill__demo".to_owned(),
            description: String::new(),
            parameters: json!({}),
            annotations: ToolAnnotations::default(),
        },
        invoker: Arc::new(Recorder::default()),
    }]);
    let registry = ExtensionRegistry::new(&collision);
    assert!(
        registry
            .install_skill(Arc::new(Prompt(descriptor("demo", &[]))))
            .is_err()
    );
}

struct Deny;

impl ApprovalGate for Deny {
    fn request<'a>(&'a self, _request: &'a ApprovalRequest) -> BoxFuture<'a, Decision> {
        Box::pin(async { Err("用户否决".to_owned()) })
    }
}

struct DenyGuard;

impl ToolGuard for DenyGuard {
    fn name(&self) -> &str {
        "deny"
    }
    fn inspect(&self, _call: &ToolCall, _context: &GuardContext<'_>) -> GuardVerdict {
        GuardVerdict::Deny("附加守卫否决".to_owned())
    }
}

#[tokio::test]
async fn policy_additional_guards_and_approval_denial_preserve_no_side_effects() {
    for mode in 0..3 {
        let recorder = Arc::new(Recorder::default());
        let tools = fake_tools(recorder.clone());
        let registry = Arc::new(ExtensionRegistry::new(&tools));
        install(&registry, "demo", &["write_file"]);
        registry.enable("demo", &["write_file".to_owned()]).unwrap();
        let evidence = Arc::new(Events::default());
        let mut deps = dependencies(tools, evidence.clone());
        match mode {
            0 => deps.policy = policy(PolicyVerdict::Deny),
            1 => {
                deps.policy = policy(PolicyVerdict::RequireApproval);
                deps.approvals = Arc::new(Deny);
            }
            _ => deps.additional_guards.push(Arc::new(DenyGuard)),
        }
        let adapters = registry
            .adapters(
                &deps,
                AgentRole::Coder,
                ExtensionSelection::all(),
                limiter(),
            )
            .unwrap();
        let tools = ToolRegistry::from_sources(&[adapters.source]);
        assert_eq!(
            tools.invoke(&action("write_file")).await.status,
            OutcomeStatus::Refused
        );
        assert!(recorder.0.lock().unwrap().is_empty());
        let events = evidence.0.lock().unwrap();
        assert!(matches!(events[0], AuditEvent::ToolRequested { .. }));
        assert!(
            events
                .iter()
                .any(|event| matches!(event, AuditEvent::PolicyEvaluated { .. }))
        );
        assert!(matches!(
            events.last().unwrap(),
            AuditEvent::ToolObserved { .. }
        ));
        if mode == 1 {
            assert!(
                events.iter().any(|event| matches!(
                    event,
                    AuditEvent::ApprovalDecided { allowed: false, .. }
                ))
            );
        }
    }
}

struct DisableDuringApproval(Arc<ExtensionRegistry>);

impl ApprovalGate for DisableDuringApproval {
    fn request<'a>(&'a self, _request: &'a ApprovalRequest) -> BoxFuture<'a, Decision> {
        Box::pin(async move {
            self.0.disable("demo").unwrap();
            self.0.enable("demo", &["write_file".to_owned()]).unwrap();
            Ok(())
        })
    }
}

#[tokio::test]
async fn a_disable_and_reenable_invalidates_a_parked_approval() {
    let recorder = Arc::new(Recorder::default());
    let tools = fake_tools(recorder.clone());
    let registry = Arc::new(ExtensionRegistry::new(&tools));
    install(&registry, "demo", &["write_file"]);
    registry.enable("demo", &["write_file".to_owned()]).unwrap();
    let mut deps = dependencies(tools, Arc::new(Events::default()));
    deps.policy = policy(PolicyVerdict::RequireApproval);
    deps.approvals = Arc::new(DisableDuringApproval(registry.clone()));
    let adapters = registry
        .adapters(
            &deps,
            AgentRole::Coder,
            ExtensionSelection::all(),
            limiter(),
        )
        .unwrap();
    let tools = ToolRegistry::from_sources(&[adapters.source]);
    assert_eq!(
        tools.invoke(&action("write_file")).await.status,
        OutcomeStatus::Refused
    );
    assert!(recorder.0.lock().unwrap().is_empty());
}

struct ChangeRoleDuringApproval(Mutex<Option<Arc<dyn RunHooks>>>);

impl ApprovalGate for ChangeRoleDuringApproval {
    fn request<'a>(&'a self, _request: &'a ApprovalRequest) -> BoxFuture<'a, Decision> {
        Box::pin(async move {
            let hooks = self.0.lock().unwrap().clone().unwrap();
            let spec = AgentSpec {
                role: AgentRole::Tester,
                system_prompt: String::new(),
                model: None,
                effort: None,
                verbosity: None,
            };
            let request = ModelRequest {
                history: Vec::new(),
                spec: &spec,
                tools: &[],
                cache_key: None,
            };
            hooks.before_model_call(&request).await;
            Ok(())
        })
    }
}

#[tokio::test]
async fn role_change_during_approval_invalidates_the_previous_roles_request() {
    let recorder = Arc::new(Recorder::default());
    let tools = fake_tools(recorder.clone());
    let registry = Arc::new(ExtensionRegistry::new(&tools));
    install(&registry, "demo", &["write_file"]);
    registry.enable("demo", &["write_file".to_owned()]).unwrap();
    let mut deps = dependencies(tools, Arc::new(Events::default()));
    let gate = Arc::new(ChangeRoleDuringApproval(Mutex::new(None)));
    deps.policy = policy(PolicyVerdict::RequireApproval);
    deps.approvals = gate.clone();
    let adapters = registry.adapters(&deps, AgentRole::Coder, ExtensionSelection::all(), limiter()).unwrap();
    *gate.0.lock().unwrap() = Some(adapters.hooks.clone());
    let tools = ToolRegistry::from_sources(&[adapters.source]);
    assert_eq!(tools.invoke(&action("write_file")).await.status, OutcomeStatus::Refused);
    assert!(recorder.0.lock().unwrap().is_empty());
    gate.0.lock().unwrap().take();
}

struct Pending;

impl ToolInvoker for Pending {
    fn invoke<'a>(&'a self, _call: &'a ToolCall) -> BoxFuture<'a, ToolOutcome> {
        Box::pin(std::future::pending())
    }
}

#[tokio::test]
async fn timeout_is_audited_and_success_preserves_execution_boundary() {
    for timed_out in [false, true] {
        let invoker: Arc<dyn ToolInvoker> = if timed_out {
            Arc::new(Pending)
        } else {
            Arc::new(Recorder::default())
        };
        let tools = fake_tools(invoker);
        let registry = Arc::new(ExtensionRegistry::new(&tools));
        install(&registry, "demo", &["read_file"]);
        registry.enable("demo", &["read_file".to_owned()]).unwrap();
        let evidence = Arc::new(Events::default());
        let adapters = registry
            .adapters(
                &dependencies(tools, evidence.clone()),
                AgentRole::Coder,
                ExtensionSelection::all(),
                limiter_with(Duration::from_millis(10)),
            )
            .unwrap();
        let tools = ToolRegistry::from_sources(&[adapters.source]);
        let outcome = tools.invoke(&action("read_file")).await;
        assert_eq!(
            outcome.status,
            if timed_out {
                OutcomeStatus::Failed
            } else {
                OutcomeStatus::Success
            }
        );
        let events = evidence.0.lock().unwrap();
        let observed: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                AuditEvent::ToolObserved { outcome, .. } => Some(outcome.status),
                _ => None,
            })
            .collect();
        assert_eq!(observed, vec![outcome.status], "证据必须如实记录这次结局");
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, AuditEvent::SandboxApplied { .. }))
                .count(),
            usize::from(!timed_out)
        );
        if timed_out {
            assert!(outcome.content.contains("超时"));
        }
    }
}

struct PlannedHook {
    id: &'static str,
    after: bool,
}

impl Hook for PlannedHook {
    fn descriptor(&self) -> ExtensionDescriptor {
        descriptor(self.id, &["read_file"])
    }
    fn plan(&self, event: HookEvent<'_>) -> Vec<ToolCall> {
        let is_after = matches!(event, HookEvent::AfterToolResult { .. });
        if is_after != self.after {
            return Vec::new();
        }
        vec![call("read_file", json!({"hook": self.id}))]
    }
}

#[tokio::test]
async fn hooks_are_ordered_do_not_recurse_and_honor_live_disable() {
    let recorder = Arc::new(Recorder::default());
    let tools = fake_tools(recorder.clone());
    let registry = Arc::new(ExtensionRegistry::new(&tools));
    registry
        .install_hook(Arc::new(PlannedHook {
            id: "z-after",
            after: true,
        }))
        .unwrap();
    registry
        .install_hook(Arc::new(PlannedHook {
            id: "a-before",
            after: false,
        }))
        .unwrap();
    for id in ["z-after", "a-before"] {
        registry.enable(id, &["read_file".to_owned()]).unwrap();
    }
    let evidence = Arc::new(Events::default());
    let adapters = registry
        .adapters(
            &dependencies(tools, evidence.clone()),
            AgentRole::Coder,
            ExtensionSelection::all(),
            limiter(),
        )
        .unwrap();
    let outer = call("write_file", json!({}));
    adapters.hooks.before_tool_call(&outer).await;
    recorder.0.lock().unwrap().push("outer".to_owned());
    adapters
        .hooks
        .after_tool_result(&outer, &ToolOutcome::success("完成"))
        .await;
    assert_eq!(
        *recorder.0.lock().unwrap(),
        ["read_file", "outer", "read_file"]
    );
    let ids: Vec<String> = evidence
        .0
        .lock()
        .unwrap()
        .iter()
        .filter_map(|event| match event {
            AuditEvent::ToolRequested { call, .. } => {
                Some(call.arguments["hook"].as_str().unwrap().to_owned())
            }
            _ => None,
        })
        .collect();
    assert_eq!(ids, ["a-before", "z-after"]);
    registry.disable("a-before").unwrap();
    adapters.hooks.before_tool_call(&outer).await;
    assert_eq!(recorder.0.lock().unwrap().len(), 3);
}

#[tokio::test]
async fn current_model_role_also_limits_delegated_actions() {
    let recorder = Arc::new(Recorder::default());
    let tools = fake_tools(recorder.clone());
    let registry = Arc::new(ExtensionRegistry::new(&tools));
    install(&registry, "demo", &["write_file"]);
    registry.enable("demo", &["write_file".to_owned()]).unwrap();
    let adapters = registry
        .adapters(
            &dependencies(tools, Arc::new(Events::default())),
            AgentRole::Coder,
            ExtensionSelection::all(),
            limiter(),
        )
        .unwrap();
    let tools = ToolRegistry::from_sources(&[adapters.source]);
    let spec = AgentSpec {
        role: AgentRole::Tester,
        system_prompt: String::new(),
        model: None,
        effort: None,
        verbosity: None,
    };
    let request = ModelRequest {
        history: Vec::new(),
        spec: &spec,
        tools: &[],
        cache_key: None,
    };
    adapters.hooks.before_model_call(&request).await;
    assert_eq!(
        tools.invoke(&action("write_file")).await.status,
        OutcomeStatus::Refused
    );
    assert!(recorder.0.lock().unwrap().is_empty());
}

#[test]
fn invalid_data_unknown_saved_grants_and_oversized_files_fail_closed() {
    let workspace = Workspace::new();
    let tools = fake_tools(Arc::new(Recorder::default()));
    let store = workspace.store();
    let registry = store.load(&tools).unwrap();
    for text in [
        MANIFEST.replace("id = \"demo\"", "id = \"../demo\""),
        format!("{MANIFEST}\nscript = 'arbitrary runner'"),
        format!(
            "{MANIFEST}\n[hook]\nevent = 'before_tool'\ntool = 'run_command'\nargs = {{ command = 'anything' }}"
        ),
        "x".repeat(65_537),
    ] {
        let path = workspace.file("bad.toml", &text);
        assert!(store.install(&registry, &path).is_err());
    }
    assert!(registry.list().unwrap().is_empty());
    let source = workspace.file("good.toml", MANIFEST);
    store.install(&registry, &source).unwrap();
    let state = workspace.state_path();
    std::fs::write(
        &state,
        "[[extensions]]\nid='missing'\nenabled=true\npermissions=['read_file']\n",
    )
    .unwrap();
    assert!(store.load(&tools).is_err());
    std::fs::write(
        &state,
        "[[extensions]]\nid='demo'\nenabled=true\npermissions=['run_command']\n",
    )
    .unwrap();
    assert!(store.load(&tools).is_err());
}

#[tokio::test]
async fn local_manifest_hook_invokes_declared_builtin_and_can_be_revoked() {
    let workspace = Workspace::new();
    let source = workspace.file(
        "hook.toml",
        &format!(
            "{MANIFEST}\n[hook]\nevent = 'after_tool'\ntool = 'write_file'\n\
             args = {{ path = 'hook.txt', content = '实际写入' }}\n"
        ),
    );
    let tools = ToolRegistry::from_sources(&[Arc::new(BuiltinToolSource::new(
        WorkspaceRoot::new(&workspace.0),
    ))]);
    let store = workspace.store();
    let registry = store.load(&tools).unwrap();
    store.install(&registry, &source).unwrap();
    store
        .enable(&registry, "demo", &["write_file".to_owned()])
        .unwrap();
    let adapters = registry
        .adapters(
            &dependencies(tools, Arc::new(Events::default())),
            AgentRole::Coder,
            ExtensionSelection::all(),
            limiter(),
        )
        .unwrap();
    let outer = call("read_file", json!({}));
    adapters.hooks.before_tool_call(&outer).await;
    assert!(!workspace.0.join("hook.txt").exists());
    adapters
        .hooks
        .after_tool_result(&outer, &ToolOutcome::success("完成"))
        .await;
    assert_eq!(
        std::fs::read_to_string(workspace.0.join("hook.txt")).unwrap(),
        "实际写入"
    );
    std::fs::remove_file(workspace.0.join("hook.txt")).unwrap();
    store.revoke(&registry, "demo", "write_file").unwrap();
    adapters
        .hooks
        .after_tool_result(&outer, &ToolOutcome::success("完成"))
        .await;
    assert!(!workspace.0.join("hook.txt").exists());
}

struct Bundle;

impl Plugin for Bundle {
    fn id(&self) -> &str {
        "bundle"
    }
    fn skills(&self) -> Vec<Arc<dyn Skill>> {
        vec![Arc::new(Prompt(descriptor("bundled", &[])))]
    }
    fn hooks(&self) -> Vec<Arc<dyn Hook>> {
        Vec::new()
    }
}

#[test]
fn compiled_plugin_registration_is_atomic_and_disabled_by_default() {
    let registry = ExtensionRegistry::new(&fake_tools(Arc::new(Recorder::default())));
    registry.install_plugin(&Bundle).unwrap();
    assert!(!registry.list().unwrap()[0].enabled);
    assert!(registry.install_plugin(&Bundle).is_err());
    assert_eq!(registry.list().unwrap().len(), 1);
}

struct RoleHook(Arc<Mutex<Vec<AgentRole>>>);

impl Hook for RoleHook {
    fn descriptor(&self) -> ExtensionDescriptor {
        descriptor("role-observer", &[])
    }
    fn plan(&self, event: HookEvent<'_>) -> Vec<ToolCall> {
        let role = match event {
            HookEvent::BeforeToolCall { role, .. } | HookEvent::AfterToolResult { role, .. } => {
                role
            }
        };
        self.0.lock().unwrap().push(role);
        Vec::new()
    }
}

#[tokio::test]
async fn hook_events_report_initial_and_updated_roles_without_mutable_capabilities() {
    let tools = fake_tools(Arc::new(Recorder::default()));
    let registry = Arc::new(ExtensionRegistry::new(&tools));
    let seen = Arc::new(Mutex::new(Vec::new()));
    registry
        .install_hook(Arc::new(RoleHook(seen.clone())))
        .unwrap();
    registry.enable("role-observer", &[]).unwrap();
    let adapters = registry
        .adapters(
            &dependencies(tools, Arc::new(Events::default())),
            AgentRole::Coder,
            ExtensionSelection::all(),
            limiter(),
        )
        .unwrap();
    let outer = call("read_file", json!({}));
    adapters.hooks.before_tool_call(&outer).await;
    let spec = AgentSpec {
        role: AgentRole::Tester,
        system_prompt: String::new(),
        model: None,
        effort: None,
        verbosity: None,
    };
    let request = ModelRequest {
        history: Vec::new(),
        spec: &spec,
        tools: &[],
        cache_key: None,
    };
    adapters.hooks.before_model_call(&request).await;
    adapters
        .hooks
        .after_tool_result(&outer, &ToolOutcome::success("完成"))
        .await;
    assert_eq!(*seen.lock().unwrap(), [AgentRole::Coder, AgentRole::Tester]);
}

#[cfg(unix)]
fn symlink_file(original: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(original, link)
}

#[cfg(windows)]
fn symlink_file(original: &Path, link: &Path) -> std::io::Result<()> {
    std::os::windows::fs::symlink_file(original, link)
}

#[test]
fn symlinked_manifest_is_refused_when_platform_allows_creating_links() {
    let workspace = Workspace::new();
    let source = workspace.file("source.toml", MANIFEST);
    let link = workspace.0.join("linked.toml");
    if let Err(error) = symlink_file(&source, &link) {
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        return;
    }
    let store = workspace.store();
    let registry = store
        .load(&fake_tools(Arc::new(Recorder::default())))
        .unwrap();
    assert!(store.install(&registry, &link).is_err());
}

struct NamedBundle {
    id: &'static str,
    entry: &'static str,
}

impl Plugin for NamedBundle {
    fn id(&self) -> &str {
        self.id
    }
    fn skills(&self) -> Vec<Arc<dyn Skill>> {
        vec![Arc::new(Prompt(descriptor(self.entry, &[])))]
    }
    fn hooks(&self) -> Vec<Arc<dyn Hook>> {
        Vec::new()
    }
}

#[test]
fn plugin_and_entry_ids_share_one_namespace() {
    let registry = ExtensionRegistry::new(&fake_tools(Arc::new(Recorder::default())));
    registry
        .install_plugin(&NamedBundle {
            id: "bundle",
            entry: "child",
        })
        .unwrap();
    assert!(
        registry
            .install_skill(Arc::new(Prompt(descriptor("bundle", &[]))))
            .is_err()
    );
    assert!(
        registry
            .install_plugin(&NamedBundle {
                id: "child",
                entry: "other"
            })
            .is_err()
    );
    assert!(
        registry
            .install_plugin(&NamedBundle {
                id: "same",
                entry: "same"
            })
            .is_err()
    );
    assert_eq!(registry.list().unwrap().len(), 1);
}

#[test]
fn failed_state_save_does_not_publish_grants_or_replace_an_existing_manifest() {
    let workspace = Workspace::new();
    let tools = fake_tools(Arc::new(Recorder::default()));
    let store = workspace.store();
    let registry = store.load(&tools).unwrap();
    let source = workspace.file("source.toml", MANIFEST);
    store.install(&registry, &source).unwrap();
    let state = workspace.state_path();
    std::fs::create_dir(&state).unwrap();
    assert!(
        store
            .enable(&registry, "demo", &["read_file".to_owned()])
            .is_err()
    );
    let unchanged = registry.list().unwrap();
    assert!(!unchanged[0].enabled);
    assert!(unchanged[0].approved_permissions.is_empty());
    let manifest = workspace
        .0
        .join(".qcodeharness/extensions/manifests/demo.toml");
    let installed = std::fs::read(&manifest).unwrap();
    workspace.file("source.toml", &MANIFEST.replace("先读文件", "新的规范"));
    let stale_registry = ExtensionRegistry::new(&tools);
    assert!(store.install(&stale_registry, &source).is_err());
    assert_eq!(std::fs::read(&manifest).unwrap(), installed);
    assert!(stale_registry.list().unwrap().is_empty());
}

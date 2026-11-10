//! 生产服务装配走真实工具与审批路径，模型使用确定性替身。

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use qcode_harness::application::agent_roster::AgentRoster;
use qcode_harness::application::agent_run::{AgentDependencies, AgentRunner};
use qcode_harness::application::error::ApplicationError;
use qcode_harness::application::port::{
    BoxFuture, CancelHandle, ModelGateway, ModelReply, ModelRequest,
};
use qcode_harness::application::service::EventJournal;
use qcode_harness::application::service_factory::{TaskBuild, TaskFactory};
use qcode_harness::application::tool_registry::ToolRegistry;
use qcode_harness::domain::agent::{AgentRole, AgentSpec, AgentTurn, Effort, Verbosity};
use qcode_harness::domain::budget::Budget;
use qcode_harness::domain::config::AppConfig;
use qcode_harness::domain::evidence::TokenUsage;
use qcode_harness::domain::ids::{CallId, SessionId};
use qcode_harness::domain::service::{
    EventContext, EventOrigin, PendingApproval, ServiceEventPayload,
};
use qcode_harness::domain::tool::ToolCall;
use qcode_harness::domain::workspace::WorkspaceRoot;
use qcode_harness::infrastructure::approval::async_gate::ApprovalBroker;
use qcode_harness::infrastructure::delegation::StructuralVerifier;
use qcode_harness::infrastructure::extensions::LocalExtensionStore;
use qcode_harness::infrastructure::mcp::config::{McpConfig, McpServerConfig};
use qcode_harness::infrastructure::policy;
use qcode_harness::infrastructure::service_events::{JsonEventJournal, ServiceEvidence};
use qcode_harness::infrastructure::service_factory::{ProductionTaskFactory, ServiceFactoryConfig};
use qcode_harness::infrastructure::session_store::JsonSessionStore;
use qcode_harness::infrastructure::tool::all_tools;
use serde_json::json;

static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

struct Workspace(PathBuf);

impl Workspace {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "qch-service-factory-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT_DIR.fetch_add(1, Ordering::Relaxed),
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn dir(&self, name: &str) -> PathBuf {
        let path = self.0.join(name);
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn config(&self, model: Arc<dyn ModelGateway>) -> ServiceFactoryConfig {
        let mut settings = AppConfig::default();
        settings.parallel.max_subtasks = 0;
        ServiceFactoryConfig {
            model,
            policy: policy::load(None).unwrap(),
            roster: AgentRoster::builtin(),
            context_budget: Default::default(),
            budget: Budget::new(8),
            user_dir: None,
            extensions: LocalExtensionStore::with_state_directory(
                &self.0,
                Some(self.0.join("grants")),
            ),
            settings,
            mcp: McpConfig::default(),
            verifier: Arc::new(StructuralVerifier),
        }
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[derive(Default)]
struct Model {
    turns: Mutex<VecDeque<AgentTurn>>,
    specs: Mutex<Vec<AgentSpec>>,
    tools: Mutex<Vec<Vec<String>>>,
}

impl Model {
    fn scripted(turns: Vec<AgentTurn>) -> Arc<Self> {
        Arc::new(Self {
            turns: Mutex::new(turns.into()),
            ..Default::default()
        })
    }
}

impl ModelGateway for Model {
    fn complete<'a>(
        &'a self,
        request: ModelRequest<'a>,
    ) -> BoxFuture<'a, Result<ModelReply, ApplicationError>> {
        Box::pin(async move {
            self.specs.lock().unwrap().push(request.spec.clone());
            self.tools
                .lock()
                .unwrap()
                .push(request.tools.iter().map(|tool| tool.name.clone()).collect());
            Ok(ModelReply {
                turn: self
                    .turns
                    .lock()
                    .unwrap()
                    .pop_front()
                    .expect("模型脚本未耗尽"),
                usage: TokenUsage::default(),
            })
        })
    }
}

struct Runtime {
    journal: Arc<JsonEventJournal>,
    broker: Arc<ApprovalBroker>,
    root: PathBuf,
}

impl Runtime {
    fn new(root: &Path) -> Self {
        let journal = Arc::new(JsonEventJournal::open(root.join("events.jsonl")).unwrap());
        let broker = Arc::new(ApprovalBroker::new(journal.clone()));
        Self {
            journal,
            broker,
            root: root.to_path_buf(),
        }
    }

    fn build(&self, workspace: &Path, execution: &str) -> TaskBuild {
        let context = EventContext {
            task_id: "task-root".to_owned(),
            session_id: "session-root".to_owned(),
            execution_id: execution.to_owned(),
            origin: EventOrigin::default(),
        };
        let evidence = Arc::new(ServiceEvidence::new(
            self.journal.clone(),
            context.clone(),
            CancelHandle::default(),
        ));
        TaskBuild {
            workspace: workspace.to_path_buf(),
            context: context.clone(),
            evidence: evidence.clone(),
            hooks: evidence,
            approvals: self.broker.gate(context),
            session_store: Arc::new(JsonSessionStore::new(&self.root)),
            journal: self.journal.clone(),
            approval_factory: self.broker.clone(),
        }
    }

    async fn approval(&self) -> PendingApproval {
        let mut events = self.journal.subscribe(0).unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let ServiceEventPayload::ApprovalWaiting { request } =
                    events.next().await.unwrap().payload
                {
                    if self
                        .broker
                        .pending(None)
                        .iter()
                        .any(|pending| pending.approval_id == request.approval_id)
                    {
                        return request;
                    }
                }
            }
        })
        .await
        .expect("工具应等待异步审批")
    }

    fn allow(&self, request: &PendingApproval) {
        self.broker
            .decide(
                &request.approval_id,
                &request.task_id,
                &request.execution_id,
                true,
            )
            .unwrap();
    }
}

fn call(name: &str, arguments: serde_json::Value) -> ToolCall {
    ToolCall {
        id: CallId::new(format!("call-{name}")),
        name: name.to_owned(),
        arguments,
    }
}

fn turn(call: ToolCall) -> AgentTurn {
    AgentTurn {
        text: "执行工具".to_owned(),
        calls: vec![call],
    }
}

fn answer() -> AgentTurn {
    AgentTurn {
        text: "完成".to_owned(),
        calls: Vec::new(),
    }
}

fn runner(deps: AgentDependencies) -> AgentRunner {
    let profile = deps.roster.find(AgentRole::Coder).unwrap();
    let spec = AgentSpec {
        role: profile.role,
        system_prompt: profile.system_prompt.clone(),
        model: profile.model.clone(),
        effort: profile.effort,
        verbosity: profile.verbosity,
    };
    AgentRunner::new(spec, SessionId::new("session-root"), "执行装配测试", deps)
}

#[tokio::test]
async fn each_execution_invokes_builtins_in_its_own_root() {
    let root = Workspace::new();
    let first = root.dir("first");
    let second = root.dir("second");
    std::fs::write(root.0.join("data.txt"), "父工作区").unwrap();
    std::fs::write(first.join("data.txt"), "第一个工作区").unwrap();
    std::fs::write(second.join("data.txt"), "第二个工作区").unwrap();
    let runtime = Runtime::new(&root.0);
    let factory = ProductionTaskFactory::new(root.config(Arc::new(Model::default())));
    let first_deps = factory.build(runtime.build(&first, "first")).await.unwrap();
    let second_deps = factory
        .build(runtime.build(&second, "second"))
        .await
        .unwrap();
    for (deps, expected) in [
        (&first_deps, "第一个工作区"),
        (&second_deps, "第二个工作区"),
    ] {
        let outcome = deps
            .tools
            .invoke(&call("read_file", json!({"path":"data.txt"})))
            .await;
        assert!(!outcome.is_error(), "{outcome:?}");
        assert!(outcome.content.contains(expected), "{outcome:?}");
    }
    let outcome = first_deps
        .tools
        .invoke(&call(
            "write_file",
            json!({"path":"data.txt", "content":"独立修改"}),
        ))
        .await;
    assert!(!outcome.is_error(), "{outcome:?}");
    assert_eq!(
        std::fs::read_to_string(first.join("data.txt")).unwrap(),
        "独立修改"
    );
    assert_eq!(
        std::fs::read_to_string(second.join("data.txt")).unwrap(),
        "第二个工作区"
    );
    assert_eq!(
        std::fs::read_to_string(root.0.join("data.txt")).unwrap(),
        "父工作区"
    );
    assert!(
        first_deps
            .tools
            .invoke(&call(
                "write_file",
                json!({"path":"../data.txt", "content":"越界"})
            ))
            .await
            .is_error()
    );
    factory.close("first").await.unwrap();
    factory.close("second").await.unwrap();
}

#[tokio::test]
async fn writable_calls_wait_for_the_supplied_async_gate() {
    let root = Workspace::new();
    let isolated = root.dir("isolated");
    let runtime = Runtime::new(&root.0);
    let model = Model::scripted(vec![
        turn(call(
            "write_file",
            json!({"path":"approved.txt", "content":"已批准"}),
        )),
        answer(),
    ]);
    let factory = ProductionTaskFactory::new(root.config(model));
    let build = runtime.build(&isolated, "write");
    let provided_gate = build.approvals.clone();
    let deps = factory.build(build).await.unwrap();
    assert!(Arc::ptr_eq(&provided_gate, &deps.approvals));
    let run = tokio::spawn(async move { runner(deps).run(&Budget::new(8)).await });
    let approval = runtime.approval().await;
    assert_eq!(approval.execution_id, "write");
    assert_eq!(approval.session_id, "session-root");
    assert!(!isolated.join("approved.txt").exists());
    assert!(!root.0.join("approved.txt").exists());
    runtime.allow(&approval);
    run.await.unwrap().unwrap();
    assert_eq!(
        std::fs::read_to_string(isolated.join("approved.txt")).unwrap(),
        "已批准"
    );
    assert!(!root.0.join("approved.txt").exists());
    factory.close("write").await.unwrap();
}

#[tokio::test]
async fn service_hooks_extensions_instructions_and_role_options_survive_assembly() {
    let root = Workspace::new();
    let isolated = root.dir("isolated");
    std::fs::write(root.0.join("QCODEHARNESS.md"), "父目录规则不可注入").unwrap();
    std::fs::write(isolated.join("QCODEHARNESS.md"), "只使用本执行目录规则").unwrap();
    std::fs::write(isolated.join("input.txt"), "读取内容").unwrap();
    let model = Model::scripted(vec![
        turn(call("read_file", json!({"path":"input.txt"}))),
        answer(),
    ]);
    let mut config = root.config(model.clone());
    let user_dir = root.dir("user");
    std::fs::write(user_dir.join("QCODEHARNESS.md"), "用户级共同规则").unwrap();
    config.user_dir = Some(user_dir);
    config.roster = config.roster.with_role_models(
        &HashMap::from([(AgentRole::Coder, "coder-special".to_owned())]),
        &HashMap::from([(AgentRole::Coder, Effort::High)]),
        &HashMap::from([(AgentRole::Coder, Verbosity::Low)]),
    );
    let source = root.0.join("hook.toml");
    std::fs::write(
        &source,
        r#"id = 'marker'
description = '记录工具观察'
instructions = '保留扩展约定'
permissions = ['write_file']
[hook]
event = 'after_tool'
tool = 'write_file'
args = { path = 'hook.txt', content = '扩展已执行' }
"#,
    )
    .unwrap();
    let backing = ToolRegistry::new(all_tools(WorkspaceRoot::new(&isolated)));
    let registry = config.extensions.load(&backing).unwrap();
    config.extensions.install(&registry, &source).unwrap();
    config
        .extensions
        .enable(&registry, "marker", &["write_file".to_owned()])
        .unwrap();
    let factory = ProductionTaskFactory::new(config);
    let runtime = Runtime::new(&root.0);
    let deps = factory
        .build(runtime.build(&isolated, "hooks"))
        .await
        .unwrap();
    assert!(deps.tools.find("skill__marker").is_some());
    let run = tokio::spawn(async move { runner(deps).run(&Budget::new(8)).await });
    let approval = runtime.approval().await;
    assert_eq!(approval.origin.extension_id.as_deref(), Some("marker"));
    assert!(!isolated.join("hook.txt").exists());
    runtime.allow(&approval);
    run.await.unwrap().unwrap();
    assert_eq!(
        std::fs::read_to_string(isolated.join("hook.txt")).unwrap(),
        "扩展已执行"
    );
    assert!(!root.0.join("hook.txt").exists());
    for spec in model.specs.lock().unwrap().iter() {
        assert_eq!(spec.model.as_deref(), Some("coder-special"));
        assert_eq!(spec.effort, Some(Effort::High));
        assert_eq!(spec.verbosity, Some(Verbosity::Low));
        assert!(spec.system_prompt.contains("只使用本执行目录规则"));
        assert!(spec.system_prompt.contains("用户级共同规则"));
        assert!(spec.system_prompt.contains("保留扩展约定"));
        assert!(!spec.system_prompt.contains("父目录规则不可注入"));
    }
    let events = runtime.journal.read(0, usize::MAX).unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event.payload, ServiceEventPayload::ModelCallStarted { .. }))
            .count(),
        2
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event.payload, ServiceEventPayload::ModelCallFinished { .. }))
            .count(),
        2
    );
    assert!(events.iter().any(
        |event| event.origin.extension_id.as_deref() == Some("marker")
            && matches!(event.payload, ServiceEventPayload::Audit { .. })
    ));
    factory.close("hooks").await.unwrap();
}

fn mcp_config() -> McpConfig {
    McpConfig {
        mcp_servers: vec![McpServerConfig {
            name: "fixture".to_owned(),
            command: env!("CARGO_BIN_EXE_mcp-lifecycle-server").to_owned(),
            args: vec!["ok".to_owned(), "counter.txt".to_owned()],
        }],
    }
}

async fn eof(path: &Path) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !path.exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("MCP 进程应收到 EOF 并优雅退出");
}

#[tokio::test]
async fn mcp_launch_is_deferred_rooted_and_closed_with_its_execution() {
    let root = Workspace::new();
    let isolated = root.dir("isolated");
    let runtime = Runtime::new(&root.0);
    let mut config = root.config(Arc::new(Model::default()));
    config.mcp = mcp_config();
    let factory = ProductionTaskFactory::new(config);
    assert!(!isolated.join("counter.txt.pid").exists());
    let deps = factory
        .build(runtime.build(&isolated, "mcp"))
        .await
        .unwrap();
    assert!(isolated.join("counter.txt.pid").exists());
    assert!(!root.0.join("counter.txt.pid").exists());
    let invocation = call("mcp__fixture__side_effect", json!({}));
    assert!(!deps.tools.invoke(&invocation).await.is_error());
    assert_eq!(
        std::fs::read_to_string(isolated.join("counter.txt"))
            .unwrap()
            .trim(),
        "1"
    );
    factory.close("mcp").await.unwrap();
    eof(&isolated.join("counter.txt.eof")).await;
    assert!(deps.tools.invoke(&invocation).await.is_error());
    factory.close("mcp").await.unwrap();
}

#[tokio::test]
async fn a_failed_build_closes_already_connected_mcp_resources() {
    let root = Workspace::new();
    let isolated = root.dir("isolated");
    let runtime = Runtime::new(&root.0);
    let mut config = root.config(Arc::new(Model::default()));
    config.mcp = mcp_config();
    config.settings.skills.enabled = Some(vec!["missing-skill".to_owned()]);
    let factory = ProductionTaskFactory::new(config);
    let error = match factory.build(runtime.build(&isolated, "failed")).await {
        Ok(_) => panic!("不存在的扩展必须使装配失败"),
        Err(error) => error,
    };
    assert!(error.message.contains("missing-skill"), "{error}");
    eof(&isolated.join("counter.txt.eof")).await;
    factory.close("failed").await.unwrap();
}

fn git(root: &Path, args: &[&str]) {
    let mut command = std::process::Command::new("git");
    command
        .args([
            "-c",
            "user.name=assembly-test",
            "-c",
            "user.email=assembly@example.invalid",
        ])
        .args(args)
        .current_dir(root)
        .stdin(std::process::Stdio::null());
    for (name, _) in std::env::vars_os() {
        if name
            .to_string_lossy()
            .to_ascii_uppercase()
            .starts_with("GIT_")
        {
            command.env_remove(name);
        }
    }
    let output = command.output().expect("测试需要 Git");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn repository(root: &Path) {
    git(root, &["init", "-q"]);
    git(root, &["config", "core.autocrlf", "false"]);
    std::fs::write(root.join("baseline.txt"), "基线文件\n").unwrap();
    std::fs::create_dir(root.join("src")).unwrap();
    std::fs::write(root.join("QCODEHARNESS.md"), "执行根目录规则\n").unwrap();
    std::fs::write(root.join("src/QCODEHARNESS.md"), "子任务目录规则\n").unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "baseline"]);
}

#[derive(Default)]
struct ChildModel {
    sessions: Mutex<HashMap<String, (usize, u32)>>,
    specs: Mutex<Vec<AgentSpec>>,
    tools: Mutex<Vec<Vec<String>>>,
}

impl ModelGateway for ChildModel {
    fn complete<'a>(
        &'a self,
        request: ModelRequest<'a>,
    ) -> BoxFuture<'a, Result<ModelReply, ApplicationError>> {
        Box::pin(async move {
            self.specs.lock().unwrap().push(request.spec.clone());
            self.tools
                .lock()
                .unwrap()
                .push(request.tools.iter().map(|tool| tool.name.clone()).collect());
            let mut sessions = self.sessions.lock().unwrap();
            let next = sessions.len();
            let (index, count) = sessions
                .entry(request.cache_key.expect("子会话应有缓存标识").to_owned())
                .or_insert((next, 0));
            let reply = if *count == 0 {
                turn(call(
                    "write_file",
                    json!({"path":format!("child-{index}.txt"), "content":"子工作区写入"}),
                ))
            } else {
                answer()
            };
            *count += 1;
            Ok(ModelReply {
                turn: reply,
                usage: TokenUsage::default(),
            })
        })
    }
}

#[tokio::test]
async fn delegated_children_have_independent_approval_owners_and_service_evidence() {
    let root = Workspace::new();
    let isolated = root.dir("isolated");
    repository(&isolated);
    std::fs::write(root.0.join("QCODEHARNESS.md"), "宿主规则不能越界注入").unwrap();
    let model = Arc::new(ChildModel::default());
    let mut config = root.config(model.clone());
    config.settings.parallel.max_subtasks = 2;
    let manifest = root.0.join("child-context.toml");
    std::fs::write(&manifest, "id = 'child-context'\ndescription = '子会话读取观察'\ninstructions = '子扩展规则'\npermissions = ['read_file']\n[hook]\nevent = 'after_tool'\ntool = 'read_file'\nargs = { path = 'baseline.txt' }\n").unwrap();
    let backing = ToolRegistry::new(all_tools(WorkspaceRoot::new(&isolated)));
    let extensions = config.extensions.load(&backing).unwrap();
    config.extensions.install(&extensions, &manifest).unwrap();
    config
        .extensions
        .enable(&extensions, "child-context", &["read_file".into()])
        .unwrap();
    config.roster = config.roster.with_role_models(
        &HashMap::from([(AgentRole::Coder, "child-model".to_owned())]),
        &HashMap::from([(AgentRole::Coder, Effort::High)]),
        &HashMap::from([(AgentRole::Coder, Verbosity::Low)]),
    );
    let runtime = Runtime::new(&root.0);
    let mut events = runtime.journal.subscribe(0).unwrap();
    let factory = ProductionTaskFactory::new(config);
    let deps = factory
        .build(runtime.build(&isolated, "delegated"))
        .await
        .unwrap();
    let parent_cancel = CancelHandle::default();
    deps.approvals.bind_cancel(parent_cancel.clone());
    let invocation = call(
        "delegate_subtasks",
        json!({"tasks":[
            {"id":"one", "goal":"写入第一个文件", "scope":"src"},
            {"id":"two", "goal":"写入第二个文件", "scope":"src"},
        ]}),
    );
    let run = tokio::spawn(async move { deps.tools.invoke(&invocation).await });
    let approvals = tokio::time::timeout(Duration::from_secs(30), async {
        let mut approvals = Vec::new();
        while approvals.len() < 2 {
            if let ServiceEventPayload::ApprovalWaiting { request } =
                events.next().await.unwrap().payload
            {
                approvals.push(request);
            }
        }
        approvals
    })
    .await
    .expect("两个子会话应各自等待审批");
    assert_ne!(approvals[0].session_id, approvals[1].session_id);
    for request in &approvals {
        assert_eq!(request.task_id, "task-root");
        assert_eq!(request.execution_id, "delegated");
        assert_ne!(request.session_id, "session-root");
        assert_eq!(
            request.origin.parent_session_id.as_deref(),
            Some("session-root")
        );
    }
    // 父闸门的取消绑定不能覆盖子 runner 的取消通道。
    parent_cancel.cancel();
    for request in &approvals {
        runtime.allow(request);
    }
    let outcome = run.await.unwrap();
    assert!(!outcome.is_error(), "{outcome:?}");
    let result: qcode_harness::application::delegation::DelegationResult =
        serde_json::from_str(&outcome.content).unwrap();
    assert!(result.merged, "{result:?}");
    assert!(
        result
            .tasks
            .iter()
            .all(|task| task.workspace_removed && task.verified)
    );
    for index in 0..2 {
        assert_eq!(
            std::fs::read_to_string(isolated.join(format!("child-{index}.txt"))).unwrap(),
            "子工作区写入"
        );
        assert!(!root.0.join(format!("child-{index}.txt")).exists());
    }
    for spec in model.specs.lock().unwrap().iter() {
        assert_eq!(spec.model.as_deref(), Some("child-model"));
        assert_eq!(spec.effort, Some(Effort::High));
        assert_eq!(spec.verbosity, Some(Verbosity::Low));
        assert!(spec.system_prompt.contains("执行根目录规则"));
        assert!(spec.system_prompt.contains("子任务目录规则"));
        assert!(spec.system_prompt.contains("子扩展规则"));
        assert!(!spec.system_prompt.contains("宿主规则不能越界注入"));
    }
    assert!(
        model
            .tools
            .lock()
            .unwrap()
            .iter()
            .all(|tools| !tools.iter().any(|name| name == "delegate_subtasks"))
    );
    let history = runtime.journal.read(0, usize::MAX).unwrap();
    for request in approvals {
        assert!(history.iter().any(|event| event.session_id.as_deref()
            == Some(request.session_id.as_str())
            && event.origin.parent_session_id.as_deref() == Some("session-root")
            && matches!(event.payload, ServiceEventPayload::ModelCallStarted { .. })));
        assert!(history.iter().any(|event| event.session_id.as_deref()
            == Some(request.session_id.as_str())
            && event.origin.extension_id.as_deref() == Some("child-context")
            && event.origin.parent_session_id.as_deref() == Some("session-root")
            && matches!(event.payload, ServiceEventPayload::Audit { .. })));
    }
    factory.close("delegated").await.unwrap();
}

#[tokio::test]
async fn child_extensions_cannot_reuse_a_parent_mcp_connection() {
    let root = Workspace::new();
    let isolated = root.dir("isolated");
    repository(&isolated);
    let mut config = root.config(Arc::new(Model::default()));
    config.settings.parallel.max_subtasks = 1;
    config.mcp = mcp_config();
    // 进程状态留在受保护目录，避免夹具文件使委派基线变脏。
    std::fs::create_dir_all(isolated.join(".qcodeharness")).unwrap();
    config.mcp.mcp_servers[0].args[1] = ".qcodeharness/counter.txt".to_owned();
    let manifest = root.0.join("remote.toml");
    std::fs::write(&manifest, "id = 'remote'\ndescription = '外部工具权限'\ninstructions = '使用外部工具'\npermissions = ['mcp__fixture__side_effect']\n").unwrap();
    let backing = ToolRegistry::new(all_tools(WorkspaceRoot::new(&isolated)));
    let extensions = config.extensions.load(&backing).unwrap();
    config.extensions.install(&extensions, &manifest).unwrap();
    config
        .extensions
        .enable(&extensions, "remote", &["mcp__fixture__side_effect".into()])
        .unwrap();
    let runtime = Runtime::new(&root.0);
    let factory = ProductionTaskFactory::new(config);
    let deps = factory
        .build(runtime.build(&isolated, "remote-child"))
        .await
        .unwrap();
    let outcome = deps
        .tools
        .invoke(&call(
            "delegate_subtasks",
            json!({"tasks":[{"id":"one", "goal":"检查外部工具"}]}),
        ))
        .await;
    let result: qcode_harness::application::delegation::DelegationResult =
        serde_json::from_str(&outcome.content).unwrap();
    assert!(!result.merged);
    assert!(
        result.tasks[0]
            .error
            .as_deref()
            .unwrap()
            .contains("mcp__fixture__side_effect"),
        "{result:?}"
    );
    assert!(result.tasks[0].workspace_removed);
    assert!(!isolated.join(".qcodeharness/counter.txt").exists());
    factory.close("remote-child").await.unwrap();
    eof(&isolated.join(".qcodeharness/counter.txt.eof")).await;
}

fn frontend_process(root: &Path) -> std::process::Command {
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_QCodeHarness"));
    for (name, _) in std::env::vars_os() {
        let upper = name.to_string_lossy().to_ascii_uppercase();
        if upper.starts_with("LLM_") || upper.starts_with("QCH_") {
            command.env_remove(name);
        }
    }
    command
        .current_dir(root)
        .stdin(std::process::Stdio::null())
        .env("LLM_BASE_URL", "http://127.0.0.1:1/v1")
        .env("LLM_API_KEY", "unused-for-idle-service")
        .env("LLM_MODEL", "no-network")
        .env("APPDATA", root.join("user-config"))
        .env("XDG_CONFIG_HOME", root.join("user-config"))
        .env("QCH_AUTO_APPROVE", "1");
    command
}

#[test]
fn stdio_idle_entry_writes_only_protocol_frames_and_does_not_launch_mcp() {
    let root = Workspace::new();
    repository(&root.0);
    let mcp_path = root.0.join("mcp.toml");
    let helper = env!("CARGO_BIN_EXE_mcp-lifecycle-server").replace('\\', "/");
    std::fs::write(&mcp_path, format!(
        "[[mcp_servers]]\nname = 'fixture'\ncommand = '{helper}'\nargs = ['ok', 'idle-counter']\n",
    )).unwrap();
    for _ in 0..2 {
        let output = frontend_process(&root.0)
            .arg("--stdio")
            .env("QCH_MCP_CONFIG", &mcp_path)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8(output.stdout).unwrap();
        let frames: Vec<qcode_harness::interface::stdio::StdioResponse> = stdout
            .lines()
            .map(|line| serde_json::from_str(line).expect("stdout 每行都应为协议帧"))
            .collect();
        assert!(matches!(
            frames.as_slice(),
            [qcode_harness::interface::stdio::StdioResponse::Hello { version: 1 }]
        ));
        assert!(!root.0.join("idle-counter.pid").exists());
    }
    let state: serde_json::Value = serde_json::from_slice(
        &std::fs::read(root.0.join(".qcodeharness/service/state.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        state["receipts"]
            .as_object()
            .unwrap()
            .keys()
            .filter(|id| id.starts_with("host-shutdown-"))
            .count(),
        2
    );
}

#[test]
fn piped_interactive_uses_text_and_explicit_tui_rejects_nonterminals() {
    let root = Workspace::new();
    repository(&root.0);
    for mode in ["--text", "--interactive"] {
        let output = frontend_process(&root.0).arg(mode).output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(
            !stdout.contains('\u{1b}'),
            "文本前端不应切换终端模式: {stdout:?}"
        );
        assert!(stdout.contains("服务已关闭"), "{stdout}");
    }
    let output = frontend_process(&root.0).arg("--tui").output().unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("terminal_required"));
}

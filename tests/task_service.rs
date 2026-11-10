//! 常驻任务服务通过公共命令驱动；模型屏障保证控制请求不会被运行占住。

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use qcode_harness::application::agent_roster::AgentRoster;
use qcode_harness::application::agent_run::AgentDependencies;
use qcode_harness::application::context_policy::DefaultContextPolicy;
use qcode_harness::application::error::ApplicationError;
use qcode_harness::application::isolation::WorkspaceVerifier;
use qcode_harness::application::port::{BoxFuture, ModelGateway, ModelReply, ModelRequest};
use qcode_harness::application::service::TaskService;
use qcode_harness::application::service_factory::{TaskBuild, TaskFactory};
use qcode_harness::application::tool_registry::ToolRegistry;
use qcode_harness::domain::agent::AgentTurn;
use qcode_harness::domain::budget::Budget;
use qcode_harness::domain::evidence::TokenUsage;
use qcode_harness::domain::policy::PolicyEngine;
use qcode_harness::domain::service::{
    ServiceCommand, ServiceError, ServiceReply, TaskStatus, TaskView,
};
use qcode_harness::domain::task::TaskKind;
use qcode_harness::infrastructure::clock::SystemClock;
use qcode_harness::infrastructure::task_service::LocalTaskService;
use tokio::sync::{Notify, Semaphore};

struct Repo(PathBuf);

impl Repo {
    fn new(label: &str) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "qch-service-{}-{label}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        git(&root, &["init", "-q"]);
        git(&root, &["config", "core.autocrlf", "false"]);
        fs::write(root.join("file.txt"), b"baseline\n").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "-qm", "baseline"]);
        Self(fs::canonicalize(root).unwrap())
    }
}

impl Drop for Repo {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_dir_all(&self.0)
            && !std::thread::panicking()
        {
            panic!("清理测试仓库失败: {error}");
        }
    }
}

fn git(root: &Path, args: &[&str]) -> std::process::Output {
    let mut command = Command::new("git");
    command
        .args([
            "-c",
            "user.name=service-test",
            "-c",
            "user.email=service@example.invalid",
            "-c",
            "commit.gpgSign=false",
            "-c",
            "core.hooksPath=",
        ])
        .args(args)
        .current_dir(root)
        .stdin(Stdio::null());
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
    output
}

struct Accept;
impl WorkspaceVerifier for Accept {
    fn verify<'a>(&'a self, _: &'a Path) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }
}

struct Model {
    calls: AtomicUsize,
    entered: Notify,
    permits: Semaphore,
    histories: Mutex<Vec<String>>,
    roles: Mutex<Vec<qcode_harness::domain::agent::AgentRole>>,
    prompts: Mutex<Vec<String>>,
    answer: String,
    after_reply: Option<Box<dyn Fn(usize) + Send + Sync>>,
}

impl Model {
    fn new() -> Self {
        Self {
            calls: AtomicUsize::new(0),
            entered: Notify::new(),
            permits: Semaphore::new(0),
            histories: Mutex::new(Vec::new()),
            roles: Mutex::new(Vec::new()),
            prompts: Mutex::new(Vec::new()),
            answer: "完成".to_owned(),
            after_reply: None,
        }
    }
    async fn wait_calls(&self, count: usize) {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let next = self.entered.notified();
                if self.calls.load(Ordering::SeqCst) >= count {
                    break;
                }
                next.await;
            }
        })
        .await
        .expect("模型应开始调用");
    }
}

impl ModelGateway for Model {
    fn complete<'a>(
        &'a self,
        request: ModelRequest<'a>,
    ) -> BoxFuture<'a, Result<ModelReply, ApplicationError>> {
        Box::pin(async move {
            self.histories
                .lock()
                .unwrap()
                .push(format!("{:?}", request.history));
            self.roles.lock().unwrap().push(request.spec.role);
            self.prompts
                .lock()
                .unwrap()
                .push(request.spec.system_prompt.clone());
            let index = self.calls.fetch_add(1, Ordering::SeqCst);
            self.entered.notify_waiters();
            self.permits.acquire().await.unwrap().forget();
            if let Some(after_reply) = &self.after_reply {
                after_reply(index);
            }
            Ok(ModelReply {
                turn: AgentTurn {
                    text: self.answer.clone(),
                    calls: Vec::new(),
                },
                usage: TokenUsage::default(),
            })
        })
    }
}

struct Factory {
    model: Arc<Model>,
    builds: AtomicUsize,
    closes: AtomicUsize,
    workspaces: Mutex<Vec<PathBuf>>,
    roster: AgentRoster,
    hooks: Option<Arc<dyn qcode_harness::application::port::RunHooks>>,
    close_gate: Option<Arc<Semaphore>>,
    close_error: Option<String>,
    close_panics: bool,
}

impl Factory {
    fn new(model: Arc<Model>) -> Self {
        Self {
            model,
            builds: AtomicUsize::new(0),
            closes: AtomicUsize::new(0),
            workspaces: Mutex::new(Vec::new()),
            roster: AgentRoster::builtin(),
            hooks: None,
            close_gate: None,
            close_error: None,
            close_panics: false,
        }
    }

    fn configure(self, configure: impl FnOnce(&mut Self)) -> Arc<Self> {
        let mut factory = self;
        configure(&mut factory);
        Arc::new(factory)
    }

    async fn wait_closes(&self, count: usize) {
        tokio::time::timeout(Duration::from_secs(10), async {
            while self.closes.load(Ordering::SeqCst) < count {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("工厂应开始关闭资源");
    }
}

impl TaskFactory for Factory {
    fn build<'a>(
        &'a self,
        build: TaskBuild,
    ) -> BoxFuture<'a, Result<AgentDependencies, ServiceError>> {
        Box::pin(async move {
            self.builds.fetch_add(1, Ordering::SeqCst);
            self.workspaces.lock().unwrap().push(build.workspace);
            Ok(AgentDependencies {
                model: self.model.clone(),
                tools: ToolRegistry::new(vec![]),
                approvals: build.approvals,
                evidence: build.evidence,
                clock: Arc::new(SystemClock),
                policy: PolicyEngine::new(),
                additional_guards: Vec::new(),
                roster: self.roster.clone(),
                context_budget: Default::default(),
                context_policy: Arc::new(DefaultContextPolicy),
                session_store: Some(build.session_store),
                hooks: self.hooks.clone().unwrap_or(build.hooks),
            })
        })
    }
    fn close<'a>(&'a self, _: &'a str) -> BoxFuture<'a, Result<(), ServiceError>> {
        Box::pin(async move {
            self.closes.fetch_add(1, Ordering::SeqCst);
            if let Some(gate) = &self.close_gate {
                gate.acquire().await.unwrap().forget();
            }
            assert!(!self.close_panics, "模拟工厂关闭崩溃");
            if let Some(message) = &self.close_error {
                return Err(ServiceError::new("close_error", message));
            }
            Ok(())
        })
    }
}

struct RecordingHooks {
    model_calls: AtomicUsize,
}

impl Default for RecordingHooks {
    fn default() -> Self {
        Self {
            model_calls: AtomicUsize::new(0),
        }
    }
}

impl qcode_harness::application::port::RunHooks for RecordingHooks {
    fn before_model_call<'a>(&'a self, _: &'a ModelRequest<'a>) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            self.model_calls.fetch_add(1, Ordering::SeqCst);
        })
    }
}

async fn open(repo: &Repo, factory: Arc<Factory>, budget: u32) -> LocalTaskService {
    LocalTaskService::open(
        repo.0.clone(),
        factory,
        Budget::new(budget),
        Arc::new(Accept),
    )
    .await
    .unwrap()
}

async fn create(service: &LocalTaskService, id: &str) -> TaskView {
    let ServiceReply::Task { task } = service
        .call(
            id,
            ServiceCommand::Create {
                goal: "修复问题".to_owned(),
                kind: TaskKind::BugFix,
            },
        )
        .await
        .unwrap()
    else {
        panic!("应返回任务")
    };
    task
}

async fn get(service: &LocalTaskService, id: &str) -> TaskView {
    let ServiceReply::Task { task } = service
        .call(
            "query",
            ServiceCommand::Get {
                task_id: id.to_owned(),
            },
        )
        .await
        .unwrap()
    else {
        panic!("应返回任务")
    };
    task
}

async fn stopped(service: &LocalTaskService, id: &str) -> TaskView {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let task = get(service, id).await;
            if !matches!(task.status, TaskStatus::Running | TaskStatus::Stopping) {
                return task;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("任务应收束")
}

#[tokio::test]
async fn simultaneous_tasks_have_distinct_workspaces_and_create_receipts_are_durable() {
    let repo = Repo::new("concurrent");
    let model = Arc::new(Model::new());
    let factory = Arc::new(Factory::new(model.clone()));
    let service = open(&repo, factory.clone(), 10).await;
    let first = create(&service, "create-1").await;
    let repeated = create(&service, "create-1").await;
    assert_eq!(repeated, first);
    let second = create(&service, "create-2").await;
    model.wait_calls(2).await;
    assert_ne!(first.task_id, second.task_id);
    assert_ne!(first.session_id, second.session_id);
    assert_ne!(first.workspace, second.workspace);
    assert_ne!(first.workspace, repo.0);
    assert_eq!(factory.builds.load(Ordering::SeqCst), 2);
    let conflict = service
        .call(
            "create-1",
            ServiceCommand::Cancel {
                task_id: first.task_id.clone(),
            },
        )
        .await
        .unwrap_err();
    assert_eq!(conflict.code, "request_conflict");
    assert!(
        tokio::time::timeout(
            Duration::from_secs(1),
            service.call("list", ServiceCommand::List)
        )
        .await
        .unwrap()
        .is_ok()
    );
    model.permits.add_permits(8);
    assert_eq!(
        stopped(&service, &first.task_id).await.status,
        TaskStatus::Completed
    );
    assert_eq!(
        stopped(&service, &second.task_id).await.status,
        TaskStatus::Completed
    );
    service
        .call("shutdown", ServiceCommand::Shutdown)
        .await
        .unwrap();
    drop(service);
    let reopened = open(&repo, factory.clone(), 10).await;
    assert_eq!(create(&reopened, "create-1").await, first);
    assert_eq!(factory.builds.load(Ordering::SeqCst), 2);
    reopened
        .call("shutdown-2", ServiceCommand::Shutdown)
        .await
        .unwrap();
}

#[tokio::test]
async fn cancelling_an_inflight_model_then_continuing_uses_a_fresh_execution_and_budget() {
    let repo = Repo::new("cancel");
    let model = Arc::new(Model::new());
    let factory = Arc::new(Factory::new(model.clone()));
    let service = open(&repo, factory.clone(), 10).await;
    let first = create(&service, "create").await;
    model.wait_calls(1).await;
    service
        .call(
            "cancel",
            ServiceCommand::Cancel {
                task_id: first.task_id.clone(),
            },
        )
        .await
        .unwrap();
    assert_eq!(
        get(&service, &first.task_id).await.status,
        TaskStatus::Stopping
    );
    model.permits.add_permits(1);
    assert_eq!(
        stopped(&service, &first.task_id).await.status,
        TaskStatus::Cancelled
    );
    let ServiceReply::Task { task: continued } = service
        .call(
            "continue",
            ServiceCommand::Continue {
                task_id: first.task_id.clone(),
            },
        )
        .await
        .unwrap()
    else {
        panic!("应返回新执行")
    };
    assert_eq!(continued.session_id, first.session_id);
    assert_ne!(continued.execution_id, first.execution_id);
    assert_eq!(continued.attempt, 2);
    model.permits.add_permits(4);
    assert_eq!(
        stopped(&service, &first.task_id).await.status,
        TaskStatus::Completed
    );
    assert_eq!(factory.builds.load(Ordering::SeqCst), 2);
    service
        .call("shutdown", ServiceCommand::Shutdown)
        .await
        .unwrap();
}

#[tokio::test]
async fn mutation_receipts_compare_the_entire_command_and_preserve_shutdown_reply() {
    let repo = Repo::new("receipts");
    let model = Arc::new(Model::new());
    let factory = Arc::new(Factory::new(model.clone()));
    let service = open(&repo, factory.clone(), 10).await;
    service.call("create", ServiceCommand::List).await.unwrap();
    let task = create(&service, "create").await;
    let changed = service
        .call(
            "create",
            ServiceCommand::Create {
                goal: "另一目标".to_owned(),
                kind: TaskKind::BugFix,
            },
        )
        .await;
    service
        .call(
            "cancel",
            ServiceCommand::Cancel {
                task_id: task.task_id.clone(),
            },
        )
        .await
        .unwrap();
    let reused = service
        .call(
            "cancel",
            ServiceCommand::Cleanup {
                task_id: task.task_id.clone(),
            },
        )
        .await;
    model.permits.add_permits(8);
    let shutdown = service
        .call("shutdown", ServiceCommand::Shutdown)
        .await
        .unwrap();
    let repeated = service
        .call("shutdown", ServiceCommand::Shutdown)
        .await
        .unwrap();
    drop(service);
    let reopened = open(&repo, factory, 10).await;
    let durable = reopened
        .call("shutdown", ServiceCommand::Shutdown)
        .await
        .unwrap();
    reopened
        .call("close-reopened", ServiceCommand::Shutdown)
        .await
        .unwrap();
    assert_eq!(changed.unwrap_err().code, "request_conflict");
    assert_eq!(reused.unwrap_err().code, "request_conflict");
    assert_eq!(shutdown, ServiceReply::Shutdown);
    assert_eq!(repeated, shutdown);
    assert_eq!(durable, shutdown);
}

#[tokio::test]
async fn concurrent_same_request_creates_only_one_task_and_one_execution() {
    let repo = Repo::new("same-request");
    let model = Arc::new(Model::new());
    let factory = Arc::new(Factory::new(model.clone()));
    let service = open(&repo, factory.clone(), 10).await;
    let (first, second) = tokio::join!(create(&service, "same"), create(&service, "same"));
    model.permits.add_permits(16);
    service
        .call("shutdown", ServiceCommand::Shutdown)
        .await
        .unwrap();
    assert_eq!(first, second);
    assert_eq!(factory.builds.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn completed_tasks_queue_durable_inputs_and_message_ids_are_content_checked() {
    let repo = Repo::new("input");
    let model = Arc::new(Model::new());
    let factory = Arc::new(Factory::new(model.clone()));
    let service = open(&repo, factory.clone(), 10).await;
    let task = create(&service, "create").await;
    model.permits.add_permits(4);
    assert_eq!(
        stopped(&service, &task.task_id).await.status,
        TaskStatus::Completed
    );
    let input = ServiceCommand::Input {
        task_id: task.task_id.clone(),
        message_id: "message-1".to_owned(),
        text: "保留此补充约束".to_owned(),
    };
    let accepted = service.call("input-1", input.clone()).await;
    let duplicate = service.call("input-2", input.clone()).await;
    let conflict = service
        .call(
            "input-3",
            ServiceCommand::Input {
                task_id: task.task_id.clone(),
                message_id: "message-1".to_owned(),
                text: "改写原输入".to_owned(),
            },
        )
        .await;
    service
        .call("shutdown", ServiceCommand::Shutdown)
        .await
        .unwrap();
    drop(service);
    let reopened = open(&repo, factory, 10).await;
    assert!(accepted.is_ok(), "已停止任务也应受理输入: {accepted:?}");
    assert_eq!(duplicate, accepted);
    assert_eq!(conflict.unwrap_err().code, "input_conflict");
    reopened
        .call(
            "continue",
            ServiceCommand::Continue {
                task_id: task.task_id.clone(),
            },
        )
        .await
        .unwrap();
    model.wait_calls(5).await;
    assert!(model.histories.lock().unwrap()[4].contains("保留此补充约束"));
    model.permits.add_permits(1);
    assert_eq!(
        stopped(&reopened, &task.task_id).await.status,
        TaskStatus::Completed
    );
    let ServiceReply::Events { events, .. } = reopened
        .call(
            "events",
            ServiceCommand::Events {
                after: 0,
                limit: 1000,
            },
        )
        .await
        .unwrap()
    else {
        panic!("应返回事件")
    };
    assert_eq!(events.iter().filter(|event| matches!(&event.payload, qcode_harness::domain::service::ServiceEventPayload::InputReceived { message_id, .. } if message_id == "message-1")).count(), 1);
    assert_eq!(events.iter().filter(|event| matches!(&event.payload, qcode_harness::domain::service::ServiceEventPayload::InputApplied { message_id } if message_id == "message-1")).count(), 1);
    reopened
        .call("shutdown-2", ServiceCommand::Shutdown)
        .await
        .unwrap();
}

#[tokio::test]
async fn configured_roster_hooks_and_close_failures_are_faithful() {
    let repo = Repo::new("assembly");
    let model = Arc::new(Model::new());
    let hooks = Arc::new(RecordingHooks::default());
    let factory = Factory::new(model.clone()).configure(|factory| {
        factory.roster =
            AgentRoster::builtin().with_instructions_section("测试标题", "使用测试用语回答");
        factory.hooks = Some(hooks.clone());
    });
    assert!(
        factory
            .roster
            .find(qcode_harness::domain::agent::AgentRole::Coder)
            .unwrap()
            .system_prompt
            .contains("使用测试用语回答")
    );
    let service = open(&repo, factory.clone(), 10).await;
    let task = create(&service, "create").await;
    model.wait_calls(1).await;
    model.permits.add_permits(4);
    assert_eq!(
        stopped(&service, &task.task_id).await.status,
        TaskStatus::Completed
    );
    assert_eq!(
        model.roles.lock().unwrap()[0],
        qcode_harness::domain::agent::AgentRole::Planner
    );
    assert!(
        model.prompts.lock().unwrap()[0].contains("使用测试用语回答"),
        "提示词应来自花名册"
    );
    assert!(hooks.model_calls.load(Ordering::SeqCst) >= 1);
    factory.wait_closes(1).await;
    service
        .call("shutdown", ServiceCommand::Shutdown)
        .await
        .unwrap();
}

#[tokio::test]
async fn factory_close_errors_surface_in_the_shutdown_reply_and_journal_close() {
    let repo = Repo::new("close-error");
    let model = Arc::new(Model::new());
    let factory = Factory::new(model.clone()).configure(|factory| {
        factory.close_error = Some("下游断连".to_owned());
    });
    let service = open(&repo, factory.clone(), 10).await;
    let task = create(&service, "create").await;
    model.permits.add_permits(4);
    assert_eq!(
        stopped(&service, &task.task_id).await.status,
        TaskStatus::Failed
    );
    let view = get(&service, &task.task_id).await;
    assert!(
        view.error
            .as_deref()
            .unwrap_or_default()
            .contains("下游断连"),
        "{view:?}"
    );
    factory.wait_closes(1).await;
    let shutdown = service.call("shutdown", ServiceCommand::Shutdown).await;
    assert!(shutdown.unwrap_err().message.contains("下游断连"));
    // 租约已随 Shutdown 释放，错误也不能挡住重开。
    let reopened = open(&repo, factory.clone(), 10).await;
    reopened
        .call("close-reopened", ServiceCommand::Shutdown)
        .await
        .unwrap();
}

#[tokio::test]
async fn dropping_a_service_keeps_its_lease_until_execution_resources_close() {
    let repo = Repo::new("drop");
    let model = Arc::new(Model::new());
    let close_gate = Arc::new(Semaphore::new(0));
    let factory = Factory::new(model.clone()).configure(|factory| {
        factory.close_gate = Some(close_gate.clone());
    });
    let service = open(&repo, factory.clone(), 10).await;
    let task = create(&service, "create").await;
    model.wait_calls(1).await;
    let mut events = service.subscribe(0).unwrap();
    drop(service);
    model.permits.add_permits(1);
    factory.wait_closes(1).await;
    let competing = LocalTaskService::open(
        repo.0.clone(),
        factory.clone(),
        Budget::new(10),
        Arc::new(Accept),
    )
    .await;
    assert!(matches!(competing, Err(ref error) if error.code == "service_locked"));
    close_gate.add_permits(1);
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match events.next().await {
                Ok(_) => {}
                Err(error) => {
                    assert_eq!(error.code, "journal_closed");
                    break;
                }
            }
        }
    })
    .await
    .unwrap();
    let reopened = open(&repo, factory.clone(), 10).await;
    assert_eq!(
        get(&reopened, &task.task_id).await.status,
        TaskStatus::Cancelled
    );
    assert_eq!(factory.closes.load(Ordering::SeqCst), 1);
    reopened
        .call("shutdown", ServiceCommand::Shutdown)
        .await
        .unwrap();
}

#[tokio::test]
async fn forking_a_completed_task_preserves_history_progress_and_origin() {
    let repo = Repo::new("fork");
    let model = Arc::new(Model::new());
    let factory = Arc::new(Factory::new(model.clone()));
    let service = open(&repo, factory.clone(), 10).await;
    let task = create(&service, "create").await;
    model.permits.add_permits(4);
    assert_eq!(
        stopped(&service, &task.task_id).await.status,
        TaskStatus::Completed
    );
    let ServiceReply::Task { task: forked } = service
        .call(
            "fork",
            ServiceCommand::Fork {
                task_id: task.task_id.clone(),
                goal: Some("继续处理".to_owned()),
            },
        )
        .await
        .unwrap()
    else {
        panic!("应分支")
    };
    model.wait_calls(5).await;
    assert_ne!(forked.task_id, task.task_id);
    assert_ne!(forked.session_id, task.session_id);
    assert_eq!(forked.source_task.as_deref(), Some(task.task_id.as_str()));
    assert_eq!(forked.parent_task.as_deref(), Some(task.task_id.as_str()));
    model.permits.add_permits(1);
    assert_eq!(
        stopped(&service, &forked.task_id).await.status,
        TaskStatus::Completed
    );
    assert_eq!(
        model.roles.lock().unwrap()[4],
        qcode_harness::domain::agent::AgentRole::Coder,
        "分支应保留任务进度"
    );
    assert!(model.histories.lock().unwrap()[4].contains("完成"));
    assert!(model.histories.lock().unwrap()[4].contains("继续处理"));
    let ServiceReply::Events { events, .. } = service
        .call(
            "events",
            ServiceCommand::Events {
                after: 0,
                limit: 1000,
            },
        )
        .await
        .unwrap()
    else {
        panic!("应返回事件")
    };
    let forked_context = events
        .iter()
        .find(|event| event.task_id.as_deref() == Some(forked.task_id.as_str()))
        .unwrap();
    assert_eq!(
        forked_context.origin.parent_session_id.as_deref(),
        Some(task.session_id.as_str())
    );
    // 原任务的工作区与会话快照必须原样保留。
    assert!(std::path::Path::new(&task.workspace).exists());
    assert!(
        repo.0
            .join(".qcodeharness/snapshots")
            .read_dir()
            .unwrap()
            .count()
            >= 2
    );
    service
        .call("shutdown", ServiceCommand::Shutdown)
        .await
        .unwrap();
}

#[tokio::test]
async fn forking_a_stopped_task_preserves_steps_history_and_workspace_contents() {
    let repo = Repo::new("fork-progress");
    let model = Arc::new(Model::new());
    let factory = Arc::new(Factory::new(model.clone()));
    let service = open(&repo, factory, 1).await;
    let task = create(&service, "create").await;
    model.wait_calls(1).await;
    fs::write(task.workspace.join("file.txt"), "任务中产生的改动\n").unwrap();
    model.permits.add_permits(1);
    let source_view = stopped(&service, &task.task_id).await;
    assert_eq!(source_view.status, TaskStatus::BudgetExhausted);
    let source_task_path = repo
        .0
        .join(".qcodeharness/tasks")
        .join(format!("{}.json", task.task_id));
    let source_session_path = repo
        .0
        .join(".qcodeharness/snapshots")
        .join(format!("{}.json", task.session_id));
    let source_task = fs::read(&source_task_path).unwrap();
    let source_session = fs::read(&source_session_path).unwrap();
    let ServiceReply::Task { task: forked } = service
        .call(
            "fork",
            ServiceCommand::Fork {
                task_id: task.task_id.clone(),
                goal: Some("保留已有修复".to_owned()),
            },
        )
        .await
        .unwrap()
    else {
        panic!("应分支")
    };
    assert_eq!(forked.consumption, source_view.consumption);
    model.wait_calls(2).await;
    assert_eq!(
        fs::read(forked.workspace.join("file.txt")).unwrap(),
        fs::read(task.workspace.join("file.txt")).unwrap()
    );
    assert_eq!(
        model.roles.lock().unwrap()[1],
        qcode_harness::domain::agent::AgentRole::Coder
    );
    assert!(model.histories.lock().unwrap()[1].contains("完成"));
    assert!(model.histories.lock().unwrap()[1].contains("保留已有修复"));
    model.permits.add_permits(1);
    assert_eq!(
        stopped(&service, &forked.task_id).await.status,
        TaskStatus::BudgetExhausted
    );
    let fork_snapshot: qcode_harness::application::snapshot::TaskSnapshot = serde_json::from_slice(
        &fs::read(
            repo.0
                .join(".qcodeharness/tasks")
                .join(format!("{}.json", forked.task_id)),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(fork_snapshot.task_id().as_str(), forked.task_id);
    assert_eq!(fork_snapshot.run.session_id.as_str(), forked.session_id);
    assert_eq!(fork_snapshot.completed_outputs.len(), 2);
    assert_eq!(fs::read(source_task_path).unwrap(), source_task);
    assert_eq!(fs::read(source_session_path).unwrap(), source_session);
    service
        .call("shutdown", ServiceCommand::Shutdown)
        .await
        .unwrap();
}

#[tokio::test]
async fn terminal_status_waits_for_resource_close_and_allows_immediate_continuation() {
    let repo = Repo::new("terminal-window");
    let model = Arc::new(Model::new());
    let close_gate = Arc::new(Semaphore::new(0));
    let factory = Factory::new(model.clone()).configure(|factory| {
        factory.close_gate = Some(close_gate.clone());
    });
    let service = open(&repo, factory.clone(), 10).await;
    let task = create(&service, "create").await;
    model.permits.add_permits(4);
    factory.wait_closes(1).await;
    assert_eq!(
        get(&service, &task.task_id).await.status,
        TaskStatus::Running
    );
    close_gate.add_permits(1);
    assert_eq!(
        stopped(&service, &task.task_id).await.status,
        TaskStatus::Completed
    );
    let continued = service
        .call(
            "continue-check",
            ServiceCommand::Continue {
                task_id: task.task_id.clone(),
            },
        )
        .await;
    assert!(
        continued.is_ok(),
        "终态可见后执行表应已释放任务: {continued:?}"
    );
    model.permits.add_permits(1);
    close_gate.add_permits(1);
    assert_eq!(
        stopped(&service, &task.task_id).await.status,
        TaskStatus::Completed
    );
    service
        .call("shutdown", ServiceCommand::Shutdown)
        .await
        .unwrap();
}

#[tokio::test]
async fn shutdown_completion_reports_resource_failures_and_waits_for_close() {
    let repo = Repo::new("close");
    let model = Arc::new(Model::new());
    let close_gate = Arc::new(Semaphore::new(0));
    let factory = Factory::new(model.clone()).configure(|factory| {
        factory.close_gate = Some(close_gate.clone());
        factory.close_error = Some("扩展会话泄漏".to_owned());
    });
    let service = Arc::new(open(&repo, factory.clone(), 10).await);
    create(&service, "create").await;
    model.wait_calls(1).await;
    let shutdown_service = service.clone();
    let shutdown = tokio::spawn(async move {
        shutdown_service
            .call("shutdown", ServiceCommand::Shutdown)
            .await
    });
    model.permits.add_permits(1);
    factory.wait_closes(1).await;
    assert!(!shutdown.is_finished(), "资源关闭完成前关闭回复不应返回");
    close_gate.add_permits(1);
    let reply = shutdown.await.unwrap().unwrap_err();
    assert!(
        reply.message.contains("扩展会话泄漏"),
        "关闭错误必须如实上报: {reply}"
    );
    let reopened = open(&repo, factory, 10).await;
    reopened
        .call("close-reopened", ServiceCommand::Shutdown)
        .await
        .unwrap();
}

struct CrashProcess(std::process::Child);

impl Drop for CrashProcess {
    fn drop(&mut self) {
        if self.0.try_wait().unwrap().is_none() {
            self.0.kill().unwrap();
        }
        self.0.wait().unwrap();
    }
}

#[tokio::test]
#[ignore = "由崩溃恢复测试在独立进程中启动"]
async fn crash_service_child() {
    let root = PathBuf::from(std::env::var_os("QCH_SERVICE_CRASH_ROOT").unwrap());
    let model = Arc::new(Model::new());
    let factory = Arc::new(Factory::new(model.clone()));
    let service = LocalTaskService::open(root.clone(), factory, Budget::new(10), Arc::new(Accept))
        .await
        .unwrap();
    let task = create(&service, "create").await;
    model.wait_calls(1).await;
    if std::env::var_os("QCH_SERVICE_CRASH_STOPPING").is_some() {
        service
            .call(
                "cancel",
                ServiceCommand::Cancel {
                    task_id: task.task_id.clone(),
                },
            )
            .await
            .unwrap();
    }
    let ready = root.join(".qcodeharness/service/crash-ready.json");
    let temp = ready.with_extension("tmp");
    fs::write(&temp, serde_json::to_vec(&task).unwrap()).unwrap();
    fs::rename(temp, ready).unwrap();
    std::future::pending::<()>().await;
}

#[tokio::test]
async fn reopening_after_a_crash_marks_interrupted_and_does_not_reexecute() {
    for stopping in [false, true] {
        let repo = Repo::new(if stopping {
            "crash-stopping"
        } else {
            "crash-running"
        });
        let ready = repo.0.join(".qcodeharness/service/crash-ready.json");
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", "crash_service_child", "--ignored", "--nocapture"])
            .env("QCH_SERVICE_CRASH_ROOT", &repo.0)
            .env_remove("QCH_SERVICE_CRASH_STOPPING")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit());
        if stopping {
            command.env("QCH_SERVICE_CRASH_STOPPING", "1");
        }
        let mut process = CrashProcess(command.spawn().unwrap());
        tokio::time::timeout(Duration::from_secs(20), async {
            while !ready.exists() {
                assert!(
                    process.0.try_wait().unwrap().is_none(),
                    "崩溃夹具应先写就绪标记"
                );
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let task: TaskView = serde_json::from_slice(&fs::read(&ready).unwrap()).unwrap();
        drop(process);
        let model = Arc::new(Model::new());
        let factory = Arc::new(Factory::new(model.clone()));
        let reopened = open(&repo, factory.clone(), 10).await;
        assert_eq!(
            get(&reopened, &task.task_id).await.status,
            TaskStatus::Interrupted
        );
        assert_eq!(
            factory.builds.load(Ordering::SeqCst),
            0,
            "重启不得自动重放执行"
        );
        assert_eq!(
            model.calls.load(Ordering::SeqCst),
            0,
            "重启不得再次调用模型"
        );
        assert_eq!(
            create(&reopened, "create").await,
            task,
            "创建凭据跨崩溃保留原始回复"
        );
        let ServiceReply::Task { task: continued } = reopened
            .call(
                "continue",
                ServiceCommand::Continue {
                    task_id: task.task_id.clone(),
                },
            )
            .await
            .unwrap()
        else {
            panic!("应继续")
        };
        assert_eq!(continued.session_id, task.session_id);
        assert_ne!(continued.execution_id, task.execution_id);
        assert_eq!(continued.attempt, 2);
        model.permits.add_permits(4);
        assert_eq!(
            stopped(&reopened, &task.task_id).await.status,
            TaskStatus::Completed
        );
        reopened
            .call("shutdown", ServiceCommand::Shutdown)
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn continuing_a_continued_workspace_is_rejected_until_the_digest_matches() {
    let repo = Repo::new("digest");
    let model = Arc::new(Model::new());
    let factory = Arc::new(Factory::new(model.clone()));
    let service = open(&repo, factory.clone(), 10).await;
    let task = create(&service, "create").await;
    model.permits.add_permits(4);
    assert_eq!(
        stopped(&service, &task.task_id).await.status,
        TaskStatus::Completed
    );
    fs::write(task.workspace.join("extra.txt"), "外部改动\n").unwrap();
    let refused = service
        .call(
            "continue-1",
            ServiceCommand::Continue {
                task_id: task.task_id.clone(),
            },
        )
        .await
        .unwrap_err();
    assert_eq!(refused.code, "workspace_changed");
    fs::remove_file(task.workspace.join("extra.txt")).unwrap();
    let ServiceReply::Task { task: continued } = service
        .call(
            "continue-2",
            ServiceCommand::Continue {
                task_id: task.task_id.clone(),
            },
        )
        .await
        .unwrap()
    else {
        panic!("应恢复执行")
    };
    assert_eq!(continued.status, TaskStatus::Running);
    model.wait_calls(5).await;
    model.permits.add_permits(4);
    assert_eq!(
        stopped(&service, &task.task_id).await.status,
        TaskStatus::Completed
    );
    let ServiceReply::Task { task: forked } = service
        .call(
            "fork",
            ServiceCommand::Fork {
                task_id: task.task_id.clone(),
                goal: None,
            },
        )
        .await
        .unwrap()
    else {
        panic!("应分支出任务")
    };
    assert_eq!(forked.parent_task.as_deref(), Some(task.task_id.as_str()));
    assert_ne!(forked.session_id, task.session_id);
    assert_ne!(forked.task_id, task.task_id);
    model.wait_calls(6).await;
    model.permits.add_permits(8);
    assert_eq!(
        stopped(&service, &forked.task_id).await.status,
        TaskStatus::Completed
    );
    let ServiceReply::Events { events, .. } = service
        .call(
            "events",
            ServiceCommand::Events {
                after: 0,
                limit: 1000,
            },
        )
        .await
        .unwrap()
    else {
        panic!("应返回事件")
    };
    let origin = events
        .iter()
        .find(|event| {
            event.task_id.as_deref() == Some(forked.task_id.as_str())
                && matches!(
                    &event.payload,
                    qcode_harness::domain::service::ServiceEventPayload::TaskCreated { .. }
                )
        })
        .map(|event| event.origin.parent_session_id.clone())
        .flatten();
    assert_eq!(
        origin.as_deref(),
        Some(task.session_id.as_str()),
        "分支出任务的父会话应是源任务会话"
    );
    service
        .call("shutdown", ServiceCommand::Shutdown)
        .await
        .unwrap();
}

#[tokio::test]
async fn a_failed_input_snapshot_is_not_acknowledged_and_can_be_continued_once() {
    let repo = Repo::new("input-save-error");
    let model = Arc::new(Model::new());
    let factory = Arc::new(Factory::new(model.clone()));
    let service = open(&repo, factory.clone(), 10).await;
    let task = create(&service, "create").await;
    service
        .call(
            "input",
            ServiceCommand::Input {
                task_id: task.task_id.clone(),
                message_id: "durable-message".to_owned(),
                text: "不得丢失的输入".to_owned(),
            },
        )
        .await
        .unwrap();
    let blocked_temp = repo
        .0
        .join(".qcodeharness/snapshots")
        .join(format!("{}.json.tmp", task.session_id));
    fs::create_dir_all(&blocked_temp).unwrap();
    let failed = stopped(&service, &task.task_id).await;
    assert_eq!(failed.status, TaskStatus::Failed);
    assert_eq!(
        model.calls.load(Ordering::SeqCst),
        0,
        "输入未保存前不能询问模型"
    );
    let ServiceReply::Events { events, .. } = service
        .call(
            "events",
            ServiceCommand::Events {
                after: 0,
                limit: 1000,
            },
        )
        .await
        .unwrap()
    else {
        panic!("应返回事件")
    };
    assert!(!events.iter().any(|event| matches!(
        event.payload,
        qcode_harness::domain::service::ServiceEventPayload::InputApplied { .. }
    )));
    fs::remove_dir(&blocked_temp).unwrap();
    assert!(
        service
            .call("shutdown", ServiceCommand::Shutdown)
            .await
            .is_err()
    );
    let reopened = open(&repo, factory, 10).await;
    reopened
        .call(
            "continue",
            ServiceCommand::Continue {
                task_id: task.task_id.clone(),
            },
        )
        .await
        .unwrap();
    model.wait_calls(1).await;
    assert_eq!(
        model.histories.lock().unwrap()[0]
            .matches("不得丢失的输入")
            .count(),
        1
    );
    model.permits.add_permits(4);
    assert_eq!(
        stopped(&reopened, &task.task_id).await.status,
        TaskStatus::Completed
    );
    let ServiceReply::Events { events, .. } = reopened
        .call(
            "events",
            ServiceCommand::Events {
                after: 0,
                limit: 1000,
            },
        )
        .await
        .unwrap()
    else {
        panic!("应返回事件")
    };
    assert_eq!(events.iter().filter(|event| matches!(&event.payload, qcode_harness::domain::service::ServiceEventPayload::InputApplied { message_id } if message_id == "durable-message")).count(), 1);
    reopened
        .call("shutdown-2", ServiceCommand::Shutdown)
        .await
        .unwrap();
}

#[tokio::test]
async fn a_failed_start_event_does_not_leave_a_running_task_or_hide_the_shutdown_error() {
    use std::io::Write;
    let repo = Repo::new("start-event-error");
    let model = Arc::new(Model::new());
    let factory = Arc::new(Factory::new(model));
    let service = open(&repo, factory.clone(), 10).await;
    let journal = repo.0.join(".qcodeharness/service/events.jsonl");
    fs::OpenOptions::new()
        .append(true)
        .open(&journal)
        .unwrap()
        .write_all(b" ")
        .unwrap();
    let command = ServiceCommand::Create {
        goal: "修复问题".to_owned(),
        kind: TaskKind::BugFix,
    };
    let failed = service.call("create", command.clone()).await.unwrap_err();
    assert_eq!(failed.code, "journal_changed");
    assert_eq!(service.call("create", command).await.unwrap_err(), failed);
    let ServiceReply::Tasks { tasks } = service.call("list", ServiceCommand::List).await.unwrap()
    else {
        panic!("应返回任务")
    };
    let shutdown = service.call("shutdown", ServiceCommand::Shutdown).await;
    assert_eq!(tasks.len(), 1);
    assert_eq!(
        tasks[0].status,
        TaskStatus::Failed,
        "启动失败不能遗留无人驱动的 Running"
    );
    assert_eq!(factory.builds.load(Ordering::SeqCst), 0);
    assert!(shutdown.unwrap_err().message.contains("journal_changed"));
}

#[tokio::test]
async fn a_failed_fork_checkpoint_removes_its_unadmitted_workspace() {
    let repo = Repo::new("fork-save-error");
    let model = Arc::new(Model::new());
    let factory = Arc::new(Factory::new(model.clone()));
    let service = open(&repo, factory, 10).await;
    let task = create(&service, "create").await;
    model.permits.add_permits(4);
    assert_eq!(
        stopped(&service, &task.task_id).await.status,
        TaskStatus::Completed
    );
    let worktrees = git(&repo.0, &["worktree", "list", "--porcelain"]).stdout;
    let sessions = repo.0.join(".qcodeharness/snapshots");
    let backup = repo.0.join(".qcodeharness/snapshots-backup");
    fs::rename(&sessions, &backup).unwrap();
    fs::write(&sessions, "阻断会话保存").unwrap();
    let failed = service
        .call(
            "fork",
            ServiceCommand::Fork {
                task_id: task.task_id.clone(),
                goal: None,
            },
        )
        .await;
    fs::remove_file(&sessions).unwrap();
    fs::rename(backup, sessions).unwrap();
    let after = git(&repo.0, &["worktree", "list", "--porcelain"]).stdout;
    service
        .call("shutdown", ServiceCommand::Shutdown)
        .await
        .unwrap();
    assert_eq!(failed.unwrap_err().code, "checkpoint_failed");
    assert_eq!(
        after, worktrees,
        "检查点保存失败不能遗留未受理的 Git 工作树"
    );
}

#[tokio::test]
async fn model_and_factory_panics_still_close_resources_and_publish_failure() {
    for close_panics in [false, true] {
        let repo = Repo::new(if close_panics {
            "close-panic"
        } else {
            "model-panic"
        });
        let model = Arc::new(Model {
            after_reply: (!close_panics)
                .then(|| Box::new(|_| -> () { panic!("模拟模型崩溃") }) as Box<dyn Fn(usize) + Send + Sync>),
            ..Model::new()
        });
        let factory = Factory::new(model.clone()).configure(|factory| {
            factory.close_panics = close_panics;
        });
        let service = open(&repo, factory.clone(), 10).await;
        let task = create(&service, "create").await;
        model.permits.add_permits(4);
        assert_eq!(
            stopped(&service, &task.task_id).await.status,
            TaskStatus::Failed
        );
        assert_eq!(factory.closes.load(Ordering::SeqCst), 1);
        let code = if close_panics {
            "factory_close_panicked"
        } else {
            "execution_panicked"
        };
        assert_eq!(
            service
                .call("shutdown", ServiceCommand::Shutdown)
                .await
                .unwrap_err()
                .code,
            code
        );
        let reopened = open(&repo, factory, 10).await;
        reopened
            .call("shutdown-2", ServiceCommand::Shutdown)
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn terminal_persistence_errors_remove_the_answer_and_release_the_live_entry() {
    let repo = Repo::new("completion-save-error");
    let blocked_temp = repo.0.join(".qcodeharness/service/state.json.tmp");
    let sabotage = blocked_temp.clone();
    let model = Arc::new(Model {
        after_reply: Some(Box::new(move |index| {
            if index == 3 {
                fs::create_dir(&sabotage).unwrap();
            }
        })),
        ..Model::new()
    });
    let factory = Arc::new(Factory::new(model.clone()));
    let service = open(&repo, factory.clone(), 10).await;
    let task = create(&service, "create").await;
    model.permits.add_permits(4);
    let failed = stopped(&service, &task.task_id).await;
    fs::remove_dir(blocked_temp).unwrap();
    let shutdown = service.call("shutdown", ServiceCommand::Shutdown).await;
    assert_eq!(failed.status, TaskStatus::Failed);
    assert!(failed.answer.is_none(), "终态无法保存时不能保留成功答案");
    assert!(shutdown.is_err());
    let reopened = open(&repo, factory, 10).await;
    reopened
        .call(
            "continue",
            ServiceCommand::Continue {
                task_id: task.task_id.clone(),
            },
        )
        .await
        .unwrap();
    model.permits.add_permits(1);
    assert_eq!(
        stopped(&reopened, &task.task_id).await.status,
        TaskStatus::Completed
    );
    reopened
        .call("shutdown-2", ServiceCommand::Shutdown)
        .await
        .unwrap();
}

#[tokio::test]
async fn exclusive_service_lock_is_released_after_shutdown_and_dirty_parent_refuses_merge() {
    let repo = Repo::new("lock");
    let model = Arc::new(Model::new());
    let factory = Arc::new(Factory::new(model.clone()));
    let service = open(&repo, factory.clone(), 10).await;
    assert!(
        LocalTaskService::open(
            repo.0.clone(),
            factory.clone(),
            Budget::new(10),
            Arc::new(Accept)
        )
        .await
        .is_err()
    );
    let task = create(&service, "create").await;
    model.permits.add_permits(4);
    stopped(&service, &task.task_id).await;
    fs::write(repo.0.join("file.txt"), "用户改动\n").unwrap();
    assert!(
        service
            .call(
                "merge",
                ServiceCommand::Merge {
                    task_id: task.task_id.clone()
                }
            )
            .await
            .is_err()
    );
    assert_eq!(
        fs::read_to_string(repo.0.join("file.txt")).unwrap(),
        "用户改动\n"
    );
    service
        .call(
            "cleanup",
            ServiceCommand::Cleanup {
                task_id: task.task_id.clone(),
            },
        )
        .await
        .unwrap();
    assert_eq!(
        get(&service, &task.task_id).await.status,
        TaskStatus::Cleaned
    );
    assert!(!task.workspace.exists());
    service
        .call("shutdown", ServiceCommand::Shutdown)
        .await
        .unwrap();
    let reopened = open(&repo, factory, 10).await;
    assert_eq!(
        get(&reopened, &task.task_id).await.status,
        TaskStatus::Cleaned
    );
    reopened
        .call("shutdown-2", ServiceCommand::Shutdown)
        .await
        .unwrap();
}

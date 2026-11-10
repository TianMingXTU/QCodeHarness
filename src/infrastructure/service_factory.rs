//! 常驻服务的生产装配：每次执行重建工作区工具、扩展与外部连接。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::Mutex as AsyncMutex;

use crate::application::agent_roster::AgentRoster;
use crate::application::agent_run::AgentDependencies;
use crate::application::context_policy::DefaultContextPolicy;
use crate::application::delegation::ChildAgentFactory;
use crate::application::isolation::{IsolationWorkspace, WorkspaceVerifier};
use crate::application::mcp_lifecycle::McpLifecycleRecorder;
use crate::application::port::{
    ApprovalGate, ApprovalRequest, BoxFuture, CancelHandle, Decision, EvidenceSink, ModelGateway,
    ModelReply, ModelRequest, RunHooks, TimeLimiter, Timestamp,
};
use crate::application::service::EventJournal;
use crate::application::service_factory::{ApprovalFactory, TaskBuild, TaskFactory};
use crate::application::snapshot::SessionStore;
use crate::application::tool_registry::{ToolRegistry, ToolSource};
use crate::domain::agent::AgentRole;
use crate::domain::budget::Budget;
use crate::domain::config::AppConfig;
use crate::domain::context::ContextBudget;
use crate::domain::evidence::AuditEvent;
use crate::domain::ids::SessionId;
use crate::domain::policy::PolicyEngine;
use crate::domain::service::{EventContext, EventOrigin, ServiceError};
use crate::domain::workspace::WorkspaceRoot;
use crate::infrastructure::clock::{SystemClock, TokioTimeLimiter};
use crate::infrastructure::delegation::SubAgentToolSource;
use crate::infrastructure::extensions::LocalExtensionStore;
use crate::infrastructure::git_worktree::GitWorktreeManager;
use crate::infrastructure::instructions;
use crate::infrastructure::mcp::{self, McpToolSource, config::McpConfig};
use crate::infrastructure::service_events::ServiceEvidence;
use crate::infrastructure::tool::BuiltinToolSource;

const EXTENSION_TIMEOUT: Duration = Duration::from_secs(120);
const EXTENSION_INSTRUCTIONS_HEADER: &str =
    "──── 扩展指令（来自已启用的 Skill，与上述规则冲突时以项目指令为准）────";

/// 已解析的生产配置。构造工厂不连接模型或 MCP，测试可注入模型替身。
///
/// `extensions` 只读取宿主清单及用户授权；其动作始终绑定本次执行的工具。
/// `roster` 应包含角色模型与档位，项目指令在执行工作区中重新加载。
pub struct ServiceFactoryConfig {
    pub model: Arc<dyn ModelGateway>,
    pub policy: PolicyEngine,
    pub roster: AgentRoster,
    pub context_budget: ContextBudget,
    pub budget: Budget,
    pub user_dir: Option<PathBuf>,
    pub extensions: LocalExtensionStore,
    pub settings: AppConfig,
    pub mcp: McpConfig,
    pub verifier: Arc<dyn WorkspaceVerifier>,
}

#[derive(Default)]
struct ExecutionResources {
    mcp: AsyncMutex<Vec<McpToolSource>>,
    children: Mutex<Vec<Arc<ChildRuntime>>>,
}

/// 供 `LocalTaskService` 使用的真实工厂；资源按执行标识持有并显式关闭。
pub struct ProductionTaskFactory {
    config: Arc<ServiceFactoryConfig>,
    limiter: Arc<dyn TimeLimiter>,
    executions: AsyncMutex<HashMap<String, Arc<ExecutionResources>>>,
}

impl ProductionTaskFactory {
    pub fn new(config: ServiceFactoryConfig) -> Self {
        Self {
            config: Arc::new(config),
            limiter: Arc::new(TokioTimeLimiter::new(EXTENSION_TIMEOUT).expect("扩展时限大于零")),
            executions: AsyncMutex::new(HashMap::new()),
        }
    }

    async fn build_execution(
        &self,
        build: TaskBuild,
        resources: Arc<ExecutionResources>,
    ) -> Result<AgentDependencies, ServiceError> {
        let recorder = Arc::new(McpLifecycleRecorder::new(
            build.evidence.clone(),
            Arc::new(SystemClock),
        ));
        let (mcp, warnings) =
            mcp::connect_all_in(&self.config.mcp, Some(recorder), &build.workspace).await;
        for warning in warnings {
            eprintln!("警告：{warning}");
        }
        // 注册表和收尾清单共享连接状态；装配后半段失败也能主动关闭已连接的进程。
        *resources.mcp.lock().await = mcp.clone();
        let mut sources: Vec<Arc<dyn ToolSource>> = vec![Arc::new(BuiltinToolSource::new(
            WorkspaceRoot::new(&build.workspace),
        ))];
        sources.extend(
            mcp.into_iter()
                .map(|source| Arc::new(source) as Arc<dyn ToolSource>),
        );
        let tools = ToolRegistry::from_sources(&sources);
        let delegate = self.delegation(&build, resources).await;
        let mut deps = assemble(
            &self.config,
            build,
            tools,
            Path::new("."),
            AgentRole::Coder,
            self.limiter.clone(),
        )?;
        if let Some(delegate) = delegate {
            deps.tools = deps
                .tools
                .merged_with(&ToolRegistry::from_sources(&[delegate]));
        }
        Ok(deps)
    }

    async fn delegation(
        &self,
        build: &TaskBuild,
        resources: Arc<ExecutionResources>,
    ) -> Option<Arc<dyn ToolSource>> {
        if self.config.settings.parallel.max_subtasks == 0 {
            return None;
        }
        let manager = match GitWorktreeManager::new(WorkspaceRoot::new(&build.workspace)).await {
            Ok(manager) => manager,
            Err(error) => {
                eprintln!("警告：子任务委派不可用（{error}）");
                return None;
            }
        };
        let child = ChildFactory {
            config: self.config.clone(),
            limiter: self.limiter.clone(),
            parent: build.context.clone(),
            approval_factory: build.approval_factory.clone(),
            session_store: build.session_store.clone(),
            journal: build.journal.clone(),
            resources,
        };
        Some(Arc::new(SubAgentToolSource::new(
            Arc::new(manager),
            Arc::new(child),
            self.config.verifier.clone(),
            self.config.budget,
            self.config.settings.parallel.max_subtasks,
        )))
    }
}

impl TaskFactory for ProductionTaskFactory {
    fn build<'a>(
        &'a self,
        build: TaskBuild,
    ) -> BoxFuture<'a, Result<AgentDependencies, ServiceError>> {
        Box::pin(async move {
            let execution = build.context.execution_id.clone();
            let resources = Arc::new(ExecutionResources::default());
            {
                let mut executions = self.executions.lock().await;
                if executions.contains_key(&execution) {
                    return Err(ServiceError::new(
                        "execution_already_built",
                        "该执行的依赖尚未关闭",
                    ));
                }
                executions.insert(execution.clone(), resources.clone());
            }
            match self.build_execution(build, resources).await {
                Ok(deps) => Ok(deps),
                Err(mut error) => {
                    if let Err(cleanup) = self.close(&execution).await {
                        error
                            .message
                            .push_str(&format!("；资源关闭失败：{cleanup}"));
                    }
                    Err(error)
                }
            }
        })
    }

    fn close<'a>(&'a self, execution: &'a str) -> BoxFuture<'a, Result<(), ServiceError>> {
        Box::pin(async move {
            let resources = self.executions.lock().await.remove(execution);
            let Some(resources) = resources else {
                return Ok(());
            };
            let sources = std::mem::take(&mut *resources.mcp.lock().await);
            for source in sources {
                source.close().await;
            }
            let children = resources.children.lock().map_err(|_| factory_unhealthy())?;
            for child in children.iter() {
                child.evidence.health()?;
            }
            Ok(())
        })
    }
}

fn assemble(
    config: &ServiceFactoryConfig,
    build: TaskBuild,
    tools: ToolRegistry,
    scope: &Path,
    role: AgentRole,
    limiter: Arc<dyn TimeLimiter>,
) -> Result<AgentDependencies, ServiceError> {
    let mut roster = config.roster.clone();
    match instructions::load_layered_instructions(
        config.user_dir.as_deref(),
        &build.workspace,
        scope,
    ) {
        Ok(loaded) => {
            for warning in loaded.warnings {
                eprintln!("警告：{warning}");
            }
            if !loaded.text.is_empty() {
                roster = roster.with_project_instructions(&loaded.text);
            }
        }
        Err(error) => eprintln!("警告：项目指令未加载（{error}）"),
    }
    let extensions = config.extensions.load(&tools).map_err(assembly_error)?;
    let selection = extensions
        .select_optional(
            config.settings.skills.enabled.as_deref(),
            config.settings.hooks.enabled.as_deref(),
        )
        .map_err(assembly_error)?;
    let text = extensions
        .enabled_instructions(&selection)
        .map_err(assembly_error)?;
    if !text.is_empty() {
        roster = roster.with_instructions_section(EXTENSION_INSTRUCTIONS_HEADER, &text);
    }
    roster = roster.with_extension_tools(
        &extensions
            .skill_tool_names(&selection)
            .map_err(assembly_error)?,
    );
    let host = AgentDependencies {
        model: config.model.clone(),
        tools,
        approvals: build.approvals,
        clock: Arc::new(SystemClock),
        policy: config.policy.clone(),
        roster,
        context_budget: config.context_budget,
        evidence: build.evidence,
        session_store: Some(build.session_store),
        hooks: build.hooks,
        context_policy: Arc::new(DefaultContextPolicy),
        additional_guards: Vec::new(),
    };
    // adapters 已转发服务挂点；用 NoopHooks 替换会使模型事件从服务日志消失。
    let adapters = extensions
        .adapters(&host, role, selection, limiter)
        .map_err(assembly_error)?;
    Ok(AgentDependencies {
        tools: host
            .tools
            .merged_with(&ToolRegistry::from_sources(&[adapters.source])),
        hooks: adapters.hooks,
        ..host
    })
}

fn assembly_error(error: impl std::fmt::Display) -> ServiceError {
    ServiceError::new("assembly_failed", error.to_string())
}

fn factory_unhealthy() -> ServiceError {
    ServiceError::new("factory_unhealthy", "子运行资源表锁已损坏")
}

struct ChildFactory {
    config: Arc<ServiceFactoryConfig>,
    limiter: Arc<dyn TimeLimiter>,
    parent: EventContext,
    approval_factory: Arc<dyn ApprovalFactory>,
    session_store: Arc<dyn SessionStore>,
    journal: Arc<dyn EventJournal>,
    resources: Arc<ExecutionResources>,
}

impl ChildAgentFactory for ChildFactory {
    fn dependencies(
        &self,
        workspace: &IsolationWorkspace,
        role: AgentRole,
        session_id: &SessionId,
        scope: &Path,
    ) -> Result<AgentDependencies, String> {
        let context = EventContext {
            task_id: self.parent.task_id.clone(),
            session_id: session_id.as_str().to_owned(),
            execution_id: self.parent.execution_id.clone(),
            origin: EventOrigin {
                parent_session_id: Some(self.parent.session_id.clone()),
                extension_id: None,
            },
        };
        let runtime = Arc::new(ChildRuntime {
            evidence: ServiceEvidence::new(
                self.journal.clone(),
                context.clone(),
                CancelHandle::default(),
            ),
            approvals: self.approval_factory.gate(context.clone()),
            cancel: Mutex::new(CancelHandle::default()),
        });
        self.resources
            .children
            .lock()
            .map_err(|_| factory_unhealthy().to_string())?
            .push(runtime.clone());
        let build = TaskBuild {
            workspace: workspace.root.clone(),
            context,
            evidence: runtime.clone(),
            hooks: runtime.clone(),
            approvals: runtime,
            session_store: self.session_store.clone(),
            journal: self.journal.clone(),
            approval_factory: self.approval_factory.clone(),
        };
        // 子运行重新绑定扩展，绝不复用父工具的 invoker；同步子工厂只提供内置工具。
        let tools = ToolRegistry::from_sources(&[Arc::new(BuiltinToolSource::new(
            WorkspaceRoot::new(&workspace.root),
        ))]);
        assemble(
            &self.config,
            build,
            tools,
            scope,
            role,
            self.limiter.clone(),
        )
        .map_err(|error| error.to_string())
    }
}

/// 子 runner 自己创建取消通道，审批绑定时同时接到证据健康检查。
/// 否则日志失败只能取消一个无运行者持有的占位句柄。
struct ChildRuntime {
    evidence: ServiceEvidence,
    approvals: Arc<dyn ApprovalGate>,
    cancel: Mutex<CancelHandle>,
}

impl ChildRuntime {
    fn check(&self) -> Result<(), String> {
        if let Err(error) = self.evidence.health() {
            self.cancel
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .cancel();
            return Err(error.to_string());
        }
        Ok(())
    }
}

impl ApprovalGate for ChildRuntime {
    fn request<'a>(&'a self, request: &'a ApprovalRequest) -> BoxFuture<'a, Decision> {
        Box::pin(async move {
            self.check()?;
            self.approvals.request(request).await
        })
    }

    fn bind_cancel(&self, cancel: CancelHandle) {
        *self
            .cancel
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = cancel.clone();
        self.approvals.bind_cancel(cancel);
        let _ = self.check();
    }
}

impl EvidenceSink for ChildRuntime {
    fn record(&self, at: Timestamp, event: AuditEvent) {
        self.evidence.record(at, event);
        let _ = self.check();
    }
}

impl RunHooks for ChildRuntime {
    fn before_model_call<'a>(&'a self, request: &'a ModelRequest<'a>) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            self.evidence.before_model_call(request).await;
            let _ = self.check();
        })
    }

    fn after_model_reply<'a>(&'a self, reply: &'a ModelReply, retried: bool) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            self.evidence.after_model_reply(reply, retried).await;
            let _ = self.check();
        })
    }
}

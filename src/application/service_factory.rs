//! 每次执行重新装配依赖；工厂不能把一个任务的可写工作区交给另一个任务。

use std::path::PathBuf;
use std::sync::Arc;

use crate::application::agent_run::AgentDependencies;
use crate::application::port::{ApprovalGate, BoxFuture, EvidenceSink, RunHooks};
use crate::application::service::EventJournal;
use crate::application::snapshot::SessionStore;
use crate::domain::service::{EventContext, ServiceError};

/// 为父执行与子会话绑定独立的审批归属。
pub trait ApprovalFactory: Send + Sync {
    fn gate(&self, context: EventContext) -> Arc<dyn ApprovalGate>;
}

/// 服务提供的执行边界；生产装配负责从当前配置加载模型、角色、策略与扩展。
pub struct TaskBuild {
    pub workspace: PathBuf,
    pub context: EventContext,
    pub evidence: Arc<dyn EvidenceSink>,
    pub hooks: Arc<dyn RunHooks>,
    pub approvals: Arc<dyn ApprovalGate>,
    pub session_store: Arc<dyn SessionStore>,
    pub journal: Arc<dyn EventJournal>,
    pub approval_factory: Arc<dyn ApprovalFactory>,
}

/// 执行依赖的生命周期。`close` 必须等待该执行持有的外部资源完成关闭。
pub trait TaskFactory: Send + Sync {
    fn build<'a>(&'a self, build: TaskBuild) -> BoxFuture<'a, Result<AgentDependencies, ServiceError>>;

    fn close<'a>(&'a self, _execution_id: &'a str) -> BoxFuture<'a, Result<(), ServiceError>> {
        Box::pin(async { Ok(()) })
    }
}

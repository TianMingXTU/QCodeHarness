//! 后台任务服务的公共数据契约。传输层只负责封装这些命令、回复与事件。

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::agent::AgentRole;
use super::budget::Consumption;
use super::evidence::AuditEvent;
use super::task::TaskKind;
use super::tool::{ToolAnnotations, ToolCall};

/// 服务协议版本；不兼容的事件格式必须提升版本并由读取方明确拒绝。
pub const PROTOCOL_VERSION: u32 = 1;

/// 可跨传输层返回的错误。`code` 供程序分支，`message` 供用户诊断。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[error("{code}: {message}")]
pub struct ServiceError {
    pub code: String,
    pub message: String,
}

impl ServiceError {
    /// 构造带稳定错误码的错误；消息不参与调用方的控制流判断。
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self { code: code.into(), message: message.into() }
    }
}

/// 服务可观察的任务状态。一次执行的停止与任务工作区的生命周期分别保留。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Running,
    Stopping,
    Cancelled,
    BudgetExhausted,
    Failed,
    Completed,
    Interrupted,
    Merged,
    Cleaned,
}

/// 任务的只读投影。继续任务时沿用任务与会话标识，另建执行标识并增加次数。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskView {
    pub task_id: String,
    pub session_id: String,
    pub execution_id: String,
    pub attempt: u64,
    pub status: TaskStatus,
    pub workspace: PathBuf,
    pub base_revision: String,
    pub source_task: Option<String>,
    pub parent_task: Option<String>,
    pub answer: Option<String>,
    pub error: Option<String>,
    pub verification: String,
    pub consumption: Consumption,
}

/// 前端提交的命令。请求标识由服务端口单独接收，不混入业务参数。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum ServiceCommand {
    Create { goal: String, kind: TaskKind },
    List,
    Get { task_id: String },
    Input { task_id: String, message_id: String, text: String },
    Approvals { task_id: Option<String> },
    Decide { approval_id: String, task_id: String, execution_id: String, allow: bool },
    Cancel { task_id: String },
    Continue { task_id: String },
    Fork { task_id: String, goal: Option<String> },
    Merge { task_id: String },
    Cleanup { task_id: String },
    Events { after: u64, limit: usize },
    Shutdown,
}

/// 命令的直接回复。`Accepted` 仅表示命令已受理，最终状态通过事件或查询获取。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum ServiceReply {
    Task { task: TaskView },
    Tasks { tasks: Vec<TaskView> },
    Accepted { task_id: String, execution_id: Option<String> },
    PendingApprovals { requests: Vec<PendingApproval> },
    Events { events: Vec<ServiceEvent>, next: u64 },
    Shutdown,
}

/// 子会话与扩展来源，沿事件链保留；不用于推断审批权限。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventOrigin {
    pub parent_session_id: Option<String>,
    pub extension_id: Option<String>,
}

/// 一次执行的事件上下文，由装配方绑定，避免从审计文本猜测任务归属。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventContext {
    pub task_id: String,
    pub session_id: String,
    pub execution_id: String,
    pub origin: EventOrigin,
}

/// 尚未得到结论的单次审批。恢复标记和升级理由不能被会话信任省略。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PendingApproval {
    pub approval_id: String,
    pub task_id: String,
    pub session_id: String,
    pub execution_id: String,
    pub turn: u32,
    pub call: ToolCall,
    pub annotations: ToolAnnotations,
    pub reason: Option<String>,
    pub resumed: bool,
    pub origin: EventOrigin,
}

/// 已提交的服务事件。`sequence` 只由服务日志分配，与旧 evidence 序号独立。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServiceEvent {
    pub version: u32,
    pub sequence: u64,
    pub time_unix_ms: u64,
    pub task_id: Option<String>,
    pub session_id: Option<String>,
    pub execution_id: Option<String>,
    pub origin: EventOrigin,
    pub payload: ServiceEventPayload,
}

/// 事件事实；回放这些数据不会再次调用模型、工具或审批闸门。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServiceEventPayload {
    TaskCreated { task: TaskView },
    ExecutionStarted,
    Audit { event: AuditEvent },
    ModelCallStarted { role: AgentRole },
    ModelCallFinished { text: String, calls: Vec<ToolCall>, retried: bool },
    ApprovalWaiting { request: PendingApproval },
    ApprovalResolved { approval_id: String, allowed: bool, reason: Option<String> },
    InputReceived { message_id: String, text: String },
    InputApplied { message_id: String },
    CancelRequested,
    Stopped { status: TaskStatus, error: Option<String> },
    Completed { answer: String },
    Forked { source_task: String },
    WorkspaceMerged { verification: String },
    WorkspaceRemoved,
}

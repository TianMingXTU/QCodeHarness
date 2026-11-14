//! ACP（Agent Client Protocol）与内部任务服务的映射层。
//!
//! 既有 versioned stdio 协议（`PROTOCOL_VERSION = 1`）已是我们的线格式；ACP
//! 适配只把"A 启动一个任务、等待审批、继续到完成"这类外部会话语义翻译成
//! 既有的 `ServiceCommand` / `ServiceReply` / `ServiceEvent`，不新增线格式，也
//! 不引入远程多用户、云调度或 IDE 自研插件。本模块是无 IO 的纯映射，便于
//! 测试；带 IO 的会话客户端在 `infrastructure::acp`。

use crate::application::port::BoxFuture;
use crate::domain::service::{
    PendingApproval, ServiceCommand, ServiceError, ServiceEvent, ServiceEventPayload, ServiceReply,
    TaskStatus, TaskView,
};
use crate::domain::task::TaskKind;

/// ACP 适配层锚定到内部 versioned stdio 协议的版本 1。
///
/// ACP 概念在此只做语义映射，线格式与版本协商完全复用既有协议，因此没有
/// 独立的 ACP 协议版本号；这里只记录"本适配器面向的线协议版本"。
pub const ACP_PROTOCOL_VERSION: u32 = 1;

/// 映射失败：要么是 ACP 请求无法落到既有命令，要么是回复形状不在 ACP 范围内。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcpError(pub String);

impl std::fmt::Display for AcpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for AcpError {}

/// ACP 会话依赖的最小传输面：发命令、收事件。
///
/// 抽象出来是为了让会话编排可以在不碰真实 stdio 的情况下测试；生产实现由
/// `infrastructure::acp` 基于既有 `SdkClient` 提供。
pub trait AcpTaskClient: Send + Sync {
    /// 发送一条命令并等待同一个 ID 的回复。
    fn call<'a>(
        &'a self,
        id: &'a str,
        command: ServiceCommand,
    ) -> BoxFuture<'a, Result<ServiceReply, ServiceError>>;

    /// 取下一个事件；没有事件时等待。
    fn next_event(&self) -> BoxFuture<'_, Result<ServiceEvent, ServiceError>>;
}

/// 运行中遇到审批时暴露给外部的提示。
#[derive(Debug, Clone, PartialEq)]
pub struct AcpApprovalPrompt {
    pub request: PendingApproval,
}

/// 一次 ACP 会话驱到的结果。
#[derive(Debug, Clone, PartialEq)]
pub enum AcpRunOutcome {
    /// 任务完成，携带最终答案。
    Completed { answer: String },
    /// 任务被取消或异常停止，不一定有答案。
    Stopped {
        status: TaskStatus,
        error: Option<String>,
    },
    /// 运行需要外部审批；外部决定后应继续驱动。
    NeedsApproval { prompt: AcpApprovalPrompt },
    /// 协议或传输层错误。
    Error { message: String },
}

/// ACP 视角下的请求：一个外部会话能驱动的动作集合。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AcpRequest {
    /// 启动一个任务（评审 / 修复 / 提问）。
    RunTask {
        goal: String,
        kind: TaskKind,
    },
    /// 列出已知任务。
    ListTasks,
    /// 取单个任务视图。
    GetTask {
        id: String,
    },
    /// 给运行中的任务追加一条用户消息。
    SendInput {
        task_id: String,
        message_id: String,
        text: String,
    },
    /// 批准一条待审批。
    Approve {
        approval_id: String,
        task_id: String,
        execution_id: String,
    },
    /// 拒绝一条待审批。
    Reject {
        approval_id: String,
        task_id: String,
        execution_id: String,
    },
    /// 取消任务。
    Cancel {
        id: String,
    },
    /// 关闭服务连接。
    Shutdown,
}

/// ACP 视角下的回复：与 `ServiceReply` 一一对应的精简投影。
#[derive(Debug, Clone, PartialEq)]
pub enum AcpResponse {
    Task {
        view: TaskView,
    },
    Tasks {
        views: Vec<TaskView>,
    },
    Accepted {
        task_id: String,
        execution_id: Option<String>,
    },
    Approvals {
        requests: Vec<crate::domain::service::PendingApproval>,
    },
    Events {
        events: Vec<ServiceEvent>,
        next: u64,
    },
    Shutdown,
}

/// ACP 视角下关心的事件；其余事件归为 `Other`，会话层按需忽略。
#[derive(Debug, Clone, PartialEq)]
pub enum AcpEvent {
    TaskCreated {
        view: TaskView,
    },
    ExecutionStarted,
    ApprovalWaiting {
        request: crate::domain::service::PendingApproval,
    },
    ApprovalResolved {
        approval_id: String,
        allowed: bool,
    },
    Completed {
        answer: String,
    },
    Stopped {
        status: crate::domain::service::TaskStatus,
        error: Option<String>,
    },
    Other,
}

/// 把 ACP 请求翻译为既有服务命令；翻译不了的显式报错，不猜测。
pub fn to_service_command(request: AcpRequest) -> Result<ServiceCommand, AcpError> {
    use AcpRequest::*;
    Ok(match request {
        RunTask { goal, kind } => ServiceCommand::Create { goal, kind },
        ListTasks => ServiceCommand::List,
        GetTask { id } => ServiceCommand::Get { task_id: id },
        SendInput {
            task_id,
            message_id,
            text,
        } => ServiceCommand::Input {
            task_id,
            message_id,
            text,
        },
        Approve {
            approval_id,
            task_id,
            execution_id,
        } => ServiceCommand::Decide {
            approval_id,
            task_id,
            execution_id,
            allow: true,
        },
        Reject {
            approval_id,
            task_id,
            execution_id,
        } => ServiceCommand::Decide {
            approval_id,
            task_id,
            execution_id,
            allow: false,
        },
        Cancel { id } => ServiceCommand::Cancel { task_id: id },
        Shutdown => ServiceCommand::Shutdown,
    })
}

/// 把既有服务回复投影为 ACP 回复。
pub fn from_service_reply(reply: ServiceReply) -> AcpResponse {
    match reply {
        ServiceReply::Task { task } => AcpResponse::Task { view: *task },
        ServiceReply::Tasks { tasks } => AcpResponse::Tasks { views: tasks },
        ServiceReply::Accepted {
            task_id,
            execution_id,
        } => AcpResponse::Accepted {
            task_id,
            execution_id,
        },
        ServiceReply::PendingApprovals { requests } => AcpResponse::Approvals { requests },
        ServiceReply::Events { events, next } => AcpResponse::Events { events, next },
        ServiceReply::Shutdown => AcpResponse::Shutdown,
    }
}

/// 把内部事件投影为 ACP 事件；不关心的事件返回 `Other`，由会话层忽略。
pub fn from_service_event(event: &ServiceEvent) -> AcpEvent {
    match &event.payload {
        ServiceEventPayload::TaskCreated { task } => AcpEvent::TaskCreated {
            view: task.clone(),
        },
        ServiceEventPayload::ExecutionStarted => AcpEvent::ExecutionStarted,
        ServiceEventPayload::ApprovalWaiting { request } => AcpEvent::ApprovalWaiting {
            request: request.clone(),
        },
        ServiceEventPayload::ApprovalResolved {
            approval_id,
            allowed,
            reason: _,
        } => AcpEvent::ApprovalResolved {
            approval_id: approval_id.clone(),
            allowed: *allowed,
        },
        ServiceEventPayload::Completed { answer } => AcpEvent::Completed {
            answer: answer.clone(),
        },
        ServiceEventPayload::Stopped { status, error } => AcpEvent::Stopped {
            status: *status,
            error: error.clone(),
        },
        _ => AcpEvent::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::service::{EventOrigin, ServiceEvent};
    use crate::domain::tool::ToolCall;

    fn approval_request() -> crate::domain::service::PendingApproval {
        crate::domain::service::PendingApproval {
            approval_id: "ap1".into(),
            task_id: "task-1".into(),
            session_id: "session-1".into(),
            execution_id: "exec-1".into(),
            turn: 1,
            call: ToolCall {
                id: crate::domain::ids::CallId::new("call-1"),
                name: "write_file".into(),
                arguments: serde_json::json!({}),
            },
            annotations: crate::domain::tool::ToolAnnotations::default(),
            reason: None,
            resumed: false,
            origin: EventOrigin::default(),
        }
    }

    #[test]
    fn run_task_maps_to_create_with_kind() {
        let command =
            to_service_command(AcpRequest::RunTask {
                goal: "修复解析".into(),
                kind: TaskKind::BugFix,
            })
            .expect("合法映射");
        assert_eq!(
            command,
            ServiceCommand::Create {
                goal: "修复解析".into(),
                kind: TaskKind::BugFix,
            }
        );
    }

    #[test]
    fn approve_and_reject_map_to_decide_with_opposite_allow() {
        let approve = to_service_command(AcpRequest::Approve {
            approval_id: "ap1".into(),
            task_id: "t1".into(),
            execution_id: "e1".into(),
        })
        .expect("合法映射");
        assert_eq!(
            approve,
            ServiceCommand::Decide {
                approval_id: "ap1".into(),
                task_id: "t1".into(),
                execution_id: "e1".into(),
                allow: true,
            }
        );
        let reject = to_service_command(AcpRequest::Reject {
            approval_id: "ap1".into(),
            task_id: "t1".into(),
            execution_id: "e1".into(),
        })
        .expect("合法映射");
        assert_eq!(
            reject,
            ServiceCommand::Decide {
                approval_id: "ap1".into(),
                task_id: "t1".into(),
                execution_id: "e1".into(),
                allow: false,
            }
        );
    }

    #[test]
    fn replies_project_to_acp_shapes() {
        let view = crate::domain::service::TaskView {
            task_id: "t1".into(),
            session_id: "s1".into(),
            execution_id: "e1".into(),
            attempt: 1,
            status: crate::domain::service::TaskStatus::Running,
            workspace: std::path::PathBuf::from("wt"),
            base_revision: "a".repeat(40),
            source_task: None,
            parent_task: None,
            answer: None,
            error: None,
            verification: String::new(),
            consumption: crate::domain::budget::Consumption::default(),
        };
        assert_eq!(
            from_service_reply(ServiceReply::Task {
                task: Box::new(view.clone())
            }),
            AcpResponse::Task { view }
        );
        assert_eq!(
            from_service_reply(ServiceReply::Shutdown),
            AcpResponse::Shutdown
        );
    }

    #[test]
    fn approval_and_completion_events_project_to_acp() {
        let event = ServiceEvent {
            version: ACP_PROTOCOL_VERSION,
            sequence: 1,
            time_unix_ms: 0,
            task_id: Some("t1".into()),
            session_id: Some("s1".into()),
            execution_id: Some("e1".into()),
            origin: EventOrigin::default(),
            payload: ServiceEventPayload::ApprovalWaiting {
                request: approval_request(),
            },
        };
        assert!(matches!(
            from_service_event(&event),
            AcpEvent::ApprovalWaiting { .. }
        ));
        let done = ServiceEvent {
            version: ACP_PROTOCOL_VERSION,
            sequence: 2,
            time_unix_ms: 0,
            task_id: Some("t1".into()),
            session_id: Some("s1".into()),
            execution_id: Some("e1".into()),
            origin: EventOrigin::default(),
            payload: ServiceEventPayload::Completed {
                answer: "已修复".into(),
            },
        };
        assert_eq!(
            from_service_event(&done),
            AcpEvent::Completed {
                answer: "已修复".into()
            }
        );
    }
}

//! ACP 会话行为测试。
//!
//! 为了让 `src/application/acp.rs` 与 `src/infrastructure/acp.rs` 在**不改动
//! 共享 `mod.rs`** 的前提下可编译、可实跑，本测试用 `#[path]` 把两个文件包含
//! 进来，并把 `crate::domain` / `crate::application` / `crate::interface` 等
//! 路径桥接到库的公开模块。主代理补上 `pub mod acp;` 后，本文件仍可继续用。

// 用 `#[path]` 把两个尚未在共享 mod.rs 中声明的新文件包含进来，使得本测试
// 能在不改动任何 `mod.rs` 的前提下编译并实跑。主代理补上 `pub mod acp;` 后，
// 这两个顶层模块可删除，下面 `application` / `infrastructure` 的 re-export 直接
// 指向库内模块即可。
#[path = "../src/application/acp.rs"]
pub mod acp_application;

#[path = "../src/infrastructure/acp.rs"]
pub mod acp_infrastructure;

pub mod domain {
    pub use qcode_harness::domain::*;
}

pub mod interface {
    pub use qcode_harness::interface::*;
}

pub mod application {
    pub use qcode_harness::application::*;
    pub use crate::acp_application as acp;
}

pub mod infrastructure {
    pub use qcode_harness::infrastructure::*;
    pub use crate::acp_infrastructure as acp;
}

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use tokio::sync::Notify;

use crate::application::acp::{
    AcpApprovalPrompt, AcpRunOutcome, AcpTaskClient,
};
use crate::application::port::BoxFuture;
use crate::domain::ids::CallId;
use crate::domain::service::{
    EventOrigin, PendingApproval, ServiceCommand, ServiceError, ServiceEvent, ServiceEventPayload,
    ServiceReply, TaskStatus, TaskView,
};
use crate::domain::tool::{ToolAnnotations, ToolCall};
use crate::infrastructure::acp::AcpSession;

fn view() -> TaskView {
    TaskView {
        task_id: "task-1".into(),
        session_id: "session-1".into(),
        execution_id: "exec-1".into(),
        attempt: 1,
        status: TaskStatus::Running,
        workspace: std::path::PathBuf::from("wt"),
        base_revision: "a".repeat(40),
        source_task: None,
        parent_task: None,
        answer: None,
        error: None,
        verification: String::new(),
        consumption: crate::domain::budget::Consumption::default(),
    }
}

fn approval() -> PendingApproval {
    PendingApproval {
        approval_id: "ap1".into(),
        task_id: "task-1".into(),
        session_id: "session-1".into(),
        execution_id: "exec-1".into(),
        turn: 1,
        call: ToolCall {
            id: CallId::new("call-1"),
            name: "write_file".into(),
            arguments: serde_json::json!({}),
        },
        annotations: ToolAnnotations::default(),
        reason: None,
        resumed: false,
        origin: EventOrigin::default(),
    }
}

/// 脚本化的 ACP 传输替身：Create 后立刻抛出待审批事件，Decide 时按 allow
/// 分支给出"完成"或"停止"。事件都先入队再被消费，测试无需处理真实时序。
struct FakeClient {
    commands: Mutex<Vec<(String, ServiceCommand)>>,
    events: Mutex<VecDeque<ServiceEvent>>,
    notify: Notify,
}

impl FakeClient {
    fn new() -> Self {
        Self {
            commands: Mutex::new(Vec::new()),
            events: Mutex::new(VecDeque::new()),
            notify: Notify::new(),
        }
    }

    fn push(&self, payload: ServiceEventPayload) {
        let mut events = self.events.lock().unwrap();
        let sequence = events.len() as u64 + 1;
        events.push_back(ServiceEvent {
            version: 1,
            sequence,
            time_unix_ms: 0,
            task_id: Some("task-1".into()),
            session_id: Some("session-1".into()),
            execution_id: Some("exec-1".into()),
            origin: EventOrigin::default(),
            payload,
        });
        drop(events);
        self.notify.notify_waiters();
    }
}

impl AcpTaskClient for FakeClient {
    fn call<'a>(
        &'a self,
        id: &'a str,
        command: ServiceCommand,
    ) -> BoxFuture<'a, Result<ServiceReply, ServiceError>> {
        Box::pin(async move {
            self.commands
                .lock()
                .unwrap()
                .push((id.to_owned(), command.clone()));
            match &command {
                ServiceCommand::Create { .. } => {
                    self.push(ServiceEventPayload::ExecutionStarted);
                    self.push(ServiceEventPayload::ApprovalWaiting {
                        request: approval(),
                    });
                    Ok(ServiceReply::Task {
                        task: Box::new(view()),
                    })
                }
                ServiceCommand::Decide { allow, .. } => {
                    self.push(ServiceEventPayload::ApprovalResolved {
                        approval_id: "ap1".into(),
                        allowed: *allow,
                        reason: None,
                    });
                    if *allow {
                        self.push(ServiceEventPayload::Completed {
                            answer: "已修复".into(),
                        });
                    } else {
                        self.push(ServiceEventPayload::Stopped {
                            status: TaskStatus::Cancelled,
                            error: None,
                        });
                    }
                    Ok(ServiceReply::Accepted {
                        task_id: "task-1".into(),
                        execution_id: Some("exec-1".into()),
                    })
                }
                _ => Ok(ServiceReply::Accepted {
                    task_id: "task-1".into(),
                    execution_id: Some("exec-1".into()),
                }),
            }
        })
    }

    fn next_event(&self) -> BoxFuture<'_, Result<ServiceEvent, ServiceError>> {
        Box::pin(async move {
            loop {
                let notified = self.notify.notified();
                if let Some(event) = self.events.lock().unwrap().pop_front() {
                    return Ok(event);
                }
                notified.await;
            }
        })
    }
}

/// 审批通过后，ACP 会话应把任务驱到完成，并原样把 goal 与 allow 传给服务。
#[tokio::test]
async fn acp_session_completes_a_task_after_approval() {
    let client = Arc::new(FakeClient::new());
    let session = AcpSession::new(client.clone());

    let first = session.run_to_completion("评审这次改动").await;
    let prompt: AcpApprovalPrompt = match first {
        AcpRunOutcome::NeedsApproval { prompt } => prompt,
        other => panic!("应停在审批，实际得到 {other:?}"),
    };
    assert_eq!(prompt.request.approval_id, "ap1");
    assert_eq!(prompt.request.call.name, "write_file");

    let outcome = session
        .decide(
            &prompt.request.approval_id,
            &prompt.request.task_id,
            &prompt.request.execution_id,
            true,
        )
        .await;
    assert_eq!(
        outcome,
        AcpRunOutcome::Completed {
            answer: "已修复".into()
        }
    );

    let commands = client.commands.lock().unwrap();
    assert_eq!(commands.len(), 2, "只应发出 Create 与 Decide 两条命令");
    assert!(
        matches!(&commands[0].1, ServiceCommand::Create { goal, .. } if goal == "评审这次改动"),
        "Create 必须携带原始 goal，实际为 {:?}",
        commands[0].1
    );
    assert!(
        matches!(
            &commands[1].1,
            ServiceCommand::Decide {
                approval_id,
                allow: true,
                ..
            } if approval_id == "ap1"
        ),
        "Decide 必须是 ap1 且 allow=true，实际为 {:?}",
        commands[1].1
    );
}

/// 拒绝审批时，ACP 会话应如实返回停止状态，而不是伪装成完成。
#[tokio::test]
async fn acp_session_reports_stopped_when_approval_is_rejected() {
    let client = Arc::new(FakeClient::new());
    let session = AcpSession::new(client.clone());

    let first = session.run_to_completion("评审这次改动").await;
    let prompt = match first {
        AcpRunOutcome::NeedsApproval { prompt } => prompt,
        other => panic!("应停在审批，实际得到 {other:?}"),
    };

    let outcome = session
        .decide(
            &prompt.request.approval_id,
            &prompt.request.task_id,
            &prompt.request.execution_id,
            false,
        )
        .await;
    assert_eq!(
        outcome,
        AcpRunOutcome::Stopped {
            status: TaskStatus::Cancelled,
            error: None,
        }
    );
}

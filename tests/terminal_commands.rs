//! 终端前端通过公开命令驱动服务；不连接模型、不启用真实终端模式。

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crossterm::event::KeyCode;
use qcode_harness::application::port::BoxFuture;
use qcode_harness::application::service::{EventSubscription, TaskService};
use qcode_harness::domain::budget::Consumption;
use qcode_harness::domain::evidence::AuditEvent;
use qcode_harness::domain::ids::CallId;
use qcode_harness::domain::service::{
    EventOrigin, PROTOCOL_VERSION, PendingApproval, ServiceCommand, ServiceError, ServiceEvent,
    ServiceEventPayload, ServiceReply, TaskStatus, TaskView,
};
use qcode_harness::domain::task::TaskKind;
use qcode_harness::domain::tool::{ToolAnnotations, ToolCall};
use qcode_harness::interface::terminal::{InputEditor, TerminalCommand, parse_command, run_text};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, duplex};
use tokio::sync::{Notify, Semaphore};

fn task() -> TaskView {
    TaskView {
        task_id: "task-1".into(),
        session_id: "session-1".into(),
        execution_id: "execution-1".into(),
        attempt: 1,
        status: TaskStatus::Running,
        workspace: "worktree".into(),
        base_revision: "base".into(),
        source_task: None,
        parent_task: None,
        answer: None,
        error: None,
        verification: String::new(),
        consumption: Consumption::default(),
    }
}

fn approval() -> PendingApproval {
    PendingApproval {
        approval_id: "approval-1".into(),
        task_id: "task-2".into(),
        session_id: "session-2".into(),
        execution_id: "execution-9".into(),
        turn: 2,
        call: ToolCall {
            id: CallId::new("call-1"),
            name: "write_file".into(),
            arguments: serde_json::json!({"path": "a.txt"}),
        },
        annotations: ToolAnnotations::default(),
        reason: Some("需要确认写入范围".into()),
        resumed: true,
        origin: EventOrigin::default(),
    }
}

fn event(sequence: u64, payload: ServiceEventPayload) -> ServiceEvent {
    ServiceEvent {
        version: PROTOCOL_VERSION,
        sequence,
        time_unix_ms: 0,
        task_id: Some("task-1".into()),
        session_id: Some("session-1".into()),
        execution_id: Some("execution-1".into()),
        origin: EventOrigin::default(),
        payload,
    }
}

struct Bus {
    events: Mutex<VecDeque<Result<ServiceEvent, ServiceError>>>,
    ready: Notify,
}

impl Bus {
    fn push(&self, event: Result<ServiceEvent, ServiceError>) {
        self.events.lock().unwrap().push_back(event);
        self.ready.notify_one();
    }
}

struct Subscription(Arc<Bus>);
impl EventSubscription for Subscription {
    fn next(&mut self) -> BoxFuture<'_, Result<ServiceEvent, ServiceError>> {
        Box::pin(async move {
            loop {
                let ready = self.0.ready.notified();
                if let Some(event) = self.0.events.lock().unwrap().pop_front() {
                    return event;
                }
                ready.await;
            }
        })
    }
}

struct Service {
    bus: Arc<Bus>,
    seen: Mutex<Vec<(String, ServiceCommand)>>,
    pending: Mutex<Vec<PendingApproval>>,
    merge_started: Notify,
    merge_release: Semaphore,
    shutdown_error: Mutex<Option<ServiceError>>,
    subscriptions: Mutex<Vec<u64>>,
}

impl Service {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            bus: Arc::new(Bus {
                events: Mutex::new(VecDeque::new()),
                ready: Notify::new(),
            }),
            seen: Mutex::new(Vec::new()),
            pending: Mutex::new(vec![approval()]),
            merge_started: Notify::new(),
            merge_release: Semaphore::new(0),
            shutdown_error: Mutex::new(None),
            subscriptions: Mutex::new(Vec::new()),
        })
    }

    fn commands(&self) -> Vec<ServiceCommand> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .map(|(_, command)| command.clone())
            .collect()
    }
}

impl TaskService for Service {
    fn call<'a>(
        &'a self,
        id: &'a str,
        command: ServiceCommand,
    ) -> BoxFuture<'a, Result<ServiceReply, ServiceError>> {
        Box::pin(async move {
            self.seen.lock().unwrap().push((id.into(), command.clone()));
            match command {
                ServiceCommand::Get { task_id } if task_id != "task-1" => {
                    Err(ServiceError::new("task_not_found", "任务不存在"))
                }
                ServiceCommand::Create { .. }
                | ServiceCommand::Get { .. }
                | ServiceCommand::Continue { .. }
                | ServiceCommand::Fork { .. } => Ok(ServiceReply::Task { task: task() }),
                ServiceCommand::List => Ok(ServiceReply::Tasks {
                    tasks: vec![task()],
                }),
                ServiceCommand::Approvals { .. } => Ok(ServiceReply::PendingApprovals {
                    requests: self.pending.lock().unwrap().clone(),
                }),
                ServiceCommand::Decide {
                    approval_id,
                    task_id,
                    execution_id,
                    ..
                } => {
                    assert_eq!(
                        (
                            approval_id.as_str(),
                            task_id.as_str(),
                            execution_id.as_str()
                        ),
                        ("approval-1", "task-2", "execution-9")
                    );
                    self.pending.lock().unwrap().clear();
                    Ok(ServiceReply::Accepted {
                        task_id,
                        execution_id: Some(execution_id),
                    })
                }
                ServiceCommand::Merge { task_id } => {
                    self.merge_started.notify_one();
                    self.merge_release.acquire().await.unwrap().forget();
                    Ok(ServiceReply::Accepted {
                        task_id,
                        execution_id: None,
                    })
                }
                ServiceCommand::Events { after, .. } => Ok(ServiceReply::Events {
                    events: vec![event(
                        after + 1,
                        ServiceEventPayload::Completed {
                            answer: "历史回复".into(),
                        },
                    )],
                    next: after + 1,
                }),
                ServiceCommand::Shutdown => {
                    if let Some(error) = self.shutdown_error.lock().unwrap().clone() {
                        return Err(error);
                    }
                    Ok(ServiceReply::Shutdown)
                }
                _ => Ok(ServiceReply::Accepted {
                    task_id: "task-1".into(),
                    execution_id: None,
                }),
            }
        })
    }

    fn subscribe(&self, after: u64) -> Result<Box<dyn EventSubscription>, ServiceError> {
        self.subscriptions.lock().unwrap().push(after);
        Ok(Box::new(Subscription(self.bus.clone())))
    }
}

#[test]
fn parser_routes_plain_text_and_requires_explicit_destructive_commands() {
    assert!(
        matches!(parse_command("修复中文问题", None).unwrap(), TerminalCommand::Service(ServiceCommand::Create { goal, kind: TaskKind::BugFix }) if goal == "修复中文问题")
    );
    assert!(
        matches!(parse_command("继续检查", Some("task-1")).unwrap(), TerminalCommand::Input { task_id, text } if task_id == "task-1" && text == "继续检查")
    );
    assert!(matches!(
        parse_command("/create --kind review 检查补丁", Some("task-1")).unwrap(),
        TerminalCommand::Service(ServiceCommand::Create {
            kind: TaskKind::Review,
            ..
        })
    ));
    for command in [
        "/merge",
        "/cleanup",
        "/approve",
        "/approve a1 t1",
        "/trust all",
        "/cancel t1 extra",
        "/events -1",
        "/list extra",
    ] {
        assert!(parse_command(command, Some("task-1")).is_err(), "{command}");
    }
    assert!(
        matches!(parse_command("/cancel", Some("task-1")).unwrap(), TerminalCommand::Service(ServiceCommand::Cancel { task_id }) if task_id == "task-1")
    );
    assert!(matches!(
        parse_command("/events 7 20", None).unwrap(),
        TerminalCommand::Service(ServiceCommand::Events {
            after: 7,
            limit: 20
        })
    ));
}

#[test]
fn unicode_editor_keeps_cursor_on_character_boundaries_and_paste_is_one_input() {
    let mut editor = InputEditor::default();
    editor.insert("中🙂文");
    editor.apply(KeyCode::Left);
    editor.apply(KeyCode::Backspace);
    assert_eq!(editor.text(), "中文");
    editor.insert("é");
    assert_eq!(editor.text(), "中é文");
    editor.apply(KeyCode::Home);
    editor.apply(KeyCode::Delete);
    editor.apply(KeyCode::End);
    editor.insert("\n/quit\u{1b}[31m");
    assert!(editor.text().is_char_boundary(editor.cursor()));
    assert_eq!(editor.text(), "é文 /quit[31m");
}

async fn script(service: Arc<Service>, input: &str) -> Result<String, ServiceError> {
    let (mut client, server) = duplex(64 * 1024);
    let (read, write) = tokio::io::split(server);
    let frontend = tokio::spawn(async move { run_text(service, read, write).await });
    client.write_all(input.as_bytes()).await.unwrap();
    client.shutdown().await.unwrap();
    let mut output = String::new();
    tokio::time::timeout(Duration::from_secs(5), client.read_to_string(&mut output))
        .await
        .unwrap()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), frontend)
        .await
        .unwrap()
        .unwrap()?;
    Ok(output)
}

#[tokio::test]
async fn piped_commands_preserve_selection_and_eof_drains_commands_then_shuts_down() {
    let service = Service::new();
    let output = script(service.clone(), "修复问题\n补充输入\n/select none\n/create --kind refactor 重构\n/cancel\n/continue\n/fork task-1 新目标\n/events 7 2\n/cleanup task-1\n").await.unwrap();
    let commands = service.commands();
    assert_eq!(
        commands[0],
        ServiceCommand::Create {
            goal: "修复问题".into(),
            kind: TaskKind::BugFix
        }
    );
    assert!(
        matches!(&commands[1], ServiceCommand::Input { task_id, text, .. } if task_id == "task-1" && text == "补充输入")
    );
    assert!(commands.iter().any(|command| matches!(
        command,
        ServiceCommand::Create {
            kind: TaskKind::Refactor,
            ..
        }
    )));
    for command in [
        ServiceCommand::Cancel {
            task_id: "task-1".into(),
        },
        ServiceCommand::Continue {
            task_id: "task-1".into(),
        },
        ServiceCommand::Fork {
            task_id: "task-1".into(),
            goal: Some("新目标".into()),
        },
        ServiceCommand::Cleanup {
            task_id: "task-1".into(),
        },
    ] {
        assert!(commands.contains(&command), "{command:?}");
    }
    assert_eq!(commands.last(), Some(&ServiceCommand::Shutdown));
    assert_eq!(
        commands
            .iter()
            .filter(|command| matches!(command, ServiceCommand::Shutdown))
            .count(),
        1
    );
    let seen = service.seen.lock().unwrap();
    let ids: std::collections::HashSet<_> = seen.iter().map(|(id, _)| id).collect();
    assert_eq!(ids.len(), seen.len(), "请求与输入标识不能在当前会话复用");
    assert_eq!(*service.subscriptions.lock().unwrap(), vec![0]);
    assert_eq!(output.matches("[选择] task-1").count(), 4);
    assert_eq!(output.matches("[回放] 下一游标 8").count(), 1);
    assert!(output.contains("历史回复"));
    assert!(!output.contains('\u{1b}'), "文本模式不能输出终端控制序列");
}

#[tokio::test]
async fn eof_bounds_a_stalled_command_without_restarting_the_event_subscription() {
    let service = Service::new();
    let input = format!("/merge task-1\n{}", "/list\n".repeat(65));
    let output = script(service.clone(), &input).await.unwrap();
    assert_eq!(
        service.commands(),
        vec![
            ServiceCommand::Merge {
                task_id: "task-1".into(),
            },
            ServiceCommand::Shutdown,
        ]
    );
    assert_eq!(*service.subscriptions.lock().unwrap(), vec![0]);
    assert!(output.contains("未完成命令停止等待"));
    assert!(output.contains("命令可能已生效"));
    assert_eq!(output.matches("command_queue_full").count(), 1);
}

#[tokio::test]
async fn eof_renders_ready_events_once_from_the_existing_subscription() {
    let service = Service::new();
    service.bus.push(Ok(event(
        1,
        ServiceEventPayload::Completed {
            answer: "最后一条事件".into(),
        },
    )));
    let output = script(service.clone(), "").await.unwrap();
    assert_eq!(output.matches("最后一条事件").count(), 1);
    assert_eq!(*service.subscriptions.lock().unwrap(), vec![0]);
}

#[tokio::test]
async fn lagged_events_resume_from_the_last_successful_cursor_without_duplicates() {
    let service = Service::new();
    service.bus.push(Ok(event(
        1,
        ServiceEventPayload::Completed {
            answer: "游标之前".into(),
        },
    )));
    service
        .bus
        .push(Err(ServiceError::new("lagged", "订阅缓冲已溢出")));
    service.bus.push(Ok(event(
        2,
        ServiceEventPayload::Completed {
            answer: "游标之后".into(),
        },
    )));
    let output = script(service.clone(), "").await.unwrap();
    assert_eq!(output.matches("游标之前").count(), 1);
    assert_eq!(output.matches("游标之后").count(), 1);
    assert_eq!(*service.subscriptions.lock().unwrap(), vec![0, 1]);
}

#[tokio::test]
async fn invalid_commands_and_service_errors_are_visible_and_quit_stops_input() {
    let service = Service::new();
    let output = script(
        service.clone(),
        "/unknown\n/cancel task-1 extra\n/select missing\n/help\n/quit\n/create 不应执行\n",
    )
    .await
    .unwrap();
    assert_eq!(output.matches("invalid_command").count(), 2);
    assert!(output.contains("task_not_found"));
    assert!(output.contains("命令（普通文字"));
    assert_eq!(
        service.commands(),
        vec![
            ServiceCommand::Get {
                task_id: "missing".into(),
            },
            ServiceCommand::Shutdown,
        ]
    );
}

#[tokio::test]
async fn approvals_resolve_live_ownership_and_never_use_the_selected_task() {
    let service = Service::new();
    let output = script(service.clone(), "/select task-1\n/approvals\n/approve approval-1 task-1 execution-1\n/approve approval-1\n/approve approval-1\n/quit\n").await.unwrap();
    let decisions: Vec<_> = service
        .commands()
        .into_iter()
        .filter(|command| matches!(command, ServiceCommand::Decide { .. }))
        .collect();
    assert_eq!(
        decisions,
        vec![ServiceCommand::Decide {
            approval_id: "approval-1".into(),
            task_id: "task-2".into(),
            execution_id: "execution-9".into(),
            allow: true
        }]
    );
    assert!(output.contains("需要确认写入范围"));
    assert!(output.contains("resumed=true"));
    assert!(output.contains("approval_owner_mismatch"));
    assert!(output.contains("approval_not_pending"));
}

async fn line<R: tokio::io::AsyncRead + Unpin>(reader: &mut BufReader<R>) -> String {
    let mut output = String::new();
    let count = tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut output))
        .await
        .unwrap()
        .unwrap();
    assert_ne!(count, 0, "前端不应提前退出");
    output
}

#[tokio::test]
async fn events_and_approval_decisions_remain_responsive_during_a_slow_merge() {
    let service = Service::new();
    let (client, server) = duplex(64 * 1024);
    let (client_read, mut client_write) = tokio::io::split(client);
    let (read, write) = tokio::io::split(server);
    let frontend_service = service.clone();
    let frontend = tokio::spawn(async move { run_text(frontend_service, read, write).await });
    let mut reader = BufReader::new(client_read);
    client_write.write_all(b"/merge task-1\n").await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), service.merge_started.notified())
        .await
        .unwrap();
    let mut pending = approval();
    pending.reason = Some("\u{1b}]0;forged\u{7}需要确认写入范围".into());
    service.bus.push(Ok(event(
        1,
        ServiceEventPayload::ApprovalWaiting { request: pending },
    )));
    let mut output = String::new();
    while !output.contains("需要确认写入范围") {
        output.push_str(&line(&mut reader).await);
    }
    assert!(output.contains("execution-9"));
    assert!(output.contains("resumed=true"));
    assert!(!output.contains('\u{1b}'));
    assert!(!output.contains('\u{7}'));
    client_write
        .write_all(b"/approve approval-1\n")
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while !service
            .commands()
            .iter()
            .any(|command| matches!(command, ServiceCommand::Decide { .. }))
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("审批不能被慢速合并阻塞");
    client_write.write_all(b"/cancel task-1\n").await.unwrap();
    let mut urgent_output = String::new();
    while !urgent_output.contains("[受理] 任务 task-1") {
        urgent_output.push_str(&line(&mut reader).await);
    }
    assert!(service.commands().contains(&ServiceCommand::Cancel {
        task_id: "task-1".into(),
    }));
    service.merge_release.add_permits(1);
    client_write.write_all(b"/quit\n").await.unwrap();
    let mut rest = String::new();
    tokio::time::timeout(Duration::from_secs(5), reader.read_to_string(&mut rest))
        .await
        .unwrap()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), frontend)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(rest.matches("[受理] 任务 task-1").count(), 1);
}

#[tokio::test]
async fn sandbox_and_unknown_result_events_render_structured_facts() {
    let service = Service::new();
    service.bus.push(Ok(event(
        1,
        ServiceEventPayload::Audit {
            event: AuditEvent::SandboxApplied {
                turn: 1,
                call: approval().call,
                platform: "Windows JobObject".into(),
                active: false,
                boundary_checked: true,
            },
        },
    )));
    service.bus.push(Ok(event(
        2,
        ServiceEventPayload::Audit {
            event: AuditEvent::ToolCallReplayed {
                turn: 1,
                tool: "write_file".into(),
                reason: "执行中断".into(),
                previous_outcome: None,
            },
        },
    )));
    let (client, server) = duplex(64 * 1024);
    let (client_read, mut client_write) = tokio::io::split(client);
    let (read, write) = tokio::io::split(server);
    let frontend = tokio::spawn(async move { run_text(service, read, write).await });
    let mut reader = BufReader::new(client_read);
    let mut output = String::new();
    while !output.contains("结果未知") {
        output.push_str(&line(&mut reader).await);
    }
    assert!(output.contains("无进程沙箱保护"));
    assert!(output.contains("不提供文件系统或网络隔离"));
    client_write.write_all(b"/quit\n").await.unwrap();
    reader.read_to_string(&mut output).await.unwrap();
    frontend.await.unwrap().unwrap();
}

#[tokio::test]
async fn event_and_shutdown_errors_propagate_after_attempting_service_shutdown() {
    let service = Service::new();
    service
        .bus
        .push(Err(ServiceError::new("journal_io", "事件读取失败")));
    assert_eq!(
        script(service.clone(), "").await.unwrap_err().code,
        "journal_io"
    );
    assert!(service.commands().contains(&ServiceCommand::Shutdown));
    let service = Service::new();
    *service.shutdown_error.lock().unwrap() =
        Some(ServiceError::new("shutdown_failed", "关闭失败"));
    assert_eq!(
        script(service, "/quit\n").await.unwrap_err().code,
        "shutdown_failed"
    );
}

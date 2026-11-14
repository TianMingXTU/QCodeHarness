//! 本地 Web UI 外部行为测试：真实 HTTP 访问单机服务，走完带审批的一次任务。
//!
//! 测试替身复用服务公开契约（ServiceCommand / ServiceEvent），不引入另一套
//! 状态机。浏览器可见的断言只认服务事件：审批等待、否决证据、放行结论与
//! 任务完成事件必须和 CLI 的 JSON 形状一致，并出现在 events.jsonl 回放里。

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use qcode_harness::application::nested_delegation::{
    ApprovalFramePayload, ApprovalState, DelegationFrame, MAX_NESTING, approval_frame, frame_line,
    parse_frame, validate_nesting,
};
use qcode_harness::application::port::BoxFuture;
use qcode_harness::application::service::{EventJournal, EventSubscription, TaskService};
use qcode_harness::application::web_ui::{StaticAssets, WebUiConfig, WebUiServer};
use qcode_harness::domain::budget::Consumption;
use qcode_harness::domain::evidence::{AuditEvent, TokenUsage};
use qcode_harness::domain::ids::CallId;
use qcode_harness::domain::service::{
    EventContext, EventOrigin, PendingApproval, ServiceCommand, ServiceError, ServiceEventPayload,
    ServiceReply, TaskStatus, TaskView,
};
use qcode_harness::domain::tool::{ToolAnnotations, ToolCall};
use qcode_harness::infrastructure::nested_delegation::ApprovalFrameTransport;
use qcode_harness::infrastructure::service_events::JsonEventJournal;
use qcode_harness::infrastructure::web_ui::LocalWebUiServer;
use serde_json::{Value, json};
use tokio::time::timeout;

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

/// 每个用例独占临时目录；失败时也按目录清理，不复用其他用例的事件游标。
struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let serial = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "qcode-r5-web-ui-{}-{stamp}-{serial}",
            std::process::id()
        ));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn log(&self) -> PathBuf {
        self.0.join("events.jsonl")
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn context() -> EventContext {
    EventContext {
        task_id: "task-web-1".into(),
        session_id: "session-task-web-1".into(),
        execution_id: "execution-task-web-1".into(),
        origin: EventOrigin::default(),
    }
}

fn task_view(status: TaskStatus, workspace: &Path) -> TaskView {
    TaskView {
        task_id: "task-web-1".into(),
        session_id: "session-task-web-1".into(),
        execution_id: "execution-task-web-1".into(),
        attempt: 1,
        status,
        workspace: workspace.to_path_buf(),
        base_revision: "base-revision".into(),
        source_task: None,
        parent_task: None,
        answer: None,
        error: None,
        verification: "sha256:test".into(),
        consumption: Consumption::default(),
    }
}

fn pending() -> PendingApproval {
    PendingApproval {
        approval_id: "approval-web-1".into(),
        task_id: "task-web-1".into(),
        session_id: "session-task-web-1".into(),
        execution_id: "execution-task-web-1".into(),
        turn: 2,
        call: ToolCall {
            id: CallId::new("call-web-1"),
            name: "write_file".into(),
            arguments: json!({ "path": "notes.txt", "content": "hello" }),
        },
        annotations: ToolAnnotations::new(false, true, false, false),
        reason: Some("需要人工复核".into()),
        resumed: false,
        origin: EventOrigin::default(),
    }
}

/// 事件顺序固定的测试服务：命令不自行改写状态，只按真实事件日志追加事实。
struct ScriptedService {
    journal: Arc<JsonEventJournal>,
    workspace: PathBuf,
    request_id: Mutex<String>,
    input_seen: AtomicBool,
}

impl TaskService for ScriptedService {
    fn call<'a>(
        &'a self,
        id: &'a str,
        command: ServiceCommand,
    ) -> BoxFuture<'a, Result<ServiceReply, ServiceError>> {
        Box::pin(async move {
            *self.request_id.lock().unwrap() = id.to_owned();
            let ctx = context();
            match command {
                ServiceCommand::Create { .. } => {
                    let task = task_view(TaskStatus::Running, &self.workspace);
                    self.journal
                        .append(
                            Some(&ctx),
                            ServiceEventPayload::TaskCreated { task: task.clone() },
                        )
                        .unwrap();
                    Ok(ServiceReply::Task {
                        task: Box::new(task),
                    })
                }
                ServiceCommand::Input {
                    message_id, text, ..
                } => {
                    self.input_seen.store(true, Ordering::SeqCst);
                    self.journal
                        .append(
                            Some(&ctx),
                            ServiceEventPayload::InputReceived { message_id, text },
                        )
                        .unwrap();
                    Ok(ServiceReply::Accepted {
                        task_id: "task-web-1".into(),
                        execution_id: None,
                    })
                }
                ServiceCommand::Decide {
                    approval_id, allow, ..
                } => {
                    if !self.input_seen.load(Ordering::SeqCst) {
                        return Err(ServiceError::new("input_missing", "补充输入尚未受理"));
                    }
                    // 否决证据与放行结论都由服务记账，Web UI 不自行合成。
                    self.journal
                        .append(
                            Some(&ctx),
                            ServiceEventPayload::Audit {
                                event: AuditEvent::ApprovalDecided {
                                    turn: 2,
                                    tool: "write_file".into(),
                                    allowed: false,
                                    reason: Some("先否决高风险写入".into()),
                                },
                            },
                        )
                        .unwrap();
                    self.journal
                        .append(
                            Some(&ctx),
                            ServiceEventPayload::ApprovalResolved {
                                approval_id: approval_id.clone(),
                                allowed: allow,
                                reason: None,
                            },
                        )
                        .unwrap();
                    self.journal
                        .append(
                            Some(&ctx),
                            ServiceEventPayload::Audit {
                                event: AuditEvent::Thought {
                                    turn: 3,
                                    text: "任务完成".into(),
                                    usage: TokenUsage::default(),
                                },
                            },
                        )
                        .unwrap();
                    let answer = "已完成并保留审批证据".to_owned();
                    self.journal
                        .append(
                            Some(&ctx),
                            ServiceEventPayload::Completed {
                                answer: answer.clone(),
                            },
                        )
                        .unwrap();
                    Ok(ServiceReply::Accepted {
                        task_id: "task-web-1".into(),
                        execution_id: Some("execution-task-web-1".into()),
                    })
                }
                ServiceCommand::Approvals { .. } => Ok(ServiceReply::PendingApprovals {
                    requests: vec![pending()],
                }),
                ServiceCommand::Get { .. } => Ok(ServiceReply::Task {
                    task: Box::new(task_view(TaskStatus::Running, &self.workspace)),
                }),
                ServiceCommand::Events { after, limit } => {
                    let events = self.journal.read(after, limit)?;
                    let next = events.last().map_or(after, |event| event.sequence);
                    Ok(ServiceReply::Events { events, next })
                }
                ServiceCommand::List => Ok(ServiceReply::Tasks { tasks: Vec::new() }),
                _ => Err(ServiceError::new(
                    "unsupported_command",
                    "测试脚本未覆盖此命令",
                )),
            }
        })
    }

    fn subscribe(&self, after: u64) -> Result<Box<dyn EventSubscription>, ServiceError> {
        self.journal.subscribe(after)
    }
}

fn assets() -> StaticAssets {
    StaticAssets {
        index_html: b"<!doctype html><html><head><title>QCodeHarness local Web UI</title></head><body><main id=app></main><script src=/app.js></script></body></html>".to_vec(),
        app_js: b"document.getElementById('app').textContent='QCodeHarness local Web UI';".to_vec(),
    }
}

fn config(port: u16, log: &Path) -> WebUiConfig {
    WebUiConfig {
        port,
        host: "127.0.0.1".into(),
        events_path: log.to_path_buf(),
        assets: assets(),
    }
}

async fn get(url: &str) -> (u16, String, String) {
    let output = tokio::process::Command::new("curl")
        .args(["-sS", "-i", "--max-time", "5", url])
        .output()
        .await
        .expect("本机 curl 应可启动；未安装时该测试如实失败");
    assert!(
        output.status.success(),
        "curl 失败: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8(output.stdout).unwrap();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((text.as_str(), ""));
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap()
        .parse::<u16>()
        .unwrap();
    (status, head.to_owned(), body.to_owned())
}

async fn post(url: &str, body: Value) -> (u16, String) {
    let payload = serde_json::to_string(&body).unwrap();
    let output = tokio::process::Command::new("curl")
        .args([
            "-sS",
            "-i",
            "--max-time",
            "5",
            "-H",
            "Content-Type: application/json",
            "-X",
            "POST",
            "--data-binary",
            &payload,
            url,
        ])
        .output()
        .await
        .expect("本机 curl 应可启动；未安装时该测试如实失败");
    assert!(
        output.status.success(),
        "curl 失败: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8(output.stdout).unwrap();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((text.as_str(), ""));
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap()
        .parse::<u16>()
        .unwrap();
    (status, body.to_owned())
}

fn json_lines(text: &str) -> Vec<Value> {
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("事件流每行都是 JSON"))
        .collect()
}

#[tokio::test]
async fn local_web_ui_completes_an_approved_task_with_cli_event_shapes() {
    let dir = TestDirectory::new();
    let journal = Arc::new(JsonEventJournal::open(dir.log()).unwrap());
    journal
        .append(
            Some(&context()),
            ServiceEventPayload::ApprovalWaiting { request: pending() },
        )
        .unwrap();
    let service = Arc::new(ScriptedService {
        journal: journal.clone(),
        workspace: dir.0.clone(),
        request_id: Mutex::new(String::new()),
        input_seen: AtomicBool::new(false),
    });
    let server = LocalWebUiServer::new(service.clone());
    let port = timeout(Duration::from_secs(3), server.bind(config(0, &dir.log())))
        .await
        .expect("绑定不应挂住")
        .expect("应绑定 127.0.0.1 的临时端口");
    assert_eq!(server.bound_host(), "127.0.0.1");
    let base = format!("http://127.0.0.1:{port}");

    let (status, _, html) = get(&format!("{base}/")).await;
    assert_eq!(status, 200);
    assert!(html.contains("local Web UI"), "应提供静态页面");
    let (status, _, js) = get(&format!("{base}/app.js")).await;
    assert_eq!(status, 200);
    assert!(js.contains("QCodeHarness"), "静态脚本不应来自 CDN");

    let (status, body) = post(
        &format!("{base}/command"),
        json!({"id": "web-create", "command": {"op": "create", "goal": "修复登录页", "kind": "BugFix"}}),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(
        serde_json::from_str::<Value>(&body).unwrap()["result"],
        "task"
    );

    let (status, _body) = post(
        &format!("{base}/command"),
        json!({"id": "web-input", "command": {"op": "input", "task_id": "task-web-1", "message_id": "m1", "text": "保留日志"}}),
    )
    .await;
    assert_eq!(status, 200);

    let (status, _, body) = get(&format!("{base}/events")).await;
    assert_eq!(status, 200);
    let events = json_lines(&body);
    assert!(
        events
            .iter()
            .any(|event| event["payload"]["type"] == "approval_waiting")
    );
    assert!(
        events
            .iter()
            .any(|event| event["payload"]["type"] == "task_created")
    );
    assert!(
        events
            .iter()
            .any(|event| event["payload"]["type"] == "input_received")
    );

    let (status, _, body) = get(&format!("{base}/command?op=approvals")).await;
    assert_eq!(status, 200);
    let approvals: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(approvals["result"], "pending_approvals");
    assert_eq!(approvals["requests"][0]["approval_id"], "approval-web-1");

    let (status, _) = post(
        &format!("{base}/command"),
        json!({"id": "web-decide", "command": {"op": "decide", "approval_id": "approval-web-1", "task_id": "task-web-1", "execution_id": "execution-task-web-1", "allow": true}}),
    )
    .await;
    assert_eq!(status, 200);

    let (status, _, body) = get(&format!("{base}/events?after=0&limit=100")).await;
    assert_eq!(status, 200);
    let events = json_lines(&body);
    let cli_events = service
        .journal
        .read(0, 100)
        .unwrap()
        .into_iter()
        .map(|event| serde_json::to_value(event).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(events, cli_events, "Web UI 与 CLI 必须消费同一份服务事件");
    let denial = events
        .iter()
        .find(|event| {
            event["payload"]["type"] == "audit"
                && event["payload"]["event"]["ApprovalDecided"]["allowed"] == false
        })
        .expect("否决证据必须留在事件流");
    assert_eq!(
        denial["payload"]["event"]["ApprovalDecided"]["tool"],
        "write_file"
    );
    assert!(
        events
            .iter()
            .any(|event| event["payload"]["type"] == "approval_resolved"
                && event["payload"]["allowed"] == true)
    );
    assert!(
        events
            .iter()
            .any(|event| event["payload"]["type"] == "completed"
                && event["payload"]["answer"] == "已完成并保留审批证据")
    );
    assert_eq!(
        *service.request_id.lock().unwrap(),
        "web-decide",
        "命令须原样转发给 TaskService"
    );

    let raw = fs::read_to_string(dir.log()).unwrap();
    assert_eq!(json_lines(&raw), cli_events, "events.jsonl 是同一事实来源");
    timeout(Duration::from_secs(3), server.shutdown())
        .await
        .expect("关闭不应挂住")
        .expect("关闭应成功");
}

#[tokio::test]
async fn web_ui_rejects_remote_connections_and_unknown_commands() {
    let dir = TestDirectory::new();
    let journal = Arc::new(JsonEventJournal::open(dir.log()).unwrap());
    let service = Arc::new(ScriptedService {
        journal,
        workspace: dir.0.clone(),
        request_id: Mutex::new(String::new()),
        input_seen: AtomicBool::new(false),
    });
    let server = LocalWebUiServer::new(service.clone());
    let port = server.bind(config(0, &dir.log())).await.unwrap();
    let base = format!("http://127.0.0.1:{port}");

    let (status, _, _) = get(&format!("{base}/nope")).await;
    assert_eq!(status, 404);
    let (status, body) = post(
        &format!("{base}/command"),
        json!({"id": "bad", "command": {"op": "unknown"}}),
    )
    .await;
    assert_eq!(status, 400);
    assert!(body.contains("invalid_command"));
    // 只监听 127.0.0.1；Windows 的弱主机模型允许其他 127/8 源地址连入，
    // 因此服务端显式拒绝来源不是 127.0.0.1 的连接，再检查非本机网段不可达。
    let loopback = tokio::process::Command::new("curl")
        .args([
            "-sS",
            "--max-time",
            "2",
            "--interface",
            "127.0.0.2",
            &format!("{base}/"),
        ])
        .output()
        .await
        .unwrap();
    assert!(
        !loopback.status.success(),
        "来源不是 127.0.0.1 的连接应被服务拒绝"
    );
    let rejected_config = WebUiConfig {
        host: "0.0.0.0".into(),
        ..config(0, &dir.log())
    };
    assert_eq!(
        LocalWebUiServer::new(service.clone())
            .bind(rejected_config)
            .await
            .unwrap_err()
            .code,
        "web_ui_bind",
        "配置非本机地址时必须明确报错"
    );
    let stream = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    assert_eq!(stream.peer_addr().unwrap().ip().to_string(), "127.0.0.1");
    drop(stream);
    server.shutdown().await.unwrap();
}

#[test]
fn nesting_allows_three_levels_and_stops_unbounded_children() {
    // 深度 0/1/2（根/子/孙）在有限上限内，深度 3 起不再装配子运行。
    let mut parent = String::from("root");
    for depth in 0..MAX_NESTING {
        assert!(
            validate_nesting(depth).is_ok(),
            "深度 {depth} 应在有限上限内"
        );
        let frame = DelegationFrame::new(
            parent.clone(),
            depth,
            ApprovalState::Pending,
            ApprovalFramePayload::ApprovalRequest {
                approval_id: format!("approval-{depth}"),
                call: ToolCall {
                    id: CallId::new("call-web-1"),
                    name: "write_file".into(),
                    arguments: json!({"goal": format!("第 {depth} 层")}),
                },
                reason: None,
            },
        )
        .unwrap();
        assert_eq!(frame.depth, depth);
        assert_eq!(
            frame.version,
            qcode_harness::application::nested_delegation::FRAME_VERSION
        );
        parent = frame.parent_id;
    }
    let over = validate_nesting(MAX_NESTING).unwrap_err();
    assert!(over.code == "nesting_limit" || over.message.contains("深度"));
    assert_eq!(
        DelegationFrame::new(
            "root",
            MAX_NESTING,
            ApprovalState::Pending,
            ApprovalFramePayload::ApprovalRequest {
                approval_id: "too-deep".into(),
                call: ToolCall {
                    id: CallId::new("call-web-1"),
                    name: "write_file".into(),
                    arguments: json!({}),
                },
                reason: None,
            },
        )
        .unwrap_err()
        .code,
        "nesting_limit"
    );
    assert_eq!(
        validate_nesting(MAX_NESTING + 1).unwrap_err().code,
        "nesting_limit",
        "子子任务不能无限派生"
    );
}

#[tokio::test]
async fn approval_chain_uses_explicit_frames_per_level_and_keeps_each_evidence() {
    let dir = TestDirectory::new();
    let journal = Arc::new(JsonEventJournal::open(dir.log()).unwrap());
    let transport = ApprovalFrameTransport::new(journal.clone());
    let call = pending().call;
    let mut frames = Vec::new();
    for depth in 0..MAX_NESTING {
        let request = approval_frame(
            &format!("level-{depth}"),
            depth,
            format!("approval-{depth}"),
            ApprovalFramePayload::ApprovalRequest {
                approval_id: format!("approval-{depth}"),
                call: call.clone(),
                reason: Some(format!("第 {depth} 级审批")),
            },
        )
        .unwrap();
        let line = frame_line(&request).unwrap();
        let parsed = parse_frame(&line).unwrap();
        assert_eq!(parsed.depth, depth);
        assert_eq!(parsed.approval_state, ApprovalState::Pending);
        frames.push(parsed);
    }
    let outcomes = timeout(
        Duration::from_secs(3),
        transport.exchange(Some(&context()), frames.clone()),
    )
    .await
    .expect("显式帧交换不应挂住")
    .expect("每一级审批都应写入服务事件日志");
    assert_eq!(
        outcomes.len(),
        MAX_NESTING * 2,
        "每级有 request 与 resolved 两类帧"
    );
    let events = journal.read(0, 100).unwrap();
    assert_eq!(events.len(), MAX_NESTING * 2);
    let raw = fs::read_to_string(dir.log()).unwrap();
    let lines: Vec<Value> = raw
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(lines.len(), MAX_NESTING * 2);
    for depth in 0..MAX_NESTING {
        assert!(events.iter().any(|event| matches!(
            &event.payload,
            ServiceEventPayload::Audit { event: AuditEvent::ApprovalReasked { tool, .. } } if tool == "write_file"
        )), "第 {depth} 级审批请求不能靠读回合并推断");
        assert!(events.iter().any(|event| matches!(
            &event.payload,
            ServiceEventPayload::ApprovalResolved { approval_id, allowed: true, .. }
                if approval_id == &format!("approval-{depth}")
        )));
    }
    assert_eq!(
        validate_nesting(MAX_NESTING + 1).unwrap_err().code,
        "nesting_limit",
        "子子任务不能无限派生"
    );
}

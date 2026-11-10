//! 版本化 NDJSON stdio 协议与 Rust SDK 的外部契约测试。
//!
//! 用内存服务替身驱动协议，不启动模型、不访问真实工作区。协议封套的
//! 版本、错误码、事件流与请求 ID 都是对外契约，改动要能在这里被发现。

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use qcode_harness::application::port::BoxFuture;
use qcode_harness::application::service::{EventSubscription, TaskService};
use qcode_harness::domain::budget::Consumption;
use qcode_harness::domain::service::{
    EventOrigin, ServiceCommand, ServiceError, ServiceEvent, ServiceEventPayload, ServiceReply,
    TaskStatus, TaskView, PROTOCOL_VERSION,
};
use qcode_harness::domain::task::TaskKind;
use qcode_harness::interface::sdk::SdkClient;
use qcode_harness::interface::stdio::{StdioRequest, StdioResponse, StdioServer};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, duplex};
use tokio::sync::Notify;

struct Bus {
    events: Mutex<VecDeque<ServiceEvent>>,
    notify: Notify,
}

impl Bus {
    fn push(&self, event: ServiceEvent) {
        self.events.lock().unwrap().push_back(event);
        self.notify.notify_waiters();
    }
}

struct BusSubscription {
    bus: Arc<Bus>,
}

impl EventSubscription for BusSubscription {
    fn next(&mut self) -> BoxFuture<'_, Result<ServiceEvent, ServiceError>> {
        Box::pin(async move {
            loop {
                if let Some(event) = self.bus.events.lock().unwrap().pop_front() {
                    return Ok(event);
                }
                self.bus.notify.notified().await;
            }
        })
    }
}

struct StubService {
    bus: Arc<Bus>,
    seen: Mutex<Vec<ServiceCommand>>,
}

impl StubService {
    fn new(bus: Arc<Bus>) -> Self {
        Self { bus, seen: Mutex::new(Vec::new()) }
    }

    fn view(&self) -> TaskView {
        TaskView {
            task_id: "task-1".into(),
            session_id: "session-1".into(),
            execution_id: "execution-1".into(),
            attempt: 1,
            status: TaskStatus::Running,
            workspace: std::path::PathBuf::from("worktree"),
            base_revision: "a".repeat(40),
            source_task: None,
            parent_task: None,
            answer: None,
            error: None,
            verification: String::new(),
            consumption: Consumption::default(),
        }
    }
}

impl TaskService for StubService {
    fn call<'a>(
        &'a self,
        _request_id: &'a str,
        command: ServiceCommand,
    ) -> BoxFuture<'a, Result<ServiceReply, ServiceError>> {
        Box::pin(async move {
            self.seen.lock().unwrap().push(command.clone());
            match command {
                ServiceCommand::Create { .. } => Ok(ServiceReply::Task { task: self.view() }),
                ServiceCommand::List => Ok(ServiceReply::Tasks { tasks: vec![self.view()] }),
                ServiceCommand::Events { after, .. } => Ok(ServiceReply::Events { events: vec![], next: after }),
                ServiceCommand::Shutdown => Ok(ServiceReply::Shutdown),
                ServiceCommand::Get { .. } => Ok(ServiceReply::Task { task: self.view() }),
                ServiceCommand::Approvals { .. } => Ok(ServiceReply::PendingApprovals { requests: vec![] }),
                _ => Ok(ServiceReply::Accepted { task_id: "task-1".into(), execution_id: None }),
            }
        })
    }

    fn subscribe(&self, _after: u64) -> Result<Box<dyn EventSubscription>, ServiceError> {
        Ok(Box::new(BusSubscription { bus: self.bus.clone() }))
    }
}

fn event(sequence: u64, payload: ServiceEventPayload) -> ServiceEvent {
    ServiceEvent {
        version: PROTOCOL_VERSION,
        sequence,
        time_unix_ms: SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64,
        task_id: Some("task-1".into()),
        session_id: Some("session-1".into()),
        execution_id: Some("execution-1".into()),
        origin: EventOrigin::default(),
        payload,
    }
}

async fn read_line<R: AsyncBufReadExt + Unpin>(reader: &mut R) -> String {
    let mut line = String::new();
    let count = tokio::time::timeout(std::time::Duration::from_secs(5), reader.read_line(&mut line))
        .await
        .expect("协议应在超时前输出")
        .unwrap();
    assert!(count > 0, "服务不应提前关闭输出");
    line
}

#[tokio::test]
async fn hello_reports_protocol_version_and_unknown_versions_are_rejected() {
    let bus = Arc::new(Bus { events: Mutex::new(VecDeque::new()), notify: Notify::new() });
    let service = Arc::new(StubService::new(bus.clone()));
    let server = StdioServer::new(service.clone());
    let (client, server_io) = duplex(16 * 1024);
    let (client_read, mut client_write) = tokio::io::split(client);
    let (server_read, server_write) = tokio::io::split(server_io);
    let task = tokio::spawn(async move { server.serve(server_read, server_write).await });

    let mut reader = BufReader::new(client_read);
    let hello: StdioResponse = serde_json::from_str(&read_line(&mut reader).await).unwrap();
    assert!(matches!(hello, StdioResponse::Hello { version } if version == PROTOCOL_VERSION));

    let wrong = StdioRequest {
        version: PROTOCOL_VERSION + 1,
        id: "r1".into(),
        command: ServiceCommand::List,
    };
    client_write.write_all(format!("{}\n", serde_json::to_string(&wrong).unwrap()).as_bytes()).await.unwrap();
    let response: StdioResponse = serde_json::from_str(&read_line(&mut reader).await).unwrap();
    match response {
        StdioResponse::Error { id, error, .. } => {
            assert_eq!(id, "r1");
            assert_eq!(error.code, "unsupported_version");
        }
        other => panic!("应拒绝未知版本，实际 {other:?}"),
    }
    assert!(service.seen.lock().unwrap().is_empty(), "被拒绝的请求不能执行命令");

    // 拒绝之后连接仍然可用：合法版本继续处理。
    let ok = StdioRequest { version: PROTOCOL_VERSION, id: "r2".into(), command: ServiceCommand::List };
    client_write.write_all(format!("{}\n", serde_json::to_string(&ok).unwrap()).as_bytes()).await.unwrap();
    let response: StdioResponse = serde_json::from_str(&read_line(&mut reader).await).unwrap();
    assert!(matches!(response, StdioResponse::Reply { id, result: ServiceReply::Tasks { .. }, .. } if id == "r2"));

    client_write.write_all(format!("{}\n", serde_json::to_string(&StdioRequest {
        version: PROTOCOL_VERSION, id: "r3".into(), command: ServiceCommand::Shutdown,
    }).unwrap()).as_bytes()).await.unwrap();
    let response: StdioResponse = serde_json::from_str(&read_line(&mut reader).await).unwrap();
    assert!(matches!(response, StdioResponse::Reply { result: ServiceReply::Shutdown, .. }));
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn commands_round_trip_and_events_are_pushed_on_the_same_stream() {
    let bus = Arc::new(Bus { events: Mutex::new(VecDeque::new()), notify: Notify::new() });
    let service = Arc::new(StubService::new(bus.clone()));
    let (client, server_io) = duplex(16 * 1024);
    let (client_read, mut client_write) = tokio::io::split(client);
    let (server_read, server_write) = tokio::io::split(server_io);
    let server = StdioServer::new(service.clone());
    let task = tokio::spawn(async move { server.serve(server_read, server_write).await });
    let mut reader = BufReader::new(client_read);
    let _hello: StdioResponse = serde_json::from_str(&read_line(&mut reader).await).unwrap();

    let create = StdioRequest {
        version: PROTOCOL_VERSION,
        id: "create-1".into(),
        command: ServiceCommand::Create { goal: "修复解析".into(), kind: TaskKind::BugFix },
    };
    client_write.write_all(format!("{}\n", serde_json::to_string(&create).unwrap()).as_bytes()).await.unwrap();
    let reply: StdioResponse = serde_json::from_str(&read_line(&mut reader).await).unwrap();
    assert!(matches!(reply, StdioResponse::Reply { id, result: ServiceReply::Task { .. }, .. } if id == "create-1"));

    // 事件与命令回复共用一条 stdout，靠封套类型区分；回放不触发命令。
    bus.push(event(1, ServiceEventPayload::ExecutionStarted));
    let pushed: StdioResponse = serde_json::from_str(&read_line(&mut reader).await).unwrap();
    assert!(matches!(pushed, StdioResponse::Event { event, .. } if event.sequence == 1));
    assert_eq!(service.seen.lock().unwrap().len(), 1, "事件推送不能伪装成命令");

    client_write.write_all(format!("{}\n", serde_json::to_string(&StdioRequest {
        version: PROTOCOL_VERSION, id: "r3".into(), command: ServiceCommand::Shutdown,
    }).unwrap()).as_bytes()).await.unwrap();
    let reply: StdioResponse = serde_json::from_str(&read_line(&mut reader).await).unwrap();
    assert!(matches!(reply, StdioResponse::Reply { result: ServiceReply::Shutdown, .. }));
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn malformed_lines_are_reported_without_killing_the_connection() {
    let bus = Arc::new(Bus { events: Mutex::new(VecDeque::new()), notify: Notify::new() });
    let service = Arc::new(StubService::new(bus));
    let (client, server_io) = duplex(16 * 1024);
    let (client_read, mut client_write) = tokio::io::split(client);
    let (server_read, server_write) = tokio::io::split(server_io);
    let server = StdioServer::new(service);
    let task = tokio::spawn(async move { server.serve(server_read, server_write).await });
    let mut reader = BufReader::new(client_read);
    let _hello: StdioResponse = serde_json::from_str(&read_line(&mut reader).await).unwrap();

    client_write.write_all("{ not json\n".as_bytes()).await.unwrap();
    let response: StdioResponse = serde_json::from_str(&read_line(&mut reader).await).unwrap();
    assert!(matches!(response, StdioResponse::Error { error, .. } if error.code == "invalid_request"));

    client_write.write_all(format!("{}\n", serde_json::to_string(&StdioRequest {
        version: PROTOCOL_VERSION, id: "list".into(), command: ServiceCommand::List,
    }).unwrap()).as_bytes()).await.unwrap();
    let response: StdioResponse = serde_json::from_str(&read_line(&mut reader).await).unwrap();
    assert!(matches!(response, StdioResponse::Reply { result: ServiceReply::Tasks { .. }, .. }));

    client_write.write_all(format!("{}\n", serde_json::to_string(&StdioRequest {
        version: PROTOCOL_VERSION, id: "quit".into(), command: ServiceCommand::Shutdown,
    }).unwrap()).as_bytes()).await.unwrap();
    let _: StdioResponse = serde_json::from_str(&read_line(&mut reader).await).unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn sdk_call_matches_request_ids_and_buffers_pushed_events() {
    let bus = Arc::new(Bus { events: Mutex::new(VecDeque::new()), notify: Notify::new() });
    let service = Arc::new(StubService::new(bus.clone()));
    let (client, server_io) = duplex(16 * 1024);
    let (server_read, server_write) = tokio::io::split(server_io);
    let server = StdioServer::new(service);
    let task = tokio::spawn(async move { server.serve(server_read, server_write).await });

    let sdk = SdkClient::from_io(client).await.unwrap();
    bus.push(event(1, ServiceEventPayload::ExecutionStarted));
    let outcome = sdk.call("create-1", ServiceCommand::Create {
        goal: "检查".into(), kind: TaskKind::Review,
    }).await.unwrap();
    assert!(matches!(outcome, ServiceReply::Task { .. }));
    // call 期间到达的事件被缓存，随后仍可读取，不会被当成回复丢掉。
    assert_eq!(sdk.next_event().await.unwrap().sequence, 1);
    assert!(matches!(sdk.call("quit", ServiceCommand::Shutdown).await.unwrap(), ServiceReply::Shutdown));
    drop(sdk);
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn unsupported_versions_are_rejected_before_decoding_the_command() {
    let service = Arc::new(StubService::new(Arc::new(Bus {
        events: Mutex::new(VecDeque::new()), notify: Notify::new(),
    })));
    let (client, server_io) = duplex(16 * 1024);
    let (client_read, mut client_write) = tokio::io::split(client);
    let (server_read, server_write) = tokio::io::split(server_io);
    let server = StdioServer::new(service.clone());
    let task = tokio::spawn(async move { server.serve(server_read, server_write).await });
    let mut reader = BufReader::new(client_read);
    let _hello = read_line(&mut reader).await;
    client_write.write_all(b"{\"version\":999,\"id\":\"newer-client\",\"command\":{\"op\":\"future_command\"}}\n").await.unwrap();
    let reply: StdioResponse = serde_json::from_str(&read_line(&mut reader).await).unwrap();
    assert!(matches!(reply, StdioResponse::Error { id, error, .. }
        if id == "newer-client" && error.code == "unsupported_version"));
    assert!(service.seen.lock().unwrap().is_empty());
    drop(reader);
    drop(client_write);
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn a_partly_received_request_survives_interleaved_events() {
    let bus = Arc::new(Bus { events: Mutex::new(VecDeque::new()), notify: Notify::new() });
    let service = Arc::new(StubService::new(bus.clone()));
    let (client, server_io) = duplex(16 * 1024);
    let (client_read, mut client_write) = tokio::io::split(client);
    let (server_read, server_write) = tokio::io::split(server_io);
    let server = StdioServer::new(service);
    let task = tokio::spawn(async move { server.serve(server_read, server_write).await });
    let mut reader = BufReader::new(client_read);
    let _hello = read_line(&mut reader).await;
    client_write.write_all(b"{\"version\":1,\"id\":\"partial\",").await.unwrap();
    bus.push(event(1, ServiceEventPayload::ExecutionStarted));
    let pushed: StdioResponse = serde_json::from_str(&read_line(&mut reader).await).unwrap();
    assert!(matches!(pushed, StdioResponse::Event { .. }));
    client_write.write_all(b"\"command\":{\"op\":\"list\"}}\n").await.unwrap();
    let reply: StdioResponse = serde_json::from_str(&read_line(&mut reader).await).unwrap();
    assert!(matches!(reply, StdioResponse::Reply { id, .. } if id == "partial"));
    drop(reader);
    drop(client_write);
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn oversized_frames_are_drained_and_the_next_request_still_works() {
    let service = Arc::new(StubService::new(Arc::new(Bus {
        events: Mutex::new(VecDeque::new()), notify: Notify::new(),
    })));
    let (client, server_io) = duplex(16 * 1024);
    let (client_read, mut client_write) = tokio::io::split(client);
    let (server_read, server_write) = tokio::io::split(server_io);
    let server = StdioServer::new(service);
    let task = tokio::spawn(async move { server.serve(server_read, server_write).await });
    let mut reader = BufReader::new(client_read);
    let _hello = read_line(&mut reader).await;
    let mut oversized = vec![b'x'; qcode_harness::interface::stdio::MAX_FRAME_BYTES + 1];
    oversized.push(b'\n');
    client_write.write_all(&oversized).await.unwrap();
    let reply: StdioResponse = serde_json::from_str(&read_line(&mut reader).await).unwrap();
    assert!(matches!(reply, StdioResponse::Error { error, .. } if error.code == "frame_too_large"));
    client_write.write_all(b"{\"version\":1,\"id\":\"next\",\"command\":{\"op\":\"list\"}}\n").await.unwrap();
    let reply: StdioResponse = serde_json::from_str(&read_line(&mut reader).await).unwrap();
    assert!(matches!(reply, StdioResponse::Reply { id, .. } if id == "next"));
    drop(reader);
    drop(client_write);
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn sdk_demultiplexes_concurrent_out_of_order_replies() {
    let (client, server_io) = duplex(16 * 1024);
    let server = tokio::spawn(async move {
        let (read, mut write) = tokio::io::split(server_io);
        write.write_all(b"{\"type\":\"hello\",\"version\":1}\n").await.unwrap();
        let mut read = BufReader::new(read);
        let first: StdioRequest = serde_json::from_str(&read_line(&mut read).await).unwrap();
        let second: StdioRequest = serde_json::from_str(&read_line(&mut read).await).unwrap();
        for request in [second, first] {
            let reply = StdioResponse::Reply {
                version: PROTOCOL_VERSION,
                result: ServiceReply::Accepted { task_id: request.id.clone(), execution_id: None },
                id: request.id,
            };
            write.write_all(format!("{}\n", serde_json::to_string(&reply).unwrap()).as_bytes()).await.unwrap();
        }
    });
    let sdk = SdkClient::from_io(client).await.unwrap();
    let (left, right) = tokio::join!(sdk.call("left", ServiceCommand::List), sdk.call("right", ServiceCommand::List));
    assert!(matches!(left.unwrap(), ServiceReply::Accepted { task_id, .. } if task_id == "left"));
    assert!(matches!(right.unwrap(), ServiceReply::Accepted { task_id, .. } if task_id == "right"));
    server.await.unwrap();
}

#[tokio::test]
async fn sdk_reports_event_overflow_after_preserving_buffered_events() {
    let (client, server_io) = duplex(16 * 1024);
    let (sent, received) = tokio::sync::oneshot::channel();
    let (release, wait_release) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (_read, mut write) = tokio::io::split(server_io);
        write.write_all(b"{\"type\":\"hello\",\"version\":1}\n").await.unwrap();
        for sequence in 1..=qcode_harness::interface::sdk::SDK_EVENT_CAPACITY + 1 {
            let response = StdioResponse::Event {
                version: PROTOCOL_VERSION,
                event: event(sequence as u64, ServiceEventPayload::ExecutionStarted),
            };
            write.write_all(format!("{}\n", serde_json::to_string(&response).unwrap()).as_bytes()).await.unwrap();
        }
        sent.send(()).unwrap();
        wait_release.await.unwrap();
    });
    let sdk = SdkClient::from_io(client).await.unwrap();
    received.await.unwrap();
    // 等待断连通过命令通道被确认，期间不消费事件，使溢出可确定复现。
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), sdk.call("after-overflow", ServiceCommand::List)).await.unwrap();
    assert_eq!(result.unwrap_err().code, "lagged");
    for sequence in 1..=qcode_harness::interface::sdk::SDK_EVENT_CAPACITY {
        assert_eq!(sdk.next_event().await.unwrap().sequence, sequence as u64);
    }
    assert_eq!(sdk.next_event().await.unwrap_err().code, "lagged");
    release.send(()).unwrap();
    server.await.unwrap();
}

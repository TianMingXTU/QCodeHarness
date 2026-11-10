//! Rust stdio 客户端。单个读取任务分发回复与事件，调用方可并发提交命令。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;

use crate::domain::service::{PROTOCOL_VERSION, ServiceCommand, ServiceError, ServiceEvent, ServiceReply};
use super::stdio::{Frame, FrameReader, MAX_FRAME_BYTES, StdioRequest, StdioResponse, frame_too_large, transport_error, unsupported_version};

/// 未消费的实时事件有界；溢出后显式报错，调用方用最后成功序号补读日志。
pub const SDK_EVENT_CAPACITY: usize = 128;
const MAX_PENDING_CALLS: usize = 64;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

enum Reply {
    Service(ServiceReply),
    Subscribed(u64),
}

type ReplySender = oneshot::Sender<Result<Reply, ServiceError>>;

#[derive(Default)]
struct Pending {
    replies: HashMap<String, ReplySender>,
    closed: Option<ServiceError>,
}

struct Shared {
    pending: Mutex<Pending>,
    failure: watch::Sender<Option<ServiceError>>,
}

impl Shared {
    fn fail(&self, error: ServiceError) {
        let mut pending = self.pending.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if pending.closed.is_some() { return; }
        pending.closed = Some(error.clone());
        self.failure.send_replace(Some(error.clone()));
        for (_, reply) in pending.replies.drain() {
            let _ = reply.send(Err(error.clone()));
        }
    }

    fn deliver(&self, id: &str, result: Result<Reply, ServiceError>) {
        let reply = self.pending.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).replies.remove(id);
        // 调用方可能已取消等待；迟到的回复不能匹配到其他请求。
        if let Some(reply) = reply { let _ = reply.send(result); }
    }
}

/// 拥有一条 stdio 连接的客户端；丢弃后停止本地读写任务。
///
/// 命令 ID 由调用方提供。有副作用的请求若等待被取消，服务仍可能已执行；
/// 重试必须沿用原 ID 与参数。`next_event` 有独立通道，不阻塞命令回复。
/// 子进程由宿主拥有；发送 Shutdown 后还应等待子进程退出。
pub struct SdkClient {
    outgoing: mpsc::Sender<Vec<u8>>,
    events: tokio::sync::Mutex<mpsc::Receiver<Result<ServiceEvent, ServiceError>>>,
    shared: Arc<Shared>,
    reader: JoinHandle<()>,
    writer: JoinHandle<()>,
}

impl SdkClient {
    /// 接入可双向读写的字节流，并校验服务的首帧版本声明。
    pub async fn from_io<T>(io: T) -> Result<Self, ServiceError>
    where T: AsyncRead + AsyncWrite + Unpin + Send + 'static {
        let (reader, writer) = tokio::io::split(io);
        Self::from_parts(reader, writer).await
    }

    /// 接入分离的输出与输入句柄，例如子进程的 stdout 与 stdin。
    pub async fn from_parts<R, W>(reader: R, writer: W) -> Result<Self, ServiceError>
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let mut frames = FrameReader::new(reader);
        let hello = tokio::time::timeout(HANDSHAKE_TIMEOUT, read_response(&mut frames)).await
            .map_err(|_| ServiceError::new("handshake_timeout", "等待服务版本声明超时"))??;
        if hello.version() != PROTOCOL_VERSION { return Err(unsupported_version(hello.version())); }
        if !matches!(hello, StdioResponse::Hello { .. }) {
            return Err(ServiceError::new("invalid_handshake", "服务首帧必须为 hello"));
        }
        let (failure, _) = watch::channel(None);
        let shared = Arc::new(Shared { pending: Mutex::new(Pending::default()), failure });
        let (events_tx, events_rx) = mpsc::channel(SDK_EVENT_CAPACITY);
        let (outgoing, outgoing_rx) = mpsc::channel(MAX_PENDING_CALLS);
        let reader_state = shared.clone();
        let reader = tokio::spawn(async move { read_loop(frames, reader_state, events_tx).await });
        let writer_state = shared.clone();
        let writer = tokio::spawn(async move { write_loop(writer, outgoing_rx, writer_state).await });
        Ok(Self { outgoing, events: tokio::sync::Mutex::new(events_rx), shared, reader, writer })
    }

    /// 等待与 `id` 匹配的命令回复；期间到达的事件保留在独立的有界队列中。
    pub async fn call(&self, id: &str, command: ServiceCommand) -> Result<ServiceReply, ServiceError> {
        let request = StdioRequest { version: PROTOCOL_VERSION, id: id.to_owned(), command };
        match self.exchange(id, &request).await? {
            Reply::Service(reply) => Ok(reply),
            Reply::Subscribed(_) => Err(ServiceError::new("unexpected_reply", "业务命令收到了订阅结论")),
        }
    }

    /// 替换事件订阅，从排他游标 `after` 回放后接收实时流。
    ///
    /// 已在本地队列中的事件仍可读取；按序号去重后再推进持久化游标。
    pub async fn subscribe(&self, id: &str, after: u64) -> Result<(), ServiceError> {
        let request = serde_json::json!({
            "version": PROTOCOL_VERSION, "id": id,
            "command": { "op": "subscribe", "after": after },
        });
        match self.exchange(id, &request).await? {
            Reply::Subscribed(cursor) if cursor == after => Ok(()),
            _ => Err(ServiceError::new("unexpected_reply", "订阅结论与请求不符")),
        }
    }

    /// 等待一条事件；溢出或断连时返回明确错误，不静默丢弃事件。
    pub async fn next_event(&self) -> Result<ServiceEvent, ServiceError> {
        let mut events = self.events.lock().await;
        let mut failure = self.shared.failure.subscribe();
        loop {
            if let Ok(event) = events.try_recv() { return event; }
            if let Some(error) = failure.borrow().clone() { return Err(error); }
            tokio::select! {
                event = events.recv() => return event.unwrap_or_else(|| Err(ServiceError::new("transport_closed", "事件通道已关闭"))),
                _ = failure.changed() => {}
            }
        }
    }

    async fn exchange<T: serde::Serialize>(&self, id: &str, request: &T) -> Result<Reply, ServiceError> {
        if id.trim().is_empty() { return Err(ServiceError::new("invalid_request", "请求 ID 不能为空")); }
        let mut bytes = serde_json::to_vec(request).map_err(|error| ServiceError::new("protocol_encoding", error.to_string()))?;
        bytes.push(b'\n');
        if bytes.len() > MAX_FRAME_BYTES { return Err(frame_too_large()); }
        let (reply, receiver) = oneshot::channel();
        {
            let mut pending = self.shared.pending.lock().map_err(|_| ServiceError::new("client_unhealthy", "客户端请求表锁已损坏"))?;
            if let Some(error) = &pending.closed { return Err(error.clone()); }
            if pending.replies.contains_key(id) { return Err(ServiceError::new("request_in_flight", "请求 ID 已有等待中的调用")); }
            if pending.replies.len() >= MAX_PENDING_CALLS { return Err(ServiceError::new("client_busy", "等待中的调用数量已达到上限")); }
            pending.replies.insert(id.to_owned(), reply);
        }
        let _wait = PendingWait { shared: self.shared.clone(), id: id.to_owned() };
        // 写入由单独任务完成。调用方取消等待不会在半个 JSON 帧中断 write_all。
        self.outgoing.send(bytes).await.map_err(|_| ServiceError::new("transport_closed", "写入通道已关闭"))?;
        receiver.await.map_err(|_| ServiceError::new("transport_closed", "回复通道已关闭"))?
    }
}

impl Drop for SdkClient {
    fn drop(&mut self) {
        self.shared.fail(ServiceError::new("client_closed", "客户端已关闭"));
        self.reader.abort();
        self.writer.abort();
    }
}

struct PendingWait { shared: Arc<Shared>, id: String }

impl Drop for PendingWait {
    fn drop(&mut self) {
        if let Ok(mut pending) = self.shared.pending.lock() { pending.replies.remove(&self.id); }
    }
}

async fn read_response<R: AsyncRead + Unpin>(frames: &mut FrameReader<R>) -> Result<StdioResponse, ServiceError> {
    match frames.next().await? {
        Frame::End => Err(ServiceError::new("transport_closed", "服务关闭了输出")),
        Frame::TooLarge => Err(frame_too_large()),
        Frame::Line(line) => serde_json::from_slice(&line).map_err(|error| ServiceError::new("invalid_response", error.to_string())),
    }
}

async fn read_loop<R: AsyncRead + Unpin>(
    mut frames: FrameReader<R>, shared: Arc<Shared>, events: mpsc::Sender<Result<ServiceEvent, ServiceError>>,
) {
    let mut failure = shared.failure.subscribe();
    loop {
        let response = tokio::select! {
            response = read_response(&mut frames) => response,
            _ = failure.changed() => return,
        };
        let response = match response {
            Ok(response) if response.version() == PROTOCOL_VERSION => response,
            Ok(response) => { shared.fail(unsupported_version(response.version())); return; }
            Err(error) => { shared.fail(error); return; }
        };
        let event = match response {
            StdioResponse::Reply { id, result, .. } => { shared.deliver(&id, Ok(Reply::Service(result))); continue; }
            StdioResponse::Subscribed { id, after, .. } => { shared.deliver(&id, Ok(Reply::Subscribed(after))); continue; }
            StdioResponse::Error { id, error, .. } if !id.is_empty() => { shared.deliver(&id, Err(error)); continue; }
            StdioResponse::Error { error, .. } => Err(error),
            StdioResponse::Event { event, .. } => {
                if event.version != PROTOCOL_VERSION { shared.fail(unsupported_version(event.version)); return; }
                Ok(event)
            }
            StdioResponse::Hello { .. } => { shared.fail(ServiceError::new("invalid_response", "重复的 hello 帧")); return; }
        };
        if events.try_send(event).is_err() {
            shared.fail(ServiceError::new("lagged", "客户端事件队列已满；读取已缓存事件后，用最后成功序号补读并重新连接"));
            return;
        }
    }
}

async fn write_loop<W: AsyncWrite + Unpin>(mut writer: W, mut outgoing: mpsc::Receiver<Vec<u8>>, shared: Arc<Shared>) {
    let mut failure = shared.failure.subscribe();
    loop {
        let bytes = tokio::select! {
            bytes = outgoing.recv() => match bytes { Some(bytes) => bytes, None => return },
            _ = failure.changed() => return,
        };
        if let Err(cause) = writer.write_all(&bytes).await {
            shared.fail(transport_error(cause));
            return;
        }
        if let Err(cause) = writer.flush().await {
            shared.fail(transport_error(cause));
            return;
        }
    }
}

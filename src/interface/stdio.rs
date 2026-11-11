//! 版本化 NDJSON 传输。stdout 只写协议帧，业务动作交给任务服务。

use std::collections::VecDeque;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};

use crate::application::port::BoxFuture;
use crate::application::service::TaskService;
use crate::domain::service::{PROTOCOL_VERSION, ServiceCommand, ServiceError, ServiceEvent, ServiceReply};

/// 封套计入上限；比单条服务事件多留空间，容纳事件外层的协议字段。
pub const MAX_FRAME_BYTES: usize = 2 * 1024 * 1024;
const MAX_PENDING_REQUESTS: usize = 64;

/// 客户端命令。重试有副作用的命令时必须沿用原请求 ID 与参数。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StdioRequest {
    pub version: u32,
    pub id: String,
    pub command: ServiceCommand,
}

/// stdout 上的帧；类型字段区分命令结论、实时事件与传输错误。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum StdioResponse {
    Hello { version: u32 },
    Reply { version: u32, id: String, result: ServiceReply },
    Event { version: u32, event: Box<ServiceEvent> },
    Error { version: u32, id: String, error: ServiceError },
    Subscribed { version: u32, id: String, after: u64 },
}

impl StdioResponse {
    pub(crate) fn version(&self) -> u32 {
        match self {
            Self::Hello { version } | Self::Reply { version, .. }
            | Self::Event { version, .. } | Self::Error { version, .. }
            | Self::Subscribed { version, .. } => *version,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireRequestHeader {
    version: u32,
    id: String,
    command: serde_json::Value,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum WireCommand {
    Service(ServiceCommand),
    Subscription(SubscriptionCommand),
}

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
enum SubscriptionCommand {
    Subscribe { after: u64 },
}

type PendingReply = BoxFuture<'static, (String, Result<ServiceReply, ServiceError>)>;

/// 单连接服务。初始游标为 0，可用 `subscribe` 传输命令替换回放游标。
///
/// 命令按接收顺序执行；等待命令结束时仍消费事件与输入，队列有界。慢客户端
/// 收到 `lagged` 后可补读事件并重新订阅，历史不会作为业务命令重放。
pub struct StdioServer {
    service: Arc<dyn TaskService>,
}

impl StdioServer {
    pub fn new(service: Arc<dyn TaskService>) -> Self {
        Self { service }
    }

    /// 在一对异步字节流上传输，EOF 或成功的 Shutdown 结束连接。
    ///
    /// 本函数不拥有服务生命周期；宿主应在任何退出路径上关闭任务服务。
    pub async fn serve<R, W>(&self, reader: R, mut writer: W) -> Result<(), ServiceError>
    where
        R: AsyncRead + Unpin,
        W: AsyncWrite + Unpin,
    {
        write_response(&mut writer, &StdioResponse::Hello { version: PROTOCOL_VERSION }).await?;
        let mut subscription = Some(self.service.subscribe(0)?);
        let mut frames = FrameReader::new(reader);
        let mut queue = VecDeque::new();
        let mut pending: Option<PendingReply> = None;
        let mut input_closed = false;
        let mut shutting_down = false;
        loop {
            if pending.is_none() {
                if let Some(request) = queue.pop_front() {
                    let service = self.service.clone();
                    let request: StdioRequest = request;
                    pending = Some(Box::pin(async move {
                        let result = service.call(&request.id, request.command).await;
                        (request.id, result)
                    }));
                } else if input_closed {
                    return Ok(());
                }
            }
            tokio::select! {
                frame = frames.next(), if !input_closed && !shutting_down => {
                    match frame? {
                        Frame::End => input_closed = true,
                        Frame::TooLarge => {
                            write_error(&mut writer, "", frame_too_large()).await?;
                        }
                        Frame::Line(line) => {
                            let header: WireRequestHeader = match serde_json::from_slice(&line) {
                                Ok(header) => header,
                                Err(cause) => {
                                    write_error(&mut writer, "", ServiceError::new("invalid_request", cause.to_string())).await?;
                                    continue;
                                }
                            };
                            if header.version != PROTOCOL_VERSION {
                                write_error(&mut writer, &header.id, unsupported_version(header.version)).await?;
                                continue;
                            }
                            if header.id.trim().is_empty() {
                                write_error(&mut writer, "", ServiceError::new("invalid_request", "请求 ID 不能为空")).await?;
                                continue;
                            }
                            let command: WireCommand = match serde_json::from_value(header.command) {
                                Ok(command) => command,
                                Err(cause) => {
                                    write_error(&mut writer, &header.id, ServiceError::new("invalid_request", cause.to_string())).await?;
                                    continue;
                                }
                            };
                            match command {
                                WireCommand::Subscription(SubscriptionCommand::Subscribe { after }) => {
                                    match self.service.subscribe(after) {
                                        Ok(next) => {
                                            subscription = Some(next);
                                            write_response(&mut writer, &StdioResponse::Subscribed {
                                                version: PROTOCOL_VERSION, id: header.id.clone(), after,
                                            }).await?;
                                        }
                                        Err(error) => write_error(&mut writer, &header.id, error).await?,
                                    }
                                }
                                WireCommand::Service(command) => {
                                    if queue.len() + usize::from(pending.is_some()) >= MAX_PENDING_REQUESTS {
                                        write_error(&mut writer, &header.id, ServiceError::new("server_busy", "待处理请求队列已满")).await?;
                                        continue;
                                    }
                                    shutting_down = matches!(command, ServiceCommand::Shutdown);
                                    queue.push_back(StdioRequest { version: header.version, id: header.id, command });
                                }
                            }
                        }
                    }
                }
                (id, result) = async {
                    match pending.as_mut() {
                        Some(reply) => reply.await,
                        None => std::future::pending().await,
                    }
                } => {
                    pending = None;
                    match result {
                        Ok(result) => {
                            let shutdown = matches!(result, ServiceReply::Shutdown);
                            let response = StdioResponse::Reply { version: PROTOCOL_VERSION, id: id.clone(), result };
                            match write_response(&mut writer, &response).await {
                                Err(error) if error.code == "frame_too_large" => write_error(&mut writer, &id, error).await?,
                                result => result?,
                            }
                            if shutdown { return Ok(()); }
                        }
                        Err(error) => {
                            write_error(&mut writer, &id, error).await?;
                            if shutting_down && queue.is_empty() { shutting_down = false; }
                        }
                    }
                }
                event = async {
                    match subscription.as_mut() {
                        Some(subscription) => subscription.next().await,
                        None => std::future::pending().await,
                    }
                } => {
                    match event {
                        Ok(event) => write_response(&mut writer, &StdioResponse::Event { version: PROTOCOL_VERSION, event: Box::new(event) }).await?,
                        Err(error) => {
                            subscription = None;
                            write_error(&mut writer, "", error).await?;
                        }
                    }
                }
            }
        }
    }
}

async fn write_error<W: AsyncWrite + Unpin>(writer: &mut W, id: &str, error: ServiceError) -> Result<(), ServiceError> {
    write_response(writer, &StdioResponse::Error { version: PROTOCOL_VERSION, id: id.to_owned(), error }).await
}

async fn write_response<W: AsyncWrite + Unpin>(writer: &mut W, response: &StdioResponse) -> Result<(), ServiceError> {
    let mut bytes = serde_json::to_vec(response).map_err(|error| ServiceError::new("protocol_encoding", error.to_string()))?;
    bytes.push(b'\n');
    if bytes.len() > MAX_FRAME_BYTES { return Err(frame_too_large()); }
    writer.write_all(&bytes).await.map_err(transport_error)?;
    writer.flush().await.map_err(transport_error)
}

pub(crate) fn transport_error(cause: std::io::Error) -> ServiceError {
    ServiceError::new("transport_io", cause.to_string())
}

pub(crate) fn unsupported_version(version: u32) -> ServiceError {
    ServiceError::new("unsupported_version", format!("不支持协议版本 {version}，当前版本为 {PROTOCOL_VERSION}"))
}

pub(crate) fn frame_too_large() -> ServiceError {
    ServiceError::new("frame_too_large", format!("单帧超过 {MAX_FRAME_BYTES} 字节上限"))
}

pub(crate) enum Frame {
    End,
    Line(Vec<u8>),
    TooLarge,
}

/// 部分输入保存在结构内，`select!` 被事件唤醒时不会丢掉半行请求。
pub(crate) struct FrameReader<R> {
    reader: BufReader<R>,
    bytes: Vec<u8>,
    oversized: bool,
}

impl<R: AsyncRead + Unpin> FrameReader<R> {
    pub(crate) fn new(reader: R) -> Self {
        Self { reader: BufReader::new(reader), bytes: Vec::new(), oversized: false }
    }

    pub(crate) async fn next(&mut self) -> Result<Frame, ServiceError> {
        loop {
            let available = self.reader.fill_buf().await.map_err(transport_error)?;
            if available.is_empty() {
                if self.oversized { return Ok(self.finish()); }
                return if self.bytes.is_empty() { Ok(Frame::End) } else { Ok(self.finish()) };
            }
            let newline = available.iter().position(|byte| *byte == b'\n');
            let count = newline.map_or(available.len(), |index| index + 1);
            if !self.oversized {
                if self.bytes.len() + count > MAX_FRAME_BYTES {
                    self.oversized = true;
                    self.bytes.clear();
                } else {
                    self.bytes.extend_from_slice(&available[..count]);
                }
            }
            self.reader.consume(count);
            if newline.is_some() { return Ok(self.finish()); }
        }
    }

    fn finish(&mut self) -> Frame {
        if std::mem::take(&mut self.oversized) {
            Frame::TooLarge
        } else {
            Frame::Line(std::mem::take(&mut self.bytes))
        }
    }
}

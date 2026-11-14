//! 本机单用户 Web UI 的 HTTP 服务实现。
//!
//! 只提供静态页面与事件/命令转发，不引入新状态机：页面看到的一切都来自
//! `TaskService` 的公开命令（CLI 也走同一套），保证证据语义与命令行一致。
//! 监听严格绑定 127.0.0.1，只服务单台本机，不做远程多用户或云形态。

use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::net::TcpListener;
use tokio::sync::watch::{Receiver, Sender};

use crate::application::port::BoxFuture;
use crate::application::service::TaskService;
use crate::application::web_ui::{StaticAssets, WebUiConfig, WebUiServer};
use crate::domain::service::{ServiceCommand, ServiceError, ServiceReply};

/// 单个请求体上限，防止本机页面误传超大内容把服务内存耗尽。
const MAX_BODY_BYTES: usize = 2 * 1024 * 1024;

/// 本机 Web UI 服务；把 HTTP 请求转成 `TaskService` 命令，不保存任何任务状态。
pub struct LocalWebUiServer {
    service: Arc<dyn TaskService>,
    host: OnceLock<String>,
    shutdown: Arc<Sender<bool>>,
    handle: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl LocalWebUiServer {
    /// 用同一个任务服务装配 Web 前端；页面与 CLI 共享一份事件来源。
    pub fn new(service: Arc<dyn TaskService>) -> Self {
        let (shutdown, _) = tokio::sync::watch::channel(false);
        Self {
            service,
            host: OnceLock::new(),
            shutdown: Arc::new(shutdown),
            handle: Mutex::new(None),
        }
    }

    /// 在独立任务里接受连接；关闭信号来自 bind 前订阅的 watch 通道，避免
    /// "绑定后立刻关闭"因接收方创建太晚而漏掉通知。
    async fn serve(
        listener: TcpListener,
        service: Arc<dyn TaskService>,
        assets: Arc<StaticAssets>,
        events_path: Arc<PathBuf>,
        mut shutdown_rx: Receiver<bool>,
    ) {
        loop {
            if *shutdown_rx.borrow() {
                break;
            }
            tokio::select! {
                stopped = shutdown_rx.changed() => {
                    if stopped.is_err() || *shutdown_rx.borrow() {
                        break;
                    }
                }
                accepted = listener.accept() => {
                    let (stream, peer) = match accepted {
                        Ok(pair) => pair,
                        Err(_) => break,
                    };
                    // Windows 允许其他 127/8 源地址连到 127.0.0.1 监听口；
                    // 拒绝非 127.0.0.1 来源，避免把弱主机行为当成远程授权。
                    if peer.ip() != IpAddr::V4(Ipv4Addr::LOCALHOST) {
                        drop(stream);
                        continue;
                    }
                    let service = service.clone();
                    let assets = assets.clone();
                    let events_path = events_path.clone();
                    tokio::spawn(async move {
                        // 单连接失败不应拖垮整个服务；把错误打到 stderr 便于排查，
                        // 但不静默吞掉——排查线索要保留。
                        if let Err(error) =
                            handle_connection(stream, service, assets, events_path).await
                        {
                            eprintln!("web_ui 连接处理失败: {error}");
                        }
                    });
                }
            }
        }
    }
}

impl WebUiServer for LocalWebUiServer {
    fn bind<'a>(&'a self, config: WebUiConfig) -> BoxFuture<'a, Result<u16, ServiceError>> {
        Box::pin(async move {
            config.validate()?;
            if self.handle.lock().unwrap().is_some() {
                return Err(ServiceError::new("web_ui_bind", "Web UI 已经启动"));
            }
            let addr: std::net::SocketAddr = format!("{}:{}", config.host, config.port)
                .parse()
                .map_err(|_| {
                    ServiceError::new("web_ui_bind", format!("无法解析监听地址 {}", config.host))
                })?;
            let listener = TcpListener::bind(addr).await.map_err(|error| {
                ServiceError::new("web_ui_bind", format!("绑定 127.0.0.1 失败: {error}"))
            })?;
            let port = listener
                .local_addr()
                .map_err(|error| {
                    ServiceError::new("web_ui_bind", format!("读取本机监听端口失败: {error}"))
                })?
                .port();
            let _ = self.host.set(config.host.clone());
            let shutdown_rx = self.shutdown.subscribe();
            let handle = tokio::spawn(Self::serve(
                listener,
                self.service.clone(),
                Arc::new(config.assets),
                Arc::new(config.events_path),
                shutdown_rx,
            ));
            *self.handle.lock().unwrap() = Some(handle);
            Ok(port)
        })
    }

    fn bound_host(&self) -> &str {
        self.host.get().map(String::as_str).unwrap_or("")
    }

    fn shutdown(&self) -> BoxFuture<'_, Result<(), ServiceError>> {
        Box::pin(async move {
            let handle = self.handle.lock().unwrap().take();
            let Some(handle) = handle else {
                return Ok(());
            };
            self.shutdown
                .send(true)
                .map_err(|_| ServiceError::new("web_ui_shutdown", "关闭信号没有接收方"))?;
            handle
                .await
                .map_err(|_| ServiceError::new("web_ui_shutdown", "服务任务已异常结束"))
        })
    }
}

/// 处理单条 HTTP 连接：解析请求、分流路由、写回 CRLF 分隔的响应。
async fn handle_connection(
    stream: tokio::net::TcpStream,
    service: Arc<dyn TaskService>,
    assets: Arc<StaticAssets>,
    events_path: Arc<PathBuf>,
) -> Result<(), ServiceError> {
    let mut reader = BufReader::new(stream);
    let mut request_line = String::new();
    if reader
        .read_line(&mut request_line)
        .await
        .map_err(io_error)?
        == 0
    {
        return Ok(());
    }
    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        let read = reader.read_line(&mut line).await.map_err(io_error)?;
        if read == 0 {
            return Err(ServiceError::new("web_ui_request", "HTTP 头部提前结束"));
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                content_length = value.trim().parse().map_err(|_| {
                    ServiceError::new("web_ui_request", "Content-Length 不是有效长度")
                })?;
            }
        }
    }
    if content_length > MAX_BODY_BYTES {
        let mut stream = reader.into_inner();
        return respond(
            &mut stream,
            413,
            "Payload Too Large",
            "text/plain; charset=utf-8",
            b"invalid_command",
        )
        .await;
    }
    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body).await.map_err(io_error)?;
    }
    let mut stream = reader.into_inner();

    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("");
    let (path, query) = match target.split_once('?') {
        Some((path, query)) => (path, Some(query)),
        None => (target, None),
    };

    match (method, path) {
        ("GET", "/") => {
            respond(
                &mut stream,
                200,
                "OK",
                "text/html; charset=utf-8",
                &assets.index_html,
            )
            .await
        }
        ("GET", "/app.js") => {
            respond(
                &mut stream,
                200,
                "OK",
                "application/javascript; charset=utf-8",
                &assets.app_js,
            )
            .await
        }
        ("GET", "/events") => serve_events(&events_path, query, &mut stream).await,
        ("GET", "/command") => serve_command_get(service, query, &mut stream).await,
        ("POST", "/command") => serve_command_post(service, body, &mut stream).await,
        _ => {
            respond(
                &mut stream,
                404,
                "Not Found",
                "text/plain; charset=utf-8",
                b"",
            )
            .await
        }
    }
}

/// 直接回放服务写入的 events.jsonl；与 CLI 的 events 命令共享同一事实来源。
///
/// 这里不调用 `ServiceCommand::Events`，避免把查询本身伪装成一次命令受理。
fn read_event_lines(path: &Path, after: u64, limit: usize) -> Result<String, ServiceError> {
    let raw = std::fs::read_to_string(path).map_err(|error| {
        ServiceError::new(
            "web_ui_events",
            format!("读取 {} 失败: {error}", path.display()),
        )
    })?;
    let mut lines = String::new();
    for line in raw.lines().filter(|line| !line.trim().is_empty()) {
        let event: serde_json::Value = serde_json::from_str(line).map_err(|error| {
            ServiceError::new("web_ui_events", format!("事件日志含无效 JSON 行: {error}"))
        })?;
        let sequence = event
            .get("sequence")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| ServiceError::new("web_ui_events", "事件缺少 sequence 字段"))?;
        if sequence <= after || lines.matches('\n').count() >= limit {
            continue;
        }
        lines.push_str(&serde_json::to_string(&event).map_err(|error| {
            ServiceError::new("web_ui_events", format!("事件无法重新编码: {error}"))
        })?);
        lines.push('\n');
    }
    Ok(lines)
}

/// 把事件日志按 NDJSON 返回；行结构与 services events.jsonl 完全一致。
async fn serve_events(
    events_path: &Path,
    query: Option<&str>,
    stream: &mut tokio::net::TcpStream,
) -> Result<(), ServiceError> {
    let (after, limit) = parse_after_limit(query)?;
    match read_event_lines(events_path, after, limit) {
        Ok(body) => {
            respond(
                stream,
                200,
                "OK",
                "application/x-ndjson; charset=utf-8",
                body.as_bytes(),
            )
            .await
        }
        Err(error) => service_error(stream, error).await,
    }
}

/// 只读命令通道：目前只暴露待审批列表，与 CLI 的 approvals 命令同源。
async fn serve_command_get(
    service: Arc<dyn TaskService>,
    query: Option<&str>,
    stream: &mut tokio::net::TcpStream,
) -> Result<(), ServiceError> {
    let op = query
        .and_then(|query| {
            query.split('&').find_map(|pair| {
                let (key, value) = pair.split_once('=')?;
                (key == "op").then_some(value)
            })
        })
        .unwrap_or("");
    let command = match op {
        "approvals" => ServiceCommand::Approvals { task_id: None },
        _ => {
            return respond(
                stream,
                400,
                "Bad Request",
                "text/plain; charset=utf-8",
                b"invalid_command",
            )
            .await;
        }
    };
    dispatch(service, "web-ui-command", command, stream).await
}

/// 写命令通道：把请求体原样转成 `ServiceCommand` 转发给任务服务。
///
/// 命令解析失败（未知 op 或字段缺失）按 400 返回，便于前端明确报错，
/// 而不是默默当成空操作。
async fn serve_command_post(
    service: Arc<dyn TaskService>,
    body: Vec<u8>,
    stream: &mut tokio::net::TcpStream,
) -> Result<(), ServiceError> {
    let value: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(_) => return invalid_command(stream).await,
    };
    let id = value
        .get("id")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("web-ui-command")
        .to_owned();
    let Some(command_value) = value.get("command") else {
        return invalid_command(stream).await;
    };
    let command: ServiceCommand = match serde_json::from_value(command_value.clone()) {
        Ok(command) => command,
        Err(_) => return invalid_command(stream).await,
    };
    dispatch(service, &id, command, stream).await
}

/// 将命令交给 TaskService；服务错误以 400 明确回给前端，保留错误码与原因。
async fn dispatch(
    service: Arc<dyn TaskService>,
    id: &str,
    command: ServiceCommand,
    stream: &mut tokio::net::TcpStream,
) -> Result<(), ServiceError> {
    match service.call(id, command).await {
        Ok(reply) => reply_body(reply, stream).await,
        Err(error) => service_error(stream, error).await,
    }
}

async fn reply_body(
    reply: ServiceReply,
    stream: &mut tokio::net::TcpStream,
) -> Result<(), ServiceError> {
    let body = serde_json::to_vec(&reply)
        .map_err(|error| ServiceError::new("web_ui_reply", format!("命令回执无法编码: {error}")))?;
    respond(stream, 200, "OK", "application/json; charset=utf-8", &body).await
}

async fn service_error(
    stream: &mut tokio::net::TcpStream,
    error: ServiceError,
) -> Result<(), ServiceError> {
    let body = format!("{}: {}", error.code, error.message);
    respond(
        stream,
        400,
        "Bad Request",
        "text/plain; charset=utf-8",
        body.as_bytes(),
    )
    .await
}

async fn invalid_command(stream: &mut tokio::net::TcpStream) -> Result<(), ServiceError> {
    respond(
        stream,
        400,
        "Bad Request",
        "text/plain; charset=utf-8",
        b"invalid_command",
    )
    .await
}

/// 解析 `after`/`limit` 查询参数；非法值明确报错，不悄悄回退到起始位置。
fn parse_after_limit(query: Option<&str>) -> Result<(u64, usize), ServiceError> {
    let mut after = 0u64;
    let mut limit = 100usize;
    if let Some(query) = query {
        for pair in query.split('&') {
            let Some((key, value)) = pair.split_once('=') else {
                continue;
            };
            match key {
                "after" => {
                    after = value
                        .parse()
                        .map_err(|_| ServiceError::new("web_ui_events", "after 不是有效序号"))?
                }
                "limit" => {
                    limit = value
                        .parse()
                        .map_err(|_| ServiceError::new("web_ui_events", "limit 不是有效条数"))?
                }
                _ => {}
            }
        }
    }
    Ok((after, limit))
}

/// 写回一个 CRLF 分隔的 HTTP/1.1 响应；头与体之间用空行隔开。
async fn respond(
    stream: &mut tokio::net::TcpStream,
    status: u16,
    reason: &str,
    content_type: &str,
    body: &[u8],
) -> Result<(), ServiceError> {
    let header = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let mut writer = BufWriter::new(stream);
    writer
        .write_all(header.as_bytes())
        .await
        .map_err(io_error)?;
    writer.write_all(body).await.map_err(io_error)?;
    writer.flush().await.map_err(io_error)
}

fn io_error(error: std::io::Error) -> ServiceError {
    ServiceError::new("web_ui_io", error.to_string())
}

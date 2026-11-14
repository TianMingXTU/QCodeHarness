//! MCP HTTP/SSE 传输的最小基础设施实现。
//!
//! 依赖已随 stdio 引入的 tokio/rmcp 网络能力，不再新增 crate。本实现负责
//! HTTP 握手、SSE 帧读取、断连/超时分类；它不在现有 stdio 路径上改代码。
//! 生命周期由上层 `ManagedServer` 统一持有：这里只提供可注入工厂。

use crate::application::mcp_http_sse::{
    HttpSseConfig, HttpSseFault, HttpSseHandle, HttpSseLifecycle, McpTransportFactory,
    TransportStage, classify_transport_fault,
};
use crate::application::port::BoxFuture;
use crate::domain::mcp::McpFaultKind;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};

/// 用 std::net 的阻塞 TCP 客户端做一次 HTTP GET 与 SSE 首帧读取。
///
/// 假 HTTP Server 与真实 MCP 网关都必须给出 `event: endpoint` 或至少一条
/// SSE 事件才视为握手成功；只返回 200 的空响应不够，因为连接还不可用。
pub struct HttpSseTransportFactory;

impl McpTransportFactory for HttpSseTransportFactory {
    fn handshake(
        &self,
        config: &HttpSseConfig,
    ) -> BoxFuture<'_, Result<HttpSseHandle, HttpSseFault>> {
        let config = config.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || blocking_handshake(&config))
                .await
                .map_err(|error| {
                    classify_transport_fault(
                        TransportStage::Handshake,
                        format!("SSE 握手任务异常: {error}"),
                    )
                })?
        })
    }

    fn close(&self, handle: &HttpSseHandle) -> BoxFuture<'_, Result<(), HttpSseFault>> {
        let server = handle.server.clone();
        let already_closed = handle.lifecycle == HttpSseLifecycle::Closed;
        Box::pin(async move {
            if already_closed {
                return Ok(());
            }
            Err(classify_transport_fault(
                TransportStage::Close,
                format!("HTTP/SSE Server `{server}` 已断连，只能标记 Closed"),
            ))
        })
    }
}

/// 解析 `http://host:port/path`，拒绝非 HTTP scheme 与缺失端口之外的地址。
fn parse_url(url: &str) -> Result<(String, String), String> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| format!("HTTP/SSE 只支持 http:// 地址: {url}"))?;
    let (authority, path) = match rest.find('/') {
        Some(index) => (&rest[..index], &rest[index..]),
        None => (rest, "/"),
    };
    if authority.is_empty() {
        return Err(format!("HTTP/SSE 地址缺少主机: {url}"));
    }
    Ok((authority.to_owned(), path.to_owned()))
}

fn blocking_handshake(config: &HttpSseConfig) -> Result<HttpSseHandle, HttpSseFault> {
    let (authority, path) = parse_url(&config.url)
        .map_err(|error| classify_transport_fault(TransportStage::Handshake, error))?;
    let addr = authority
        .to_socket_addrs()
        .map_err(|error| {
            classify_transport_fault(
                TransportStage::Handshake,
                format!("解析 HTTP/SSE 地址失败: {error}"),
            )
        })?
        .next()
        .ok_or_else(|| {
            classify_transport_fault(
                TransportStage::Handshake,
                format!("HTTP/SSE 地址未解析到端点: {authority}"),
            )
        })?;
    let timeout = config.handshake_timeout();
    let mut stream = TcpStream::connect_timeout(&addr, timeout).map_err(|error| {
        let reason = if error.kind() == std::io::ErrorKind::TimedOut {
            format!("连接 HTTP/SSE 超时: {error}")
        } else {
            format!("连接 HTTP/SSE 失败: {error}")
        };
        classify_transport_fault(TransportStage::Handshake, reason)
    })?;
    stream
        .set_read_timeout(Some(timeout))
        .and_then(|_| stream.set_write_timeout(Some(timeout)))
        .map_err(|error| {
            classify_transport_fault(
                TransportStage::Handshake,
                format!("设置 HTTP/SSE 超时失败: {error}"),
            )
        })?;
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: {authority}\r\nAccept: text/event-stream\r\nCache-Control: no-cache\r\nConnection: keep-alive\r\n\r\n"
    )
    // flush 前不关闭写端；SSE 是长连接，shutdown(Write) 会让部分 HTTP
    // Server 立刻断开，Windows 上客户端随后读到 10054。
    .and_then(|_| stream.flush()).map_err(|error| {
        classify_transport_fault(
            TransportStage::Handshake,
            format!("发送 SSE 握手请求失败: {error}"),
        )
    })?;

    let mut reader = BufReader::new(stream);
    let mut status = String::new();
    let status_read = reader.read_line(&mut status).map_err(|error| {
        classify_transport_fault(TransportStage::Handshake, handshake_read_reason(&error))
    })?;
    if status_read == 0 {
        return Err(classify_transport_fault(
            TransportStage::Handshake,
            "SSE 首帧前断连: 对端在状态行前关闭",
        ));
    }
    if !status.starts_with("HTTP/1.1 200") && !status.starts_with("HTTP/1.0 200") {
        return Err(classify_transport_fault(
            TransportStage::Handshake,
            format!("SSE 握手被拒绝: {}", status.trim()),
        ));
    }
    let mut line = String::new();
    let mut saw_event = false;
    loop {
        line.clear();
        let read = reader.read_line(&mut line).map_err(|error| {
            classify_transport_fault(TransportStage::Handshake, handshake_read_reason(&error))
        })?;
        if read == 0 {
            // 200 后没有 SSE 首帧就 EOF，与 stdio 的握手断连同分类；
            // 不能用「缺少事件」冒充一般握手错误。
            if !saw_event {
                return Err(classify_transport_fault(
                    TransportStage::Handshake,
                    "SSE 首帧前断连: 对端关闭事件流",
                ));
            }
            break;
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.starts_with("event:") || trimmed.starts_with("data:") {
            saw_event = true;
        }
        if line.trim().is_empty() && saw_event {
            break;
        }
    }
    Ok(HttpSseHandle {
        server: config.server.clone(),
        session_id: None,
        lifecycle: HttpSseLifecycle::Connected,
    })
}

fn handshake_read_reason(error: &std::io::Error) -> String {
    if error.kind() == std::io::ErrorKind::TimedOut
        || error.kind() == std::io::ErrorKind::WouldBlock
    {
        format!("读取 SSE 首帧超时: {error}")
    } else if error.kind() == std::io::ErrorKind::UnexpectedEof {
        format!("SSE 首帧前断连: {error}")
    } else {
        format!("读取 SSE 首帧失败: {error}")
    }
}

/// 把 HTTP/SSE 的连接故障映射到 stdio 的生命周期证据分类。
pub fn lifecycle_fault(fault: &HttpSseFault) -> McpFaultKind {
    fault.kind
}

/// 供上层做一次 SSE 帧读取；断连或超时同样按调用阶段分类。
pub fn read_sse_event(stream: &mut TcpStream) -> Result<String, HttpSseFault> {
    let mut reader = BufReader::new(stream);
    let mut body = String::new();
    reader.read_to_string(&mut body).map_err(|error| {
        classify_transport_fault(TransportStage::Call, format!("读取 SSE 事件失败: {error}"))
    })?;
    if body.trim().is_empty() {
        return Err(classify_transport_fault(
            TransportStage::Call,
            "SSE 事件流已断连",
        ));
    }
    Ok(body)
}

/// 明确限制：本工厂不实现 JSON-RPC tools/list 与 tools/call。
///
/// HTTP/SSE 只是传输层；完整 MCP 协议复用需要 rmcp 暴露的 SSE client
/// transport，由装配层显式选择，不能把本模块的握手当成已注册工具。
pub const LIMITATION: &str = "仅验证 HTTP/SSE 握手与故障分类，不实现完整 JSON-RPC 工具发现与调用";

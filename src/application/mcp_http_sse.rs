//! MCP HTTP/SSE 传输的纯契约。
//!
//! 这里只定义握手、关闭、故障分类的边界。真实 HTTP/SSE 协议解析放在
//! 基础设施层；stdio 的状态机已经规定超时与断连不得谎称调用结果已知。

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::application::port::BoxFuture;
use crate::domain::mcp::McpFaultKind;

/// HTTP/SSE 连接配置。URL 仅供本地或用户显式配置的 MCP Server 使用。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HttpSseConfig {
    pub server: String,
    pub url: String,
    pub handshake_timeout_ms: u64,
    pub call_timeout_ms: u64,
}

impl HttpSseConfig {
    pub fn handshake_timeout(&self) -> Duration {
        Duration::from_millis(self.handshake_timeout_ms)
    }

    pub fn call_timeout(&self) -> Duration {
        Duration::from_millis(self.call_timeout_ms)
    }
}

/// 连接生命周期与 stdio 对齐：握手、断连、主动关闭、工具调用超时。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HttpSseLifecycle {
    Connected,
    Disconnected,
    Closed,
}

/// 传输故障分类，复用 stdio 的 `McpFaultKind`，不另立一套显示口径。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HttpSseFault {
    pub kind: McpFaultKind,
    /// 请求已发出后超时或断连，远端可能已执行；不得自动重放调用。
    pub result_unknown: bool,
    pub detail: String,
}

impl HttpSseFault {
    pub fn new(kind: McpFaultKind, result_unknown: bool, detail: impl Into<String>) -> Self {
        Self {
            kind,
            result_unknown,
            detail: detail.into(),
        }
    }
}

/// 握手阶段观察到的连接句柄。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HttpSseHandle {
    pub server: String,
    pub session_id: Option<String>,
    pub lifecycle: HttpSseLifecycle,
}

/// 传输工厂扩展点：先定义 application trait，再由 infrastructure 实现。
///
/// HTTP/SSE 只作一次 MCP 握手的本地假服务验证；没有真实 rmcp 协议通道时
/// 工厂必须报错，不允许把「HTTP 返回了 200」伪装成 MCP Server 已就绪。
pub trait McpTransportFactory: Send + Sync {
    fn handshake(
        &self,
        config: &HttpSseConfig,
    ) -> BoxFuture<'_, Result<HttpSseHandle, HttpSseFault>>;
    fn close(&self, handle: &HttpSseHandle) -> BoxFuture<'_, Result<(), HttpSseFault>>;
}

/// 把 HTTP/SSE 的底层失败统一映射到 stdio 故障语义。
///
/// `timeout` 区分握手超时（请求可能未发出）与调用超时（请求可能已发出），
/// 后者 `result_unknown=true`；断连同理，连接主动关闭单独分类。
pub fn classify_transport_fault(stage: TransportStage, reason: impl Into<String>) -> HttpSseFault {
    let reason = reason.into();
    match stage {
        TransportStage::Handshake => {
            let kind = if is_timeout(&reason) {
                McpFaultKind::HandshakeTimeout
            } else if is_disconnect(&reason) {
                McpFaultKind::Disconnected
            } else {
                McpFaultKind::Handshake
            };
            HttpSseFault::new(kind, false, reason)
        }
        TransportStage::Call => {
            let kind = if is_timeout(&reason) {
                McpFaultKind::CallTimeout
            } else if is_disconnect(&reason) {
                McpFaultKind::Disconnected
            } else {
                McpFaultKind::Protocol
            };
            let unknown = matches!(kind, McpFaultKind::CallTimeout | McpFaultKind::Disconnected);
            HttpSseFault::new(kind, unknown, reason)
        }
        TransportStage::Close => HttpSseFault::new(McpFaultKind::Closed, false, reason),
    }
}

/// 故障发生阶段。决定同一条「超时」文本是否意味着结果未知。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TransportStage {
    Handshake,
    Call,
    Close,
}

fn is_timeout(reason: &str) -> bool {
    let lower = reason.to_lowercase();
    lower.contains("timeout") || lower.contains("timed out") || lower.contains("超时")
}

fn is_disconnect(reason: &str) -> bool {
    let lower = reason.to_lowercase();
    lower.contains("eof")
        || lower.contains("closed")
        || lower.contains("断连")
        || lower.contains("强迫关闭")
        || lower.contains("10054")
        || lower.contains("forcibly closed")
        || lower.contains("connection reset")
        || lower.contains("broken pipe")
}

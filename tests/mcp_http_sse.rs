//! MCP HTTP/SSE 传输：生命周期与故障分类的行为验收。
//!
//! 测试只用本机 std::net 假 Server，不访问网络，不修改现有 MCP stdio 路径。
//! 分类必须与 stdio 的 `McpFaultKind`、`result_unknown` 语义一致。

use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread;
use std::time::Duration;

use qcode_harness::application::mcp_http_sse::{
    HttpSseConfig, HttpSseLifecycle, McpTransportFactory, TransportStage, classify_transport_fault,
};
use qcode_harness::domain::mcp::McpFaultKind;
use qcode_harness::infrastructure::mcp_http_sse::{
    HttpSseTransportFactory, LIMITATION, lifecycle_fault,
};

/// 启动一次性的假 HTTP Server；返回端口与驱动线程。
fn http_server(reply: &'static str, hold: Duration) -> (u16, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("本机端口可绑定");
    let port = listener.local_addr().expect("有本地地址").port();
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("接到连接");
        // 必须读完请求头再回复：TCP 分段时过早关闭会让客户端在 flush
        // 尚未完成时收到 RST，Windows 上表现为一次偶发的 10054。
        let mut buffer = Vec::new();
        let mut chunk = [0u8; 256];
        while !buffer.windows(4).any(|window| window == b"\r\n\r\n") {
            let read = stream.read(&mut chunk).expect("能读到 HTTP 请求");
            if read == 0 {
                break;
            }
            buffer.extend_from_slice(&chunk[..read]);
        }
        if !hold.is_zero() {
            thread::sleep(hold);
        }
        let _ = stream.write_all(reply.as_bytes());
        let _ = stream.flush();
        thread::sleep(Duration::from_millis(20));
    });
    (port, handle)
}

fn config(port: u16, timeout_ms: u64) -> HttpSseConfig {
    HttpSseConfig {
        server: "local-test".to_owned(),
        url: format!("http://127.0.0.1:{port}/sse"),
        handshake_timeout_ms: timeout_ms,
        call_timeout_ms: timeout_ms,
    }
}

#[tokio::test]
async fn handshake_accepts_an_sse_endpoint_event_and_reports_connected() {
    let (port, server) = http_server(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\nevent: endpoint\ndata: /messages\n\n",
        Duration::ZERO,
    );
    let factory = HttpSseTransportFactory;
    let handle = factory
        .handshake(&config(port, 1500))
        .await
        .expect("首帧是 SSE 事件，握手应成功");
    assert_eq!(handle.server, "local-test");
    assert_eq!(handle.lifecycle, HttpSseLifecycle::Connected);
    server.join().expect("假 Server 正常结束");
}

#[tokio::test]
async fn a_broken_stream_is_disconnected_like_stdio() {
    let (port, server) = http_server("HTTP/1.1 200 OK\r\n\r\n", Duration::ZERO);
    let factory = HttpSseTransportFactory;
    let fault = factory
        .handshake(&config(port, 1500))
        .await
        .expect_err("没有 SSE 首帧就断开，不能算握手成功");
    assert_eq!(
        fault.kind,
        McpFaultKind::Disconnected,
        "故障详情: {}",
        fault.detail
    );
    assert!(
        !fault.result_unknown,
        "握手请求未发出工具调用，不应标记结果未知"
    );
    assert!(
        fault.detail.contains("SSE") || fault.detail.contains("事件"),
        "故障详情应说明 SSE 握手失败: {}",
        fault.detail
    );
    server.join().expect("假 Server 正常结束");
}

#[test]
fn handshake_and_call_timeouts_follow_the_stdio_fault_ledger() {
    let handshake = classify_transport_fault(TransportStage::Handshake, "连接 HTTP/SSE 超时");
    let call = classify_transport_fault(TransportStage::Call, "工具调用超时");
    assert_eq!(handshake.kind, McpFaultKind::HandshakeTimeout);
    assert!(!handshake.result_unknown);
    assert_eq!(call.kind, McpFaultKind::CallTimeout);
    assert!(call.result_unknown, "已发出的调用超时，远端可能仍在执行");
    assert_eq!(lifecycle_fault(&call), McpFaultKind::CallTimeout);
    let disconnect = classify_transport_fault(TransportStage::Call, "SSE 事件流已断连");
    assert_eq!(disconnect.kind, McpFaultKind::Disconnected);
    assert!(disconnect.result_unknown);
}

#[test]
fn the_transport_limit_is_documented() {
    assert!(LIMITATION.contains("不实现完整 JSON-RPC"));
    let raw = std::fs::read_to_string("src/infrastructure/mcp_http_sse.rs").expect("可读实现文件");
    assert!(raw.contains("LIMITATION"), "公开限制必须写在实现文件里");
}

//! 最小 MCP stdio Server：MCP 接入的连通性自测工具。
//!
//! 暴露两个工具，各对应一条审批路径：
//! - `fetch_page`：读语义（名字带 fetch），Harness 推断为只读，免审；
//! - `send_message`：写语义（名字带 send），推断为非只读，执行前走审批闸门。
//!
//! 两者都返回固定文本——这个 Server 只为验证"Harness 能不能接上、
//! 注解怎么分级、观察怎么回填"，不提供真实能力。
//!
//! 用法：`cargo build --examples` 后，把可执行文件路径填进 MCP 配置
//! 的 `command` 字段（见 mcp.example.toml），再用 QCH_MCP_CONFIG 指向它。

use rmcp::{
    ServerHandler, ServiceExt,
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ErrorData,
        ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool,
    },
    service::{RequestContext, RoleServer},
};
use serde_json::json;

struct DemoServer;

impl ServerHandler for DemoServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let page_schema = json!({
            "type": "object",
            "properties": {"url": {"type": "string", "description": "要读取的地址"}},
            "required": ["url"],
        })
        .as_object()
        .unwrap()
        .clone();
        let message_schema = json!({
            "type": "object",
            "properties": {"message": {"type": "string", "description": "要发送的消息"}},
            "required": ["message"],
        })
        .as_object()
        .unwrap()
        .clone();
        Ok(ListToolsResult::with_all_items(vec![
            Tool::new("fetch_page", "读取一个示例页面并返回其文本内容", page_schema),
            Tool::new("send_message", "发送一条消息到演示渠道", message_schema),
        ]))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        match request.name.as_ref() {
            "fetch_page" => Ok(CallToolResult::success(vec![ContentBlock::text(
                "示例页面内容：QCodeHarness 的 MCP 通道连通正常。",
            )])
            .into()),
            "send_message" => Ok(CallToolResult::success(vec![ContentBlock::text(
                "消息已送出（演示）",
            )])
            .into()),
            other => Err(ErrorData::invalid_params(format!("未知工具 {other}"), None)),
        }
    }
}

#[tokio::main]
async fn main() {
    let (stdin, stdout) = rmcp::transport::stdio();
    let service = DemoServer
        .serve((stdin, stdout))
        .await
        .expect("stdio Server 握手失败");
    // 跟随客户端生命周期：客户端断开（stdin EOF）后这里返回，进程退出。
    service.waiting().await.ok();
}

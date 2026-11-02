//! MCP（Model Context Protocol）客户端：把外部 Server 的工具接进注册表。
//!
//! 接入方式是给 [`crate::domain::tool::ToolSpec`] 加实现，不是加新端口：
//! 对 ReAct 循环来说 MCP 工具与内置工具没有区别——都是注册表里的一条
//! [`ToolEntry`]，风险照走注解 + 策略，审批照走 [`ApprovalGate`]。
//!
//! 两条硬性取向：
//! - **注解自己推断，不信 Server 自报。** 协议允许 Server 声明
//!   `readOnlyHint` 等注解，但那是对方的一面之词；据此放行等于把
//!   审批边界交给被接的一方。推断规则保守优先：拿不准一律按可写处理
//!   （默认要审批）。
//! - **单 Server 失败降级，不拖垮任务。** 配了三个 Server 挂了一个，
//!   剩下两个照常可用——与证据汇的降级哲学一致。

pub mod config;

use std::sync::Arc;
use std::time::Duration;

use rmcp::model::{CallToolRequestParams, Tool as RmcpTool};
use rmcp::service::{RoleClient, RunningService};
use rmcp::transport::TokioChildProcess;
use rmcp::ServiceExt;
use serde_json::Value;

use crate::application::port::{BoxFuture, ToolInvoker};
use crate::application::tool_registry::ToolEntry;
use crate::domain::tool::{ToolAnnotations, ToolCall, ToolOutcome, ToolSpec};

/// 连接超时：覆盖子进程启动 + MCP 握手 + 工具发现。
///
/// stdio Server 通常是 `npx`/`uvx` 拉起，冷启动可能要拉包，给宽一点；
/// 超了就当它挂了，走降级。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// 工具发现阶段的独立时限。实测取证（`examples/hang_probe.rs`，已删）：
/// 一个"应答初始化握手、但对 `tools/list` 请求静默"的半死 stdio Server，
/// 会让 rmcp 的 `list_all_tools()` 永久挂起——该调用没有内部超时，rmcp
/// 对请求只做一次发送后干等响应。CONNECT_TIMEOUT（30 秒）罩住整个
/// 连接过程，半死 Server 最终会被它收口，但错误信息分不清卡在哪一段：
/// "握手慢"和"握手成功后不回话"需要不同的排查方向（前者查网络/进程
/// 启动，后者查 Server 的事件循环）。发现阶段的合理预算远小于整体
/// 连接预算：握手成功后 `tools/list` 只是一次本地请求，正常 Server
/// 毫秒级响应，15 秒已经宽裕到不会误伤慢启动的 Server。
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(15);

/// 单次工具调用超时。MCP 工具没有内置的超时语义，不设上限会挂死整个步骤。
const CALL_TIMEOUT: Duration = Duration::from_secs(120);

/// MCP 工具进注册表的完整名：`mcp__<server>__<tool>`。
///
/// 前缀让来源在证据、审批请求与策略规则里都肉眼可辨；也从根本上杜绝
/// 与内置工具（或别的 Server 的同名工具）撞名后被静默覆盖。
pub fn mcp_tool_name(server: &str, tool: &str) -> String {
    format!("mcp__{server}__{tool}")
}

/// 判断词是不是"读"类动词（整词匹配，避免 `delete` 里的 `le` 之类误伤）。
fn is_read_verb(token: &str) -> bool {
    matches!(
        token,
        "read" | "get" | "list" | "search" | "query" | "status" | "diff" | "fetch" | "find"
            | "describe" | "view"
    )
}

/// 判断词是不是"写"类动词。
///
/// 除 delete/update 这类一眼可辨的，变更类动词（replace/rename/move/
/// edit/append/import）也在列：它们叠加读动词出现时（如
/// `search_and_replace`）若被判只读，就是一条免审后门。
fn is_write_verb(token: &str) -> bool {
    matches!(
        token,
        "write" | "create" | "delete" | "update" | "send" | "exec" | "run" | "post" | "put"
            | "patch" | "remove" | "insert" | "set" | "submit" | "deploy" | "kill"
            | "replace" | "rename" | "move" | "edit" | "append" | "import"
    )
}

/// 词表匹配（含复数还原）：token 本身或其去尾 `s` 的单数形式命中即算。
///
/// Server 起名常用第三人称变形（`deletes`、`removes`），整词匹配
/// 永远打不中它们；漏一个变形就是一条免审后门。
fn has_verb(tokens: &[String], verb: fn(&str) -> bool) -> bool {
    tokens
        .iter()
        .any(|token| verb(token) || token.strip_suffix('s').is_some_and(verb))
}

/// 从工具名与描述推断风险注解。
///
/// 规则保守优先：名字 + 描述里同时出现读与写动词，或只出现写动词，
/// 都判非只读（要审批）；只有"有读、无写"才判只读。判断不了的
/// （两个动词表都不沾）也没有读动词，同样判非只读。
///
/// Server 自报的注解（`annotations` 字段）完全忽略：见模块注释。
fn infer_annotations(name: &str, description: &str) -> ToolAnnotations {
    let tokens: Vec<String> = format!("{name} {description}")
        .to_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|token| !token.is_empty())
        .map(ToOwned::to_owned)
        .collect();
    let has_read = has_verb(&tokens, is_read_verb);
    let has_write = has_verb(&tokens, is_write_verb);
    let read_only = has_read && !has_write;
    // open_world 恒真：MCP 工具做什么 Harness 无法验证，按最宽的假使。
    // open_world 不影响裁决，只影响证据里的事实陈述。
    ToolAnnotations::new(read_only, false, read_only, true)
}

/// 一个 MCP Server 的连接句柄。抽出来是为了测试能注入替身，
/// 不必真起子进程走完整协议。
pub(crate) trait McpClientHandle: Send + Sync {
    /// 列出 Server 暴露的工具（原始名，不带 `mcp__` 前缀）。
    fn list_tools(&self) -> BoxFuture<'_, Result<Vec<RmcpTool>, String>>;

    /// 调用一个工具，返回拼好的文本结果。
    ///
    /// 传输错误、协议错误与 Server 报告的执行失败（`isError`）都归入
    /// `Err`：对调用方来说它们都是"这次调用没成，原因如下"。
    fn call_tool<'a>(
        &'a self,
        remote_name: &'a str,
        arguments: Value,
    ) -> BoxFuture<'a, Result<String, String>>;
}

/// [`McpClientHandle`] 的真实实现：包住一条 rmcp 客户端连接。
struct RealMcpClient {
    /// 连接的生命周期由它管：drop 即断开并结束子进程。
    service: Arc<RunningService<RoleClient, ()>>,
}

impl RealMcpClient {
    /// 启动 Server 子进程并完成握手。超时由调用方
    /// （[`McpToolSource::connect`]）统一罩住整个连接+发现过程。
    async fn connect(config: &config::McpServerConfig) -> Result<Self, String> {
        let mut command = tokio::process::Command::new(&config.command);
        command.args(&config.args);

        let transport = TokioChildProcess::new(command)
            .map_err(|error| format!("启动进程 `{}` 失败: {error}", config.command))?;
        let service = ()
            .serve(transport)
            .await
            .map_err(|error| format!("MCP 握手失败: {error}"))?;
        Ok(Self::from_service(service))
    }

    /// 从现成的连接造客户端，把连接生命周期装进 Arc。
    fn from_service(service: RunningService<RoleClient, ()>) -> Self {
        Self {
            service: Arc::new(service),
        }
    }
}

impl McpClientHandle for RealMcpClient {
    fn list_tools(&self) -> BoxFuture<'_, Result<Vec<RmcpTool>, String>> {
        Box::pin(async {
            // list_all_tools 自动翻页，拿全量。
            self.service
                .list_all_tools()
                .await
                .map_err(|error| format!("列出工具失败: {error}"))
        })
    }

    fn call_tool<'a>(
        &'a self,
        remote_name: &'a str,
        arguments: Value,
    ) -> BoxFuture<'a, Result<String, String>> {
        Box::pin(async move {
            let mut params = CallToolRequestParams::new(remote_name.to_owned());
            params.arguments = Some(
                arguments
                    .as_object()
                    .cloned()
                    .unwrap_or_default(),
            );
            let result = tokio::time::timeout(CALL_TIMEOUT, self.service.call_tool(params))
                .await
                .map_err(|_| format!("调用超时（超过 {} 秒）", CALL_TIMEOUT.as_secs()))?
                .map_err(|error| format!("{error}"))?;

            if result.is_error == Some(true) {
                // Server 报告执行失败：正文照样返回，模型需要看到原因。
                return Err(extract_text(&result));
            }
            Ok(extract_text(&result))
        })
    }
}

/// 把 `CallToolResult` 拼成给模型看的文字。
///
/// 优先取文本块；没有文本块时回退到 `structured_content`（MCP 规范
/// 只建议、不强制 Server 附带序列化文本，纯结构化结果是合法响应），
/// 再没有时如实报告收到了什么，而不是谎称"空内容"——那会让模型把
/// 有结果误判成没结果。
fn extract_text(result: &rmcp::model::CallToolResult) -> String {
    let text = result
        .content
        .iter()
        .filter_map(|block| block.as_text())
        .map(|text| text.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    if !text.is_empty() {
        return text;
    }
    if let Some(structured) = &result.structured_content {
        return serde_json::to_string_pretty(structured)
            .unwrap_or_else(|_| structured.to_string());
    }
    if result.content.is_empty() {
        "（MCP 工具返回了空内容）".to_owned()
    } else {
        format!(
            "（MCP 工具返回了 {} 个非文本内容块，无文本正文）",
            result.content.len()
        )
    }
}

/// 发现并接好的一个 MCP Server：工具清单 + 调用通道。
pub struct McpToolSource {
    /// Server 名（配置里的 `name`），进前缀与错误信息。
    server: String,
    client: Arc<dyn McpClientHandle>,
    /// 发现的工具，规格已按 `mcp__<server>__<tool>` 改名。
    tools: Vec<ToolSpec>,
}

impl std::fmt::Debug for McpToolSource {
    // 连接句柄是 trait 对象，没法派生；调试信息里给名字与清单就够用。
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpToolSource")
            .field("server", &self.server)
            .field("tools", &self.tools)
            .finish_non_exhaustive()
    }
}

impl McpToolSource {
    /// 连接一个真实 Server：起进程、握手、发现工具。
    ///
    /// 超时分两段罩：[`CONNECT_TIMEOUT`] 罩"启动进程+握手+发现"整体
    /// 兜底，[`DISCOVERY_TIMEOUT`] 罩发现阶段——rmcp 的 `tools/list`
    /// 请求没有内部超时，一个"握手正常但对 tools/list 永不回应"的
    /// 半死 Server 若只有整体超时，用户拿到的错误分不清卡在哪一段；
    /// 分段后错误信息直接指明排查方向。
    pub async fn connect(config: &config::McpServerConfig) -> Result<Self, String> {
        let attempt = tokio::time::timeout(CONNECT_TIMEOUT, async {
            let client = Arc::new(RealMcpClient::connect(config).await?);
            Self::discover_with_timeout(&config.name, client, DISCOVERY_TIMEOUT).await
        })
        .await;
        match attempt {
            Ok(result) => result,
            Err(_) => Err(format!(
                "连接超时（{} 秒内未完成握手与工具发现）",
                CONNECT_TIMEOUT.as_secs()
            )),
        }
    }

    /// 发现一段：握手成功的连接上拿工具清单，受 `timeout` 约束。
    ///
    /// 独立成方法是为了让 `connect`（传 [`DISCOVERY_TIMEOUT`]）与
    /// 半死 Server 测试（传短时限）共用同一条超时路径——测试替身
    /// 进不了 `connect` 的真进程分支，错误信息与降级行为必须在
    /// 替身可达的入口上验证。
    async fn discover_with_timeout(
        server: &str,
        client: Arc<dyn McpClientHandle>,
        timeout: Duration,
    ) -> Result<Self, String> {
        tokio::time::timeout(timeout, Self::from_client(server, client))
            .await
            .map_err(|_| {
                format!(
                    "工具发现超时（{} 秒）：Server 完成了握手但对工具清单无响应，检查 Server 的 list_tools 实现",
                    timeout.as_secs()
                )
            })?
    }

    /// 从给定的连接句柄发现工具。测试从这条路径注入替身。
    pub(crate) async fn from_client(
        server: &str,
        client: Arc<dyn McpClientHandle>,
    ) -> Result<Self, String> {
        let remote_tools = client.list_tools().await?;
        let tools = remote_tools
            .into_iter()
            .map(|tool| {
                let description = tool
                    .description
                    .as_deref()
                    .unwrap_or("")
                    .to_owned();
                ToolSpec {
                    name: mcp_tool_name(server, &tool.name),
                    // 来源写进描述：模型看到工具列表时知道这是外部能力。
                    description: format!("（MCP Server `{server}` 提供）{description}"),
                    parameters: Value::Object(
                        (*tool.input_schema).clone(),
                    ),
                    annotations: infer_annotations(&tool.name, &description),
                }
            })
            .collect();
        Ok(Self {
            server: server.to_owned(),
            client,
            tools,
        })
    }

    /// 发现的工具数。
    pub fn tool_count(&self) -> usize {
        self.tools.len()
    }

    /// Server 名。
    pub fn server(&self) -> &str {
        &self.server
    }

    /// 装配成注册表登记项。规格与执行入口在这里绑死。
    pub fn entries(&self) -> Vec<ToolEntry> {
        self.tools
            .iter()
            .map(|spec| {
                let remote_name = spec
                    .name
                    .strip_prefix(&format!("mcp__{}__", self.server))
                    .expect("tools 由 from_client 生成，必带本 Server 前缀")
                    .to_owned();
                ToolEntry {
                    spec: spec.clone(),
                    invoker: Arc::new(McpToolInvoker {
                        server: self.server.clone(),
                        remote_name,
                        client: self.client.clone(),
                    }),
                }
            })
            .collect()
    }
}

/// MCP 工具的执行入口：把调用转给对应的 Server。
struct McpToolInvoker {
    server: String,
    remote_name: String,
    client: Arc<dyn McpClientHandle>,
}

impl ToolInvoker for McpToolInvoker {
    fn invoke<'a>(&'a self, call: &'a ToolCall) -> BoxFuture<'a, ToolOutcome> {
        Box::pin(async move {
            match self
                .client
                .call_tool(&self.remote_name, call.arguments.clone())
                .await
            {
                Ok(text) => ToolOutcome::success(text),
                Err(error) => ToolOutcome::failed(format!(
                    "MCP 工具 `{}.{}` 调用失败: {error}",
                    self.server, self.remote_name
                )),
            }
        })
    }
}

/// 依次连接配置里的全部 Server。
///
/// 单点失败降级为警告（返回值里的第二个元素），不阻止其余 Server——
/// 一个挂掉的 MCP Server 不该让整个任务泡汤。顺序连接而非并发：
/// Server 数量是个位数，并发省的几秒不值得为此引入聚合复杂度。
pub async fn connect_all(config: &config::McpConfig) -> (Vec<McpToolSource>, Vec<String>) {
    let mut sources = Vec::new();
    let mut warnings = Vec::new();
    for server in &config.mcp_servers {
        match McpToolSource::connect(server).await {
            Ok(source) => sources.push(source),
            Err(error) => warnings.push(format!(
                "MCP Server `{}` 连接失败，其工具本次不可用: {error}",
                server.name
            )),
        }
    }
    (sources, warnings)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infrastructure::tool::all_tools;
    use crate::domain::workspace::WorkspaceRoot;
    use serde_json::json;
    use std::sync::Mutex;

    /// 固定清单与回复的假连接。
    struct FakeClient {
        tools: Vec<RmcpTool>,
        reply: Result<String, String>,
        /// 记录收到的调用（原始名），断言转发用了不带前缀的名字。
        calls: Mutex<Vec<String>>,
    }

    /// 半死 Server 的假连接：握手能成（对 `connect` 而言它已经是
    /// `RealMcpClient` 成功返回后的产物），但对 `tools/list` 永不回应。
    /// 实测模型：rmcp 的 `list_all_tools()` 对请求只发送一次后干等，
    /// Server 不回就永久挂起。
    struct HalfDeadClient;

    impl McpClientHandle for HalfDeadClient {
        fn list_tools(&self) -> BoxFuture<'_, Result<Vec<RmcpTool>, String>> {
            Box::pin(std::future::pending())
        }

        fn call_tool<'a>(
            &'a self,
            _remote_name: &'a str,
            _arguments: Value,
        ) -> BoxFuture<'a, Result<String, String>> {
            Box::pin(std::future::pending())
        }
    }

    impl FakeClient {
        fn with_tools(tools: Vec<RmcpTool>) -> Arc<Self> {
            Arc::new(Self {
                tools,
                reply: Ok("工具结果".to_owned()),
                calls: Mutex::new(Vec::new()),
            })
        }

        fn failing(tools: Vec<RmcpTool>) -> Arc<Self> {
            Arc::new(Self {
                tools,
                reply: Err("连接已断".to_owned()),
                calls: Mutex::new(Vec::new()),
            })
        }
    }

    impl McpClientHandle for FakeClient {
        fn list_tools(&self) -> BoxFuture<'_, Result<Vec<RmcpTool>, String>> {
            Box::pin(async { Ok(self.tools.clone()) })
        }

        fn call_tool<'a>(
            &'a self,
            remote_name: &'a str,
            _arguments: Value,
        ) -> BoxFuture<'a, Result<String, String>> {
            Box::pin(async {
                self.calls.lock().unwrap().push(remote_name.to_owned());
                self.reply.clone()
            })
        }
    }

    /// 造一个最小可用的 rmcp 工具描述。
    fn rmcp_tool(name: &str, description: &str) -> RmcpTool {
        RmcpTool::new(
            name.to_owned(),
            description.to_owned(),
            json!({"type": "object"}).as_object().unwrap().clone(),
        )
    }

    #[test]
    fn tool_names_carry_the_server_prefix() {
        assert_eq!(mcp_tool_name("fetch", "get_page"), "mcp__fetch__get_page");
    }

    #[test]
    fn infer_annotations_reads_are_read_only() {
        let read_only = infer_annotations("get_page", "Fetches a web page and returns its text");
        assert!(read_only.read_only, "纯读动词应判只读");
        assert!(read_only.requires_approval() == false);
        assert!(read_only.open_world);
    }

    #[test]
    fn infer_annotations_defaults_to_approval_when_unsure() {
        // 动词表都不沾、只沾写、读写混合、写动词变形——所有"拿不准"
        // 都必须判非只读。前两个变形用例（replace/import 叠加读动词、
        // deletes 复数）是免审后门的经典来源，回归价值最高。
        for (name, description) in [
            ("commit_changes", "提交一次变更"),
            ("send_report", "Sends the report by email"),
            ("search_and_delete", "Search for files and delete them"),
            ("search_and_replace", "Search and replace text in files"),
            ("fetch_and_import", "Fetches a URL and imports it"),
            ("get_file", "Gets files and deletes matches"),
        ] {
            let annotations = infer_annotations(name, description);
            assert!(
                !annotations.read_only,
                "{name} 拿不准必须判非只读，实际判了只读"
            );
            assert!(annotations.requires_approval(), "{name} 必须走审批");
        }
    }

    #[tokio::test]
    async fn discovered_tools_are_renamed_prefixed_and_annotated() {
        let client = FakeClient::with_tools(vec![
            rmcp_tool("get_page", "Fetches a web page"),
            rmcp_tool("submit_form", "Submits a form"),
        ]);

        let source = McpToolSource::from_client("fetch", client).await.unwrap();
        let entries = source.entries();

        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].spec.name, "mcp__fetch__get_page");
        assert!(entries[0].spec.annotations.read_only);
        assert_eq!(entries[1].spec.name, "mcp__fetch__submit_form");
        assert!(!entries[1].spec.annotations.read_only);
        // 来源要写进描述，模型才知道这是外部能力。
        assert!(entries[0].spec.description.contains("MCP Server `fetch` 提供"));
    }

    #[tokio::test]
    async fn half_dead_server_times_out_during_discovery_with_a_readable_error() {
        // 与 connect() 发现段同路径，用短时限：半死替身永不回应，
        // 错误必须在时限内回来（挂死即本测试失败），信息指明卡在
        // 发现阶段而不是握手或调用。
        let started = std::time::Instant::now();
        let error = McpToolSource::discover_with_timeout(
            "half-dead",
            Arc::new(HalfDeadClient),
            std::time::Duration::from_millis(100),
        )
        .await
        .expect_err("半死 Server 不应返回工具清单");
        let elapsed = started.elapsed();

        assert!(
            elapsed >= std::time::Duration::from_millis(100),
            "应等满时限（防过早放行），实际 {elapsed:?}"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "应快速失败而不是永久挂起，实际 {elapsed:?}"
        );
        assert!(error.contains("工具发现超时"), "{error}");
        assert!(error.contains("list_tools"), "应指明排查方向: {error}");
    }

    #[tokio::test]
    async fn invoker_forwards_the_remote_name_without_prefix() {
        let client = FakeClient::with_tools(vec![rmcp_tool("get_page", "Fetches a page")]);
        let source = McpToolSource::from_client("fetch", client.clone()).await.unwrap();
        let entry = source.entries().remove(0);

        let call = ToolCall {
            id: crate::domain::ids::CallId::new("call_1"),
            name: "mcp__fetch__get_page".to_owned(),
            arguments: json!({"url": "https://example.com"}),
        };
        let outcome = entry.invoker.invoke(&call).await;

        assert_eq!(outcome.status, crate::domain::tool::OutcomeStatus::Success);
        assert_eq!(outcome.content, "工具结果");
        assert_eq!(client.calls.lock().unwrap().as_slice(), ["get_page"]);
    }

    #[tokio::test]
    async fn invocation_errors_become_failed_outcomes_not_panics() {
        let client = FakeClient::failing(vec![rmcp_tool("get_page", "Fetches a page")]);
        let source = McpToolSource::from_client("fetch", client).await.unwrap();
        let entry = source.entries().remove(0);

        let call = ToolCall {
            id: crate::domain::ids::CallId::new("call_1"),
            name: "mcp__fetch__get_page".to_owned(),
            arguments: json!({}),
        };
        let outcome = entry.invoker.invoke(&call).await;

        assert_eq!(outcome.status, crate::domain::tool::OutcomeStatus::Failed);
        assert!(outcome.content.contains("fetch.get_page"));
        assert!(outcome.content.contains("连接已断"));
        assert!(outcome.is_error());
    }

    #[tokio::test]
    async fn mcp_tools_do_not_shadow_builtin_namesakes() {
        // 验收项：MCP 工具带 `mcp__` 前缀，不覆盖内置同名工具。
        // 极端用例：某 Server 暴露了一个跟 `read_file` 同名的工具。
        let client = FakeClient::with_tools(vec![rmcp_tool("read_file", "Reads a remote file")]);
        let source = McpToolSource::from_client("ext", client).await.unwrap();

        let mut entries = all_tools(WorkspaceRoot::new(std::env::temp_dir()));
        entries.extend(source.entries());
        let registry = crate::application::tool_registry::ToolRegistry::new(entries);

        let specs = registry.specs();
        let names: Vec<&str> = specs.iter().map(|spec| spec.name.as_str()).collect();
        assert_eq!(
            names.iter().filter(|n| **n == "read_file").count(),
            1,
            "内置 read_file 只有一个"
        );
        assert!(names.contains(&"mcp__ext__read_file"), "MCP 版本以前缀名并存");
        assert_eq!(names.len(), 9, "8 个内置 + 1 个 MCP，谁也不覆盖谁");
    }

    #[tokio::test]
    async fn non_read_only_mcp_tools_require_approval_by_default_policy() {
        // 验收项：拿不准的 MCP 工具默认要审批——走真实默认策略验证。
        let annotations = infer_annotations("send_report", "Sends the report by email");
        let spec = ToolSpec {
            name: "mcp__mailer__send_report".to_owned(),
            description: "Sends the report by email".to_owned(),
            parameters: json!({"type": "object"}),
            annotations,
        };
        let call = ToolCall {
            id: crate::domain::ids::CallId::new("call_1"),
            name: spec.name.clone(),
            arguments: json!({}),
        };
        let engine = crate::infrastructure::policy::load(None).unwrap();

        assert_eq!(
            engine.evaluate(&call, &spec).verdict,
            crate::domain::policy::PolicyVerdict::RequireApproval
        );
    }

    #[test]
    fn structured_only_results_are_serialized_not_reported_empty() {
        // 规范允许 Server 只回 structuredContent：此时正文应是结构化
        // 数据本身，而不是"返回了空内容"。
        let mut result = rmcp::model::CallToolResult::success(vec![]);
        result.structured_content = Some(json!({"rows": 3, "query": "select 1"}));
        let text = extract_text(&result);
        assert!(text.contains("\"rows\"") && text.contains("3"), "{text}");
    }

    #[test]
    fn non_text_blocks_are_reported_honestly() {
        // 纯图片结果（截图类工具很常见）不能谎报为空。
        let image = serde_json::from_value::<rmcp::model::ContentBlock>(json!({
            "type": "image",
            "data": "aWNvbg==",
            "mimeType": "image/png"
        }))
        .expect("合法的 image 内容块应能反序列化");
        let result = rmcp::model::CallToolResult::success(vec![image]);
        let text = extract_text(&result);
        assert!(text.contains("非文本内容块"), "{text}");
    }

    #[tokio::test]
    async fn connect_failure_of_a_missing_binary_is_a_clean_error() {
        // 真实子进程路径的失败面：二进制不存在时 spawn 立刻失败，
        // 不该挂死也不该 panic——降级路径靠的就是这个错误。
        let config = config::McpServerConfig {
            name: "ghost".to_owned(),
            command: "qch-no-such-mcp-binary-4f3a".to_owned(),
            args: vec![],
        };
        let error = McpToolSource::connect(&config).await.unwrap_err();
        assert!(error.contains("启动进程"), "{error}");
    }

    #[tokio::test]
    async fn connect_to_the_bundled_demo_server_end_to_end() {
        // 真协议连通性：起仓库自带的演示 Server（cargo test 会先编译
        // examples，产物在固定位置），走完整的 stdio 子进程链路——
        // 握手、发现、调用、文本提取。这是被 #[ignore] 的 duplex 测试
        // 想覆盖而没覆盖成的成功路径。
        let exe = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join(format!(
                "target/debug/examples/mcp-echo-server{}",
                std::env::consts::EXE_SUFFIX
            ));
        let config = config::McpServerConfig {
            name: "demo".to_owned(),
            command: exe.display().to_string(),
            args: vec![],
        };
        let source = McpToolSource::connect(&config)
            .await
            .expect("演示 Server 应能连通");
        assert_eq!(source.tool_count(), 2);

        let entries = source.entries();
        assert_eq!(entries[0].spec.name, "mcp__demo__fetch_page");
        assert!(entries[0].spec.annotations.read_only);
        assert!(!entries[1].spec.annotations.read_only);

        let call = ToolCall {
            id: crate::domain::ids::CallId::new("call_1"),
            name: "mcp__demo__fetch_page".to_owned(),
            arguments: json!({"url": "https://example.com"}),
        };
        let outcome = entries[0].invoker.invoke(&call).await;
        assert_eq!(outcome.status, crate::domain::tool::OutcomeStatus::Success);
        assert!(outcome.content.contains("MCP 通道连通正常"), "{outcome:?}");
    }

    #[tokio::test]
    async fn connect_all_degrades_a_failed_server_to_a_warning() {
        // 验收项：单 Server 连接失败不阻止其余工具——这里两个 Server
        // 一个必挂（二进制不存在），结果应是 0 个源 + 1 条带名字的警告，
        // 而不是整个连接过程报错。
        let config = config::McpConfig {
            mcp_servers: vec![
                config::McpServerConfig {
                    name: "ghost".to_owned(),
                    command: "qch-no-such-mcp-binary-4f3a".to_owned(),
                    args: vec![],
                },
                config::McpServerConfig {
                    name: "also-ghost".to_owned(),
                    command: "qch-no-such-mcp-binary-90b2".to_owned(),
                    args: vec![],
                },
            ],
        };
        let (sources, warnings) = connect_all(&config).await;

        assert!(sources.is_empty());
        assert_eq!(warnings.len(), 2);
        assert!(warnings[0].contains("ghost") && warnings[0].contains("不可用"));
    }
}

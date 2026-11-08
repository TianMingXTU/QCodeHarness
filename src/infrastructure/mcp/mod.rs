//! MCP（Model Context Protocol）客户端：把外部 Server 的工具接进注册表。
//!
//! 接入方式是提供 [`ToolSpec`] 并实现 [`ToolInvoker`]，不新增端口。对
//! ReAct 循环而言 MCP 工具与内置工具相同：都是注册表里的一条
//! [`ToolEntry`]，风险走注解与策略，审批走
//! [`crate::application::port::ApprovalGate`]。
//!
//! 两条硬性约束：
//! - 注解自行推断，不信任 Server 自报。协议允许 Server 声明
//!   `readOnlyHint` 等注解，但那是对方的一面之词，据此放行会把审批
//!   边界交给被接入方。推断规则保守优先，拿不准一律按可写处理。
//! - 单个 Server 失败降级，不拖垮任务。三个 Server 挂了一个，另外两个
//!   照常可用，与证据汇的降级策略一致。

pub mod config;

use std::sync::Arc;
use std::time::Duration;

use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, Tool as RmcpTool};
use rmcp::service::{RoleClient, RunningService};
use rmcp::transport::TokioChildProcess;
use serde_json::Value;

use crate::application::mcp_lifecycle::McpLifecycleRecorder;
use crate::application::port::{BoxFuture, ToolInvoker};
use crate::application::tool_registry::ToolEntry;
use crate::domain::sandbox::{PlatformCapabilities, SandboxPolicy};
use crate::domain::tool::{MCP_TOOL_PREFIX, ToolAnnotations, ToolCall, ToolOutcome, ToolSpec};
use crate::infrastructure::sandbox::JobGuard;

mod lifecycle;
pub(crate) use lifecycle::{Connector, ManagedServer};

/// MCP 握手超时。stdio Server 通常由 `npx`/`uvx` 拉起，冷启动可能要拉
/// 包，因此放宽。只包住 `serve` 这一段（见 `RealMcpClient::connect`），
/// 进程启动在此之前；握手超时视为不可用，走降级。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// 工具发现阶段的独立时限。实测（`examples/hang_probe.rs`，已删）：
/// 一个应答握手但对 `tools/list` 静默的 stdio Server 会让 rmcp 的
/// `list_all_tools()` 永久挂起。该调用没有内部超时，rmcp 发送请求后
/// 一直等待响应。发现阶段不归 CONNECT_TIMEOUT 管，需要自己的预算：
/// "握手慢"与"握手成功后不回话"排查方向不同，前者查网络或进程启动，
/// 后者查 Server 的事件循环。发现阶段的预算应远小于握手预算：握手
/// 成功后 `tools/list` 只是一次本地请求，正常 Server 毫秒级响应，15 秒
/// 足以覆盖慢启动的 Server。
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(15);

/// 单次工具调用超时。MCP 工具没有内置的超时语义，不设上限会让整个步骤
/// 永久挂起。
const CALL_TIMEOUT: Duration = Duration::from_secs(120);

/// MCP 工具进注册表的完整名：`mcp__<server>__<tool>`。
///
/// 前缀让来源在证据、审批请求与策略规则里可辨，也避免与内置工具或
/// 其他 Server 的同名工具撞名后被覆盖。
pub fn mcp_tool_name(server: &str, tool: &str) -> String {
    format!("{MCP_TOOL_PREFIX}{server}__{tool}")
}

/// 判断词是否为读类动词。按整词匹配，只比较完整 token。
fn is_read_verb(token: &str) -> bool {
    matches!(
        token,
        "read"
            | "get"
            | "list"
            | "search"
            | "query"
            | "status"
            | "diff"
            | "fetch"
            | "find"
            | "describe"
            | "view"
    )
}

/// 判断词是否为写类动词。
///
/// 除 delete/update 这类明显的动词，replace/rename/move/edit/append/
/// import 等变更类动词也在列。它们与读动词同时出现时（如
/// `search_and_replace`）若判为只读，会形成免审后门。
fn is_write_verb(token: &str) -> bool {
    matches!(
        token,
        "write"
            | "create"
            | "delete"
            | "update"
            | "send"
            | "exec"
            | "run"
            | "post"
            | "put"
            | "patch"
            | "remove"
            | "insert"
            | "set"
            | "submit"
            | "deploy"
            | "kill"
            | "replace"
            | "rename"
            | "move"
            | "edit"
            | "append"
            | "import"
    )
}

/// 词表匹配，含复数还原：token 本身或去掉末尾 `s` 后命中即算。
///
/// Server 常用第三人称变形（`deletes`、`removes`）命名，整词匹配无法
/// 命中。漏一个变形就是一条免审后门。
fn has_verb(tokens: &[String], verb: fn(&str) -> bool) -> bool {
    tokens
        .iter()
        .any(|token| verb(token) || token.strip_suffix('s').is_some_and(verb))
}

/// 从工具名与描述推断风险注解。
///
/// 规则保守优先：出现写动词，或读写动词同时出现，判非只读（需要审批）。
/// 只有出现读动词且没有写动词时才判只读。两个词表都不沾也判非只读。
///
/// 完全忽略 Server 自报的 `annotations` 字段，理由见模块注释。
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
    // open_world 恒真：Harness 无法验证 MCP 工具的行为，按最宽的情形假设。
    // 它不进证据，但体现在审批提示的风险摘要、AI 复核的风险标记与发给
    // 模型的工具注解里。
    ToolAnnotations::new(read_only, false, read_only, true)
}

/// MCP 故障的领域分类，只描述执行层证实的事实。
///
/// rmcp 拿不到 Server 退出码，连接结束一律记为断开，不推测进程崩溃。
/// `detail` 是给人读的说明，不参与分类，类别只看 `kind`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpError {
    pub kind: crate::domain::mcp::McpFaultKind,
    /// 请求是否已经发出。发出后的超时或断连表示远端结果未知。
    pub result_unknown: bool,
    /// 故障发生时进程是否真的在进程沙箱内。
    pub process_isolation: bool,
    /// 给人看的原因说明；不含参数、环境变量或凭据。
    pub detail: String,
}

impl McpError {
    pub fn new(kind: crate::domain::mcp::McpFaultKind, result_unknown: bool) -> Self {
        Self {
            kind,
            result_unknown,
            process_isolation: false,
            detail: String::new(),
        }
    }

    /// 附上给人读的说明。分类不变：文本只解释，不判定。
    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = detail.into();
        self
    }

    /// 附上故障发生时进程的真实保护状态。默认 `false` 表示未进沙箱；
    /// 握手阶段进程已在 Job 内，漏报为 `false` 是错误陈述。
    pub fn with_isolation(mut self, process_isolation: bool) -> Self {
        self.process_isolation = process_isolation;
        self
    }
}

impl std::fmt::Display for McpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.detail.is_empty() {
            write!(f, "{}", self.kind)
        } else {
            write!(f, "{}（{}）", self.detail, self.kind)
        }
    }
}

/// 各阶段的超时预算。生产环境用常量，测试注入短时限而不改常量。
#[derive(Debug, Clone, Copy)]
pub struct McpTimeouts {
    pub connect: Duration,
    pub discovery: Duration,
    pub call: Duration,
}

impl McpTimeouts {
    fn production() -> Self {
        Self {
            connect: CONNECT_TIMEOUT,
            discovery: DISCOVERY_TIMEOUT,
            call: CALL_TIMEOUT,
        }
    }
}

/// 一个 MCP Server 的连接句柄。抽象出来以便测试注入替身，不必启动
/// 真实子进程。
///
/// 唯一所有者：实现持有 transport/service/Job 的完整生存期。
/// [`McpClientHandle::close`] 是唯一关闭入口，先等 rmcp 收尾再释放
/// Job；顺序反了 KILL_ON_JOB_CLOSE 会先杀掉正在优雅关闭的进程。
pub(crate) trait McpClientHandle: Send + Sync {
    /// 这个 Server 的进程是否在宿主进程沙箱内。真实连接在握手前将
    /// launcher 纳入 Job Object 时为 `true`；替身、非 Windows、纳入失败
    /// 为 `false`。边界证据按此记录，不虚报保护，也不一概写死为无保护。
    fn process_isolation(&self) -> bool;

    /// 列出 Server 暴露的工具（原始名，不带 `mcp__` 前缀）。
    fn list_tools(&self) -> BoxFuture<'_, Result<Vec<RmcpTool>, McpError>>;

    /// 调用一个工具，返回拼好的文本结果。
    ///
    /// Server 报告的执行失败（`isError`）属于业务错误，连接仍可用，用
    /// [`McpFaultKind::ServerError`] 与传输故障区分。
    fn call_tool<'a>(
        &'a self,
        remote_name: &'a str,
        arguments: Value,
    ) -> BoxFuture<'a, Result<String, McpError>>;

    /// 进程是否已被观察到结束。只用于粗判（如缓存失效），权威判定依靠
    /// [`McpClientHandle::closed`] 的通知或调用返回的故障分类。
    fn is_closed(&self) -> bool;

    /// 连接结束的通知。`None` 表示该实现没有可观察的关闭通道。future
    /// 只持有通知句柄，不引用 service/transport/Job。
    fn closed(&self) -> Option<BoxFuture<'static, ()>>;

    /// 结束连接并等待 transport close 完成后返回。重复调用是空操作。
    fn close(&mut self) -> BoxFuture<'_, ()>;
}

/// [`McpClientHandle`] 的真实实现，包住一条 rmcp 客户端连接。
///
/// 唯一所有者：service/transport/Job 均由它持有并按序释放。不用 `Arc`，
/// 由最后一个引用决定释放时机会导致旧连接与 Job 句柄滞留：
/// `RunningService` 的 drop 只发取消通知、不等收尾，随后 JobGuard 的
/// KILL_ON_JOB_CLOSE 会先于 transport close 杀进程树。
struct RealMcpClient {
    /// 连接生命周期。先 `close_with_timeout` 等 rmcp 完成 transport close
    /// （关闭 stdin，最多等子进程 3 秒），之后才释放 Job。
    service: RunningService<RoleClient, ()>,
    /// 进程沙箱句柄。声明在 service 之后，字段按声明序析构，Job 必须等
    /// transport 收尾后才释放；否则 KILL_ON_JOB_CLOSE 会在优雅关闭前
    /// 杀进程树。close 超时未收尾时，这里是最后的兜底。
    ///
    /// 没有进程沙箱的平台上该字段为 `None`：只 kill 直接子进程，后代
    /// 进程不在清理边界内。
    _job: Option<JobGuard>,
    /// 进程是否真正进入沙箱，供 [`McpClientHandle::process_isolation`] 上报。
    process_isolation: bool,
}

impl RealMcpClient {
    /// 建进程沙箱、启动 Server 进程、纳入沙箱并完成握手。
    ///
    /// 握手超时只包住 `serve`，不包整个连接过程。取消 serve 的 future
    /// 会 drop transport，rmcp 的 ChildWithCleanup 在 drop 时 kill 子进程。
    /// 有进程沙箱的平台（Windows）上 `_job` 也随返回路径释放并整树兜底，
    /// "启动后握手未成"的清理无需额外代码；其他平台没有进程沙箱，只 kill
    /// 直接子进程。超时预算由调用方给出。
    async fn connect(config: &config::McpServerConfig, handshake: Duration) -> Result<Self, McpError> {
        let mut command = tokio::process::Command::new(&config.command);
        command.args(&config.args);

        // 先建 Job 再启动进程，与 shell 工具一致（见 `run_with_policy`）。
        // Windows 上建不起来就拒绝连接：拉起一个无人看管的外部进程再
        // 声明"无保护"，是用虚假证据换可用性。只有平台未实现进程沙箱
        // 时才降级照连。失败由 `connect_all` 记为警告并跳过该 Server，
        // 其余 Server 不受影响。
        let job = match JobGuard::create(&SandboxPolicy::default()) {
            Ok(job) => Some(job),
            Err(error) if !PlatformCapabilities::detect().has_process_sandbox => {
                eprintln!("警告：{error}");
                None
            }
            Err(error) => {
                return Err(McpError::new(
                    crate::domain::mcp::McpFaultKind::SandboxCreation,
                    false,
                )
                .with_detail(format!(
                    "MCP 沙箱创建失败: {error}（fail-closed：不启动没有进程边界的 Server）"
                )));
            }
        };

        let transport = TokioChildProcess::new(command).map_err(|error| {
            McpError::new(crate::domain::mcp::McpFaultKind::Spawn, false)
                .with_detail(format!("启动进程 `{}` 失败: {error}", config.command))
        })?;
        // serve 会消费 transport，之后拿不到 Child，纳入所需的 PID 必须
        // 在此获取。取不到（进程已退出）则跳过纳入，回退 rmcp 原语义。
        let launcher_pid = transport.id();
        // 纳入必须早于握手，这是整树保护能否成立的分界（见 `assign_pid`）。
        // `uvx`/`npx` 这类 launcher 会先拉起自己的 Server 子进程，握手
        // 成功即证明该子进程已存在。若此时再纳入，进 Job 的只有 launcher，
        // 证据里的 active=true 就不成立。
        let (job, process_isolation) = protect_launcher(job, launcher_pid);
        match tokio::time::timeout(handshake, ().serve(transport)).await {
            Ok(Ok(service)) => Ok(Self {
                service,
                _job: job,
                process_isolation,
            }),
            // 握手失败或超时返回时 `_job` 随之释放，KILL_ON_JOB_CLOSE 收掉
            // 已纳入的成员及其子孙，不留孤儿进程。故障携带当时是否进入
            // Job 的事实。
            Ok(Err(error)) => Err(McpError::new(
                crate::domain::mcp::McpFaultKind::Handshake,
                false,
            )
            .with_detail(format!("MCP 握手失败: {error}"))
            .with_isolation(process_isolation)),
            Err(_) => Err(McpError::new(
                crate::domain::mcp::McpFaultKind::HandshakeTimeout,
                false,
            )
            .with_detail(format!(
                "MCP 握手超时（{handshake:?} 内未完成协议握手）"
            ))
            .with_isolation(process_isolation)),
        }
    }
}

/// 把 launcher PID 纳入已建好的 Job，返回 Job 句柄与保护事实。
///
/// 单独成函数是为了所有权：纳入成功就把守卫交给 `RealMcpClient` 长期
/// 持有（drop 时终止整树）；失败或拿不到 PID 则当场释放空的 Job 句柄。
/// 就地处理会让 `Option<JobGuard>` 的借用与移动冲突。
///
/// 失败不阻断连接。进程已在运行，此时降级并记录 `active = false`，与
/// shell 工具的 assign 失败一致。
fn protect_launcher(job: Option<JobGuard>, launcher_pid: Option<u32>) -> (Option<JobGuard>, bool) {
    let Some(guard) = job else {
        // 平台不支持，创建阶段已经警告过，这里不重复。
        return (None, false);
    };
    let Some(pid) = launcher_pid else {
        eprintln!("警告：拿不到 MCP 子进程 PID，未纳入沙箱");
        return (None, false);
    };
    match guard.assign_pid(pid) {
        Ok(()) => (Some(guard), true),
        Err(error) => {
            eprintln!("警告：MCP 子进程 (pid={pid}) 未纳入沙箱: {error}");
            (None, false)
        }
    }
}

impl McpClientHandle for RealMcpClient {
    fn process_isolation(&self) -> bool {
        self.process_isolation
    }

    fn list_tools(&self) -> BoxFuture<'_, Result<Vec<RmcpTool>, McpError>> {
        Box::pin(async {
            // list_all_tools 自动翻页，拿全量。
            self.service
                .list_all_tools()
                .await
                .map_err(classify_service_error)
        })
    }

    fn call_tool<'a>(
        &'a self,
        remote_name: &'a str,
        arguments: Value,
    ) -> BoxFuture<'a, Result<String, McpError>> {
        Box::pin(async move {
            let mut params = CallToolRequestParams::new(remote_name.to_owned());
            params.arguments = Some(arguments.as_object().cloned().unwrap_or_default());
            // 调用预算统一由 ManagedServer::invoke 控制，只有一处超时与
            // 判据。这里再叠加固定超时会产生两种"调用超时"口径。外层取消
            // future 时 rmcp 会尽力发送 notifications/cancelled，这是协作式
            // 取消，远端可能继续执行，结果未知由调用点记录。
            let result = self.service.call_tool(params).await.map_err(classify_service_error)?;

            if result.is_error == Some(true) {
                // Server 报告执行失败：业务错误，连接仍可用。正文照常返回，
                // 模型需要看到原因。ServerError 分类让审计能区分"远端执行
                // 但报错"与"请求未送达"。
                return Err(McpError::new(crate::domain::mcp::McpFaultKind::ServerError, false)
                    .with_detail(extract_text(&result)));
            }
            Ok(extract_text(&result))
        })
    }

    /// rmcp 的 `is_closed()` 只反映主动关闭：transport 自行 EOF 结束时仍为
    /// `false`，粗判可能漏掉已断开。真实连接没有关闭通知，断连由调用返回
    /// 的 TransportClosed 分类发现。
    fn is_closed(&self) -> bool {
        self.service.is_closed()
    }

    /// 真实连接没有可观察的关闭通知，固定返回 `None`。
    ///
    /// rmcp 3.8.0 只提供消费 `RunningService` 的 `waiting()`，没有不交出
    /// service 就能等待关闭的 API；而 service 必须留在本结构里，`close()`
    /// 还要用它。断连因此由调用返回的 TransportClosed 分类与 `is_closed()`
    /// 粗判发现。只有替身实现提供真正的通知，供生命周期测试使用。
    fn closed(&self) -> Option<BoxFuture<'static, ()>> {
        None
    }

    /// 等待 rmcp 收尾（transport close 完成）后返回。Job 按字段顺序在此
    /// 之后释放，先 service 后 _job，KILL_ON_JOB_CLOSE 只收掉 rmcp 三秒
    /// 宽限后仍未退出的残留进程。
    fn close(&mut self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            // 用 SDK 自带的有界关闭，不用外部 tokio::time::timeout。外部
            // 超时取消的是 close() future 本身，JoinHandle 留在 service 里
            // 随 drop 消失，无法确认清理是否完成。close_with_timeout 超时
            // 时会自行 take 句柄并在后台继续收尾（Ok(None) 表示没等到，
            // 不是没在收）。5 秒的等待预算高于 drain 5 秒与 transport close
            // 3 秒这两个常见上限。结果不向外传播，本路径只负责等到或超时，
            // 残留进程由 Job 兜底。
            let _ = self
                .service
                .close_with_timeout(Duration::from_secs(5))
                .await;
            // self._job 在本 async 块结束、&mut self 释放后由所有者 drop。
        })
    }
}

/// rmcp 服务层错误的领域分类。只看变体，不匹配文本。
fn classify_service_error(error: rmcp::ServiceError) -> McpError {
    use rmcp::ServiceError;
    let kind = match &error {
        // 传输已关闭，连接结束。请求可能已经发出，结果是否未知由上层结合
        // "是否在等响应"判断。这里保守记为未发出，调用方的超时包装会
        // 覆盖为 true。
        ServiceError::TransportClosed | ServiceError::Cancelled { .. } => {
            crate::domain::mcp::McpFaultKind::Disconnected
        }
        ServiceError::TransportSend(_) | ServiceError::UnexpectedResponse => {
            crate::domain::mcp::McpFaultKind::Protocol
        }
        ServiceError::McpError(_) | ServiceError::SubscriptionLagged { .. } => {
            crate::domain::mcp::McpFaultKind::Protocol
        }
        ServiceError::Timeout { .. } | ServiceError::InputRequiredRoundsExceeded { .. } => {
            crate::domain::mcp::McpFaultKind::CallTimeout
        }
        _ => crate::domain::mcp::McpFaultKind::Protocol,
    };
    McpError::new(kind, false).with_detail(format!("{error}"))
}

/// 把 `CallToolResult` 拼成给模型看的文本。
///
/// 优先取文本块。没有文本块时回退到 `structured_content`：MCP 规范只
/// 建议、不强制 Server 附带序列化文本，纯结构化结果是合法响应。两者
/// 都没有时准确报告收到的内容，不写成空内容，以免模型把有结果误判成
/// 没结果。
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
        return serde_json::to_string_pretty(structured).unwrap_or_else(|_| structured.to_string());
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

/// 远端工具描述转换为注册表规格。首次发现与重连后的再发现共用此函数。
///
/// 独立成函数是为了审批契约与执行定义同源。规格在 [`ManagedServer`]
/// 首次注册后固定不变，重连只刷新"当前清单"用于比对。两条路径必须
/// 产出逐字段一致的 [`ToolSpec`]，否则比对会出现假阳性或假阴性。
fn tool_specs(server: &str, remote_tools: Vec<RmcpTool>) -> Vec<ToolSpec> {
    remote_tools
        .into_iter()
        .map(|tool| {
            let description = tool.description.as_deref().unwrap_or("").to_owned();
            ToolSpec {
                name: mcp_tool_name(server, &tool.name),
                // 来源写进描述：模型看到工具列表时知道这是外部能力。
                description: format!("（MCP Server `{server}` 提供）{description}"),
                parameters: Value::Object((*tool.input_schema).clone()),
                annotations: infer_annotations(&tool.name, &description),
            }
        })
        .collect()
}

/// [`Connector`] 的真实实现：启动子进程、纳入沙箱、有界握手。
///
/// 握手超时由 [`RealMcpClient::connect`] 处理。超时取消 `serve` 这一段，
/// transport 随之 drop，rmcp 的 ChildWithCleanup 在 drop 时 kill 子进程。
/// 有进程沙箱的平台（Windows）上 Job 句柄同时释放并整树兜底；其他平台
/// 没有进程沙箱，只 kill 直接子进程，后代进程需另行清理。清理无需额外
/// 代码，故障也能带上当时是否进入 Job 的真实保护状态。
struct TokioConnector {
    config: config::McpServerConfig,
}

impl Connector for TokioConnector {
    fn connect(
        &self,
        timeout: Duration,
    ) -> BoxFuture<'_, Result<Box<dyn McpClientHandle>, McpError>> {
        Box::pin(async move {
            RealMcpClient::connect(&self.config, timeout)
                .await
                .map(|client| Box::new(client) as Box<dyn McpClientHandle>)
        })
    }
}

/// 已连接并发现工具的 MCP Server：工具清单、调用通道与生命周期状态机。
///
/// 本体是 `ManagedServer` 的外壳。规格与执行入口取自同一棵状态树，
/// "先注册、再鉴权、再审批、后执行"因此作用于同一份定义。
///
/// 可克隆：克隆共享同一个状态机（内部为 `Arc`）。注册表与收尾清单各持
/// 一份，关闭对两边同时生效。
#[derive(Clone)]
pub struct McpToolSource {
    managed: Arc<ManagedServer>,
}

impl std::fmt::Debug for McpToolSource {
    // 状态机在异步锁内，调试信息给出名字与固定的清单即可。
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpToolSource")
            .field("server", &self.managed.server)
            .field("tools", &self.managed.tools)
            .finish_non_exhaustive()
    }
}

impl McpToolSource {
    /// 连接一个真实 Server：启动进程、握手、发现工具。
    ///
    /// 超时分两段：`connect` 覆盖启动进程与握手，`discovery` 覆盖发现
    /// 阶段。rmcp 的 `tools/list` 没有内部超时，若只有整体超时，"握手
    /// 正常但对 tools/list 不响应"的 Server 会给出无法定位阶段的错误。
    /// 分段后错误能直接指明排查方向。故障以 [`McpError`] 返回，
    /// `connect_all` 在边界上转为警告文本。
    pub async fn connect(
        config: &config::McpServerConfig,
        recorder: Option<Arc<McpLifecycleRecorder>>,
    ) -> Result<Self, McpError> {
        Self::connect_with(config, Arc::new(TokioConnector { config: config.clone() }), recorder, McpTimeouts::production()).await
    }

    /// 连接骨架，连接源与超时预算可注入。
    ///
    /// 独立成函数，让握手段超时与发现段超时两条真实路径都能用注入的
    /// Server 配置（测试夹具）走一遍，无需先构造替身。
    pub(crate) async fn connect_with(
        config: &config::McpServerConfig,
        connector: Arc<dyn Connector>,
        recorder: Option<Arc<McpLifecycleRecorder>>,
        timeouts: McpTimeouts,
    ) -> Result<Self, McpError> {
        let managed = ManagedServer::establish(&config.name, connector, recorder, timeouts).await?;
        Ok(Self::from_managed(managed))
    }

    /// 用已经建好的状态机构造。替身驱动的状态机测试从这里进入。
    pub(crate) fn from_managed(managed: Arc<ManagedServer>) -> Self {
        Self { managed }
    }

    /// 共享的状态机句柄：lib 内测试与断言经由它。
    pub(crate) fn managed(&self) -> &Arc<ManagedServer> {
        &self.managed
    }

    /// 显式关闭连接。幂等，正常收尾使用它，不依赖进程退出作为清理手段。
    ///
    /// 状态机保证顺序：先让 transport 优雅收尾，再释放进程组兜底。关闭
    /// 之后工具清单不可用，任何调用都会被拒绝。
    pub async fn close(&self) {
        self.managed.close().await;
    }

    /// 发现的工具数。
    pub fn tool_count(&self) -> usize {
        self.managed.tools.len()
    }

    /// Server 名。
    pub fn server(&self) -> &str {
        &self.managed.server
    }

    /// 装配成注册表登记项。规格与执行入口在这里绑定。
    pub fn entries(&self) -> Vec<ToolEntry> {
        let prefix = format!("{MCP_TOOL_PREFIX}{}__", self.managed.server);
        self.managed
            .tools
            .iter()
            .map(|spec| {
                let remote_name = spec
                    .name
                    .strip_prefix(&prefix)
                    .expect("tools 由 tool_specs 生成，必带本 Server 前缀")
                    .to_owned();
                ToolEntry {
                    spec: spec.clone(),
                    invoker: Arc::new(McpToolInvoker {
                        managed: Arc::clone(&self.managed),
                        remote_name,
                        registered: spec.clone(),
                    }),
                }
            })
            .collect()
    }
}

/// 一个 MCP Server 即一个工具来源，来源名取 Server 名。
///
/// 注册表按来源装配时无需认识 MCP。工具清单与执行入口已由
/// [`McpToolSource::entries`] 绑定，这里只做接口对接。
impl crate::application::tool_registry::ToolSource for McpToolSource {
    fn name(&self) -> &str {
        self.server()
    }

    fn tools(&self) -> Vec<ToolEntry> {
        self.entries()
    }
}

/// MCP 工具的执行入口，把调用交给 Server 状态机。
///
/// `registered` 是登记进注册表的规格。执行前状态机将它和当前清单比对，
/// 工具消失或定义变化就按旧定义拒绝调用，审批契约不随重连热更新。
struct McpToolInvoker {
    managed: Arc<ManagedServer>,
    remote_name: String,
    registered: ToolSpec,
}

impl ToolInvoker for McpToolInvoker {
    fn invoke<'a>(&'a self, call: &'a ToolCall) -> BoxFuture<'a, ToolOutcome> {
        Box::pin(self.managed.invoke(&self.remote_name, &self.registered, call))
    }
}

/// 依次连接配置中的全部 Server。
///
/// 单个 Server 失败降级为警告（返回值的第二个元素），不影响其余 Server。
/// 顺序连接而非并发：Server 数量是个位数，并发节省的时间不值得引入
/// 聚合复杂度。`recorder` 存在时，连接级事实（成功/失败/断连/恢复/
/// 关闭）写入证据链。失败文本只在 `connect_all` 边界由 [`McpError`]
/// 转为 String，分类信息不丢失。
pub async fn connect_all(
    config: &config::McpConfig,
    recorder: Option<Arc<McpLifecycleRecorder>>,
) -> (Vec<McpToolSource>, Vec<String>) {
    let mut sources = Vec::new();
    let mut warnings = Vec::new();
    for server in &config.mcp_servers {
        match McpToolSource::connect(server, recorder.clone()).await {
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
    use crate::domain::evidence::AuditEvent;
    use crate::domain::mcp::{McpFaultKind, McpLifecycleKind};
    use crate::domain::platform::Platform;
    use crate::domain::tool::OutcomeStatus;
    use crate::domain::workspace::WorkspaceRoot;
    use crate::application::memory_evidence::MemoryEvidenceSink;
    use crate::infrastructure::tool::all_tools;
    use serde_json::json;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Mutex;

    /// 断言 MCP 边界的三个字段，不用 `ExecutionBoundary::mcp` 构造期望值。
    /// 该构造函数本身是被测语义的一部分，用它作基准会让测试与实现同错。
    fn assert_mcp_boundary(outcome: &ToolOutcome, expected_active: bool) {
        let boundary = outcome
            .execution
            .expect("MCP 调用必须交代执行边界，否则报表与命令行只能猜");
        assert_eq!(
            boundary.active, expected_active,
            "active 要如实反映进程是否真的进了 Job"
        );
        assert_eq!(
            boundary.platform,
            crate::domain::sandbox::MCP_BOUNDARY_LABEL,
            "平台标签要能把它与内置工具的进程边界区分开"
        );
        assert!(
            !boundary.boundary_checked,
            "MCP 调用没做过命令字面路径检查，boundary_checked 必须是 false"
        );
    }

    /// 固定清单与回复的假连接。
    struct FakeClient {
        tools: Vec<RmcpTool>,
        /// 每次调用的稳定回复：成功文本，或同类常驻故障。
        reply: Result<String, McpError>,
        /// 预置一次性故障，下一次调用消费它（模拟 `isError` 或单次超时这类
        /// 不终止连接的故障），之后的调用回到 `reply`。
        next_error: Mutex<Option<McpError>>,
        /// 记录收到的调用（原始名），用于断言转发名不带前缀，以及故障后
        /// 没有第二次到达 Server（不重放）。
        calls: Mutex<Vec<String>>,
        /// 模拟子进程已纳入宿主沙箱，驱动边界证据的 active 分支。
        isolated: bool,
        /// `close()` 之后为 `true`，供 `is_closed` 粗判使用。
        closed: AtomicBool,
    }

    /// 半死 Server 的假连接：`tools/list` 永不返回。实测中 rmcp 的
    /// `list_all_tools()` 发送请求后一直等待，Server 不回就永久挂起，
    /// 发现段超时为此设置。
    struct HalfDeadClient;

    impl McpClientHandle for HalfDeadClient {
        fn process_isolation(&self) -> bool {
            false
        }

        fn list_tools(&self) -> BoxFuture<'_, Result<Vec<RmcpTool>, McpError>> {
            Box::pin(std::future::pending())
        }

        fn call_tool<'a>(
            &'a self,
            _remote_name: &'a str,
            _arguments: Value,
        ) -> BoxFuture<'a, Result<String, McpError>> {
            Box::pin(std::future::pending())
        }

        fn is_closed(&self) -> bool {
            false
        }

        fn closed(&self) -> Option<BoxFuture<'static, ()>> {
            None
        }

        fn close(&mut self) -> BoxFuture<'_, ()> {
            Box::pin(async {})
        }
    }

    /// 能列出工具、但调用永不返回的替身，用于驱动状态机的调用段超时。
    struct HangCallClient;

    impl McpClientHandle for HangCallClient {
        fn process_isolation(&self) -> bool {
            false
        }

        fn list_tools(&self) -> BoxFuture<'_, Result<Vec<RmcpTool>, McpError>> {
            Box::pin(async { Ok(vec![rmcp_tool("get_page", "Fetches a page")]) })
        }

        fn call_tool<'a>(
            &'a self,
            _remote_name: &'a str,
            _arguments: Value,
        ) -> BoxFuture<'a, Result<String, McpError>> {
            Box::pin(std::future::pending())
        }

        fn is_closed(&self) -> bool {
            false
        }

        fn closed(&self) -> Option<BoxFuture<'static, ()>> {
            None
        }

        fn close(&mut self) -> BoxFuture<'_, ()> {
            Box::pin(async {})
        }
    }

    /// 把 `Arc<FakeClient>` 装进唯一所有者的句柄。
    ///
    /// 调用记录留在 `Arc` 上可观察，句柄本体交给状态机独占。替身与真实
    /// 连接的所有权形状一致，测试才能覆盖真实语义。
    struct SharedHandle(Arc<FakeClient>);

    impl McpClientHandle for SharedHandle {
        fn process_isolation(&self) -> bool {
            self.0.isolated
        }

        fn list_tools(&self) -> BoxFuture<'_, Result<Vec<RmcpTool>, McpError>> {
            let client = self.0.clone();
            Box::pin(async move { Ok(client.tools.clone()) })
        }

        fn call_tool<'a>(
            &'a self,
            remote_name: &'a str,
            _arguments: Value,
        ) -> BoxFuture<'a, Result<String, McpError>> {
            let client = self.0.clone();
            Box::pin(async move {
                client.calls.lock().unwrap().push(remote_name.to_owned());
                if let Some(error) = client.next_error.lock().unwrap().take() {
                    return Err(error);
                }
                client.reply.clone()
            })
        }

        fn is_closed(&self) -> bool {
            self.0.closed.load(Ordering::SeqCst)
        }

        fn closed(&self) -> Option<BoxFuture<'static, ()>> {
            None
        }

        fn close(&mut self) -> BoxFuture<'_, ()> {
            let client = self.0.clone();
            Box::pin(async move {
                client.closed.store(true, Ordering::SeqCst);
            })
        }
    }

    /// 测试用超时预算：足够短以不拖慢测试，足够长以不误判调度延迟。
    fn test_timeouts() -> McpTimeouts {
        McpTimeouts {
            connect: Duration::from_secs(2),
            discovery: Duration::from_millis(200),
            call: Duration::from_millis(300),
        }
    }

    /// 用替身建状态机并包成注册表入口。
    async fn source_with(server: &str, fake: Arc<FakeClient>) -> McpToolSource {
        let managed = ManagedServer::discover(
            server,
            Box::new(SharedHandle(fake)),
            None,
            None,
            test_timeouts(),
        )
        .await
        .expect("替身的清单发现不该失败");
        McpToolSource::from_managed(managed)
    }

    impl FakeClient {
        fn new(tools: Vec<RmcpTool>, reply: Result<String, McpError>) -> Arc<Self> {
            Arc::new(Self {
                tools,
                reply,
                next_error: Mutex::new(None),
                calls: Mutex::new(Vec::new()),
                isolated: false,
                closed: AtomicBool::new(false),
            })
        }

        fn with_tools(tools: Vec<RmcpTool>) -> Arc<Self> {
            Self::new(tools, Ok("工具结果".to_owned()))
        }

        /// 与 `with_tools` 同，但报告"子进程已进宿主沙箱"。
        fn protected(tools: Vec<RmcpTool>) -> Arc<Self> {
            Arc::new(Self {
                tools,
                reply: Ok("工具结果".to_owned()),
                next_error: Mutex::new(None),
                calls: Mutex::new(Vec::new()),
                isolated: true,
                closed: AtomicBool::new(false),
            })
        }

        /// 常驻的连接级故障：每一次调用都以断开失败收场。
        fn failing(tools: Vec<RmcpTool>) -> Arc<Self> {
            Self::new(
                tools,
                Err(McpError::new(McpFaultKind::Disconnected, false).with_detail("连接已断")),
            )
        }

        /// 一次性故障：第一次调用返回 `error`，连接状态由状态机按分类决定。
        /// `isError` 类故障之后连接必须仍可用。
        fn flaky(tools: Vec<RmcpTool>, error: McpError) -> Arc<Self> {
            let client = Self::new(tools, Ok("工具结果".to_owned()));
            *client.next_error.lock().unwrap() = Some(error);
            client
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
        // 动词表都不沾、只沾写、读写混合、写动词变形，所有拿不准的情况
        // 都必须判非只读。replace/import 叠加读动词与 deletes 复数曾是
        // 免审后门的来源，回归价值最高。
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

        let source = source_with("fetch", client).await;
        let entries = source.entries();

        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].spec.name, "mcp__fetch__get_page");
        assert!(entries[0].spec.annotations.read_only);
        assert_eq!(entries[1].spec.name, "mcp__fetch__submit_form");
        assert!(!entries[1].spec.annotations.read_only);

        // 来源接口与本体走同一份清单：装配成注册表时不会多也不会少。
        use crate::application::tool_registry::ToolSource;
        assert_eq!(source.name(), "fetch");
        let from_source = source.tools();
        assert_eq!(from_source.len(), entries.len());
        assert_eq!(from_source[0].spec.name, entries[0].spec.name);
        // 来源要写进描述，模型才知道这是外部能力。
        assert!(
            entries[0]
                .spec
                .description
                .contains("MCP Server `fetch` 提供")
        );
    }

    #[tokio::test]
    async fn half_dead_server_times_out_during_discovery_with_a_readable_error() {
        // 与 `establish` 相同的发现路径（`ManagedServer::discover`），
        // 使用短时限。半死替身永不回应，错误必须在时限内返回，且指明
        // 卡在发现阶段。
        let started = std::time::Instant::now();
        let error = ManagedServer::discover(
            "half-dead",
            Box::new(HalfDeadClient),
            None,
            None,
            McpTimeouts {
                connect: Duration::from_secs(1),
                discovery: Duration::from_millis(100),
                call: CALL_TIMEOUT,
            },
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
        assert_eq!(error.kind, McpFaultKind::DiscoveryTimeout);
        assert!(
            error.to_string().contains("list_tools"),
            "应指明排查方向: {error}"
        );
    }

    #[tokio::test]
    async fn disconnected_call_explains_unknown_result_without_replay() {
        let client = FakeClient::failing(vec![rmcp_tool("send_message", "Sends a message")]);
        let source = source_with("demo", client.clone()).await;
        let call = ToolCall {
            id: crate::domain::ids::CallId::new("uncertain-call"),
            name: "mcp__demo__send_message".into(),
            arguments: json!({"message": "once"}),
        };
        let outcome = source.entries()[0].invoker.invoke(&call).await;
        assert!(outcome.content.contains("结果未知"), "{outcome:?}");
        assert!(
            outcome.content.contains("--mcp-reconnect"),
            "断连指引要指向显式 CLI 恢复，而不是暗示自动重连: {outcome:?}"
        );
        assert_eq!(client.calls.lock().unwrap().len(), 1, "失败不能重放");
        let failure = outcome.mcp_failure.expect("断连故障要留下事实");
        assert!(failure.result_unknown, "请求已发出，结果必须记为未知");
        assert_eq!(failure.kind, McpFaultKind::Disconnected);
    }

    #[tokio::test]
    async fn invoker_forwards_the_remote_name_without_prefix() {
        let client = FakeClient::with_tools(vec![rmcp_tool("get_page", "Fetches a page")]);
        let source = source_with("fetch", client.clone()).await;
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
        // 替身未进入 Job，边界必须记录 active=false。
        assert_mcp_boundary(&outcome, false);
    }

    #[tokio::test]
    async fn invocation_errors_become_failed_outcomes_not_panics() {
        let client = FakeClient::failing(vec![rmcp_tool("get_page", "Fetches a page")]);
        let source = source_with("fetch", client.clone()).await;
        let entry = source.entries().remove(0);

        let call = ToolCall {
            id: crate::domain::ids::CallId::new("call_1"),
            name: "mcp__fetch__get_page".to_owned(),
            arguments: json!({}),
        };
        let outcome = entry.invoker.invoke(&call).await;

        assert_eq!(outcome.status, crate::domain::tool::OutcomeStatus::Failed);
        assert!(outcome.content.contains("fetch"));
        assert!(outcome.content.contains("连接已断"));
        assert!(outcome.is_error());
        // 调用失败也要交代边界；此替身未纳入沙箱，active=false。
        assert_mcp_boundary(&outcome, false);

        // 断连后连接判定为死：第二次调用不会到达 Server，没有自动重试，
        // 直接返回短路结果。
        let second = entry.invoker.invoke(&call).await;
        assert!(second.content.contains("连接已断开"), "{second:?}");
        assert_eq!(client.calls.lock().unwrap().len(), 1, "死连接上不得再发起调用");
    }

    /// 子进程纳入宿主沙箱后，MCP 边界记录 active=true，不再一律写死为
    /// 无保护。
    #[tokio::test]
    async fn a_job_protected_mcp_call_is_recorded_as_protected() {
        let client = FakeClient::protected(vec![rmcp_tool("get_page", "Fetches a page")]);
        let source = source_with("fetch", client).await;
        let entry = source.entries().remove(0);

        let call = ToolCall {
            id: crate::domain::ids::CallId::new("call_1"),
            name: "mcp__fetch__get_page".to_owned(),
            arguments: json!({}),
        };
        let outcome = entry.invoker.invoke(&call).await;

        assert_mcp_boundary(&outcome, true);
    }

    #[tokio::test]
    async fn mcp_tools_do_not_shadow_builtin_namesakes() {
        // MCP 工具带 `mcp__` 前缀，不覆盖内置同名工具。这里用 Server 暴露
        // 同名 `read_file` 的极端情况验证。
        let client = FakeClient::with_tools(vec![rmcp_tool("read_file", "Reads a remote file")]);
        let source = source_with("ext", client).await;

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
        assert!(
            names.contains(&"mcp__ext__read_file"),
            "MCP 版本以前缀名并存"
        );
        assert_eq!(names.len(), 11, "10 个内置 + 1 个 MCP，谁也不覆盖谁");
    }

    #[tokio::test]
    async fn non_read_only_mcp_tools_require_approval_by_default_policy() {
        // 拿不准的 MCP 工具默认需要审批，这里用真实默认策略验证。
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
        // 规范允许 Server 只返回 structuredContent，此时正文应为结构化数据
        // 本身，不写成"返回了空内容"。
        let mut result = rmcp::model::CallToolResult::success(vec![]);
        result.structured_content = Some(json!({"rows": 3, "query": "select 1"}));
        let text = extract_text(&result);
        assert!(text.contains("\"rows\"") && text.contains("3"), "{text}");
    }

    #[test]
    fn non_text_blocks_are_reported_honestly() {
        // 纯图片结果（截图类工具很常见）不能报告为空。
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
        // 二进制不存在时 spawn 立即失败，不应永久挂起或 panic，降级路径依赖
        // 这个错误。分类必须是 Spawn，不能归入握手或协议错误。
        let config = config::McpServerConfig {
            name: "ghost".to_owned(),
            command: "qch-no-such-mcp-binary-4f3a".to_owned(),
            args: vec![],
        };
        let error = McpToolSource::connect(&config, None).await.unwrap_err();
        assert_eq!(error.kind, McpFaultKind::Spawn);
        assert!(error.detail.contains("启动进程"), "{error}");
        assert!(!error.result_unknown, "请求从未发出，没有未知结果");
    }

    #[tokio::test]
    async fn connect_to_the_bundled_demo_server_end_to_end() {
        // 真协议连通性：启动仓库自带的演示 Server（cargo test 会先编译
        // examples，产物路径固定），走完整的 stdio 子进程链路：握手、
        // 发现、调用、文本提取。
        let exe = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(format!(
            "target/debug/examples/mcp-echo-server{}",
            std::env::consts::EXE_SUFFIX
        ));
        let config = config::McpServerConfig {
            name: "demo".to_owned(),
            command: exe.display().to_string(),
            args: vec![],
        };
        let source = McpToolSource::connect(&config, None)
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
        // 两个 Server 都无法启动，结果应是 0 个源和 2 条带名字的警告，
        // 不报整个连接过程失败。
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
        let (sources, warnings) = connect_all(&config, None).await;

        assert!(sources.is_empty());
        assert_eq!(warnings.len(), 2);
        assert!(warnings[0].contains("ghost") && warnings[0].contains("不可用"));
    }

    // ── 故障分类与连接状态（替身）──────────────────────────────────────

    /// `isError` 是业务错误，不应终止连接，后续调用必须照常到达 Server。
    ///
    /// 若把"远端执行但报错"误判为传输故障，一次参数错误会毁掉整条
    /// 连接上的所有工具。
    #[tokio::test]
    async fn a_business_error_keeps_the_connection_usable() {
        let error = McpError::new(McpFaultKind::ServerError, false).with_detail("远端参数不合法");
        let client = FakeClient::flaky(vec![rmcp_tool("get_page", "Fetches a page")], error);
        let source = source_with("demo", client.clone()).await;
        let entry = source.entries().remove(0);
        let call = call_to("mcp__demo__get_page");

        let first = entry.invoker.invoke(&call).await;
        assert_eq!(first.status, crate::domain::tool::OutcomeStatus::Failed);
        let failure = first.mcp_failure.expect("isError 也要留下故障事实");
        assert_eq!(failure.kind, McpFaultKind::ServerError);
        assert!(!failure.result_unknown, "远端已经答了：结果是已知的（答案是错）");

        let second = entry.invoker.invoke(&call).await;
        assert_eq!(second.status, crate::domain::tool::OutcomeStatus::Success);
        assert_eq!(client.calls.lock().unwrap().len(), 2, "故障与恢复都是真实往返");
    }

    /// 调用超时：记录结果未知，不终止 Server，不自动重发。
    ///
    /// 超时后连接仍判定可用（本轮不做取消确认）。第二次调用照常执行并
    /// 再次超时，不会被短路成"连接已断开"。超时与连接死亡必须区分。
    #[tokio::test]
    async fn a_hanging_call_times_out_without_killing_the_connection() {
        let managed = ManagedServer::discover("demo", Box::new(HangCallClient), None, None, test_timeouts())
            .await
            .expect("清单能给的替身应当连接成功");
        let source = McpToolSource::from_managed(managed);
        let entry = source.entries().remove(0);
        let call = call_to("mcp__demo__get_page");

        let first = entry.invoker.invoke(&call).await;
        let failure = first.mcp_failure.as_ref().expect("超时必须留下故障事实");
        assert_eq!(failure.kind, McpFaultKind::CallTimeout);
        assert!(failure.result_unknown, "请求已发出，远端可能仍在执行");
        assert!(first.content.contains("结果未知"), "{first:?}");

        let second = entry.invoker.invoke(&call).await;
        assert!(second.content.contains("调用超时"), "超时后连接仍应可用: {second:?}");
        assert!(!second.content.contains("连接已断开"));
    }

    // ── 真实子进程（夹具）─────────────────────────────────────────────

    /// 每条测试使用独立临时目录，防止测试间污染，也避免计数文件初值与
    /// 标记文件状态受上一轮残留影响。
    fn fixture_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("qch-mcp-lc-{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("测试临时目录可创建");
        dir
    }

    fn fixture_config(dir: &Path, mode: &str) -> config::McpServerConfig {
        config::McpServerConfig {
            name: "lc".to_owned(),
            // `CARGO_BIN_EXE_…` 只提供给集成测试，不提供给 lib 单元测试
            // （`cargo check --all-targets` 会编译失败）。与演示 Server 的
            // 端到端测试相同：`cargo test` 会先构建 bin，产物路径确定。
            command: Path::new(env!("CARGO_MANIFEST_DIR"))
                .join(format!(
                    "target/debug/mcp-lifecycle-server{}",
                    std::env::consts::EXE_SUFFIX
                ))
                .display()
                .to_string(),
            args: vec![mode.to_owned(), dir.join("counter.txt").display().to_string()],
        }
    }

    /// 真实进程预算：启动给足（本机杀毒软件冷启动慢），调用给短（超时
    /// 路径的测试不能等 120 秒）。
    fn subprocess_timeouts() -> McpTimeouts {
        McpTimeouts {
            connect: Duration::from_secs(15),
            discovery: Duration::from_secs(10),
            call: Duration::from_millis(800),
        }
    }

    fn suffixed(path: &Path, suffix: &str) -> PathBuf {
        let mut text = path.as_os_str().to_os_string();
        text.push(suffix);
        PathBuf::from(text)
    }

    fn counter_lines(path: &Path) -> usize {
        std::fs::read_to_string(path).map_or(0, |text| text.lines().count())
    }

    fn read_pid(path: &Path) -> String {
        std::fs::read_to_string(path)
            .unwrap_or_else(|error| panic!("读取 {} 失败: {error}", path.display()))
            .trim()
            .to_owned()
    }

    /// 有界等待一个磁盘或进程事实成立。必须 `await` 让出执行器：rmcp 的
    /// kill-on-drop 通过 `tokio::spawn` 执行，current_thread 运行时里阻塞
    /// 等待会让 kill 永远得不到调度。
    async fn wait_until(label: &str, timeout: Duration, mut done: impl FnMut() -> bool) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if done() {
                return true;
            }
            if tokio::time::Instant::now() >= deadline {
                eprintln!("等待{label}超时（{timeout:?}）");
                return false;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// 按 PID 查询进程是否存活。只看输出中的 PID 数字：tasklist 的"没有
    /// 匹配进程"提示随系统语言变化，数字不变。
    ///
    /// 非 Windows 用 `kill -0`：向进程发信号 0 只做存在性检查，不影响它。
    /// 该调用在本进程内执行，命令本身不带副作用。
    fn process_alive(pid: &str) -> bool {
        let pid: i32 = pid.parse().unwrap_or(-1);
        if pid <= 0 {
            return false;
        }
        if Platform::current() == Platform::Windows {
            let output = std::process::Command::new("tasklist")
                .args(["/FI", &format!("PID eq {pid}"), "/NH"])
                .output()
                .expect("tasklist 应可运行");
            String::from_utf8_lossy(&output.stdout)
                .split_whitespace()
                .any(|token| token == pid.to_string())
        } else {
            // 信号 0 不投递信号，只检查进程是否存在与是否可发信号。
            // 进程已退出或属于别的用户时返回非 0。
            std::process::Command::new("kill")
                .args(["-0", &pid.to_string()])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .map(|status| status.success())
                .unwrap_or(false)
        }
    }

    fn test_recorder() -> (Arc<McpLifecycleRecorder>, Arc<MemoryEvidenceSink>) {
        let sink = Arc::new(MemoryEvidenceSink::new());
        let recorder = Arc::new(McpLifecycleRecorder::new(
            sink.clone(),
            Arc::new(crate::infrastructure::clock::SystemClock),
        ));
        (recorder, sink)
    }

    fn count_lifecycle(sink: &MemoryEvidenceSink, kind: McpLifecycleKind) -> usize {
        sink.events()
            .iter()
            .filter(|entry| {
                matches!(&entry.event, AuditEvent::McpLifecycle { fact, .. } if fact.kind == kind)
            })
            .count()
    }

    fn call_to(tool: &str) -> ToolCall {
        ToolCall {
            id: crate::domain::ids::CallId::new("lc-call"),
            name: tool.to_owned(),
            arguments: json!({}),
        }
    }

    /// 启动进程后握手失败，分类为 Handshake。子进程自行退出时 rmcp 拿不到
    /// 退出码，证据中不得出现"异常退出"之类的推测。
    #[tokio::test]
    async fn a_handshake_failing_server_is_classified_handshake_without_guessing() {
        let dir = fixture_dir("handshake-fail");
        let config = fixture_config(&dir, "handshake-fail");
        let error = McpToolSource::connect_with(
            &config,
            Arc::new(TokioConnector { config: config.clone() }),
            None,
            subprocess_timeouts(),
        )
        .await
        .expect_err("启动即退出的 Server 不该连接成功");
        assert_eq!(error.kind, McpFaultKind::Handshake);
        assert!(!error.result_unknown, "请求从未发出");
    }

    /// 能收帧但永不回应 initialize 的 Server：握手段必须有界返回，且启动后
    /// 未完成握手的进程不能留下孤儿。
    #[tokio::test]
    async fn a_server_that_never_replies_to_initialize_times_out_bounded_and_dies() {
        let dir = fixture_dir("handshake-timeout");
        let config = fixture_config(&dir, "no-init-reply");
        let started = std::time::Instant::now();
        let error = McpToolSource::connect_with(
            &config,
            Arc::new(TokioConnector { config: config.clone() }),
            None,
            McpTimeouts {
                connect: Duration::from_millis(400),
                discovery: Duration::from_secs(1),
                call: Duration::from_secs(1),
            },
        )
        .await
        .expect_err("不回握手的 Server 不该连接成功");
        assert_eq!(error.kind, McpFaultKind::HandshakeTimeout);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "握手超时必须有界，实际 {elapsed:?}",
            elapsed = started.elapsed()
        );
        // 超时取消 serve 会 drop transport，rmcp 的 ChildWithCleanup 在 drop
        // 时 kill 子进程；Windows 上 Job 句柄同时释放并整树兜底。子进程
        // 是否真的退出用 PID 查证，各平台都能查，不做未经验证的假设。
        let pid = read_pid(&suffixed(&dir.join("counter.txt"), ".pid"));
        assert!(
            wait_until("子进程退出", Duration::from_secs(10), || !process_alive(&pid)).await,
            "握手超时后子进程必须被清理，pid {pid} 仍存活"
        );
    }

    /// 应答握手但从第 1 次调用起沉默的 Server：调用超时记为结果未知，
    /// 副作用只落盘一次（计数文件），超时与关闭都不会重放。
    #[tokio::test]
    async fn a_silent_server_call_times_out_with_unknown_result_and_is_not_replayed() {
        let dir = fixture_dir("silent-call");
        let counter = dir.join("counter.txt");
        let config = fixture_config(&dir, "silent-after-1");
        let source = McpToolSource::connect_with(
            &config,
            Arc::new(TokioConnector { config: config.clone() }),
            None,
            subprocess_timeouts(),
        )
        .await
        .expect("夹具应能握手与发现");
        let entry = source.entries().remove(0);

        let outcome = entry.invoker.invoke(&call_to("mcp__lc__side_effect")).await;
        let failure = outcome.mcp_failure.expect("超时必须留下故障事实");
        assert_eq!(failure.kind, McpFaultKind::CallTimeout);
        assert!(failure.result_unknown);
        // 计数落盘发生在沉默之前：副作用确实执行过一次，Harness 无法知道
        // 远端是否仍在执行，这就是结果未知。
        assert_eq!(counter_lines(&counter), 1);

        source.managed().close().await;
        assert!(
            wait_until("EOF 标记", Duration::from_secs(5), || suffixed(&counter, ".eof").exists())
                .await,
            "优雅关闭链必须执行到子进程（stdin EOF），不留孤儿"
        );
        assert_eq!(counter_lines(&counter), 1, "关闭与超时都不重放原调用");
    }

    /// 提交副作用后自行退出的 Server：断开按连接记一次，结果未知如实上报，
    /// 第二次调用被短路而非自动重试。
    #[tokio::test]
    async fn a_dropped_server_is_reported_disconnected_once_per_connection() {
        let dir = fixture_dir("drop-once");
        let counter = dir.join("counter.txt");
        let config = fixture_config(&dir, "drop-after-1");
        let (recorder, sink) = test_recorder();
        let source = McpToolSource::connect_with(
            &config,
            Arc::new(TokioConnector { config: config.clone() }),
            Some(recorder),
            subprocess_timeouts(),
        )
        .await
        .expect("夹具应能握手与发现");
        let entry = source.entries().remove(0);

        let outcome = entry.invoker.invoke(&call_to("mcp__lc__side_effect")).await;
        assert!(outcome.is_error(), "断开不得伪装成功: {outcome:?}");
        let failure = outcome.mcp_failure.expect("断开必须留下故障事实");
        assert_eq!(failure.kind, McpFaultKind::Disconnected);
        assert!(failure.result_unknown, "副作用已落盘、答复没收到——只能未知");
        assert_eq!(counter_lines(&counter), 1);

        // 同一连接上的第二次调用：短路失败，不触碰 Server，不新增断开事件。
        let second = entry.invoker.invoke(&call_to("mcp__lc__side_effect")).await;
        assert!(second.content.contains("连接已断开"));
        assert_eq!(count_lifecycle(&sink, McpLifecycleKind::Connected), 1);
        assert_eq!(
            count_lifecycle(&sink, McpLifecycleKind::Disconnected),
            1,
            "一次断开只记一次，多工具共享同一条连接"
        );
        assert_eq!(counter_lines(&counter), 1, "死连接上不得再发起调用");
    }

    /// 显式恢复只重建通信：不重放原调用（计数不变），恢复后调用可用，
    /// 工具清单无变化时 `tools_changed` 为 false，代际推进到 2。
    #[tokio::test]
    async fn reconnect_restores_communication_without_replaying_side_effects() {
        let dir = fixture_dir("reconnect");
        let counter = dir.join("counter.txt");
        let config = fixture_config(&dir, "drop-after-1");
        let (recorder, sink) = test_recorder();
        let source = McpToolSource::connect_with(
            &config,
            Arc::new(TokioConnector { config: config.clone() }),
            Some(recorder.clone()),
            subprocess_timeouts(),
        )
        .await
        .expect("夹具应能握手与发现");
        let entry = source.entries().remove(0);

        let first = entry.invoker.invoke(&call_to("mcp__lc__side_effect")).await;
        assert!(first.is_error(), "全局第 1 次调用让夹具自杀");
        assert_eq!(counter_lines(&counter), 1);

        // 恢复动作不得发送任何 tools/call，用计数文件验证。
        let recovered = source
            .managed()
            .reconnect(Some(crate::domain::ids::CallId::new("lc-call")))
            .await
            .expect("恢复应成功");
        assert!(recovered, "断开后的恢复应报告重建了连接");
        assert_eq!(counter_lines(&counter), 1, "恢复动作不得触碰工具");

        // 新进程从全局计数 1 续接：这是新的第二次调用，不是重放。
        let second = entry.invoker.invoke(&call_to("mcp__lc__side_effect")).await;
        assert_eq!(second.status, crate::domain::tool::OutcomeStatus::Success);
        assert!(second.content.contains("side-effect-2"), "{second:?}");
        assert_eq!(counter_lines(&counter), 2, "原调用未重放，只有新调用加一笔");

        let fact = sink
            .events()
            .iter()
            .find_map(|entry| match &entry.event {
                AuditEvent::McpLifecycle { fact, .. }
                    if fact.kind == McpLifecycleKind::Recovered =>
                {
                    Some(fact.clone())
                }
                _ => None,
            })
            .expect("恢复成功要留下事实");
        assert_eq!(fact.generation, 2, "恢复是新一代连接");
        assert!(!fact.tools_changed, "清单一致就不许报变化");
        // 恢复只重建通信：主动关闭收尾，不留进程。
        source.managed().close().await;
        assert_eq!(count_lifecycle(&sink, McpLifecycleKind::Disconnected), 1);
    }

    /// 主动关闭：进程优雅退出，`Closed` 恰好一条，重复关闭幂等，关闭后的
    /// 调用被拒绝。
    #[tokio::test]
    async fn explicit_close_ends_the_process_gracefully_and_stops_further_calls() {
        let dir = fixture_dir("close-normal");
        let counter = dir.join("counter.txt");
        let config = fixture_config(&dir, "ok");
        let (recorder, sink) = test_recorder();
        let source = McpToolSource::connect_with(
            &config,
            Arc::new(TokioConnector { config: config.clone() }),
            Some(recorder),
            subprocess_timeouts(),
        )
        .await
        .expect("夹具应能握手与发现");
        let entry = source.entries().remove(0);

        let outcome = entry.invoker.invoke(&call_to("mcp__lc__side_effect")).await;
        assert_eq!(outcome.status, crate::domain::tool::OutcomeStatus::Success);

        source.managed().close().await;
        assert!(
            wait_until("EOF 标记", Duration::from_secs(5), || suffixed(&counter, ".eof").exists())
                .await,
            "关闭必须走到子进程的优雅收尾"
        );
        source.managed().close().await;
        assert_eq!(
            count_lifecycle(&sink, McpLifecycleKind::Closed),
            1,
            "重复关闭是幂等空操作，不重复记事实"
        );

        let after = entry.invoker.invoke(&call_to("mcp__lc__side_effect")).await;
        assert!(after.content.contains("已主动关闭"), "{after:?}");
        assert_eq!(counter_lines(&counter), 1, "关闭后的调用不许到达 Server");
    }

    /// 关闭连接时是否终止整棵进程树，按平台能力判定。
    ///
    /// 有进程沙箱的平台（Windows）必须把孙进程一并收掉：孙进程不是
    /// Harness 的直接子进程，只有沙箱的整树终止能处理它。这条进程语义
    /// 无法用 mock 验证。
    ///
    /// 没有进程沙箱的平台不声称保护：连接的边界 `active` 必须是 false。
    /// 声称有保护而实际没有，比不声称更危险。
    #[tokio::test]
    async fn connection_termination_follows_the_detected_platform() {
        let dir = fixture_dir("tree-kill");
        let config = fixture_config(&dir, "with-child");
        let source = McpToolSource::connect_with(
            &config,
            Arc::new(TokioConnector { config: config.clone() }),
            None,
            subprocess_timeouts(),
        )
        .await
        .expect("夹具应能握手与发现");

        let isolated = source.managed().process_isolation().await;
        let child_path = suffixed(&dir.join("counter.txt"), ".child");
        assert!(
            wait_until("孙进程 PID 文件", Duration::from_secs(5), || child_path.exists()).await
        );
        let grandchild = read_pid(&child_path);
        assert!(process_alive(&grandchild), "关闭前孙进程应还活着");

        if PlatformCapabilities::detect().has_process_sandbox {
            // 这条测试以进程进入沙箱为前提，前提不成立就明确失败，不让整树
            // 断言承担错误信号。
            assert!(isolated, "夹具进程应已纳入沙箱");
            source.managed().close().await;
            assert!(
                wait_until("孙进程退出", Duration::from_secs(10), || !process_alive(&grandchild))
                    .await,
                "沙箱关闭必须把孙进程一并收掉，pid {grandchild} 仍存活说明整树终止失败"
            );
        } else {
            assert!(
                !isolated,
                "没有进程沙箱的平台必须报无保护"
            );
            source.managed().close().await;
        }
    }

    /// 脚本化连接源：状态机每次 `connect` 消费一个预置替身并计数。
    /// `attempts` 直接反映状态机发起了几次连接，用于验证"恢复只尝试
    /// 一次、并发共享、失败不重试"。
    struct ScriptConnector {
        plan: Mutex<std::collections::VecDeque<Result<Arc<FakeClient>, McpError>>>,
        attempts: std::sync::atomic::AtomicUsize,
        /// 首次连接的挂起时长，让并发请求在锁上排队，避免先后完成。
        pause: Duration,
    }

    impl ScriptConnector {
        fn new(plan: Vec<Result<Arc<FakeClient>, McpError>>) -> Arc<Self> {
            Self::paused(plan, Duration::ZERO)
        }

        fn paused(plan: Vec<Result<Arc<FakeClient>, McpError>>, pause: Duration) -> Arc<Self> {
            Arc::new(Self {
                plan: Mutex::new(plan.into()),
                attempts: std::sync::atomic::AtomicUsize::new(0),
                pause,
            })
        }

        fn attempts(&self) -> usize {
            self.attempts.load(Ordering::SeqCst)
        }
    }

    impl Connector for ScriptConnector {
        fn connect(
            &self,
            _timeout: Duration,
        ) -> BoxFuture<'_, Result<Box<dyn McpClientHandle>, McpError>> {
            self.attempts.fetch_add(1, Ordering::SeqCst);
            let pause = self.pause;
            let next = self.plan.lock().unwrap().pop_front();
            Box::pin(async move {
                if pause > Duration::ZERO {
                    tokio::time::sleep(pause).await;
                }
                match next {
                    Some(Ok(client)) => Ok(Box::new(SharedHandle(client)) as Box<dyn McpClientHandle>),
                    Some(Err(error)) => Err(error),
                    None => Err(McpError::new(McpFaultKind::Spawn, false)
                        .with_detail("测试脚本没有预置更多连接")),
                }
            })
        }
    }

    /// 自带断连通知的句柄，用于监视器路径（`closed()`）。真实连接没有
    /// 关闭通知（`RealMcpClient::closed` 恒为 `None`），这里通过 `dead`
    /// 手动触发，替身因此能驱动监视器。
    struct NotifyingHandle {
        inner: Arc<FakeClient>,
        dead: Arc<tokio::sync::Notify>,
    }

    impl McpClientHandle for NotifyingHandle {
        fn process_isolation(&self) -> bool {
            self.inner.isolated
        }

        fn list_tools(&self) -> BoxFuture<'_, Result<Vec<RmcpTool>, McpError>> {
            let client = self.inner.clone();
            Box::pin(async move { Ok(client.tools.clone()) })
        }

        fn call_tool<'a>(
            &'a self,
            remote_name: &'a str,
            _arguments: Value,
        ) -> BoxFuture<'a, Result<String, McpError>> {
            let client = self.inner.clone();
            Box::pin(async move {
                client.calls.lock().unwrap().push(remote_name.to_owned());
                if let Some(error) = client.next_error.lock().unwrap().take() {
                    return Err(error);
                }
                client.reply.clone()
            })
        }

        fn is_closed(&self) -> bool {
            self.inner.closed.load(Ordering::SeqCst)
        }

        fn closed(&self) -> Option<BoxFuture<'static, ()>> {
            let dead = self.dead.clone();
            Some(Box::pin(async move {
                dead.notified().await;
            }))
        }

        fn close(&mut self) -> BoxFuture<'_, ()> {
            let client = self.inner.clone();
            Box::pin(async move {
                client.closed.store(true, Ordering::SeqCst);
            })
        }
    }

    /// 用一次断连故障的调用把连接驱动到 Disconnected。走 invoke 的故障
    /// 分支，与生产断连路径相同。
    async fn invoke_until_disconnected(managed: &ManagedServer) {
        let outcome = managed
            .invoke("get_page", &managed.tools[0], &call_to("mcp__svc__get_page"))
            .await;
        assert_eq!(
            outcome
                .mcp_failure
                .expect("断连调用必须留下故障事实")
                .kind,
            McpFaultKind::Disconnected
        );
    }

    fn page_tools() -> Vec<RmcpTool> {
        vec![rmcp_tool("get_page", "Fetches a page")]
    }

    /// 断连类故障样本，用于驱动状态机走断连分支。
    fn disconnected_fault() -> McpError {
        McpError::new(McpFaultKind::Disconnected, false).with_detail("替身断连")
    }

    /// 断连后一次恢复请求恰好产生一次连接尝试，恢复后调用落在新实例上。
    /// 连接可用时再请求恢复不执行任何操作，不新建实例。
    #[tokio::test]
    async fn a_recovery_request_makes_exactly_one_attempt_and_restores_calls() {
        let old = FakeClient::flaky(page_tools(), disconnected_fault());
        let fresh = FakeClient::with_tools(page_tools());
        let connector = ScriptConnector::new(vec![Ok(Arc::clone(&fresh))]);
        let managed = ManagedServer::discover(
            "svc",
            Box::new(SharedHandle(old)),
            Some(Arc::clone(&connector) as Arc<dyn Connector>),
            None,
            test_timeouts(),
        )
        .await
        .expect("替身发现不该失败");
        invoke_until_disconnected(&managed).await;

        assert_eq!(managed.reconnect(None).await, Ok(true), "恢复要宣告重建了通信");
        assert_eq!(connector.attempts(), 1, "一次请求恰好一次尝试");

        let outcome = managed
            .invoke("get_page", &managed.tools[0], &call_to("mcp__svc__get_page"))
            .await;
        assert!(!outcome.is_error(), "恢复后的调用必须真的走通: {outcome:?}");
        assert_eq!(fresh.calls.lock().unwrap().len(), 1, "调用落在新实例上");

        // 连接存活时的恢复请求不执行任何操作，不额外新建实例。
        assert_eq!(managed.reconnect(None).await, Ok(false));
        assert_eq!(connector.attempts(), 1, "已就绪的连接不再触发连接尝试");
    }

    /// 并发恢复请求共享同一次尝试的成功结果：排队者拿到首个恢复的结论，
    /// 连接源只被调用一次。
    #[tokio::test]
    async fn concurrent_recovery_requests_share_one_attempt() {
        let old = FakeClient::flaky(page_tools(), disconnected_fault());
        let fresh = FakeClient::with_tools(page_tools());
        let (recorder, sink) = test_recorder();
        // 挂起 30ms，使 current_thread 运行时下的第二个请求必定在锁上排队。
        let connector = ScriptConnector::paused(vec![Ok(Arc::clone(&fresh))], Duration::from_millis(30));
        let managed = ManagedServer::discover(
            "svc",
            Box::new(SharedHandle(old)),
            Some(Arc::clone(&connector) as Arc<dyn Connector>),
            Some(recorder),
            test_timeouts(),
        )
        .await
        .expect("替身发现不该失败");
        invoke_until_disconnected(&managed).await;

        let (first, second) = tokio::join!(managed.reconnect(None), managed.reconnect(None));
        assert_eq!(first, Ok(true));
        assert_eq!(second, first, "并发请求共享同一次结果");
        assert_eq!(connector.attempts(), 1, "不许各起一个实例");
        assert_eq!(count_lifecycle(&sink, McpLifecycleKind::Recovered), 1);
        assert_eq!(
            count_lifecycle(&sink, McpLifecycleKind::Disconnected),
            1,
            "恢复流程里 disconnect() 幂等，断连不复读"
        );
    }

    /// 失败的恢复同样有界：一次请求一次尝试，失败事实入库，并发排队者
    /// 共享同一失败结论。连接保持 Disconnected，后续调用被短路拒绝，
    /// 不自动重试。
    #[tokio::test]
    async fn a_failed_recovery_ends_bounded_and_shares_the_failure() {
        let old = FakeClient::flaky(page_tools(), disconnected_fault());
        let (recorder, sink) = test_recorder();
        let connector = ScriptConnector::paused(
            vec![Err(McpError::new(McpFaultKind::Spawn, false).with_detail("起不来"))],
            Duration::from_millis(30),
        );
        let managed = ManagedServer::discover(
            "svc",
            Box::new(SharedHandle(old)),
            Some(Arc::clone(&connector) as Arc<dyn Connector>),
            Some(recorder),
            test_timeouts(),
        )
        .await
        .expect("替身发现不该失败");
        invoke_until_disconnected(&managed).await;

        let (first, second) = tokio::join!(managed.reconnect(None), managed.reconnect(None));
        for result in [first, second] {
            let error = result.expect_err("连接源注定失败，恢复不许谎报成功");
            assert_eq!(error.kind, McpFaultKind::Spawn);
            assert!(!error.result_unknown, "恢复没发出任何工具调用");
        }
        assert_eq!(connector.attempts(), 1, "失败同样只试一次");
        assert_eq!(count_lifecycle(&sink, McpLifecycleKind::RecoveryFailed), 1);

        let after = managed
            .invoke("get_page", &managed.tools[0], &call_to("mcp__svc__get_page"))
            .await;
        assert!(after.content.contains("连接已断开"), "失败的恢复把连接留在断开态");
        let failure = after.mcp_failure.expect("短路失败也要留故障事实");
        assert!(!failure.result_unknown, "没有发出的请求没有未知可言");
        assert_eq!(connector.attempts(), 1, "短路调用不得顺手再发起恢复");
    }

    /// 没有连接源的连接拒绝恢复。
    #[tokio::test]
    async fn a_connection_without_a_connector_declines_recovery() {
        let fake = FakeClient::flaky(page_tools(), disconnected_fault());
        let (recorder, sink) = test_recorder();
        let managed = ManagedServer::discover(
            "svc",
            Box::new(SharedHandle(fake)),
            None,
            Some(recorder),
            test_timeouts(),
        )
        .await
        .expect("替身发现不该失败");
        invoke_until_disconnected(&managed).await;

        let error = managed
            .reconnect(None)
            .await
            .expect_err("没有连接源就没有恢复可言");
        assert_eq!(error.kind, McpFaultKind::RecoveryUnavailable);
        assert_eq!(
            count_lifecycle(&sink, McpLifecycleKind::Disconnected),
            1,
            "恢复请求不得复读断连事实"
        );
        assert_eq!(count_lifecycle(&sink, McpLifecycleKind::RecoveryFailed), 0);
    }

    /// 显式关闭是终点：关闭后的恢复请求被拒绝，连接源一次都不会被调用。
    #[tokio::test]
    async fn reconnect_is_refused_after_explicit_close() {
        let fake = FakeClient::with_tools(page_tools());
        let connector = ScriptConnector::new(vec![Ok(fake)]);
        let managed = ManagedServer::discover(
            "svc",
            Box::new(SharedHandle(FakeClient::with_tools(page_tools()))),
            Some(Arc::clone(&connector) as Arc<dyn Connector>),
            None,
            test_timeouts(),
        )
        .await
        .expect("替身发现不该失败");
        managed.close().await;

        let error = managed.reconnect(None).await.expect_err("关闭是主动决定，恢复不许推翻");
        assert_eq!(error.kind, McpFaultKind::Closed);
        assert_eq!(connector.attempts(), 0, "关闭后的连接一次都不该重连");
    }

    /// 重连后的工具清单只用于比对：清单变化后按旧定义拒绝调用，新实例
    /// 不会收到调用。注册、授权与审批的契约不能因恢复被绕过。
    #[tokio::test]
    async fn a_changed_tool_list_after_recovery_refuses_blind_calls() {
        let old = FakeClient::flaky(page_tools(), disconnected_fault());
        let fresh = FakeClient::with_tools(vec![rmcp_tool("search", "Searches the index")]);
        let (recorder, sink) = test_recorder();
        let connector = ScriptConnector::new(vec![Ok(Arc::clone(&fresh))]);
        let managed = ManagedServer::discover(
            "svc",
            Box::new(SharedHandle(old)),
            Some(Arc::clone(&connector) as Arc<dyn Connector>),
            Some(recorder),
            test_timeouts(),
        )
        .await
        .expect("替身发现不该失败");
        invoke_until_disconnected(&managed).await;

        assert_eq!(managed.reconnect(None).await, Ok(true));
        let recovered = sink
            .events()
            .iter()
            .find_map(|entry| match &entry.event {
                AuditEvent::McpLifecycle { fact, .. }
                    if fact.kind == McpLifecycleKind::Recovered =>
                {
                    Some(fact.clone())
                }
                _ => None,
            })
            .expect("恢复成功必须留下 Recovered 事实");
        assert!(recovered.tools_changed, "清单变化必须落账，不许悄悄吞下");

        let outcome = managed
            .invoke("get_page", &managed.tools[0], &call_to("mcp__svc__get_page"))
            .await;
        assert_eq!(outcome.status, OutcomeStatus::Refused, "旧定义拒调是拒绝，不是失败");
        assert!(!outcome.content.contains("结果未知"), "根本没发出请求，没有未知可言");
        assert_eq!(fresh.calls.lock().unwrap().len(), 0, "不许按旧定义盲调新实例");
    }

    /// 监视器路径：transport 死亡通知触发一条断开事实，之后的调用被短路，
    /// 断开计数不会因新调用重复。每条连接只记一条断连，由通知与状态机
    /// 共同保证。
    #[tokio::test]
    async fn the_watcher_reports_a_dead_connection_once_without_being_asked() {
        let fake = FakeClient::with_tools(page_tools());
        let (recorder, sink) = test_recorder();
        let dead = Arc::new(tokio::sync::Notify::new());
        let managed = ManagedServer::discover(
            "svc",
            Box::new(NotifyingHandle {
                inner: Arc::clone(&fake),
                dead: Arc::clone(&dead),
            }),
            None,
            Some(recorder),
            test_timeouts(),
        )
        .await
        .expect("替身发现不该失败");
        assert_eq!(count_lifecycle(&sink, McpLifecycleKind::Connected), 1);

        dead.notify_one();
        assert!(
            wait_until(
                "监视器断开事实",
                Duration::from_millis(800),
                || count_lifecycle(&sink, McpLifecycleKind::Disconnected) == 1
            )
            .await,
            "死亡通知必须在有界时间内变成一条断开事实"
        );

        let outcome = managed
            .invoke("get_page", &managed.tools[0], &call_to("mcp__svc__get_page"))
            .await;
        assert!(outcome.content.contains("--mcp-reconnect"), "短路失败要带恢复指引");
        assert!(outcome.execution.is_none(), "没发出的请求没有执行边界可交代");
        assert_eq!(
            count_lifecycle(&sink, McpLifecycleKind::Disconnected),
            1,
            "断开按连接记账，新调用不复读"
        );
        assert!(fake.calls.lock().unwrap().is_empty(), "短路调用不得触碰 Server");
    }
}

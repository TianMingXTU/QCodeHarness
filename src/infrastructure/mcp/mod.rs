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
    format!("{MCP_TOOL_PREFIX}{server}__{tool}")
}

/// 判断词是不是"读"类动词（整词匹配，避免 `delete` 里的 `le` 之类误伤）。
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

/// 判断词是不是"写"类动词。
///
/// 除 delete/update 这类一眼可辨的，变更类动词（replace/rename/move/
/// edit/append/import）也在列：它们叠加读动词出现时（如
/// `search_and_replace`）若被判只读，就是一条免审后门。
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

/// MCP 故障的领域分类：只描述执行层证实的事实。
///
/// rmcp 拿不到 Server 退出码，连接结束一律按断开记，不猜"进程崩溃"。
/// `detail` 是给人读的说明文本，不参与分类判定——类别只看 `kind`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpError {
    pub kind: crate::domain::mcp::McpFaultKind,
    /// 请求是否已经发出。发出后的超时/断连意味着远端结果未知。
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

    /// 附上给人读的说明。分类不变——文本只解释，不判定。
    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = detail.into();
        self
    }

    /// 附上故障发生时进程的真实保护状态。默认 false 是"没进过沙箱"的
    /// 诚实缺省；握手阶段进程已在 Job 内，漏报成 false 等于谎报。
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

/// 各阶段的超时预算。生产用常量，测试注入短时限——不碰常量本身。
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

/// 一个 MCP Server 的连接句柄。抽出来是为了测试能注入替身，
/// 不必真起子进程走完整协议。
///
/// 唯一所有者语义：实现持有 transport/service/Job 的全部生存期，
/// [`McpClientHandle::close`] 是唯一入口，负责等 rmcp 收尾后再释放
/// Job（顺序错了 KILL_ON_JOB_CLOSE 会抢先杀掉优雅关闭中的进程）。
pub(crate) trait McpClientHandle: Send + Sync {
    /// 这个 Server 的进程是否真的在宿主进程沙箱内。真实连接在握手前就把
    /// launcher 纳入 Job Object 时为 true，替身、非 Windows、纳入失败为 false
    /// ——边界证据照它写，既不谎报保护，也不因为"是外部进程"就一律写死无保护。
    fn process_isolation(&self) -> bool;

    /// 列出 Server 暴露的工具（原始名，不带 `mcp__` 前缀）。
    fn list_tools(&self) -> BoxFuture<'_, Result<Vec<RmcpTool>, McpError>>;

    /// 调用一个工具，返回拼好的文本结果。
    ///
    /// Server 报告的执行失败（`isError`）是业务错误：连接仍然可用，
    /// 用 [`McpFaultKind::ServerError`] 区分于传输故障。
    fn call_tool<'a>(
        &'a self,
        remote_name: &'a str,
        arguments: Value,
    ) -> BoxFuture<'a, Result<String, McpError>>;

    /// 进程是否已被观察到结束。只作粗判（缓存是否失效），权威判定靠
    /// [`McpClientHandle::closed`] 的通知或调用返回的故障分类。
    fn is_closed(&self) -> bool;

    /// 连接结束的通知；`None` 表示这个实现没有可观察的关闭通道。
    /// future 只持通知句柄，绝不引用 service/transport/Job。
    fn closed(&self) -> Option<BoxFuture<'static, ()>>;

    /// 结束连接并等待收尾（transport close 完成）后返回。
    /// 幂等；重复调用是幂等空操作。
    fn close(&mut self) -> BoxFuture<'_, ()>;
}

/// [`McpClientHandle`] 的真实实现：包住一条 rmcp 客户端连接。
///
/// 唯一所有者：service/transport/Job 全部由它持有并按序释放。
/// 不用 `Arc`——让最后一个引用决定释放时机，正是旧连接与 Job 句柄
/// 滞留的根源（`RunningService` drop 只发取消通知、不等收尾，随后
/// JobGuard 的 KILL_ON_JOB_CLOSE 会抢在 transport close 之前杀树）。
struct RealMcpClient {
    /// 连接生命周期：先 `close_with_timeout` 等 rmcp 走完 transport
    /// close（关 stdin、最多 3 秒等子进程），之后才轮到 Job 释放。
    service: RunningService<RoleClient, ()>,
    /// 进程沙箱句柄。声明在 service 之后：字段按声明序析构，Job 必须
    /// 等 transport 收尾后才释放——反过来 `KILL_ON_JOB_CLOSE` 会抢在
    /// 优雅关闭之前杀树。close 超时没等到收尾时，这里是最后的兜底。
    _job: Option<JobGuard>,
    /// 进程是否真的进了沙箱，供 [`McpClientHandle::process_isolation`] 如实上报。
    process_isolation: bool,
}

impl RealMcpClient {
    /// 建进程沙箱、启动 Server 进程、纳入沙箱、完成握手。
    ///
    /// 握手超时罩在 `serve` 外面而不是整个连接的循环里：取消 serve 的
    /// future 会顺带 drop transport——rmcp 的 ChildWithCleanup 在 drop 里
    /// kill 子进程，`_job` 也随返回路径释放并整树兜底，"启动后握手未成"
    /// 的清理因此不需要额外代码。超时预算由调用方给出。
    async fn connect(config: &config::McpServerConfig, handshake: Duration) -> Result<Self, McpError> {
        let mut command = tokio::process::Command::new(&config.command);
        command.args(&config.args);

        // 先建 Job 再启动进程，取向与 shell 工具同一条线（见 `run_with_policy`）：
        // Windows 上建不起来就拒绝连接——先把一个没人看管的外部进程拉起来、
        // 再宣布"它没保护"，等于用假的证据换一次可用性。只有平台本身没实现
        // 才降级照连。连接失败由 `connect_all` 记成警告并跳过该 Server，
        // 其余 Server 不受影响，与"MCP 工具不可用"是同一种降级。
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
        // serve 会消费 transport，之后再也拿不到 Child；纳入要的 PID 必须在此取。
        // 取不到（进程已退）则跳过纳入，回退 rmcp 原语义。
        let launcher_pid = transport.id();
        // 纳入必须赶在握手之前，这是整树保护成不成立的分界（见 `assign_pid`）：
        // `uvx`/`npx` 这类 launcher 会先把自己的 Server 子进程拉起来，而握手
        // 成功本身就证明那个子进程已经存在——那时再纳入，进 Job 的只有
        // launcher，报给证据的 active=true 就是假的。
        let (job, process_isolation) = protect_launcher(job, launcher_pid);
        match tokio::time::timeout(handshake, ().serve(transport)).await {
            Ok(Ok(service)) => Ok(Self {
                service,
                _job: job,
                process_isolation,
            }),
            // 握手失败/超时返回时 `_job` 随这里一起释放：KILL_ON_JOB_CLOSE
            // 把已纳入的成员（含其继承的子孙）整树收掉，不留孤儿进程。
            // 故障如实携带"当时是否进了 Job"。
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

/// 把 launcher PID 纳入已建好的 Job，交回"要不要继续持有这个 Job"与保护事实。
///
/// 单独成函数是为了所有权：纳入成功就把守卫交给 `RealMcpClient` 长期持有
/// （drop 时整树终止），失败或拿不到 PID 就当场释放一个成员都没有的 Job
/// 句柄。就地判断会让 `Option<JobGuard>` 的借用和移动打架。
///
/// 失败不阻断连接——进程已经在跑，此时唯一诚实的做法是降级并记
/// `active = false`，与 shell 工具的 assign 失败同一条口径。
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
            // 调用预算统一由 ManagedServer::invoke 罩住：一处超时、一处
            // 判据。这里再叠一个固定超时会造出两个"调用超时"口径，
            // 外层取消 future 时 rmcp 会尽力发 notifications/cancelled
            // ——那是协作式取消，远端可能继续，结果未知由调用点记录。
            let result = self.service.call_tool(params).await.map_err(classify_service_error)?;

            if result.is_error == Some(true) {
                // Server 报告执行失败：业务错误，连接仍然可用。正文照样
                // 返回，模型需要看到原因；类别 ServerError 让审计能把
                // "远端跑了但报错"与"根本没送到"分开。
                return Err(McpError::new(crate::domain::mcp::McpFaultKind::ServerError, false)
                    .with_detail(extract_text(&result)));
            }
            Ok(extract_text(&result))
        })
    }

    /// rmcp 的 `is_closed()` 只反映主动关闭：transport 自行 EOF 结束时
    /// 它仍是 false。粗判由此可能漏掉"已断开"，权威判定靠 `closed()`
    /// 的通知或调用返回的 TransportClosed 分类——两层取其一即可。
    fn is_closed(&self) -> bool {
        self.service.is_closed()
    }

    /// 连接结束通知：把 `RunningService` 交给后台任务 `waiting()`，
    /// 结果经 oneshot 转出。future 只持 Receiver（'static），不引用
    /// service/Peer/Job——旧连接不会被这个通知续命。
    fn closed(&self) -> Option<BoxFuture<'static, ()>> {
        // RunningService 被 move 进任务后本结构不再能调用 close；
        // 因此这条路径只用于"从连接建好起就交给通知、本体留在原处"
        // 的形态——这里选择克隆不了的 service 原地不动，改为后台任务
        // 只等 token……而 token 不暴露 cancelled()。
        //
        // 结论：rmcp 3.8.0 没有不消费 service 就能等关闭的 API。
        // closed() 在真实连接上返回 None，断连由调用故障
        // （TransportClosed 分类）与 is_closed() 粗判发现；替身实现
        // 提供真正的通知供生命周期测试使用。
        None
    }

    /// 等待 rmcp 收尾（transport close 完成）后再返回；Job 由本结构的
    /// 字段顺序在此之后释放——先 service 后 _job，KILL_ON_JOB_CLOSE
    /// 只收 rmcp 三秒宽限后仍未退出的残留。
    fn close(&mut self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            // 用 SDK 自带的有界关闭而不是外面罩 tokio::time::timeout：
            // 外部超时取消的是 `close()` 这个 future 本身，JoinHandle
            // 还留在 service 里，随 drop 一起消失——清理是否跑完无从确认。
            // `close_with_timeout` 在超时时会自己 take 掉句柄继续后台收尾
            // （Ok(None) = "没等到"而非"没在收"），5 秒覆盖 drain 5 秒 +
            // transport close 3 秒的常规组合的上界。结果不向外传播：
            // 这条路径的职责是"等，等到或超时为止"，Job 兜底残留进程。
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
        // 传输已关：连接结束。请求可能已发出，结果未知由上层结合
        // "是否在等响应"判断——这里保守按"未发出"记，调用方超时包装
        // 会覆盖为 true。
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

/// 远端工具描述 → 注册表规格。首次发现与重连后的再发现共用这一份转换。
///
/// 独立成函数是为了"审批契约与执行定义同源"：规格在 [`ManagedServer`] 里
/// 首次注册后就钉死，重连只刷新"当前清单"用于比对，两条路径必须产出
/// 逐字段一致的 [`ToolSpec`]，否则比对结果会漂移出假阳性/假阴性。
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

/// [`Connector`] 的真实实现：起子进程、纳入沙箱、有界握手。
///
/// 握手超时收在 [`RealMcpClient::connect`] 里面：超时取消的是 `serve`
/// 这一段，transport 随之 drop，rmcp 的 ChildWithCleanup 在 drop 里
/// kill 子进程，Job 句柄同时释放整树兜底——清理不需要任何额外代码，
/// 故障也能带上"当时是否进了 Job"的真实保护状态。
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

/// 发现并接好的一个 MCP Server：工具清单 + 调用通道 + 生命周期状态机。
///
/// 本体只是 [`ManagedServer`] 的外壳：规格与执行入口从同一棵状态树上取，
/// "先注册、再鉴权、再审批、后执行"因此作用在同一份定义上。
///
/// 可克隆：克隆出的外壳共享同一个状态机（内部是 `Arc`），注册表与
/// 收尾清单各持一份，关闭对两边同时生效。
#[derive(Clone)]
pub struct McpToolSource {
    managed: Arc<ManagedServer>,
}

impl std::fmt::Debug for McpToolSource {
    // 状态机在异步锁里，调试信息给名字与钉住的清单就够用。
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpToolSource")
            .field("server", &self.managed.server)
            .field("tools", &self.managed.tools)
            .finish_non_exhaustive()
    }
}

impl McpToolSource {
    /// 连接一个真实 Server：起进程、握手、发现工具。
    ///
    /// 超时分两段罩：`connect` 预算罩"启动进程+握手"，`discovery` 预算罩
    /// 发现阶段——rmcp 的 `tools/list` 请求没有内部超时，一个"握手正常但
    /// 对 tools/list 永不回应"的半死 Server 若只有整体超时，用户拿到的
    /// 错误分不清卡在哪一段；分段后错误信息直接指明排查方向。
    /// 故障以 [`McpError`] 返回，`connect_all` 在边界上转成警告文本。
    pub async fn connect(
        config: &config::McpServerConfig,
        recorder: Option<Arc<McpLifecycleRecorder>>,
    ) -> Result<Self, McpError> {
        Self::connect_with(config, Arc::new(TokioConnector { config: config.clone() }), recorder, McpTimeouts::production()).await
    }

    /// 连接骨架，连接源与超时预算都可注入。
    ///
    /// 独立成函数是为了让"握手段超时"与"发现段超时"两条真实路径都能被
    /// 注入的 Server 配置（测试夹具）走一遍，而不必先造出能注入的替身。
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

    /// 显式关闭这条连接：幂等，正常收尾用它，不靠进程退出当清理手段。
    ///
    /// 关闭顺序由状态机保证：先让 transport 优雅收尾，再释放进程组兜底；
    /// 关闭之后工具清单不再可用，任何调用都会被如实拒绝。
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

    /// 装配成注册表登记项。规格与执行入口在这里绑死。
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

/// 一个 MCP Server 就是一个工具来源：来源名取 Server 名。
///
/// 注册表按来源装配时不需要认识 MCP；MCP 的工具清单与执行入口已经由
/// [`McpToolSource::entries`] 绑好，这里只做接口对接。
impl crate::application::tool_registry::ToolSource for McpToolSource {
    fn name(&self) -> &str {
        self.server()
    }

    fn tools(&self) -> Vec<ToolEntry> {
        self.entries()
    }
}

/// MCP 工具的执行入口：把调用交给 Server 的状态机。
///
/// `registered` 是登记进注册表的那份规格：执行前状态机拿它与"当前清单"
/// 比对，工具消失或定义变了就按旧定义拒调——审批契约不随重连热更新。
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

/// 依次连接配置里的全部 Server。
///
/// 单点失败降级为警告（返回值里的第二个元素），不阻止其余 Server——
/// 一个挂掉的 MCP Server 不该让整个任务泡汤。顺序连接而非并发：
/// Server 数量是个位数，并发省的几秒不值得为此引入聚合复杂度。
/// `recorder` 给出时，连接级事实（成功/失败/断连/恢复/关闭）落进证据链；
/// 失败文本只在 `connect_all` 边界上由 [`McpError`] 转成 String，分类
/// 信息不丢在转换里。
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
    use crate::domain::tool::OutcomeStatus;
    use crate::domain::workspace::WorkspaceRoot;
    use crate::infrastructure::evidence::memory::MemoryEvidenceSink;
    use crate::infrastructure::tool::all_tools;
    use serde_json::json;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Mutex;

    /// 断言 MCP 边界的三个字段，而不是拿 `ExecutionBoundary::mcp` 造一个期望
    /// 值去比：那个构造函数本身就是被测语义的一部分，用它当尺子，"active
    /// 到底跟着哪个字段"写错了测试也会跟着一起错。
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
        /// 置入一次故障时，下一次调用先消费它（模拟"isError/单次超时
        /// 这类不弄死连接的故障"），之后的调用回到 `reply`。
        next_error: Mutex<Option<McpError>>,
        /// 记录收到的调用（原始名），断言转发用了不带前缀的名字，
        /// 也断言故障之后没有第二次到达 Server（不重放）。
        calls: Mutex<Vec<String>>,
        /// 模拟"子进程已纳入宿主沙箱"，驱动边界证据的 active 分支。
        isolated: bool,
        /// `close()` 之后为 true——`is_closed` 粗判的替身。
        closed: AtomicBool,
    }

    /// 半死 Server 的假连接：清单能给，但 `tools/list`… 对，就是清单
    /// 永不返回。实测模型：rmcp 的 `list_all_tools()` 对请求只发送一次
    /// 后干等，Server 不回就永久挂起——发现段超时罩的正是这种形态。
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

    /// 能列清单、但对调用永不返回的替身：状态机的调用段超时由它驱动。
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

    /// `Arc<FakeClient>` 装进唯一所有者的句柄。
    ///
    /// 调用记录留在 `Arc` 上可观察，句柄本体交给状态机独占——替身
    /// 因此与真实连接走完全一样的所有权形状，测试才测得到真语义。
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

    /// 测试用超时预算：短到不拖套件，长到不冤枉调度。
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

        /// 一次性故障：第一次调用返回 `error`，连接状态由调用它的
        /// 状态机按分类决定——isError 类故障之后连接必须还能用。
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
        // 与 `establish` 发现段同路径（`ManagedServer::discover`），用短
        // 时限：半死替身永不回应，错误必须在时限内回来（挂死即本测试
        // 失败），信息指明卡在发现阶段而不是握手或调用。
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
        // 替身没进任何 Job，边界必须如实记 active=false。
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
        // 调用失败的 MCP 尝试也要交代边界；此替身未纳入沙箱故 active=false。
        assert_mcp_boundary(&outcome, false);

        // 断连之后连接被判死：第二次调用根本不会到达 Server——没有自动
        // 重试，也没有"再试一次也许就成"的假动作，只有短路的如实回答。
        let second = entry.invoker.invoke(&call).await;
        assert!(second.content.contains("连接已断开"), "{second:?}");
        assert_eq!(client.calls.lock().unwrap().len(), 1, "死连接上不得再发起调用");
    }

    /// 子进程纳入宿主沙箱后，MCP 边界如实记 active=true——不再对 MCP 一律
    /// 写死"无保护"。这是 Unit A（MCP 进 Job Object）在证据层的兑现。
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
        // 验收项：MCP 工具带 `mcp__` 前缀，不覆盖内置同名工具。
        // 极端用例：某 Server 暴露了一个跟 `read_file` 同名的工具。
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
        // 不该挂死也不该 panic——降级路径靠的就是这个错误。分类必须
        // 是 Spawn（"没起来"），不能落到握手或协议类上去。
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
        // 真协议连通性：起仓库自带的演示 Server（cargo test 会先编译
        // examples，产物在固定位置），走完整的 stdio 子进程链路——
        // 握手、发现、调用、文本提取。这是被 #[ignore] 的 duplex 测试
        // 想覆盖而没覆盖成的成功路径。
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
        let (sources, warnings) = connect_all(&config, None).await;

        assert!(sources.is_empty());
        assert_eq!(warnings.len(), 2);
        assert!(warnings[0].contains("ghost") && warnings[0].contains("不可用"));
    }

    // ── 故障分类与连接状态（替身）──────────────────────────────────────

    /// `isError` 是业务错误：不许弄死连接，后续调用必须照常到达 Server。
    ///
    /// 这条守的是分类表里最容易越界的一格——把"远端跑了但报错"误判成
    /// 传输故障，会让一次参数写错毁掉整条连接上所有工具。
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

    /// 调用超时：记"结果未知"，不杀 Server，不自动重发。
    ///
    /// 超时后连接仍判可用是刻意的（本轮不做取消确认）：第二次调用依旧
    /// 走正常语义再次超时，而不是被短路成"连接已断开"——两种文案背后
    /// 是"超时 ≠ 连接死亡"这条不许混淆的分界。
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

    /// 每条测试一个独立临时目录：既防测试间污染，也让"计数文件"的初值
    /// 与"标记文件是否存在"这两个断言不受上一轮残留影响。
    fn fixture_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("qch-mcp-lc-{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("测试临时目录可创建");
        dir
    }

    fn fixture_config(dir: &Path, mode: &str) -> config::McpServerConfig {
        config::McpServerConfig {
            name: "lc".to_owned(),
            // `CARGO_BIN_EXE_…` 只发给集成测试目标，不发给 lib 的单元测试
            // （`cargo check --all-targets` 会当场报编译错）。与演示 Server
            // 的端到端测试同一手法：`cargo test` 会先构建 bin，产物路径
            // 是确定的。
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

    /// 真实进程预算：启动给足（本机杀软冷启动慢），调用给短（超时路径
    /// 的测试不许等 120 秒）。
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

    /// 有界等待一个磁盘/进程事实成立。必须 `await` 让出执行器：
    /// rmcp 的 kill-on-drop 是 `tokio::spawn` 出去的，current_thread
    /// 运行时里阻塞等待会让那个 kill 永远排不上。
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

    /// Windows 下按 PID 查活。判据是输出里的 PID 数字：tasklist 的
    /// "没有匹配进程"提示语随系统语言变化，数字不会。
    #[cfg(windows)]
    fn process_alive(pid: &str) -> bool {
        let output = std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH"])
            .output()
            .expect("tasklist 应可运行");
        String::from_utf8_lossy(&output.stdout)
            .split_whitespace()
            .any(|token| token == pid)
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

    /// 起进程后握手失败：分类到 Handshake 为止。子进程自己退出的，
    /// rmcp 拿不到退出码——"异常退出"的猜测一个字都不许进证据。
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

    /// 收帧但永不回 initialize 的 Server：握手段必须有界收口，
    /// 而且"启动后握手未成"的进程不能留成孤儿。
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
        // 超时取消 serve 会 drop transport：rmcp 的 ChildWithCleanup 在
        // drop 里 kill 子进程；Windows 上 Job 句柄同时释放，整树兜底。
        // 非 Windows 本机跑不到这条，标未验证而不是假设同样成立。
        #[cfg(windows)]
        {
            let pid = read_pid(&suffixed(&dir.join("counter.txt"), ".pid"));
            assert!(
                wait_until("子进程退出", Duration::from_secs(10), || !process_alive(&pid))
                    .await,
                "握手超时后子进程必须被清理，pid {pid} 仍存活"
            );
        }
    }

    /// 应答握手但第 1 次调用起保持沉默的 Server：调用超时记"结果未知"；
    /// 副作用恰好落盘一次（计数文件），超时与关闭都不会重放它。
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
        // 计数落盘发生在沉默之前：副作用真的执行过一次，
        // 而 Harness 永远不知道它"还在执行吗"——这就是"结果未知"的全部含义。
        assert_eq!(counter_lines(&counter), 1);

        source.managed().close().await;
        assert!(
            wait_until("EOF 标记", Duration::from_secs(5), || suffixed(&counter, ".eof").exists())
                .await,
            "优雅关闭链必须执行到子进程（stdin EOF），不留孤儿"
        );
        assert_eq!(counter_lines(&counter), 1, "关闭与超时都不重放原调用");
    }

    /// 提交副作用后自杀的 Server：断开记一次（按连接不按工具），
    /// 结果未知如实上报，第二次调用被短路而不是自动重试。
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

        // 同一连接上的再来一次：短路失败，不触碰 Server，也不新增断开事件。
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

    /// 显式恢复只重建通信：不重放原调用（计数不涨）、恢复后调用可用、
    /// 工具清单无变化时 `tools_changed` 为 false、代际推进到 2。
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

        // 恢复动作本身不许发送任何 tools/call：计数文件是它的测谎仪。
        let recovered = source
            .managed()
            .reconnect(Some(crate::domain::ids::CallId::new("lc-call")))
            .await
            .expect("恢复应成功");
        assert!(recovered, "断开后的恢复应报告重建了连接");
        assert_eq!(counter_lines(&counter), 1, "恢复动作不得触碰工具");

        // 新进程从全局计数 1 续接：这次调用是"新的第二次"，不是重放。
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

    /// 主动关闭：进程优雅退出、`Closed` 恰好一条、重复关闭幂等、
    /// 关闭之后的调用被如实拒绝。
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

    /// Windows Job Object：关闭连接整树终止——孙进程（不是 harness
    /// 的直接子进程）也随之而死。mock 测不到这条进程语义。
    #[cfg(windows)]
    #[tokio::test]
    async fn closing_a_job_protected_server_terminates_the_whole_process_tree() {
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
        // 这条测试只在"保护成立"的前提下才有意义：进了 Job 是前提，
        // 不是假设——前提不成立就明说，而不是让整树断言背锅。
        assert!(
            source.managed().process_isolation().await,
            "夹具进程应已纳入 Job（Windows 沙箱可用）"
        );
        let child_path = suffixed(&dir.join("counter.txt"), ".child");
        assert!(
            wait_until("孙进程 PID 文件", Duration::from_secs(5), || child_path.exists()).await
        );
        let grandchild = read_pid(&child_path);
        assert!(process_alive(&grandchild), "关闭前孙进程应还活着");

        source.managed().close().await;
        assert!(
            wait_until("孙进程退出", Duration::from_secs(10), || !process_alive(&grandchild))
                .await,
            "KILL_ON_JOB_CLOSE 必须把孙进程一并收掉，pid {grandchild} 仍存活说明整树终止失败"
        );
    }

    /// 非 Windows 平台不许声称进程保护——夹具连上后边界的 active 必须
    /// 如实为 false（本机 Windows 上则应为 true，见树杀测试的前提断言）。
    #[cfg(not(windows))]
    #[tokio::test]
    async fn non_windows_connections_never_claim_protection() {
        let dir = fixture_dir("no-claim");
        let config = fixture_config(&dir, "ok");
        let source = McpToolSource::connect_with(
            &config,
            Arc::new(TokioConnector { config: config.clone() }),
            None,
            subprocess_timeouts(),
        )
        .await
        .expect("夹具应能握手与发现");
        assert!(
            !source.managed().process_isolation().await,
            "没有进程沙箱的平台必须报无保护"
        );
        source.managed().close().await;
    }

    /// 脚本化连接源：状态机每次 `connect` 消费一个预置替身并计数。
    /// 它是"恢复有界一次 / 并发共享 / 失败不重试"这些承诺的测谎仪——
    /// `attempts` 直接暴露状态机到底敲了几次门。
    struct ScriptConnector {
        plan: Mutex<std::collections::VecDeque<Result<Arc<FakeClient>, McpError>>>,
        attempts: std::sync::atomic::AtomicUsize,
        /// 首连挂起时长：让并发请求真的能在锁上排队，而不是先后跑完。
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

    /// 一条自带断连通知的句柄：监视器路径（`closed()`）的替身。
    /// 真实连接靠 transport 死亡触发通知，这里由 `dead` 手工扳闸。
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

    /// 用一次带断连故障的真实调用，把连接驱动到 Disconnected——
    /// 走的是 invoke 的故障分支，与生产断连同一条代码路径。
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

    /// "连接断开"类故障的样张：驱动状态机走断连分支。
    fn disconnected_fault() -> McpError {
        McpError::new(McpFaultKind::Disconnected, false).with_detail("替身断连")
    }

    /// 断连后的一次恢复请求 = 恰好一次连接尝试；恢复后调用落在
    /// 新实例上；连接已可用时再请求恢复是不作为，不多起一个实例。
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

        // 连接活着时的恢复请求是不作为——不许为"再确认一下"多起实例。
        assert_eq!(managed.reconnect(None).await, Ok(false));
        assert_eq!(connector.attempts(), 1, "已就绪的连接不再触发连接尝试");
    }

    /// 并发的恢复请求共享同一次尝试的结果（成功形态）：排队者拿到
    /// 的是首个恢复的结论，连接源没有被敲第二次门。
    #[tokio::test]
    async fn concurrent_recovery_requests_share_one_attempt() {
        let old = FakeClient::flaky(page_tools(), disconnected_fault());
        let fresh = FakeClient::with_tools(page_tools());
        let (recorder, sink) = test_recorder();
        // 挂起 30ms：current_thread 运行时下让第二个请求必定在锁上排队。
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

    /// 失败的恢复也是有界的：一次请求一次尝试，事实落账，
    /// 并发排队者共享同一个失败结论；连接保持 Disconnected，
    /// 后续调用被短路拒绝而不是自动重试。
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

    /// 没有连接源的连接（`source_with` 生产之外的替身装配）如实拒绝恢复。
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

    /// 显式关闭是终点：关闭后的恢复请求被拒，连接源一次都没被敲。
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

    /// 重连后的工具清单只用于比对：清单变了就按旧定义拒调，
    /// 新实例一次调用都没收到——注册、授权与审批的契约不被恢复绕过。
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

    /// 监视器路径：transport 死亡通知触发一次断开事实，之后的调用
    /// 被短路，断开计数不因新的调用复读——"一个连接一条断连"由
    /// 通知与状态机共同保证，不靠每次调用重新发现。
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

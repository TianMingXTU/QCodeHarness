//! 应用层端口：定义与外部世界交互的抽象接口。
//!
//! 每个端口都是一个 trait，由基础设施层实现。应用层只依赖这些抽象，
//! 具体实现可以替换为测试替身或真实组件。
//!
//! `ModelGateway` 刻意保持薄——rig 的 `DynModel` 已经是合格网关，包一层即可，
//! 不重写传输与协议。但签名用领域类型而不是 rig 的 `CompletionRequest`：
//! 否则"怎么拼 chat_history、怎么读 choice"这些协议细节就会渗进应用层，
//! 防腐层也就名不副实了。

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use crate::domain::agent::{AgentSpec, AgentTurn};
use crate::domain::evidence::{AuditEvent, TokenUsage};
use crate::domain::session::ContextSlice;
use crate::domain::tool::{ToolAnnotations, ToolCall, ToolOutcome, ToolSpec};
use crate::application::error::ApplicationError;

/// 装盒的未来。
///
/// 为什么不用 `async fn` in trait：Rust 的 `async fn` trait 方法不是 dyn 兼容的，
/// 而端口必须以 `Arc<dyn ...>` 注入才能换成测试替身。也不为此引入 `async-trait`
/// 宏——手写装盒一样清楚，而且少一层宏展开，报错信息直白。
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// 时间戳。直接复用 `std` 的类型，领域层不为它再造一个。
pub type Timestamp = SystemTime;

/// 一次模型调用的输入。
#[derive(Debug, Clone)]
pub struct ModelRequest<'a> {
    /// 发给模型的历史：按上下文预算裁剪后的片段（可能含占位说明）。
    ///
    /// 只含裁剪后的历史，不携带完整 `&Session`（见 `domain::context`）：
    /// 证据、审计与恢复都依赖完整历史，而请求里只放"模型真正看到的
    /// 东西"，两者不能混为一谈。
    pub history: Vec<ContextSlice<'a>>,
    /// Agent 规格：系统提示词等静态配置。
    pub spec: &'a AgentSpec,
    /// 模型可见的工具规格，由工具注册表提供。
    ///
    /// 不放在 `AgentSpec` 上：规格与执行入口在注册表里绑成一体，
    /// 网关要发请求时从这里取，避免两处清单漂移。
    pub tools: &'a [ToolSpec],
    /// provider 路由提示缓存查找用的键，取会话 ID。
    ///
    /// OpenAI 文档明确说这个键"让同一会话的请求落到同一台机器"——
    /// 没有它，provider 按前缀哈希路由，同一会话的两轮可能落到不同
    /// 实例，前缀缓存一次都读不回来。所以它必须**跨轮次稳定**：
    /// 换键等于换一台机器，前面攒下的缓存全部作废。
    ///
    /// 它只是路由提示，不参与任何鉴权，也不该带上凭据。
    ///
    /// `None` 表示这次调用压根不参与前缀缓存——摘要、AI 审查这类
    /// 一次性调用每次输入都不同，给它们一个键只会让 provider 把互不
    /// 相干的请求往一处塞。注意"不带键"只能是 `None`：空字符串在 rig
    /// 里照样映射成 `prompt_cache_key: ""`，于是所有一次性请求共用一个
    /// 固定的空键（详见 `RigChatGateway::generation_options` 的注释）。
    pub cache_key: Option<&'a str>,
}

/// 模型的一次回复。
#[derive(Debug, Clone, PartialEq)]
pub struct ModelReply {
    /// 模型本轮输出的文本与调用意图。
    pub turn: AgentTurn,
    /// 本轮 token 用量，供证据与成本统计使用。
    pub usage: TokenUsage,
}

/// 模型网关：把会话交给模型，拿回领域层的一轮回复。
pub trait ModelGateway: Send + Sync {
    /// 调用模型。
    ///
    /// 请求的拼装（系统提示、历史、工具定义）由实现负责；
    /// 应用层只提交"现在有什么"。
    fn complete<'a>(
        &'a self,
        request: ModelRequest<'a>,
    ) -> BoxFuture<'a, Result<ModelReply, ApplicationError>>;
}

/// 模型提供方：模型网关的来源。
///
/// 与 [`ModelGateway`] 的分工：网关回答"这一次调用怎么发"，提供方回答
/// "网关从哪来"——服务地址、凭据、默认模型都是提供方的事。装配方按
/// [`id`](LlmProvider::id) 从
/// [`ProviderRegistry`](crate::application::provider_registry::ProviderRegistry)
/// 里选一家，核心循环与网关签名都不感知具体供应商。
pub trait LlmProvider: Send + Sync {
    /// 提供方标识，注册表的查找键，也是日志与报错里认人的名字。
    fn id(&self) -> &str;

    /// 该提供方对应的模型网关。
    ///
    /// 网关承载连接与凭据，构造一次后长期复用；按角色覆盖模型名走
    /// 请求上的 `model` 字段，不需要为每个角色另建网关。
    fn gateway(&self) -> Arc<dyn ModelGateway>;
}

/// 工具执行器：按调用意图真正做事。
///
/// 返回 [`ToolOutcome`] 而不是 `Result`：工具失败是正常业务结局，
/// 要作为观察回填给模型让它自行纠正，不该中断循环。
pub trait ToolInvoker: Send + Sync {
    /// 执行一次调用。
    fn invoke<'a>(&'a self, call: &'a ToolCall) -> BoxFuture<'a, ToolOutcome>;
}

/// 一次审批请求：某个工具即将以给定参数执行。
///
/// 可序列化：快照要记下"停在哪次审批上"，恢复后才能把同一请求重新递给闸门。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApprovalRequest {
    /// 当前轮次号。
    pub turn: u32,
    /// 待执行的调用。
    pub call: ToolCall,
    /// 被调工具的风险注解，审批方据此决策。
    pub annotations: ToolAnnotations,
    /// 策略之上追加的复核理由。
    ///
    /// `None` 是常规审批：策略说"要审"，闸门按自己的规则决定。
    /// `Some` 是升级复核——典型情形是策略放行、但调用被判定为疑似越界
    /// 写入（见 `domain::sandbox::write_escape_suspect`）。这种请求
    /// **每一次都要见到人**：会话信任（"同类调用不必再问"）对它无效，
    /// 否则升级就白做了。
    ///
    /// `#[serde(default)]` 保持旧版快照可反序列化。
    #[serde(default)]
    pub reason: Option<String>,
    /// 这是不是一次**恢复后的重新询问**。
    ///
    /// `true` 表示上一次已经问过、人没答复，恢复时必须再问一次：
    /// 不能凭快照放行，也不能被会话信任免掉——那等于把"人没点过"
    /// 当成"人点过了"。
    ///
    /// `#[serde(default)]` 保持旧版快照可反序列化（缺省即常规请求）。
    #[serde(default)]
    pub resumed: bool,
}

/// 审批结论：`Ok(())` 放行；`Err(reason)` 否决，原因会作为拒绝观察回填给模型。
///
/// 用 `Result` 而不是自定义枚举：两种结局里只有一种带数据，
/// 再造一个枚举只是多一层壳，还会让"否决"分支在生产构建里永远构造不到。
pub type Decision = Result<(), String>;

/// 审批闸门：副作用操作执行前的拦截点。
pub trait ApprovalGate: Send + Sync {
    /// 请求批准一次工具执行。
    fn request<'a>(&'a self, request: &'a ApprovalRequest) -> BoxFuture<'a, Decision>;
}

/// 运行挂点：模型调用与工具执行前后的观察者。
///
/// 与 [`ApprovalGate`] 的分工：闸门回答"这个调用能不能发生"，只有
/// `Allow` / `Err` 两种结局；挂点只观察、不改写、不否决，适合证据外发、
/// 结构化事件流、AI 审查旁路这类"看看发生了什么"的需求。需要拦截能力
/// 的是策略与闸门的事，不在这里。
///
/// 实现方必须把挂点当"旁路"对待：方法返回 `()`，没有错误通道——挂点
/// 自己的失败（外发超时、下游断连）由实现吞掉并自行降级，绝不能让
/// 一次遥测失败中断任务。所有方法都有空实现，实现方只挂关心的点。
pub trait RunHooks: Send + Sync {
    /// 一次模型调用即将发出。`request.history` 是裁剪后发给模型的历史。
    fn before_model_call<'a>(&'a self, _request: &'a ModelRequest<'a>) -> BoxFuture<'a, ()> {
        Box::pin(async {})
    }

    /// 模型已回复（含空回复重试后的最终版）。`retried` 表示这次回复
    /// 是空回复重试拿到的——统计空回复率时要用它。
    fn after_model_reply<'a>(&'a self, _reply: &'a ModelReply, _retried: bool) -> BoxFuture<'a, ()> {
        Box::pin(async {})
    }

    /// 一次工具调用即将执行（已过角色边界与策略裁决，尚未落执行前快照）。
    fn before_tool_call<'a>(&'a self, _call: &'a ToolCall) -> BoxFuture<'a, ()> {
        Box::pin(async {})
    }

    /// 一次工具调用已执行完毕，观察即将回填。
    fn after_tool_result<'a>(&'a self, _call: &'a ToolCall, _outcome: &'a ToolOutcome) -> BoxFuture<'a, ()> {
        Box::pin(async {})
    }
}

/// 无操作的挂点：装配方不关心运行事件时的默认实现。
pub struct NoopHooks;

impl RunHooks for NoopHooks {}

/// 证据汇：记录可审计事件。
///
/// 同步而非异步：证据要求数量少（每轮模型调用与每条工具调用各一条）、
/// 写入顺序与业务顺序严格一致；同步写让"证据已落盘"成为每步的天然前提，
/// 也避免异步引入乱序与丢尾。若要改成异步，只需改这一个 trait 和它的
/// 实现，循环不受影响。
pub trait EvidenceSink: Send + Sync {
    /// 记录一条事件。`at` 是事件发生时刻，由调用方用 [`Clock`] 取——
    /// 事件发生的时刻比落库的时刻更准确。
    fn record(&self, at: Timestamp, event: AuditEvent);
}

/// 时钟：取当前时间。
///
/// 单独一个端口是为了让测试能用固定时钟断言时序，而不是在测试里 `sleep`。
pub trait Clock: Send + Sync {
    /// 当前时间。
    fn now(&self) -> Timestamp;

    /// 距 `start` 经过的时间。
    ///
    /// 用墙钟差值实现而不是单独的单调时钟：多一个时刻字段就要改
    /// `now()` 的返回类型，牵动全部实现与序列化，而本场景是"一次运行
    /// 几分钟"的尺度，系统时钟回拨的风险可以接受。默认实现让已有实现
    /// 零改动接入。
    fn elapsed_since(&self, start: Timestamp) -> Duration {
        start.elapsed().unwrap_or(Duration::ZERO)
    }
}

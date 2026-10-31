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
use std::time::{Duration, SystemTime};

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
    /// P3 起不再携带整个 `&Session`：发给模型的历史可能被裁剪
    /// （见 `domain::context`），而证据、审计与将来的恢复都依赖完整会话。
    /// 请求里只放"模型真正看到的东西"，两者不能混为一谈。
    pub history: Vec<ContextSlice<'a>>,
    /// Agent 规格：系统提示词等静态配置。
    pub spec: &'a AgentSpec,
    /// 模型可见的工具规格，由工具注册表提供。
    ///
    /// P2 起从 `AgentSpec` 挪到这里：规格与执行入口在注册表里绑成一体，
    /// 网关要发请求时从这里取，避免两处清单漂移。
    pub tools: &'a [ToolSpec],
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

/// 工具执行器：按调用意图真正做事。
///
/// 返回 [`ToolOutcome`] 而不是 `Result`：工具失败是正常业务结局，
/// 要作为观察回填给模型让它自行纠正，不该中断循环。
pub trait ToolInvoker: Send + Sync {
    /// 执行一次调用。
    fn invoke<'a>(&'a self, call: &'a ToolCall) -> BoxFuture<'a, ToolOutcome>;
}

/// 一次审批请求：某个工具即将以给定参数执行。
#[derive(Debug, Clone, PartialEq)]
pub struct ApprovalRequest {
    /// 当前轮次号。
    pub turn: u32,
    /// 待执行的调用。
    pub call: ToolCall,
    /// 被调工具的风险注解，审批方据此决策。
    pub annotations: ToolAnnotations,
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

/// 证据汇：记录可审计事件。
///
/// 同步而非异步：P0 的内存实现只是 push，P2 换成 JSONL 落盘时
/// 若要改成异步，只需改这一个 trait 和它的实现，循环不受影响。
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
    /// P3 加入（整体墙钟预算需要量时长）。用墙钟差值实现而不是单独的
    /// 单调时钟：多一个时刻字段就要改 `now()` 的返回类型，牵动全部实现与
    /// 序列化，而本场景是"一次运行几分钟"的尺度，系统时钟回拨的风险
    /// 可以接受。默认实现让已有实现零改动接入。
    fn elapsed_since(&self, start: Timestamp) -> Duration {
        start.elapsed().unwrap_or(Duration::ZERO)
    }
}

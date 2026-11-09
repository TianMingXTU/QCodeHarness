//! 应用层端口：与外部世界交互的抽象接口。
//!
//! 每个端口是一个 trait，由基础设施层实现。应用层只依赖这些抽象，
//! 具体实现可以换成测试替身或真实组件。
//!
//! `ModelGateway` 保持很薄：rig 的 `DynModel` 已经是合格网关，包一层即可，
//! 不重写传输与协议。签名使用领域类型，不用 rig 的 `CompletionRequest`，
//! 否则"怎么拼 chat_history、怎么读 choice"会渗进应用层，防腐层失效。

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use crate::domain::agent::{AgentSpec, AgentTurn};
use crate::domain::evidence::{AuditEvent, TokenUsage};
use crate::domain::session::ContextSlice;
use crate::domain::tool::{ToolAnnotations, ToolCall, ToolOutcome, ToolSpec};
use crate::application::error::ApplicationError;

/// 装盒的未来。
///
/// 不用 `async fn` in trait：Rust 的 `async fn` trait 方法不兼容 dyn，而端口
/// 必须以 `Arc<dyn ...>` 注入才能换成测试替身。也不引入 `async-trait` 宏：
/// 手写装盒同样清楚，还少一层宏展开，报错信息更直白。
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// 协作式取消开关，运行在每轮调用模型前检查。
///
/// 放在端口层而不是 runner 里：闸门等基础设施也要用它表达"用户要求
/// 中止运行"，放在 runner 会让基础设施反向依赖应用层的具体类型。
///
/// 取消不会打断正在执行的工具（run_command 有自己的超时），从下一个安全
/// 点（下一轮模型调用前）生效。
#[derive(Debug, Clone, Default)]
pub struct CancelHandle {
    cancelled: Arc<AtomicBool>,
}

impl CancelHandle {
    /// 请求取消当前运行。
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }

    /// 是否已被请求取消。
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }
}

/// 时间戳，直接复用 `std` 的类型，领域层不再另造。
pub type Timestamp = SystemTime;

/// 一次模型调用的输入。
#[derive(Debug, Clone)]
pub struct ModelRequest<'a> {
    /// 发给模型的历史：按上下文预算裁剪后的片段，可能含占位说明。
    ///
    /// 不携带完整 `&Session`（见 `domain::context`）：证据、审计与恢复
    /// 依赖完整历史，请求里只放模型真正看到的内容。
    pub history: Vec<ContextSlice<'a>>,
    /// Agent 规格：系统提示词等静态配置。
    pub spec: &'a AgentSpec,
    /// 模型可见的工具规格，由工具注册表提供。
    ///
    /// 不放在 `AgentSpec` 上：规格与执行入口在注册表里绑定，网关从这里
    /// 取，避免两处清单漂移。
    pub tools: &'a [ToolSpec],
    /// provider 路由提示，供前缀缓存查找，取会话 ID。
    ///
    /// OpenAI 文档说明这个键让同一会话的请求落到同一台机器。没有它，
    /// provider 按前缀哈希路由，同一会话的两轮可能落到不同实例，前缀
    /// 缓存一次也命中不了。它必须**跨轮次稳定**：换键会让请求落到另一台
    /// 机器，先前攒下的缓存全部作废。
    ///
    /// 它只是路由提示，不参与鉴权，也不该带凭据。
    ///
    /// `None` 表示这次调用不参与前缀缓存。摘要、AI 审查这类一次性调用
    /// 每次输入都不同，给键只会让 provider 把互不相干的请求塞到一处。
    /// 不带键只能是 `None`：空字符串在 rig 里照样映射成
    /// `prompt_cache_key: ""`，所有一次性请求会共用一个固定的空键（见
    /// `RigChatGateway::generation_options` 的注释）。
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
    /// 请求拼装（系统提示、历史、工具定义）由实现负责，应用层只提交
    /// 当前状态。
    fn complete<'a>(
        &'a self,
        request: ModelRequest<'a>,
    ) -> BoxFuture<'a, Result<ModelReply, ApplicationError>>;
}

/// 模型提供方：模型网关的来源。
///
/// 与 [`ModelGateway`] 分工：网关负责"这一次调用怎么发"，提供方负责
/// "网关从哪来"，服务地址、凭据、默认模型都归提供方。装配方按
/// [`id`](LlmProvider::id) 从
/// [`ProviderRegistry`](crate::application::provider_registry::ProviderRegistry)
/// 选择，核心循环与网关签名不感知具体供应商。
pub trait LlmProvider: Send + Sync {
    /// 提供方标识，注册表的查找键与查重键；重复 `id` 的登记会被忽略。
    fn id(&self) -> &str;

    /// 该提供方对应的模型网关。
    ///
    /// 网关承载连接与凭据，构造一次后长期复用。按角色覆盖模型名走请求上
    /// 的 `model` 字段，不需要为每个角色另建网关。
    fn gateway(&self) -> Arc<dyn ModelGateway>;
}

/// 工具执行器：按调用意图执行。
///
/// 返回 [`ToolOutcome`]，不用 `Result`：工具失败是正常业务结局，需要作为
/// 观察回填给模型自行纠正，不应中断循环。
pub trait ToolInvoker: Send + Sync {
    /// 执行一次调用。
    fn invoke<'a>(&'a self, call: &'a ToolCall) -> BoxFuture<'a, ToolOutcome>;
}

/// 一次审批请求：某个工具即将以给定参数执行。
///
/// 可序列化：快照记录停在哪次审批上，恢复后才能把同一请求重新交给闸门。
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
    /// `None` 是常规审批：策略要求审批，闸门按自己的规则决定。
    /// `Some` 是升级复核，典型情形是策略放行、但调用被判定为疑似越界
    /// 写入（见 `domain::sandbox::write_escape_suspect`）。这类请求每次
    /// 都要见到人，会话信任（"同类调用不必再问"）对它无效，否则升级
    /// 失去意义。
    ///
    /// `#[serde(default)]` 保持旧版快照可反序列化。
    #[serde(default)]
    pub reason: Option<String>,
    /// 这是否是一次**恢复后的重新询问**。
    ///
    /// `true` 表示上次已经问过、人没答复，恢复时必须再问一次。不能凭
    /// 快照放行，也不能被会话信任免掉，否则会把"人没点过"当成
    /// "人点过了"。
    ///
    /// `#[serde(default)]` 保持旧版快照可反序列化，缺省即常规请求。
    #[serde(default)]
    pub resumed: bool,
}

/// 审批结论：`Ok(())` 放行；`Err(reason)` 否决，原因作为拒绝观察回填给模型。
///
/// 用 `Result`，不另造枚举：两种结局只有一种带数据，多一层壳没有收益。
pub type Decision = Result<(), String>;

/// 审批闸门：副作用操作执行前的拦截点。
pub trait ApprovalGate: Send + Sync {
    /// 请求批准一次工具执行。
    ///
    /// # 结局
    ///
    /// `Ok(())` 放行；`Err(reason)` 否决本次调用，原因回填给模型，
    /// 模型可以换一条路径继续。否决不等于终止运行。
    ///
    /// 用户要中止整个运行时，闸门不靠这个错误通道表达：它先记录中止
    /// 请求，再通过 [`CancelHandle`] 请求取消。取消是协作式的，在下一个
    /// 安全点生效。
    fn request<'a>(&'a self, request: &'a ApprovalRequest) -> BoxFuture<'a, Decision>;

    /// 绑定取消通道。
    ///
    /// runner 在构造时调用一次，把自己持有的 [`CancelHandle`] 交给闸门。
    /// 闸门需要"中止整个运行"这个能力时，通过它请求取消。
    ///
    /// 默认实现不做任何事：多数闸门（自动放行、AI 审查）没有中止需求，
    /// 让它们实现这个方法是徒增样板。
    fn bind_cancel(&self, _cancel: CancelHandle) {}
}

/// 运行挂点：模型调用与工具执行前后的观察者。
///
/// 与 [`ApprovalGate`] 分工：闸门决定调用能否发生，结局只有放行或拒绝；
/// 挂点只观察，不改写、不否决，适合证据外发、结构化事件流、AI 审查旁路。
/// 需要拦截能力时用策略与闸门。
///
/// 挂点是旁路：方法返回 `()`，没有错误通道。挂点自身的失败（外发超时、
/// 下游断连）由实现吞掉并降级，不能让一次遥测失败中断任务。所有方法
/// 都有空实现，实现方只挂关心的点。
pub trait RunHooks: Send + Sync {
    /// 一次模型调用即将发出。`request.history` 是裁剪后发给模型的历史。
    fn before_model_call<'a>(&'a self, _request: &'a ModelRequest<'a>) -> BoxFuture<'a, ()> {
        Box::pin(async {})
    }

    /// 模型已回复（含空回复重试后的最终版）。`retried` 表示这次回复来自
    /// 空回复重试，统计空回复率时使用。
    fn after_model_reply<'a>(&'a self, _reply: &'a ModelReply, _retried: bool) -> BoxFuture<'a, ()> {
        Box::pin(async {})
    }

    /// 一次工具调用即将执行（已过角色边界与策略裁决，执行前快照已落盘）。
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
/// 同步实现。证据数量少（每轮模型调用与每条工具调用各一条），写入顺序
/// 必须与业务顺序一致。同步写让"证据已落盘"成为每步的前提，也避免异步
/// 引入乱序与丢尾。改成异步只需改这个 trait 与实现，循环不受影响。
pub trait EvidenceSink: Send + Sync {
    /// 记录一条事件。`at` 是事件发生时刻，由调用方用 [`Clock`] 取，比落库
    /// 时刻更准确。
    fn record(&self, at: Timestamp, event: AuditEvent);
}

/// 时钟：取当前时间。
///
/// 单独一个端口，让测试能用固定时钟断言时序，不必 `sleep`。
pub trait Clock: Send + Sync {
    /// 当前时间。
    fn now(&self) -> Timestamp;

    /// 距 `start` 经过的时间。
    ///
    /// 用墙钟差值实现，不引入单调时钟：多一个时刻字段就要改 `now()` 的
    /// 返回类型，牵动全部实现与序列化。本场景是一次运行几分钟，系统时钟
    /// 回拨的风险可以接受。默认实现让已有实现零改动接入。
    fn elapsed_since(&self, start: Timestamp) -> Duration {
        start.elapsed().unwrap_or(Duration::ZERO)
    }
}

/// 动作超过时限。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Elapsed;

/// 给一次工具动作加时限的端口。
///
/// 计时器由运行时提供：应用层不直接依赖 tokio，测试可以换成不依赖运行时的
/// 实现。只用于工具动作，不用于审批：人思考多久由人决定，用超时包住交互
/// 审批只会给出"有时限"的假象。
pub trait TimeLimiter: Send + Sync {
    /// 给一次工具动作套上实现方持有的时限。
    ///
    /// 动作自身的结局原样透传；只有超过时限才返回 `Err(Elapsed)`。实现方
    /// 无法中断已经在同步阻塞的调用，时限对这类工具只是尽力而为。
    fn limit<'a>(
        &'a self,
        action: BoxFuture<'a, ToolOutcome>,
    ) -> BoxFuture<'a, Result<ToolOutcome, Elapsed>>;
}

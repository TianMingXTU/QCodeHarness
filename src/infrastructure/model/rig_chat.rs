//! Rig 客户端封装 + rig ↔ domain 类型翻译（防腐层）。
//!
//! 这一层是唯一允许出现 `use rig::` 的地方之一（除测试外）。
//! rig-core 0.45.0 把客户端重构成了 wire/transport/driver 三层，
//! `DynModel<Completion>` 是擦掉 wire 与 transport 之后的稳定入口：
//! 真实模型和 `MockCompletionModel` 擦除后是同一类型，所以
//! 生产和测试走的是同一条代码路径。

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use rig::completion::{
    AssistantContent, AssistantMessage, CompletionRequest, CompletionResponse, Message,
    ToolAnnotations as RigToolAnnotations, ToolDefinition, Usage,
};
use rig::completion::options::{CacheRetention, GenerationOptions, OnUnsupported};
use rig::driver::DynModel;
use rig::message::{CallId as RigCallId, ToolCall as RigToolCall, ToolFunction, ToolName, ToolResult, ToolResultContent};
use rig::operation::Completion;
use tokio::time::timeout;

use crate::application::error::ApplicationError;
use crate::application::port::{BoxFuture, ModelGateway, ModelReply, ModelRequest};
use crate::domain::agent::{AgentSpec, AgentTurn};
use crate::domain::evidence::TokenUsage;
use crate::domain::ids::CallId;
use crate::domain::session::ContextSlice;
use crate::domain::tool::{ToolAnnotations, ToolCall, ToolOutcome, ToolSpec};

/// 单次模型调用的默认时限。慢模型的完整输出实测可达 60 秒以上，
/// 取 300 秒兜底"连接挂死"而非"生成得慢"；预算的墙钟维度只在
/// 轮与轮之间生效，覆盖不到挂死的单次调用，所以这层必须在网关兜住。
const DEFAULT_MODEL_TIMEOUT: Duration = Duration::from_secs(300);

/// 模型网关：包住一个已擦除的 rig 模型。
#[derive(Clone, Debug)]
pub struct RigChatGateway {
    model: DynModel<Completion>,
    /// 单次调用的时限。reqwest 默认没有总超时：连接挂死时请求永远
    /// 不返回，整个任务卡在"等模型回复"。
    call_timeout: Duration,
    /// 是否已就"provider 不支持某些生成选项"警告过一次。
    ///
    /// `OnUnsupported::Ignore` 的降级是静默的——rig 只发一条
    /// `tracing::warn!`，默认不装 subscriber 就什么也看不见。用户会
    /// 以为缓存在生效，而实际上一个字段都没发出去。所以在第一次调用
    /// 前用 `check()` 主动查一次并明说。
    ///
    /// 只警告一次：一次运行有几十轮，每轮刷屏只会让人学会忽略它。
    /// 用 `AtomicBool` 而不是 `Mutex<bool>`：`complete` 拿的是 `&self`，
    /// 而这个标记不需要和任何东西一起加锁。
    warned_unsupported: Arc<AtomicBool>,
}

impl RigChatGateway {
    /// 用擦除后的模型构造网关，带默认调用时限。
    pub fn new(model: DynModel<Completion>) -> Self {
        Self {
            model,
            call_timeout: DEFAULT_MODEL_TIMEOUT,
            warned_unsupported: Arc::new(AtomicBool::new(false)),
        }
    }

    /// 自定义单次调用时限（测试与特殊网络环境用）。
    pub fn with_timeout(model: DynModel<Completion>, call_timeout: Duration) -> Self {
        Self {
            model,
            call_timeout,
            warned_unsupported: Arc::new(AtomicBool::new(false)),
        }
    }

    /// 第一次调用前查一遍 provider 支不支持我们设的生成选项。
    ///
    /// `check()` 是本地预检（不请求服务端），把 wire 与模型目录能判定
    /// 的拒绝项一次收齐。`OnUnsupported::Ignore` 下真实调用会静默丢掉
    /// 这些字段，所以必须在这里替用户问出来。
    ///
    /// 只在第一次调用：`check()` 每次都要本地编码一遍请求，几十轮下来
    /// 不是零成本，而结论不会变（同一会话的 provider 与模型不变）。
    fn warn_once_about_unsupported(&self, request: &CompletionRequest) {
        if self.warned_unsupported.swap(true, Ordering::Relaxed) {
            return;
        }
        // `Invalid` 不是"某个选项不被支持"而是"这个请求压根建不起来"，
        // 真正的调用会带着同样的错误失败，这里不用重复说一遍。
        if let Err(rig::completion::CheckError::Unsupported(refused)) = self.model.check(request) {
            let details = refused
                .iter()
                .map(|item| format!("{}（{}）", item.option, item.reason))
                .collect::<Vec<_>>()
                .join("; ");
            eprintln!(
                "警告：provider {} 不支持以下生成选项，本次运行已忽略它们: {details}",
                refused
                    .first()
                    .map(|item| item.provider.as_str())
                    .unwrap_or("未知")
            );
            eprintln!("      提示缓存与并行工具调用可能没有生效，提速幅度会小于预期。");
        }
    }
}

impl ModelGateway for RigChatGateway {
    fn complete<'a>(
        &'a self,
        request: ModelRequest<'a>,
    ) -> BoxFuture<'a, Result<ModelReply, ApplicationError>> {
        // 请求在这里就拼好（而不是进 async 块再拼）：预检要读它的字段，
        // 而 async 块里不能再借 self。拼装是纯函数，没有 IO，
        // 提前做不改变任何时序。
        let built = build_request(
            &request.history,
            request.spec,
            request.tools,
            request.cache_key,
        );
        let request = match built {
            Ok(request) => request,
            Err(error) => return Box::pin(async move { Err(error) }),
        };
        // 第一次调用前问一遍 provider 收不收我们设的选项。
        self.warn_once_about_unsupported(&request);
        // DynModel::call 内部克隆 Arc，返回 'static future，
        // 所以先取出模型再 move 进装箱的 future，不借用 self。
        let model = self.model.clone();
        let call_timeout = self.call_timeout;
        Box::pin(async move {
            // 双层 ?：外层是调用时限（ModelTimeout），内层是 provider 错误（Model）。
            let response = with_call_timeout(model.call(request), call_timeout).await??;
            Ok(ModelReply {
                turn: AgentTurn::from(&response),
                usage: TokenUsage::from(&response.usage),
            })
        })
    }
}

/// 给一次模型调用套上时限：超时返回 [`ApplicationError::ModelTimeout`]，
/// 不让挂死的连接把整个任务冻在"等模型回复"。
async fn with_call_timeout<F, T>(future: F, limit: Duration) -> Result<T, ApplicationError>
where
    F: Future<Output = T>,
{
    timeout(limit, future)
        .await
        .map_err(|_| ApplicationError::ModelTimeout {
            seconds: limit.as_secs(),
        })
}

/// 把裁剪后的历史片段与规格翻译成 rig 的请求。
///
/// 消息顺序必须是 `[System, ...历史, 最后一条]`：`new()` 收下最后一条作为
/// prompt，`messages()` 把其余历史插到它前面，`preamble()` 再插入系统提示。
///
/// 工具规格从 `ModelRequest.tools` 来（P2 起注册表是唯一清单），
/// 不再从 `AgentSpec` 取。
///
/// `cache_key` 是会话 ID，见 [`ModelRequest::cache_key`]。
fn build_request(
    history: &[ContextSlice<'_>],
    spec: &AgentSpec,
    tools: &[ToolSpec],
    cache_key: Option<&str>,
) -> Result<CompletionRequest, ApplicationError> {
    let mut messages = slices_to_messages(history);
    // Session::new 总会把用户任务放进历史，所以这里必然取得到。
    // 真取不到就报错而不是 panic：这个函数本来就返回 Result，
    // 没有理由让一条畸形历史把整个进程带走。
    let prompt = messages.pop().ok_or(ApplicationError::EmptyHistory)?;

    Ok(CompletionRequest::new(prompt)
        .messages(messages)
        .preamble(spec.system_prompt.clone())
        .tools(tools.iter().map(ToolDefinition::from))
        .options(generation_options(cache_key)))
}

/// 一次请求的生成选项：缓存保留期、路由键、并行工具调用、降级策略。
///
/// **不设 `cache_points`，这不是遗漏。** rig-core 0.45 的
/// `CompletionRequest.cache_points` 只有 Anthropic Messages 编码器会读
/// （`providers/anthropic/completion.rs` 按位置倒序取点、超额丢弃）；
/// 我们走的 OpenAI 兼容 Chat Completions 编码器
/// （`providers/openai/wire/chat.rs`）通篇不碰这个字段——`grep -rn
/// cache_points src/providers/openai/` 零匹配。给它赋值只会让读代码的
/// 人以为配了缓存断点，而 wire 上一个字节都不会多发。真正在 Chat
/// 路由上生效的是下面三个选项，它们由 `providers/openai/options.rs`
/// 映射成 body 顶层的 `prompt_cache_retention` / `prompt_cache_key` /
/// `parallel_tool_calls`。
///
/// 保留期取 `Short`：任务中途有人长时间不回复审批时缓存可能过期，
/// 长保留要多花钱，而 `Budget::max_wall_clock` 本来就是兜底。
/// `Short` 在传统路径映射为 `prompt_cache_retention: "in_memory"`，
/// 在走 `prompt_cache_options` 的模型上则不发字段（默认保留约 30 分钟）。
///
/// `parallel_tool_calls(true)` 是显式设置而不是依赖 provider 默认值：
/// 一次回复里给多个调用能省掉整轮往返，而"默认开还是默认关"各 provider
/// 说法不一，不该让吞吐取决于对端的默认配置。
///
/// `on_unsupported(Ignore)`：不是所有 provider 都认这些字段。选 `Error`
/// 会让整个 agent 在只支持部分选项的 provider 上直接不可用——那比没有
/// 缓存严重得多。代价是降级静默，所以网关在第一次调用前用 `check()`
/// 主动查一遍并把结果告诉用户（见 [`RigChatGateway::warn_once_about_unsupported`]）。
///
/// `None` 的 `cache_key` 是"这个请求压根不参与前缀缓存"（摘要、AI 审查
/// 这类一次性调用），此时**不发这个字段**。不能拿空字符串顶替：rig 只把
/// `None` 映射成 `Mapping::Nothing`，`Some("")` 会映射成
/// `Mapping::Send({"prompt_cache_key": ""})`（见 rig-core 的
/// `providers/openai/options.rs`），于是所有一次性请求共用一个固定的空键。
fn generation_options(cache_key: Option<&str>) -> GenerationOptions {
    let options = GenerationOptions::new()
        .cache(CacheRetention::Short)
        .parallel_tool_calls(true)
        .on_unsupported(OnUnsupported::Ignore);
    match cache_key {
        Some(key) => options.cache_key(key),
        None => options,
    }
}

/// 把历史片段翻译成 rig 的消息序列。
///
/// 注意 User/Agent/Observation 的映射与配对：一条 `Round` 帧产出助手消息
/// （含 tool_calls），紧跟的观察产出 tool 结果消息，
/// 两者靠调用 ID 关联——顺序错了 OpenAI 兼容端点会直接 400。
fn slices_to_messages(history: &[ContextSlice<'_>]) -> Vec<Message> {
    let mut messages = Vec::with_capacity(history.len() * 2);
    for slice in history {
        match slice {
            ContextSlice::User(text) => messages.push(Message::user(text.as_ref())),
            ContextSlice::Round {
                thought,
                calls,
                observations,
                ..
            } => {
                messages.push(assistant_message(thought, calls));
                if !observations.is_empty() {
                    messages.push(Message::tool_results(observations_to_results(
                        calls, observations,
                    )));
                }
            }
        }
    }
    messages
}

/// 拼装助手消息：文本块 + 工具调用块。
fn assistant_message(thought: &str, calls: &[ToolCall]) -> Message {
    let mut content = Vec::with_capacity(calls.len() + 1);
    // 空文本不能进 content：rig 把空 text 块判为空白内容，
    // 部分 provider 会拒收空 content 块的消息。
    if !thought.trim().is_empty() {
        content.push(AssistantContent::text(thought));
    }
    content.extend(calls.iter().map(assistant_tool_call));
    Message::Assistant(AssistantMessage::new(content))
}

/// 领域调用 → rig 的工具调用块。
fn assistant_tool_call(call: &ToolCall) -> AssistantContent {
    AssistantContent::ToolCall(RigToolCall::new(
        RigCallId::from_wire(call.id.as_str()),
        ToolFunction::new(tool_name(&call.name), call.arguments.clone()),
    ))
}

/// 领域观察 → rig 的 tool 结果。
///
/// 每条结果都从对应的调用取 ID 与工具名，而不是自己拼——rig 的文档明确
/// 要求"用它所回答的调用来构造"，这样 ID 和名字才不会和调用对不上。
fn observations_to_results(calls: &[ToolCall], observations: &[ToolOutcome]) -> Vec<ToolResult> {
    debug_assert_eq!(
        calls.len(),
        observations.len(),
        "commit_round 保证调用与观察一一对应"
    );
    calls
        .iter()
        .zip(observations)
        .map(|(call, outcome)| ToolResult {
            call: RigCallId::from_wire(call.id.as_str()),
            name: tool_name(&call.name),
            content: vec![ToolResultContent::text(outcome.content.clone())],
            is_error: outcome.is_error(),
        })
        .collect()
}

/// 工具名。领域层的调用只可能由模型响应翻译而来，那里已经是合法名字。
fn tool_name(name: &str) -> ToolName {
    ToolName::new(name).expect("工具名非空：领域 ToolCall 只能由模型响应翻译而来")
}

impl From<&CompletionResponse> for AgentTurn {
    fn from(response: &CompletionResponse) -> Self {
        Self {
            text: response.text(),
            calls: response.tool_calls().map(domain_tool_call).collect(),
        }
    }
}

/// rig 的工具调用 → 领域调用。
fn domain_tool_call(call: &RigToolCall) -> ToolCall {
    ToolCall {
        id: CallId::new(call.id.wire().into_owned()),
        name: call.function.name.as_str().to_owned(),
        // 模型给了非对象参数时，rig 把能读到的部分放进 arguments、原文放进
        // invalid_arguments。这里取前者：后者会让工具收到一段无法反序列化的
        // 文本，而前者至少能让参数校验给出"缺字段"这类可操作的错误，
        // 模型据此重试即可。
        arguments: call.function.arguments_value(),
    }
}

impl From<&Usage> for TokenUsage {
    fn from(usage: &Usage) -> Self {
        Self {
            input_tokens: usage.input_tokens.unwrap_or(0),
            output_tokens: usage.output_tokens.unwrap_or(0),
            // 缓存命中与写入的 token 必须单独平移，不能并进 input_tokens：
            // 并进去就看不出缓存有没有生效，而"缓存命中率"是 P9 判断
            // 提速有没有到手的唯一直接证据。provider 不报告时是 0 而不是
            // None——"没有缓存"与"没查到"在这里同义，都不影响合计。
            cached_input_tokens: usage.cached_input_tokens.unwrap_or(0),
            cache_creation_input_tokens: usage.cache_creation_input_tokens.unwrap_or(0),
            // rig 的诚实策略照搬：无法定价时是 None，报表显示"未知"，
            // 绝不折算成 0。
            cost_usd: usage.cost.map(|cost| cost.total),
        }
    }
}

impl From<&ToolSpec> for ToolDefinition {
    fn from(spec: &ToolSpec) -> Self {
        ToolDefinition::new(
            tool_name(&spec.name),
            spec.description.clone(),
            spec.parameters.clone(),
        )
        .with_annotations(rig_annotations(&spec.annotations))
    }
}

/// 领域注解 → rig 注解。
///
/// rig 的取值带协议默认值（未设即"非只读、破坏性、开放世界"），
/// 所以四个字段都要显式写进去，不能只设一半。
fn rig_annotations(annotations: &ToolAnnotations) -> RigToolAnnotations {
    RigToolAnnotations::default()
        .with_read_only(annotations.read_only)
        .with_destructive(annotations.destructive)
        .with_idempotent(annotations.idempotent)
        .with_open_world(annotations.open_world)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::agent::AgentRole;
    use crate::domain::ids::SessionId;
    use crate::domain::session::Session;
    use rig::completion::Cost;
    use serde_json::json;

    fn spec_with() -> AgentSpec {
        AgentSpec {
            role: AgentRole::Coder,
            system_prompt: "系统提示".to_owned(),
        }
    }

    fn read_file_spec() -> ToolSpec {
        ToolSpec {
            name: "read_file".to_owned(),
            description: "读文件".to_owned(),
            parameters: json!({"type": "object"}),
            annotations: ToolAnnotations::new(true, false, true, false),
        }
    }

    fn call(name: &str, arguments: serde_json::Value) -> ToolCall {
        ToolCall {
            id: CallId::new(format!("call_{name}")),
            name: name.to_owned(),
            arguments,
        }
    }

    #[test]
    fn session_translates_to_paired_messages() {
        let mut session = Session::new(SessionId::new("s1"), "读一下 Cargo.toml");
        session
            .commit_round(
                AgentTurn {
                    text: "我先看依赖".to_owned(),
                    calls: vec![call("read_file", json!({"path": "Cargo.toml"}))],
                },
                vec![ToolOutcome::success("[package]")],
            )
            .unwrap();

        let messages = slices_to_messages(&session.slices());

        assert_eq!(messages.len(), 3);
        assert!(matches!(&messages[0], Message::User { .. }));
        assert!(matches!(&messages[1], Message::Assistant(_)));
        assert!(matches!(&messages[2], Message::User { .. }));
    }

    #[test]
    fn failed_observation_is_marked_as_error() {
        let mut session = Session::new(SessionId::new("s1"), "任务");
        session
            .commit_round(
                AgentTurn {
                    text: String::new(),
                    calls: vec![call("read_file", json!({"path": "missing.txt"}))],
                },
                vec![ToolOutcome::failed("no such file")],
            )
            .unwrap();

        let messages = slices_to_messages(&session.slices());
        let Message::User { content } = &messages[2] else {
            panic!("观察结果应当是 user 消息");
        };
        let Some(rig::message::UserContent::ToolResult(result)) = content.first() else {
            panic!("应当是 tool 结果内容");
        };
        assert!(result.is_error);
        // 空思考不能产出空文本块，助手消息里只应有 tool_calls。
        let Message::Assistant(assistant) = &messages[1] else {
            panic!("应当是助手消息");
        };
        assert_eq!(assistant.content.len(), 1);
    }

    #[test]
    fn tool_spec_carries_annotations_into_rig() {
        let definition = ToolDefinition::from(&read_file_spec());
        assert!(definition.annotations.read_only());
        assert!(!definition.annotations.destructive());
        assert!(definition.annotations.idempotent());
        assert!(!definition.annotations.open_world());
    }

    #[test]
    fn usage_translates_token_counts_and_cost() {
        // Usage 是 non_exhaustive，只能用 Default 再改字段。
        let mut usage = Usage::default();
        usage.input_tokens = Some(11);
        usage.output_tokens = Some(7);
        // 0.25/0.5 在 f64 里精确，合计不会引入浮点误差。
        usage.cost = Some(Cost::from_parts(0.25, 0.5, 0.0, 0.0));
        assert_eq!(
            TokenUsage::from(&usage),
            TokenUsage {
                input_tokens: 11,
                output_tokens: 7,
                cached_input_tokens: 0,
                cache_creation_input_tokens: 0,
                cost_usd: Some(0.75),
            }
        );
        // provider 未报告成本 → None（报表显示"未知"，绝不折算成 0）。
        assert_eq!(TokenUsage::from(&Usage::default()).cost_usd, None);
    }

    /// 缓存计数器必须原样平移，不能并进 input_tokens。
    ///
    /// 并进去的后果不是精度损失，是**缓存有没有生效直接看不出来**——
    /// 而命中率是 P9 判断"省下的 prefill 时间有没有到手"的唯一直接证据。
    /// rig 的契约：`cached + cache_creation <= input`，即 input 已含这三类，
    /// 所以命中率的分母用 input、分子用 cached 是自洽的。
    #[test]
    fn usage_translates_cache_counters_separately() {
        let mut usage = Usage::default();
        usage.input_tokens = Some(10_000);
        usage.output_tokens = Some(200);
        usage.cached_input_tokens = Some(7_500);
        usage.cache_creation_input_tokens = Some(1_200);

        let translated = TokenUsage::from(&usage);

        assert_eq!(translated.cached_input_tokens, 7_500);
        assert_eq!(translated.cache_creation_input_tokens, 1_200);
        assert_eq!(
            translated.input_tokens, 10_000,
            "input 是 provider 报的原值，不减掉缓存部分"
        );
        // provider 不报告缓存计数时是 0：没有缓存与没查到在这里同义，
        // 都不该让合计变成 None。
        let bare = TokenUsage::from(&Usage::default());
        assert_eq!(bare.cached_input_tokens, 0);
        assert_eq!(bare.cache_creation_input_tokens, 0);
    }

    #[tokio::test]
    async fn hung_model_call_is_cut_off_at_the_timeout() {
        // 永不完成的 future 模拟挂死的连接：时限一到必须报错，
        // 而不是把任务永远冻在"等模型回复"。
        let error = with_call_timeout(std::future::pending::<CompletionResponse>(), Duration::from_millis(20))
            .await
            .unwrap_err();
        assert!(matches!(error, ApplicationError::ModelTimeout { seconds: 0 }), "{error}");
    }

    #[test]
    fn request_puts_system_prompt_first() {
        let session = Session::new(SessionId::new("s1"), "任务");
        let request = build_request(&session.slices(), &spec_with(), &[read_file_spec()], Some("s1"))
            .unwrap();

        assert_eq!(request.system_instructions(), Some("系统提示"));
        assert_eq!(request.chat_history.len(), 2);
        assert_eq!(request.tools.len(), 1);
        // 最后一条是 prompt，即用户任务。
        assert!(matches!(&*request.chat_history[1], Message::User { .. }));
    }

    /// 请求必须带上缓存保留期、路由键与并行工具调用开关。
    ///
    /// 这三项是 OpenAI 兼容 Chat 路由上**真正会发出**的缓存相关字段
    /// （映射成 body 顶层的 `prompt_cache_retention` /
    /// `prompt_cache_key` / `parallel_tool_calls`）。断言它们存在，
    /// 是为了将来有人"顺手清理未使用的 options"时测试会拦下来。
    #[test]
    fn request_carries_the_cache_and_parallel_options() {
        let session = Session::new(SessionId::new("session-42"), "任务");
        let request = build_request(
            &session.slices(),
            &spec_with(),
            &[read_file_spec()],
            Some("session-42"),
        )
        .unwrap();

        let options = &request.options;
        assert_eq!(options.cache, Some(CacheRetention::Short), "保留期取短档");
        assert_eq!(
            options.cache_key.as_deref(),
            Some("session-42"),
            "路由键用会话 ID，跨轮次必须稳定"
        );
        assert_eq!(
            options.parallel_tool_calls,
            Some(true),
            "并行工具调用显式开启，不依赖 provider 默认值"
        );
        assert_eq!(
            options.on_unsupported,
            Some(OnUnsupported::Ignore),
            "provider 不支持时降级而不是让 agent 不可用"
        );
    }

    /// 路由键跟着会话走：换一个会话就是另一个键。
    ///
    /// 这条守住的是"键必须稳定"的另一半——它还得**随会话变化**。
    /// 所有会话共用一个键会让 provider 把互不相干的前缀混在一起，
    /// 命中率与安全性双双落空。
    #[test]
    fn cache_key_follows_the_session() {
        let session = Session::new(SessionId::new("s1"), "任务");
        let first = build_request(&session.slices(), &spec_with(), &[], Some("session-a")).unwrap();
        let second = build_request(&session.slices(), &spec_with(), &[], Some("session-b")).unwrap();

        assert_eq!(first.options.cache_key.as_deref(), Some("session-a"));
        assert_eq!(second.options.cache_key.as_deref(), Some("session-b"));
    }

    /// 一次性调用（摘要、AI 审查）不带路由键：不设这个字段，
    /// 而不是设成一个空字符串。
    ///
    /// rig-core 0.45 只把 `None` 映射成 `Mapping::Nothing`；
    /// `Some("")` 照样映射成 `Mapping::Send({"prompt_cache_key": ""})`
    /// （见 `providers/openai/options.rs` 的 `prompt_cache_key`）。
    /// 用空字符串表达"不带键"时，所有摘要与审查请求就共用一个固定的
    /// 空键——注释说它们不参与前缀缓存，wire 上却句句都带着同一个键。
    #[test]
    fn a_one_off_call_sends_no_cache_key_at_all() {
        let session = Session::new(SessionId::new("s1"), "任务");
        let request = build_request(&session.slices(), &spec_with(), &[], None).unwrap();

        assert_eq!(
            request.options.cache_key, None,
            "不带键就是不设这个字段，空字符串会在 wire 上变成 prompt_cache_key: \"\""
        );
        // 只少这一个字段：保留期与并行开关与主循环一致，
        // 否则"一次性调用"会被做成"降级请求"。
        assert_eq!(request.options.cache, Some(CacheRetention::Short));
        assert_eq!(request.options.parallel_tool_calls, Some(true));
    }

    /// 不设 `cache_points`，这是查证过的决定而不是遗漏。
    ///
    /// rig-core 0.45 里只有 Anthropic Messages 编码器读这个字段；
    /// 我们走的 OpenAI 兼容 Chat 编码器通篇不碰它
    /// （`grep -rn cache_points src/providers/openai/` 零匹配）。
    /// 给它赋值只会让读代码的人以为配了缓存断点，而 wire 上一个字节
    /// 都不会多发——所以这里断言它就是空的，把决定钉在测试里。
    #[test]
    fn request_leaves_cache_points_empty_because_the_chat_wire_ignores_them() {
        let session = Session::new(SessionId::new("s1"), "任务");
        let request = build_request(&session.slices(), &spec_with(), &[], Some("s1")).unwrap();

        assert!(
            request.cache_points.is_empty(),
            "Chat 路由不读 cache_points，设了也是装饰: {:?}",
            request.cache_points
        );
    }
}

//! Rig 客户端封装与 rig ↔ domain 类型翻译（防腐层）。
//!
//! 本层是除测试外唯一允许出现 `use rig::` 的地方之一。rig-core 0.45.0
//! 将客户端重构为 wire/transport/driver 三层，`DynModel<Completion>` 是
//! 擦除 wire 与 transport 之后的稳定入口。真实模型与
//! `MockCompletionModel` 擦除后为同一类型，生产与测试因此走同一条代码
//! 路径。

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use rig::completion::{
    AssistantContent, AssistantMessage, CompletionRequest, CompletionResponse, Message,
    ToolAnnotations as RigToolAnnotations, ToolDefinition, Usage,
};
use rig::completion::options::{
    CacheRetention, Effort as RigEffort, GenerationOptions, OnUnsupported, Reasoning,
    Verbosity as RigVerbosity,
};
use rig::driver::DynModel;
use rig::message::{CallId as RigCallId, ToolCall as RigToolCall, ToolFunction, ToolName, ToolResult, ToolResultContent};
use rig::operation::Completion;
use tokio::time::timeout;

use crate::application::error::ApplicationError;
use crate::application::port::{BoxFuture, ModelGateway, ModelReply, ModelRequest};
use crate::domain::agent::{AgentSpec, AgentTurn, Effort, Verbosity};
use crate::domain::evidence::TokenUsage;
use crate::domain::ids::CallId;
use crate::domain::session::ContextSlice;
use crate::domain::tool::{ToolAnnotations, ToolCall, ToolOutcome, ToolSpec};

/// 单次模型调用的默认时限。
///
/// 慢模型的完整输出实测可超过 60 秒。300 秒用于兜底连接挂死，不用于
/// 限制生成速度。预算的墙钟维度只在轮与轮之间检查，覆盖不到挂死的
/// 单次调用，因此必须在网关层兜底。
const DEFAULT_MODEL_TIMEOUT: Duration = Duration::from_secs(300);

/// 模型网关：包住一个已擦除的 rig 模型。
#[derive(Clone, Debug)]
pub struct RigChatGateway {
    model: DynModel<Completion>,
    /// 单次调用时限。reqwest 默认没有总超时，连接挂死时请求不会返回，
    /// 任务会一直等待模型回复。
    call_timeout: Duration,
    /// 是否已就"provider 不支持某些生成选项"警告过。
    ///
    /// `OnUnsupported::Ignore` 的降级是静默的：rig 只发一条
    /// `tracing::warn!`，未安装 subscriber 时不可见。用户会以为缓存生效，
    /// 实际字段都没有发出。因此在第一次调用前用 `check()` 主动检查并告知。
    ///
    /// 只警告一次：一次运行有几十轮，每轮重复输出会被忽略。使用
    /// `AtomicBool`：`complete` 接收 `&self`，该标记无需与其他状态一起加锁。
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

    /// 第一次调用前检查 provider 是否支持已设置的生成选项。
    ///
    /// `check()` 是本地预检，不请求服务端，可一次性收集 wire 与模型目录
    /// 能判定的拒绝项。`OnUnsupported::Ignore` 下真实调用会静默丢弃这些
    /// 字段，因此必须在此明确告知用户。
    ///
    /// 仅第一次调用时检查：`check()` 每次都要在本地编码请求，几十轮下来
    /// 有成本。检查对象是首次请求的模型与生成选项；后续步骤若换了角色
    /// 模型，该模型的拒绝项不会再提示。
    fn warn_once_about_unsupported(&self, request: &CompletionRequest) {
        if self.warned_unsupported.swap(true, Ordering::Relaxed) {
            return;
        }
        // `Invalid` 表示请求无法构建，不属于单个选项不受支持的情况。真实
        // 调用会以同样的错误失败，这里无需重复提示。
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
        // 请求在此处构建，不放入 async 块：预检需要读取其字段，而 async
        // 块中无法再借用 self。构建是纯函数，无 IO，提前执行不改变时序。
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
        // DynModel::call 内部克隆 Arc 并返回 'static future。因此先取出
        // 模型，再 move 进装箱的 future，不借用 self。
        let model = self.model.clone();
        let call_timeout = self.call_timeout;
        Box::pin(async move {
            // 外层 ? 处理调用时限（ModelTimeout）；内层把 provider 错误转换
            // 为应用层的 Model 错误，避免 rig 类型泄漏到上层。
            let response = with_call_timeout(model.call(request), call_timeout)
                .await?
                .map_err(|error| ApplicationError::Model(error.to_string()))?;
            Ok(ModelReply {
                turn: AgentTurn::from(&response),
                usage: TokenUsage::from(&response.usage),
            })
        })
    }
}

/// 给一次模型调用设置时限。超时返回 [`ApplicationError::ModelTimeout`]，
/// 避免挂死的连接让任务一直等待模型回复。
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

/// 把裁剪后的历史片段与规格转换为 rig 请求。
///
/// 消息顺序必须为 `[System, ...历史, 最后一条]`。`new()` 接收最后一条
/// 作为 prompt，`messages()` 将其余历史插到它前面，`preamble()` 插入
/// 系统提示。
///
/// 工具规格来自 `ModelRequest.tools`。注册表是工具的唯一清单，不从
/// `AgentSpec` 获取。
///
/// `cache_key` 为会话 ID，见 [`ModelRequest::cache_key`]。
fn build_request(
    history: &[ContextSlice<'_>],
    spec: &AgentSpec,
    tools: &[ToolSpec],
    cache_key: Option<&str>,
) -> Result<CompletionRequest, ApplicationError> {
    let mut messages = slices_to_messages(history);
    // Session::new 总会把用户任务放入历史，因此这里应当能取到。取不到时
    // 返回错误而不 panic：函数本来返回 Result，畸形历史不应终止整个进程。
    let prompt = messages.pop().ok_or(ApplicationError::EmptyHistory)?;

    let mut request = CompletionRequest::new(prompt)
        .messages(messages)
        .preamble(spec.system_prompt.clone())
        .tools(tools.iter().map(ToolDefinition::from))
        .options(generation_options(cache_key, spec.effort, spec.verbosity));
    // 按请求覆盖模型名，`None` 时 provider 使用自己的默认模型。
    //
    // rig 的 OpenAI Chat 编码器使用
    // `request.model.clone().unwrap_or_else(|| self.model.clone())`。
    // 一个 `DynModel` 因此可以服务多个角色，模型名来自配置。
    if let Some(model) = &spec.model {
        request.model = Some(model.clone());
    }
    Ok(request)
}

/// 一次请求的生成选项：缓存保留期、路由键、并行工具调用、降级策略、
/// 推理档位与输出档位。
///
/// 不设置 `cache_points`。rig-core 0.45 中只有 Anthropic Messages 编码器
/// 读取该字段（`providers/anthropic/completion.rs` 按位置倒序取点，超额
/// 丢弃）；本项目使用的 OpenAI 兼容 Chat Completions 编码器
/// （`providers/openai/wire/chat.rs`）不读该字段，
/// `grep -rn cache_points src/providers/openai/` 无匹配。赋值只会让读者
/// 误以为配置了缓存断点，wire 上不会多发任何字节。Chat 路由实际生效的
/// 是以下三个选项，由 `providers/openai/options.rs` 映射为 body 顶层的
/// `prompt_cache_retention`、`prompt_cache_key` 与 `parallel_tool_calls`。
///
/// 保留期取 `Short`：审批停顿较久时缓存可能过期，长保留费用更高，且
/// `Budget::max_wall_clock` 已是兜底。`Short` 在传统路径映射为
/// `prompt_cache_retention: "in_memory"`；在走 `prompt_cache_options` 的
/// 模型上不发送该字段（默认保留约 30 分钟）。
///
/// `parallel_tool_calls(true)` 显式设置，不依赖 provider 默认值。一次
/// 回复返回多个调用可以省去整轮往返，而各 provider 的默认行为不一致，
/// 吞吐不应取决于对端配置。
///
/// `on_unsupported(Ignore)`：有些 provider 不支持这些字段。选择 `Error`
/// 会让 agent 在只支持部分选项的 provider 上不可用，后果比没有缓存严重。
/// 代价是降级静默，因此网关在第一次调用前用 `check()` 检查并告知用户
/// （见 [`RigChatGateway::warn_once_about_unsupported`]）。
///
/// `effort` 与 `verbosity` 使用同一降级策略。多数非 OpenAI provider 不
/// 支持 `verbosity`，不支持时丢弃该字段继续运行，避免整个 agent 不可用。
///
/// `cache_key` 为 `None` 表示请求不参与前缀缓存（摘要、AI 审查等一次性
/// 调用），此时不发送该字段。不能用空字符串代替：rig 只把 `None` 映射为
/// `Mapping::Nothing`，`Some("")` 会映射为
/// `Mapping::Send({"prompt_cache_key": ""})`（见 rig-core 的
/// `providers/openai/options.rs`），导致所有一次性请求共享固定的空键。
fn generation_options(
    cache_key: Option<&str>,
    effort: Option<Effort>,
    verbosity: Option<Verbosity>,
) -> GenerationOptions {
    let mut options = GenerationOptions::new()
        .cache(CacheRetention::Short)
        .parallel_tool_calls(true)
        .on_unsupported(OnUnsupported::Ignore);
    if let Some(effort) = effort {
        options = options.reasoning(Reasoning::Effort(rig_effort(effort)));
    }
    if let Some(verbosity) = verbosity {
        options = options.verbosity(rig_verbosity(verbosity));
    }
    match cache_key {
        Some(key) => options.cache_key(key),
        None => options,
    }
}

/// 领域档位 → rig 档位。
fn rig_effort(effort: Effort) -> RigEffort {
    match effort {
        Effort::Minimal => RigEffort::Minimal,
        Effort::Low => RigEffort::Low,
        Effort::Medium => RigEffort::Medium,
        Effort::High => RigEffort::High,
    }
}

/// 领域档位 → rig 档位。
fn rig_verbosity(verbosity: Verbosity) -> RigVerbosity {
    match verbosity {
        Verbosity::Low => RigVerbosity::Low,
        Verbosity::Medium => RigVerbosity::Medium,
        Verbosity::High => RigVerbosity::High,
    }
}

/// 把历史片段转换为 rig 消息序列。
///
/// 注意 User、Agent、Observation 的映射与配对：一条 `Round` 帧生成助手
/// 消息（含 tool_calls），紧随其后的观察生成 tool 结果消息，两者通过
/// 调用 ID 关联。顺序错误会使 OpenAI 兼容端点返回 400。
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
    // 空文本不能放入 content：rig 将空 text 块视为空白内容，部分 provider
    // 会拒收含空 content 块的消息。
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

/// 领域观察 → rig tool 结果。
///
/// 每条结果都从对应调用取 ID 与工具名。rig 文档要求用所回答的调用来
/// 构造结果，这样 ID 和名称不会与调用不一致。
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
        // 模型提供非对象参数时，rig 将可解析部分放入 arguments，原文放入
        // invalid_arguments。这里取前者：后者会让工具收到无法反序列化的
        // 文本，前者至少能让参数校验返回"缺字段"这类可操作的错误，供模型
        // 重试。
        arguments: call.function.arguments_value(),
    }
}

impl From<&Usage> for TokenUsage {
    fn from(usage: &Usage) -> Self {
        Self {
            input_tokens: usage.input_tokens.unwrap_or(0),
            output_tokens: usage.output_tokens.unwrap_or(0),
            // 缓存命中与写入的 token 必须单独保留，不能并入 input_tokens，
            // 否则无法判断缓存是否生效；缓存命中率是判断提速是否实现的
            // 唯一直接证据。provider 未报告时取 0：这里"没有缓存"与
            // "未查到"同义，都不影响合计。
            cached_input_tokens: usage.cached_input_tokens.unwrap_or(0),
            cache_creation_input_tokens: usage.cache_creation_input_tokens.unwrap_or(0),
            // 沿用 rig 的策略：无法定价时为 None，报表显示"未知"，
            // 不折算成 0。
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
/// rig 的取值带协议默认值（未设置即为非只读、破坏性、开放世界），
/// 因此四个字段都要显式写入。
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
            model: None,
            effort: None,
            verbosity: None,
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
        // provider 未报告成本时为 None，报表显示"未知"，不折算成 0。
        assert_eq!(TokenUsage::from(&Usage::default()).cost_usd, None);
    }

    /// 缓存计数器必须原样保留，不能并入 input_tokens。
    ///
    /// 并入后无法判断缓存是否生效；命中率是判断 prefill 时间是否节省的
    /// 唯一直接证据。rig 的契约是 `cached + cache_creation <= input`，
    /// input 已包含这两类，因此命中率以 input 为分母、cached 为分子。
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
        // provider 不报告缓存计数时取 0：没有缓存与未查到同义，都不应让
        // 合计变成 None。
        let bare = TokenUsage::from(&Usage::default());
        assert_eq!(bare.cached_input_tokens, 0);
        assert_eq!(bare.cache_creation_input_tokens, 0);
    }

    #[tokio::test]
    async fn hung_model_call_is_cut_off_at_the_timeout() {
        // 永不完成的 future 模拟挂死的连接。时限到达时必须报错，不能让
        // 任务一直等待模型回复。
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
    /// 这三项是 OpenAI 兼容 Chat 路由上实际发送的缓存相关字段，分别映射为
    /// body 顶层的 `prompt_cache_retention`、`prompt_cache_key` 与
    /// `parallel_tool_calls`。断言其存在，防止后续清理未使用选项时误删。
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

    /// 路由键随会话变化：换一个会话就换一个键。
    ///
    /// 键既要稳定，也要随会话变化。所有会话共用一个键会让 provider 把
    /// 互不相干的前缀混在一起，命中率与安全性都会下降。
    #[test]
    fn cache_key_follows_the_session() {
        let session = Session::new(SessionId::new("s1"), "任务");
        let first = build_request(&session.slices(), &spec_with(), &[], Some("session-a")).unwrap();
        let second = build_request(&session.slices(), &spec_with(), &[], Some("session-b")).unwrap();

        assert_eq!(first.options.cache_key.as_deref(), Some("session-a"));
        assert_eq!(second.options.cache_key.as_deref(), Some("session-b"));
    }

    /// 一次性调用（摘要、AI 审查）不设置路由键字段，不使用空字符串。
    ///
    /// rig-core 0.45 只把 `None` 映射为 `Mapping::Nothing`；
    /// `Some("")` 会映射为 `Mapping::Send({"prompt_cache_key": ""})`
    /// （见 `providers/openai/options.rs` 的 `prompt_cache_key`）。用空字符串
    /// 表示"不带键"时，所有摘要与审查请求会共用一个固定空键，wire 上每个
    /// 请求都携带该键。
    #[test]
    fn a_one_off_call_sends_no_cache_key_at_all() {
        let session = Session::new(SessionId::new("s1"), "任务");
        let request = build_request(&session.slices(), &spec_with(), &[], None).unwrap();

        assert_eq!(
            request.options.cache_key, None,
            "不带键就是不设这个字段，空字符串会在 wire 上变成 prompt_cache_key: \"\""
        );
        // 只缺这一个字段：保留期与并行开关与主循环一致，否则一次性调用
        // 会变成降级请求。
        assert_eq!(request.options.cache, Some(CacheRetention::Short));
        assert_eq!(request.options.parallel_tool_calls, Some(true));
    }

    /// 不设置 `cache_points`，这是查证后的决定。
    ///
    /// rig-core 0.45 中只有 Anthropic Messages 编码器读取该字段；项目使用的
    /// OpenAI 兼容 Chat 编码器不读它
    /// （`grep -rn cache_points src/providers/openai/` 无匹配）。赋值会让
    /// 读者误以为配置了缓存断点，wire 上不会多发任何字节。测试断言该字段
    /// 为空，固定这一决定。
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

    // ── 按角色分配模型与档位 ──────────────────────────────────────────

    /// 规格配置了模型名时，请求的 `model` 字段被覆盖。
    ///
    /// rig 的 OpenAI Chat 编码器使用
    /// `request.model.clone().unwrap_or_else(|| self.model.clone())`。
    /// 断言语义覆盖，确认"按角色换模型"确实落到请求上，不只是在配置里
    /// 保存了一个字段。
    #[test]
    fn the_spec_model_overrides_the_request_model() {
        let session = Session::new(SessionId::new("s1"), "任务");
        let mut spec = spec_with();
        spec.model = Some("qwen3-coder".to_owned());

        let request = build_request(&session.slices(), &spec, &[], Some("s1")).unwrap();

        assert_eq!(
            request.model.as_deref(),
            Some("qwen3-coder"),
            "规格里的模型名应覆盖到请求上"
        );
    }

    /// 未配置角色模型时行为保持不变（回归保护）。
    ///
    /// 请求不带 `model` 字段时，provider 使用自己配置的默认模型。按角色
    /// 选模型时容易出现"未配置也发送模型名"，这会让部署的默认模型失效。
    /// 断言未配置时 `request.model` 必须为 `None`。
    #[test]
    fn without_a_configured_model_the_request_carries_none() {
        let session = Session::new(SessionId::new("s1"), "任务");
        let spec = spec_with();
        assert_eq!(spec.model, None, "内置规格不带模型");

        let request = build_request(&session.slices(), &spec, &[], Some("s1")).unwrap();

        assert_eq!(
            request.model, None,
            "没配角色模型时不该发 model 字段，否则默认模型会被顶掉"
        );
    }

    /// 推理档位与输出档位写入请求的生成选项。
    ///
    /// 两者都是 provider 可能不支持的字段，因此同时断言降级策略为 `Ignore`。
    /// 档位选项引入后，降级策略是它们的兜底。
    #[test]
    fn the_spec_tiers_reach_the_generation_options() {
        let session = Session::new(SessionId::new("s1"), "任务");
        let mut spec = spec_with();
        spec.effort = Some(Effort::High);
        spec.verbosity = Some(Verbosity::Low);

        let request = build_request(&session.slices(), &spec, &[], Some("s1")).unwrap();

        assert_eq!(
            request.options.reasoning,
            Some(Reasoning::Effort(RigEffort::High)),
            "推理档位应落到 reasoning 选项上"
        );
        assert_eq!(
            request.options.verbosity,
            Some(RigVerbosity::Low),
            "输出档位应落到 verbosity 选项上"
        );
        assert_eq!(
            request.options.on_unsupported,
            Some(OnUnsupported::Ignore),
            "provider 不认档位时降级而不是让 agent 不可用"
        );
    }

    /// 未配置档位时，选项里不出现 reasoning 与 verbosity。
    ///
    /// 与模型名相同：未配置就不设置。填入 `None` 之外的默认值会让 provider
    /// 认为收到了明确的档位指令，覆盖自身的默认行为。
    #[test]
    fn without_configured_tiers_no_tier_options_are_sent() {
        let session = Session::new(SessionId::new("s1"), "任务");
        let request = build_request(&session.slices(), &spec_with(), &[], Some("s1")).unwrap();

        assert_eq!(request.options.reasoning, None, "没配推理档位就不设该字段");
        assert_eq!(request.options.verbosity, None, "没配输出档位就不设该字段");
    }
}

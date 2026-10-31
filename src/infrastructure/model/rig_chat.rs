//! Rig 客户端封装 + rig ↔ domain 类型翻译（防腐层）。
//!
//! 这一层是唯一允许出现 `use rig::` 的地方之一（除测试外）。
//! rig-core 0.45.0 把客户端重构成了 wire/transport/driver 三层，
//! `DynModel<Completion>` 是擦掉 wire 与 transport 之后的稳定入口：
//! 真实模型和 `MockCompletionModel` 擦除后是同一类型，所以
//! 生产和测试走的是同一条代码路径。

use rig::completion::{
    AssistantContent, AssistantMessage, CompletionRequest, CompletionResponse, Message,
    ToolAnnotations as RigToolAnnotations, ToolDefinition, Usage,
};
use rig::driver::DynModel;
use rig::message::{CallId as RigCallId, ToolCall as RigToolCall, ToolFunction, ToolName, ToolResult, ToolResultContent};
use rig::operation::Completion;

use crate::application::error::ApplicationError;
use crate::application::port::{BoxFuture, ModelGateway, ModelReply, ModelRequest};
use crate::domain::agent::{AgentSpec, AgentTurn};
use crate::domain::evidence::TokenUsage;
use crate::domain::ids::CallId;
use crate::domain::session::ContextSlice;
use crate::domain::tool::{ToolAnnotations, ToolCall, ToolOutcome, ToolSpec};

/// 模型网关：包住一个已擦除的 rig 模型。
#[derive(Clone, Debug)]
pub struct RigChatGateway {
    model: DynModel<Completion>,
}

impl RigChatGateway {
    /// 用擦除后的模型构造网关。
    pub fn new(model: DynModel<Completion>) -> Self {
        Self { model }
    }
}

impl ModelGateway for RigChatGateway {
    fn complete<'a>(
        &'a self,
        request: ModelRequest<'a>,
    ) -> BoxFuture<'a, Result<ModelReply, ApplicationError>> {
        // DynModel::call 内部克隆 Arc，返回 'static future，
        // 所以先取出模型再 move 进装箱的 future，不借用 self。
        let model = self.model.clone();
        Box::pin(async move {
            let request = build_request(&request.history, request.spec, request.tools)?;
            let response = model.call(request).await?;
            Ok(ModelReply {
                turn: AgentTurn::from(&response),
                usage: TokenUsage::from(&response.usage),
            })
        })
    }
}

/// 把裁剪后的历史片段与规格翻译成 rig 的请求。
///
/// 消息顺序必须是 `[System, ...历史, 最后一条]`：`new()` 收下最后一条作为
/// prompt，`messages()` 把其余历史插到它前面，`preamble()` 再插入系统提示。
///
/// 工具规格从 `ModelRequest.tools` 来（P2 起注册表是唯一清单），
/// 不再从 `AgentSpec` 取。
fn build_request(
    history: &[ContextSlice<'_>],
    spec: &AgentSpec,
    tools: &[ToolSpec],
) -> Result<CompletionRequest, ApplicationError> {
    let mut messages = slices_to_messages(history);
    // Session::new 总会把用户任务放进历史，所以这里必然取得到。
    // 真取不到就报错而不是 panic：这个函数本来就返回 Result，
    // 没有理由让一条畸形历史把整个进程带走。
    let prompt = messages.pop().ok_or(ApplicationError::EmptyHistory)?;

    Ok(CompletionRequest::new(prompt)
        .messages(messages)
        .preamble(spec.system_prompt.clone())
        .tools(tools.iter().map(ToolDefinition::from)))
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
    fn usage_translates_token_counts() {
        // Usage 是 non_exhaustive，只能用 Default 再改字段。
        let mut usage = Usage::default();
        usage.input_tokens = Some(11);
        usage.output_tokens = Some(7);
        assert_eq!(
            TokenUsage::from(&usage),
            TokenUsage {
                input_tokens: 11,
                output_tokens: 7
            }
        );
    }

    #[test]
    fn request_puts_system_prompt_first() {
        let session = Session::new(SessionId::new("s1"), "任务");
        let request = build_request(&session.slices(), &spec_with(), &[read_file_spec()])
            .unwrap();

        assert_eq!(request.system_instructions(), Some("系统提示"));
        assert_eq!(request.chat_history.len(), 2);
        assert_eq!(request.tools.len(), 1);
        // 最后一条是 prompt，即用户任务。
        assert!(matches!(&*request.chat_history[1], Message::User { .. }));
    }
}

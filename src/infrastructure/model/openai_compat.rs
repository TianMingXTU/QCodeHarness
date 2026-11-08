//! OpenAI 兼容提供方：内置的默认 [`LlmProvider`] 实现。
//!
//! 只负责装配，将连接配置转换为 [`RigChatGateway`]。请求构造与超时处理
//! 由网关负责，替换本层不改变线上传输内容。

use std::sync::Arc;

use rig::driver::DynModel;
use rig::operation::Completion;
use rig::providers::openai::OpenAIConfig;

use crate::application::port::{LlmProvider, ModelGateway};
use crate::infrastructure::model::rig_chat::RigChatGateway;

/// 内置提供方：OpenAI 兼容的 `/chat/completions` 端点。
pub struct OpenAiCompatProvider {
    gateway: Arc<dyn ModelGateway>,
}

impl OpenAiCompatProvider {
    /// 用连接参数构造。
    ///
    /// 必须使用 `chat()`（`/chat/completions`）。`completion()` 在 OpenAI
    /// 方言下默认指向 Responses API（`/responses`），DeepSeek 等兼容网关
    /// 不支持。
    pub fn new(base_url: &str, api_key: &str, model: &str) -> Self {
        let client = OpenAIConfig::new(api_key).with_base_url(base_url).client();
        Self::with_model(client.chat(model).erase())
    }

    /// 用已擦除的 rig 模型构造。
    ///
    /// 供自带客户端的装配方与测试使用。网关行为与 [`Self::new`] 相同，
    /// 仅模型来源不同。
    pub fn with_model(model: DynModel<Completion>) -> Self {
        Self {
            gateway: Arc::new(RigChatGateway::new(model)),
        }
    }
}

impl LlmProvider for OpenAiCompatProvider {
    fn id(&self) -> &str {
        "openai-compat"
    }

    fn gateway(&self) -> Arc<dyn ModelGateway> {
        Arc::clone(&self.gateway)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::port::ModelRequest;
    use crate::application::provider_registry::ProviderRegistry;
    use crate::domain::agent::{AgentRole, AgentSpec};
    use crate::domain::ids::SessionId;
    use crate::domain::session::Session;
    use crate::domain::tool::{ToolAnnotations, ToolSpec};
    use rig::test_utils::{MockCompletionModel, MockTurn};
    use serde_json::json;

    fn spec_without_model() -> AgentSpec {
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

    /// 注册表取出的默认网关与直连网关记录到的请求逐字段一致。
    ///
    /// 锁定"接入方式可替换、wire 行为不变"：请求拼装只有
    /// [`RigChatGateway`] 一条路径。提供方与注册表只负责把同一个网关交给
    /// 装配方。
    #[tokio::test]
    async fn registry_gateway_sends_the_same_request_as_a_direct_gateway() {
        let session = Session::new(SessionId::new("s1"), "任务");
        let spec = spec_without_model();
        let tools = [read_file_spec()];

        let via_provider_mock = MockCompletionModel::from_turns(vec![MockTurn::text("好")]);
        let direct_mock = MockCompletionModel::from_turns(vec![MockTurn::text("好")]);

        let mut registry = ProviderRegistry::new();
        assert!(registry.register(Arc::new(OpenAiCompatProvider::with_model(
            via_provider_mock.clone().erase()
        ))));
        registry
            .default_provider()
            .expect("已登记一家")
            .gateway()
            .complete(ModelRequest {
                history: session.slices(),
                spec: &spec,
                tools: &tools,
                cache_key: Some("s1"),
            })
            .await
            .unwrap();

        RigChatGateway::new(direct_mock.clone().erase())
            .complete(ModelRequest {
                history: session.slices(),
                spec: &spec,
                tools: &tools,
                cache_key: Some("s1"),
            })
            .await
            .unwrap();

        let via_provider = &via_provider_mock.requests()[0];
        let direct = &direct_mock.requests()[0];
        // mock 会把自己的模型名填入 `request.model`，因此这里比较两条路径
        // 记录到的请求是否一致，而非字面值 `None`。"未配角色模型时
        // `build_request` 产生的 `model` 为 `None`"由 rig_chat 的
        // `without_a_configured_model_the_request_carries_none` 直接验证
        // 构造侧，不经过 mock。
        assert_eq!(
            via_provider.model, direct.model,
            "注册表路径与直连路径对 model 字段的处理必须一致"
        );
        assert_eq!(via_provider.options.cache, direct.options.cache);
        assert_eq!(via_provider.options.cache_key, direct.options.cache_key);
        assert_eq!(
            via_provider.options.parallel_tool_calls,
            direct.options.parallel_tool_calls
        );
        assert_eq!(
            via_provider.options.on_unsupported,
            direct.options.on_unsupported
        );
        assert_eq!(
            via_provider.system_instructions(),
            direct.system_instructions()
        );
        assert_eq!(via_provider.chat_history.len(), direct.chat_history.len());
        assert_eq!(via_provider.tools.len(), direct.tools.len());
    }

    /// 内置提供方标识是注册表查找键，修改名称即修改契约。
    #[test]
    fn the_builtin_provider_is_identified_as_openai_compat() {
        let provider = OpenAiCompatProvider::with_model(
            MockCompletionModel::from_turns(vec![MockTurn::text("好")]).erase(),
        );
        assert_eq!(provider.id(), "openai-compat");
    }
}

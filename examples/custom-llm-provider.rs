//! 自定义 LLM 提供方：实现 `LlmProvider`，把自家的模型服务接进 Harness。
//!
//! 运行：`cargo run --example custom-llm-provider`
//!
//! 提供方只回答"网关从哪来"——服务地址、凭据、默认模型归它管；一次调用
//! 怎么拼请求、怎么读回复仍归 `ModelGateway`。这里用一个固定回复的网关
//! 演示装配，不访问网络；接真实服务时把 `EchoGateway` 换成你的客户端实现
//! （内置的 rig 网关见 `infrastructure/model/rig_chat.rs`）。

use std::sync::Arc;

use qcode_harness::application::error::ApplicationError;
use qcode_harness::application::port::{
    BoxFuture, LlmProvider, ModelGateway, ModelReply, ModelRequest,
};
use qcode_harness::application::provider_registry::ProviderRegistry;
use qcode_harness::domain::agent::{AgentRole, AgentSpec, AgentTurn};
use qcode_harness::domain::evidence::TokenUsage;
use qcode_harness::domain::ids::SessionId;
use qcode_harness::domain::session::Session;

/// 固定回复的网关：演示用，不访问网络。
struct EchoGateway {
    reply: String,
}

impl ModelGateway for EchoGateway {
    fn complete<'a>(
        &'a self,
        _request: ModelRequest<'a>,
    ) -> BoxFuture<'a, Result<ModelReply, ApplicationError>> {
        Box::pin(async move {
            Ok(ModelReply {
                turn: AgentTurn {
                    text: self.reply.clone(),
                    calls: Vec::new(),
                },
                usage: TokenUsage::default(),
            })
        })
    }
}

/// 自定义提供方：声明标识，交出一份构造好的网关。
struct MyProvider {
    gateway: Arc<dyn ModelGateway>,
}

impl LlmProvider for MyProvider {
    fn id(&self) -> &str {
        "my-llm"
    }

    fn gateway(&self) -> Arc<dyn ModelGateway> {
        Arc::clone(&self.gateway)
    }
}

#[tokio::main]
async fn main() -> Result<(), ApplicationError> {
    let mut registry = ProviderRegistry::new();
    let registered = registry.register(Arc::new(MyProvider {
        gateway: Arc::new(EchoGateway {
            reply: "来自自定义提供方的回复".to_owned(),
        }),
    }));
    assert!(registered, "首次登记同一个 id 应成功");

    // 首个登记的提供方是默认值；也可以按 id 精确取。
    let provider = registry
        .get("my-llm")
        .or_else(|| registry.default_provider())
        .expect("注册表非空");
    println!("使用提供方: {}", provider.id());

    let spec = AgentSpec::new(AgentRole::Coder, "你是编码助手。");
    let session = Session::new(SessionId::new("example-session"), "演示任务");
    let reply = provider
        .gateway()
        .complete(ModelRequest {
            history: session.slices(),
            spec: &spec,
            tools: &[],
            cache_key: Some(session.id().as_str()),
        })
        .await?;

    println!("模型回复: {}", reply.turn.text);
    Ok(())
}

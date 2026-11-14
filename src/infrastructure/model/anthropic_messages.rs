//! Anthropic Messages 提供方：复用 rig 的官方 Messages 编码器。
//!
//! API key 模式由 wire 固定发送 `x-api-key` 与 `anthropic-version`；订阅登录
//! 模式在传输边界再叠加 `authorization: Bearer …`，不改请求体编码。注入
//! 路径用于协议测试，只接受本层公开的 rig 客户端句柄，未知传输显式报错。

use std::any::Any;
use std::sync::Arc;

use rig::driver::{DynModel, Model};
use rig::http_client::{
    DynHttpClient, HeaderMap, HeaderValue, HttpMiddleware, Result as HttpResult,
};
use rig::operation::Completion;
use rig::providers::anthropic::AnthropicConfig;
use rig::wasm_compat::WasmBoxedFuture;

use crate::application::port::{LlmProvider, ModelGateway};
use crate::application::provider_auth::{HttpTransport, ProviderCredentials};
use crate::infrastructure::model::rig_chat::RigChatGateway;

/// Messages 协议要求显式 `max_tokens`；固定值使协议形状可测试、可复现。
pub const DEFAULT_MAX_TOKENS: u64 = 1024;

/// 把 rig 的 HTTP 客户端装进 application 的类型擦除端口。
///
/// 装配层不直接依赖具体传输类型：只通过 `Box<dyn HttpTransport>` 传递，
/// 由本模块在构造网关时下转型回 rig 客户端。
#[derive(Clone, Debug)]
pub struct RigHttpTransport(pub DynHttpClient);

impl HttpTransport for RigHttpTransport {
    fn as_any(&self) -> &dyn Any {
        self
    }
}

impl HttpTransport for DynHttpClient {
    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// 订阅登录的 Bearer 头。
///
/// rig 的 Messages wire 始终要求 `x-api-key`，订阅服务还识别
/// `authorization`。放在 `DynHttpClient` 的 middleware 栈里，注入传输与
/// 生产传输都按同一规则加头，不需要复制编码器。
struct BearerMiddleware {
    authorization: String,
}

impl HttpMiddleware for BearerMiddleware {
    fn before_request_headers<'a>(
        &'a self,
        _exchange: rig::http_client::Exchange<'a>,
        headers: &'a mut HeaderMap,
    ) -> WasmBoxedFuture<'a, HttpResult<()>> {
        Box::pin(async move {
            let value = HeaderValue::from_str(&self.authorization)?;
            headers.insert("authorization", value);
            Ok(())
        })
    }
}

/// Anthropic Messages 提供方。
pub struct AnthropicMessagesProvider {
    gateway: Arc<dyn ModelGateway>,
}

impl AnthropicMessagesProvider {
    /// 用任意 rig HTTP 客户端构造。生产装配用共享 reqwest 客户端；协议
    /// 测试可传请求记录器。
    pub fn with_client(
        credentials: &ProviderCredentials,
        model: &str,
        client: DynHttpClient,
    ) -> Self {
        let transport = match credentials.bearer_header() {
            Some(authorization) => client.with_middleware(BearerMiddleware { authorization }),
            None => client,
        };
        let wire = AnthropicConfig::new(credentials.api_key_value())
            .with_base_url(credentials.base_url())
            .completion(model)
            .with_default_max_tokens(DEFAULT_MAX_TOKENS);
        let model: DynModel<Completion> = Model::new(wire, transport).erase();
        Self {
            gateway: Arc::new(RigChatGateway::new(model)),
        }
    }

    /// 注入路径：接受 `RigHttpTransport` 或原始 `DynHttpClient`，其余对象
    /// 明确拒绝，避免未知传输带着凭据发出请求。
    pub fn with_transport(
        credentials: &ProviderCredentials,
        model: &str,
        transport: Box<dyn HttpTransport>,
    ) -> Result<Self, String> {
        let any = transport.as_any();
        let client = if let Some(handle) = any.downcast_ref::<RigHttpTransport>() {
            handle.0.clone()
        } else if let Some(client) = any.downcast_ref::<DynHttpClient>() {
            client.clone()
        } else {
            return Err("注入的 HTTP transport 不是 rig 客户端".to_owned());
        };
        Ok(Self::with_client(credentials, model, client))
    }
}

impl LlmProvider for AnthropicMessagesProvider {
    fn id(&self) -> &str {
        "anthropic-messages"
    }

    fn gateway(&self) -> Arc<dyn ModelGateway> {
        Arc::clone(&self.gateway)
    }

    fn as_any(&self) -> Option<&dyn Any> {
        Some(self)
    }
}

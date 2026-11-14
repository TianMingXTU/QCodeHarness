//! R1 多 provider 认证面的集成测试。
//!
//! 用 rig 的请求记录器截获真实 Messages 编码器产出的 HTTP 请求，断言协议
//! 路径、认证头、订阅凭据形状和按角色模型分配。全部测试进程内运行，
//! 不访问网络，也不打印凭据。

use std::borrow::Cow;
use std::sync::Arc;

use qcode_harness::application::doctor_provider::provider_connectivity_check;
use qcode_harness::application::port::{BoxFuture, LlmProvider, ModelRequest};
use qcode_harness::application::provider_auth::{
    AuthMode, ProviderCredentialError, ProviderCredentials, SubscriptionCredentialSource,
};
use qcode_harness::application::provider_registry::ProviderRegistry;
use qcode_harness::domain::doctor::{
    DoctorStatus, ProviderConnectivityFacts, ProviderConnectivityProbe,
};
use qcode_harness::domain::session::ContextSlice;
use qcode_harness::infrastructure::model::anthropic_messages::{
    AnthropicMessagesProvider, RigHttpTransport,
};
use qcode_harness::infrastructure::model::rig_chat::RigChatGateway;
use rig::DynModel;
use rig::http_client::DynHttpClient;
use rig::operation::Completion;
use rig::test_utils::{CapturedHttpRequest, RecordingHttpClient};
use serde_json::{Value, json};

const ANTHROPIC_REPLY: &str = r#"{"id":"msg_1","type":"message","role":"assistant","model":"claude-test","content":[{"type":"text","text":"协议应答"}],"stop_reason":"end_turn","stop_sequence":null,"usage":{"input_tokens":7,"output_tokens":3}}"#;

/// 一次协议调用的夹具：记录器与 provider 共用同一个假客户端。
struct Fixture {
    transport: RecordingHttpClient,
    provider: AnthropicMessagesProvider,
}

impl Fixture {
    fn new(credentials: &ProviderCredentials, model: &str) -> Self {
        let transport = RecordingHttpClient::new(ANTHROPIC_REPLY.as_bytes().to_vec());
        let provider = AnthropicMessagesProvider::with_transport(
            credentials,
            model,
            Box::new(RigHttpTransport(DynHttpClient::new(transport.clone()))),
        )
        .expect("构造 Anthropic provider");
        Self { transport, provider }
    }

    async fn complete(&self, spec: &qcode_harness::domain::agent::AgentSpec) {
        let history = vec![ContextSlice::User(Cow::Borrowed("用户任务"))];
        let reply = self
            .provider
            .gateway()
            .complete(ModelRequest {
                history,
                spec,
                tools: &[],
                cache_key: None,
            })
            .await
            .expect("假传输应答协议请求");
        assert_eq!(reply.turn.text, "协议应答", "应答应经 RigChatGateway 解码");
    }

    fn request_parts(&self) -> (String, Value, Value) {
        let requests: Vec<CapturedHttpRequest> = self.transport.requests();
        assert_eq!(requests.len(), 1, "一次模型调用只发一条请求: {requests:?}");
        let request = &requests[0];
        let body: Value = serde_json::from_slice(&request.body).expect("请求体是合法 JSON");
        let mut headers = serde_json::Map::new();
        for (name, value) in request.headers.iter() {
            headers.insert(
                name.as_str().to_owned(),
                json!(value.to_str().unwrap_or_default()),
            );
        }
        (request.uri.clone(), Value::Object(headers), body)
    }
}

fn api_key_credentials(secret: &str) -> ProviderCredentials {
    ProviderCredentials::new(
        AuthMode::ApiKey,
        secret.to_owned(),
        "https://api.anthropic.test",
    )
}

#[tokio::test]
async fn anthropic_messages_sends_protocol_path_and_api_key_headers() {
    let fixture = Fixture::new(&api_key_credentials("sk-ant-secret"), "claude-default");
    assert_eq!(fixture.provider.id(), "anthropic-messages");
    fixture.complete(&plain_spec(None)).await;

    let (uri, headers, body) = fixture.request_parts();
    assert!(uri.ends_with("/v1/messages"), "Messages 协议路径: {uri}");
    assert_eq!(headers["x-api-key"], "sk-ant-secret");
    assert_eq!(headers["anthropic-version"], "2023-06-01");
    assert!(
        headers.get("authorization").is_none(),
        "API key 模式不应伪装 Bearer"
    );
    assert_eq!(body["model"], "claude-default");
    assert_eq!(body["max_tokens"].as_u64().expect("max_tokens"), 1024);
    assert!(body["messages"].is_array(), "Messages 请求带 messages 数组");
}

#[tokio::test]
async fn api_key_credentials_are_never_printed_or_serialized() {
    let credentials = api_key_credentials("sk-ant-do-not-print");
    assert!(
        !format!("{credentials:?}").contains("sk-ant-do-not-print"),
        "Debug 不得泄露凭据"
    );
    assert!(
        !serde_json::to_string(&credentials)
            .expect("凭据可序列化")
            .contains("sk-ant-do-not-print")
    );
    assert_eq!(credentials.auth_mode(), AuthMode::ApiKey);
    assert_eq!(credentials.header_name(), "x-api-key");
    assert_eq!(credentials.authorization(), "sk-ant-do-not-print");
}

#[tokio::test]
async fn subscription_login_reads_only_environment_or_user_config_directory() {
    let source = SubscriptionCredentialSource {
        environment: vec![(
            "CLAUDE_CODE_OAUTH_TOKEN".to_owned(),
            "sub-env-token".to_owned(),
        )],
        user_config_file: Some((
            "credentials.json".to_owned(),
            b"{\"access_token\":\"sub-file-token\"}".to_vec(),
        )),
        project_config_file: Some((
            "credentials.json".to_owned(),
            b"{\"access_token\":\"project-token\"}".to_vec(),
        )),
    };
    let env_credentials = ProviderCredentials::from_subscription(&source).expect("环境订阅凭据");
    assert_eq!(env_credentials.auth_mode(), AuthMode::SubscriptionLogin);
    assert_eq!(env_credentials.authorization(), "Bearer sub-env-token");
    assert_eq!(env_credentials.header_name(), "authorization");
    assert!(
        !format!("{env_credentials:?}").contains("sub-env-token"),
        "订阅凭据不得打印"
    );

    let file_source = SubscriptionCredentialSource {
        environment: Vec::new(),
        user_config_file: source.user_config_file.clone(),
        project_config_file: source.project_config_file.clone(),
    };
    let file_credentials =
        ProviderCredentials::from_subscription(&file_source).expect("用户配置订阅凭据");
    assert_eq!(file_credentials.authorization(), "Bearer sub-file-token");
    assert!(
        !file_credentials.authorization().contains("project-token"),
        "项目目录凭据不得被读取"
    );

    let empty = SubscriptionCredentialSource {
        environment: Vec::new(),
        user_config_file: None,
        project_config_file: Some((
            "credentials.json".to_owned(),
            b"{\"access_token\":\"project-token\"}".to_vec(),
        )),
    };
    let error =
        ProviderCredentials::from_subscription(&empty).expect_err("订阅登录不能从项目目录取凭据");
    assert!(matches!(
        error,
        ProviderCredentialError::MissingSubscriptionCredential
    ));
    assert!(
        !error.to_string().contains("project-token"),
        "错误文本不得泄露候选凭据"
    );
}

#[tokio::test]
async fn subscription_login_sends_bearer_token_shape() {
    let source = SubscriptionCredentialSource {
        environment: vec![(
            "ANTHROPIC_SUBSCRIPTION_TOKEN".to_owned(),
            "sub-token".to_owned(),
        )],
        user_config_file: None,
        project_config_file: None,
    };
    let credentials = ProviderCredentials::from_subscription(&source).expect("订阅凭据");
    let fixture = Fixture::new(&credentials, "claude-default");
    fixture.complete(&plain_spec(None)).await;

    let (uri, headers, _) = fixture.request_parts();
    assert!(uri.ends_with("/v1/messages"));
    assert_eq!(headers["authorization"], "Bearer sub-token");
    assert_eq!(
        headers["x-api-key"], "sub-token",
        "Messages 编码器始终要求 x-api-key"
    );
    assert_eq!(headers["anthropic-version"], "2023-06-01");
}

#[tokio::test]
async fn role_model_assignment_and_provider_registry_semantics_are_unchanged() {
    let credentials = api_key_credentials("sk-ant-role");
    let fixture = Fixture::new(&credentials, "claude-default");
    let provider: Arc<dyn LlmProvider> = Arc::new(AnthropicMessagesProvider::with_transport(
        &credentials,
        "claude-default",
        Box::new(RigHttpTransport(DynHttpClient::new(fixture.transport.clone()))),
    )
    .expect("构造 Anthropic provider"));
    let mut registry = ProviderRegistry::new();
    assert!(registry.register(Arc::clone(&provider)));
    assert!(
        !registry.register(Arc::clone(&provider)),
        "同 id 只保留先登记者"
    );
    assert_eq!(
        registry.default_provider().expect("默认提供方").id(),
        "anthropic-messages"
    );
    assert_eq!(
        registry.get("anthropic-messages").expect("按 id 查询").id(),
        "anthropic-messages"
    );

    let spec = plain_spec(Some("claude-reviewer-tier".to_owned()));
    let history = vec![ContextSlice::User(Cow::Borrowed("用户任务"))];
    provider
        .gateway()
        .complete(ModelRequest {
            history,
            spec: &spec,
            tools: &[],
            cache_key: None,
        })
        .await
        .expect("假传输应答协议请求");

    let requests = fixture.transport.requests();
    assert_eq!(requests.len(), 1, "注册表封装后仍只发一条请求");
    let body: Value = serde_json::from_slice(&requests[0].body).expect("协议请求是 JSON");
    assert_eq!(
        body["model"], "claude-reviewer-tier",
        "角色模型覆盖由 RigChatGateway 保持"
    );
    let concrete = provider
        .as_any()
        .expect("provider 暴露具体类型")
        .downcast_ref::<AnthropicMessagesProvider>()
        .expect("provider 可下转型");
    assert_eq!(concrete.id(), "anthropic-messages");
}

#[test]
fn provider_connectivity_diagnosis_is_a_pure_thirteenth_check() {
    let ok = ProviderConnectivityFacts {
        credential_present: true,
        probe: ProviderConnectivityProbe::Ok,
        detail: None,
    };
    let check = provider_connectivity_check(&ok);
    assert_eq!(check.code, "provider_connectivity");
    assert_eq!(check.status, DoctorStatus::Ok);
    assert!(!check.message.contains("token"));

    let warn = ProviderConnectivityFacts {
        credential_present: true,
        probe: ProviderConnectivityProbe::Timeout,
        detail: Some("5 秒".to_owned()),
    };
    assert_eq!(
        provider_connectivity_check(&warn).status,
        DoctorStatus::Warn
    );
    let error = ProviderConnectivityFacts {
        credential_present: false,
        probe: ProviderConnectivityProbe::Unauthorized,
        detail: None,
    };
    assert_eq!(
        provider_connectivity_check(&error).status,
        DoctorStatus::Error
    );
    let unknown = ProviderConnectivityFacts {
        credential_present: true,
        probe: ProviderConnectivityProbe::Unknown,
        detail: None,
    };
    let check = provider_connectivity_check(&unknown);
    assert_eq!(check.status, DoctorStatus::Unknown);
    assert!(
        check
            .recommendation
            .as_deref()
            .unwrap_or_default()
            .contains("探测")
    );
}

fn plain_spec(model: Option<String>) -> qcode_harness::domain::agent::AgentSpec {
    let mut spec = qcode_harness::domain::agent::AgentSpec::new(
        qcode_harness::domain::agent::AgentRole::Coder,
        "系统提示",
    );
    spec.model = model;
    spec
}

/// 编译期确认注入的 transport 能装入 rig 的 DynModel。
#[allow(dead_code)]
fn erased_model_is_compatible(model: DynModel<Completion>) -> RigChatGateway {
    RigChatGateway::new(model)
}

/// 编译期确认端口用装盒 future，而不是 async fn in trait。
#[allow(dead_code)]
fn boxed_future_shape<'a>(
    provider: &'a dyn LlmProvider,
) -> BoxFuture<'a, Arc<dyn qcode_harness::application::port::ModelGateway>> {
    Box::pin(async move { provider.gateway() })
}

#[allow(dead_code)]
fn json_shape_is_used() -> Value {
    json!({"unused": true})
}

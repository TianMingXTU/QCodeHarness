//! Web 工具集成测试：策略放行、拒绝、审批闸门与证据账本的行为约束。
//!
//! 每个测试使用独立临时目录与独立 `JsonlEvidenceSink`，避免并行执行时
//! 共用一份 JSONL 造成事件串线。网络入口一律是测试内 fake transport：
//! 这里验证的是工具链行为，不是本机网络。

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use qcode_harness::application::agent_roster::AgentRoster;
use qcode_harness::application::agent_run::{AgentDependencies, AgentRunner};
use qcode_harness::application::context_policy::DefaultContextPolicy;
use qcode_harness::application::error::ApplicationError;
use qcode_harness::application::port::{
    ApprovalGate, ApprovalRequest, BoxFuture, Decision, EvidenceSink, ModelGateway, ModelReply,
    ModelRequest, NoopHooks,
};
use qcode_harness::application::web_tools::{WebFetchTool, WebSearchTool, WebToolSource};
use qcode_harness::domain::agent::{AgentRole, AgentSpec, AgentTurn};
use qcode_harness::domain::budget::Budget;
use qcode_harness::domain::evidence::{AuditEvent, Evidence, TokenUsage};
use qcode_harness::domain::ids::{CallId, SessionId};
use qcode_harness::domain::policy::PolicyEngine;
use qcode_harness::domain::tool::{OutcomeStatus, ToolCall};
use qcode_harness::infrastructure::clock::SystemClock;
use qcode_harness::infrastructure::evidence::jsonl::JsonlEvidenceSink;
use qcode_harness::infrastructure::evidence::reader::read_session;
use serde_json::json;

static DIR_SEQUENCE: AtomicUsize = AtomicUsize::new(0);

/// 每个测试独占一份证据文件；计数避免同进程并行时撞目录。
fn temp_dir(name: &str) -> PathBuf {
    let sequence = DIR_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "qch-web-tools-{name}-{}-{sequence}",
        std::process::id()
    ));
    if path.exists() {
        std::fs::remove_dir_all(&path).expect("清理测试目录");
    }
    std::fs::create_dir_all(&path).expect("创建测试目录");
    path
}

#[derive(Clone)]
struct ScriptedModel {
    turns: Arc<Mutex<Vec<AgentTurn>>>,
}

impl ScriptedModel {
    fn new(turns: Vec<AgentTurn>) -> Self {
        Self {
            turns: Arc::new(Mutex::new(turns)),
        }
    }
}

impl ModelGateway for ScriptedModel {
    fn complete<'a>(
        &'a self,
        _request: ModelRequest<'a>,
    ) -> BoxFuture<'a, Result<ModelReply, ApplicationError>> {
        Box::pin(async move {
            let turn = self
                .turns
                .lock()
                .unwrap()
                .pop()
                .expect("模型脚本必须包含最终回答");
            Ok(ModelReply {
                turn,
                usage: TokenUsage::default(),
            })
        })
    }
}

#[derive(Default)]
struct RecordingGate {
    requests: Mutex<Vec<ApprovalRequest>>,
}

impl ApprovalGate for RecordingGate {
    fn request<'a>(&'a self, request: &'a ApprovalRequest) -> BoxFuture<'a, Decision> {
        Box::pin(async move {
            self.requests.lock().unwrap().push(request.clone());
            Ok(())
        })
    }
}

#[derive(Default)]
struct FakeTransport {
    requests: AtomicUsize,
}

impl FakeTransport {
    fn calls(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }
}

/// 测试替身只返回固定字节，模拟一次成功的 HTTP 取回；生产实现不走这里。
impl qcode_harness::application::web_tools::WebTransport for FakeTransport {
    fn fetch<'a>(
        &'a self,
        request: &'a qcode_harness::application::web_tools::WebRequest,
    ) -> BoxFuture<'a, Result<qcode_harness::application::web_tools::WebResponse, String>> {
        Box::pin(async move {
            self.requests.fetch_add(1, Ordering::SeqCst);
            let bytes = match request.kind {
                qcode_harness::application::web_tools::WebRequestKind::Fetch => {
                    b"<html>examplex</html>".to_vec()
                }
                qcode_harness::application::web_tools::WebRequestKind::Search => {
                    br#"{"results":[{"url":"https://ex.co/abcd"}]}"#.to_vec()
                }
            };
            Ok(qcode_harness::application::web_tools::WebResponse {
                url: request.url.clone(),
                bytes,
                elapsed_ms: 7,
            })
        })
    }
}

struct Fixture {
    transport: Arc<FakeTransport>,
    approvals: Arc<RecordingGate>,
    evidence_path: PathBuf,
    evidence: Arc<dyn EvidenceSink>,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let dir = temp_dir(name);
        let evidence_path = dir.join("session.jsonl");
        let evidence: Arc<dyn EvidenceSink> =
            Arc::new(JsonlEvidenceSink::open(&evidence_path).expect("打开证据文件"));
        Self {
            transport: Arc::new(FakeTransport::default()),
            approvals: Arc::new(RecordingGate::default()),
            evidence_path,
            evidence,
        }
    }

    fn web_source(
        &self,
        whitelist: &[&str],
        blacklist: &[&str],
        endpoint: Option<&str>,
    ) -> WebToolSource {
        WebToolSource::new(
            WebFetchTool::new(self.transport.clone()),
            WebSearchTool::new(self.transport.clone(), endpoint.map(str::to_owned)),
            vec![
                qcode_harness::application::web_tools::WebToolSpecs::web_fetch(),
                qcode_harness::application::web_tools::WebToolSpecs::web_search(),
            ],
            whitelist.iter().map(|item| item.to_string()).collect(),
            blacklist.iter().map(|item| item.to_string()).collect(),
        )
    }

    fn runner(
        &self,
        call: ToolCall,
        whitelist: &[&str],
        blacklist: &[&str],
        endpoint: Option<&str>,
    ) -> AgentRunner {
        let source = self.web_source(whitelist, blacklist, endpoint);
        let domain_guard = source.guard();
        let registry = qcode_harness::application::tool_registry::ToolRegistry::from_sources(&[
            Arc::new(source) as Arc<dyn qcode_harness::application::tool_registry::ToolSource>,
        ]);
        let model = Arc::new(ScriptedModel::new(vec![
            AgentTurn {
                text: "完成".to_owned(),
                calls: Vec::new(),
            },
            AgentTurn {
                text: "取网页".to_owned(),
                calls: vec![call],
            },
        ]));
        let deps = AgentDependencies {
            model,
            tools: registry,
            approvals: self.approvals.clone(),
            evidence: self.evidence.clone(),
            clock: Arc::new(SystemClock),
            policy: PolicyEngine::new(),
            additional_guards: vec![Arc::new(domain_guard)],
            roster: AgentRoster::builtin(),
            context_budget: Default::default(),
            context_policy: Arc::new(DefaultContextPolicy),
            session_store: None,
            hooks: Arc::new(NoopHooks),
        };
        AgentRunner::new(
            AgentSpec::new(AgentRole::Coder, "Web 工具测试"),
            SessionId::new("web-tools"),
            "任务",
            deps,
        )
    }

    fn events(&self) -> Vec<Evidence> {
        read_session(&self.evidence_path)
            .expect("读取证据文件")
            .expect("证据文件存在")
            .events
    }

    fn approval_requests(&self) -> Vec<ApprovalRequest> {
        self.approvals.requests.lock().unwrap().clone()
    }
}

fn fetch_call(url: &str) -> ToolCall {
    ToolCall {
        id: CallId::new("call-1"),
        name: "web_fetch".to_owned(),
        arguments: json!({ "url": url }),
    }
}

fn search_call(query: &str) -> ToolCall {
    ToolCall {
        id: CallId::new("call-2"),
        name: "web_search".to_owned(),
        arguments: json!({ "query": query }),
    }
}

fn observed(events: &[Evidence], tool: &str) -> String {
    events
        .iter()
        .filter_map(|evidence| match &evidence.event {
            AuditEvent::ToolObserved { call, outcome, .. } if call.name == tool => {
                Some(outcome.content.clone())
            }
            _ => None,
        })
        .last()
        .unwrap_or_else(|| panic!("{tool} 应有工具观察"))
}

#[tokio::test]
async fn whitelisted_fetch_goes_through_approval_and_records_url_bytes_and_elapsed() {
    let fixture = Fixture::new("whitelist");
    let mut runner = fixture.runner(
        fetch_call("http://EXAMPLE.com:8080/docs"),
        &["example.com"],
        &[],
        None,
    );
    runner.run(&Budget::new(8)).await.expect("运行完成");

    assert_eq!(fixture.transport.calls(), 1, "白名单域名应发出一次请求");
    let requests = fixture.approval_requests();
    assert_eq!(requests.len(), 1, "外部网络工具必须经过审批闸门");
    assert_eq!(requests[0].call.name, "web_fetch");
    let events = fixture.events();
    assert!(
        events.iter().any(|evidence| matches!(
            &evidence.event,
            AuditEvent::PolicyEvaluated { tool, .. } if tool == "web_fetch"
        )),
        "策略裁决应进入证据账本"
    );
    assert!(events.iter().any(|evidence| matches!(
        &evidence.event,
        AuditEvent::ToolRequested { call, .. } if call.arguments["url"] == "http://EXAMPLE.com:8080/docs"
    )), "请求证据应保留原始 URL");
    assert!(
        events.iter().any(|evidence| matches!(
            &evidence.event,
            AuditEvent::ApprovalDecided { tool, allowed: true, .. } if tool == "web_fetch"
        )),
        "审批结论应进入证据账本"
    );
    let observed = observed(&events, "web_fetch");
    assert!(
        observed.contains("http://EXAMPLE.com:8080/docs"),
        "观察缺少 URL：{observed}"
    );
    assert!(observed.contains("bytes=21"), "观察缺少字节数：{observed}");
    assert!(
        observed.contains("elapsed_ms=7"),
        "观察缺少耗时：{observed}"
    );
    assert!(
        events.iter().any(|evidence| matches!(
            &evidence.event,
            AuditEvent::ToolObserved { outcome, .. }
                if outcome.status == OutcomeStatus::Success
        )),
        "成功观察应进入证据账本"
    );
}

#[tokio::test]
async fn blacklisted_domain_is_denied_before_network_and_recorded_in_evidence() {
    let fixture = Fixture::new("blacklist");
    let mut runner = fixture.runner(
        fetch_call("http://sub.evil.example.net:8080/a"),
        &["example.com"],
        &["evil.example.net"],
        None,
    );
    runner.run(&Budget::new(8)).await.expect("运行完成");

    assert_eq!(fixture.transport.calls(), 0, "黑名单拒绝不得触达网络入口");
    assert!(
        fixture.approval_requests().is_empty(),
        "守卫拒绝不应继续请求审批"
    );
    let events = fixture.events();
    assert!(
        events.iter().any(|evidence| matches!(
            &evidence.event,
            AuditEvent::PolicyEvaluated { tool, .. } if tool == "web_fetch"
        )),
        "策略裁决应进入证据账本"
    );
    let observed = observed(&events, "web_fetch");
    assert!(observed.contains("黑名单"), "拒绝原因应明确：{observed}");
    assert!(
        observed.contains("sub.evil.example.net"),
        "拒绝应指出域名：{observed}"
    );
    assert!(
        events.iter().any(|evidence| matches!(
            &evidence.event,
            AuditEvent::ToolObserved { call, outcome, .. }
                if call.name == "web_fetch" && outcome.status == OutcomeStatus::Refused
        )),
        "拒绝观察应进入证据账本"
    );
}

#[tokio::test]
async fn unmatched_domain_is_denied_without_network_request() {
    let fixture = Fixture::new("unmatched");
    let mut runner = fixture.runner(
        fetch_call("http://other.com/a"),
        &["example.com"],
        &[],
        None,
    );
    runner.run(&Budget::new(8)).await.expect("运行完成");

    assert_eq!(fixture.transport.calls(), 0, "不在白名单的域名不得联网");
    let observed = observed(&fixture.events(), "web_fetch");
    assert!(
        observed.contains("不在白名单"),
        "拒绝原因应明确：{observed}"
    );
}

#[tokio::test]
async fn unconfigured_search_endpoint_fails_with_explicit_reason() {
    let fixture = Fixture::new("no-search-endpoint");
    let mut runner = fixture.runner(search_call("rust tokio"), &["example.com"], &[], None);
    runner.run(&Budget::new(8)).await.expect("运行完成");

    assert_eq!(fixture.transport.calls(), 0, "未配置端点不得发出网络请求");
    let observed = observed(&fixture.events(), "web_search");
    assert!(
        observed.contains("搜索端点未配置"),
        "失败原因应明确：{observed}"
    );
    assert!(
        observed.contains("QCH_WEB_SEARCH_ENDPOINT"),
        "失败原因应告知配置方式：{observed}"
    );
}

#[tokio::test]
async fn https_fetch_is_rejected_without_touching_network() {
    let fixture = Fixture::new("https-unsupported");
    let mut runner = fixture.runner(
        fetch_call("https://example.com/a"),
        &["example.com"],
        &[],
        None,
    );
    runner.run(&Budget::new(8)).await.expect("运行完成");

    assert_eq!(
        fixture.transport.calls(),
        0,
        "未实现的 HTTPS 不得被降级为明文请求"
    );
    let observed = observed(&fixture.events(), "web_fetch");
    assert!(
        observed.contains("HTTPS 未实现"),
        "应明确说明 HTTPS 未实现：{observed}"
    );
}

#[tokio::test]
async fn configured_search_endpoint_fetches_structured_results_and_records_metrics() {
    let fixture = Fixture::new("search-endpoint");
    let mut runner = fixture.runner(
        search_call("rust tokio"),
        &["example.com"],
        &[],
        Some("http://example.com/search?q={query}"),
    );
    runner.run(&Budget::new(8)).await.expect("运行完成");

    assert_eq!(fixture.transport.calls(), 1, "已配置端点应发出一次请求");
    let observed = observed(&fixture.events(), "web_search");
    assert!(
        observed.contains("http://example.com/search?q=rust%20tokio"),
        "观察缺少搜索 URL：{observed}"
    );
    assert!(observed.contains("bytes=42"), "观察缺少字节数：{observed}");
    assert!(
        observed.contains("elapsed_ms=7"),
        "观察缺少耗时：{observed}"
    );
    assert!(
        observed.contains(r#""results""#),
        "应回传端点原始结构化结果：{observed}"
    );
}

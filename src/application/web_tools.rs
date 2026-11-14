//! WebFetch / WebSearch 的应用层端口、守卫与工具定义。
//!
//! 网络访问属于外部 IO，应用层只定义 `WebTransport` 端口、纯字符串解析和
//! 领域规则到工具链的接线；TCP 实现与 TOML 读取放在 infrastructure。域名
//! 判定只产生明确裁决，守卫拒绝发生在工具执行前，避免先联网再补判规则。

use std::sync::Arc;

use serde_json::{Value, json};

use crate::application::port::{BoxFuture, ToolInvoker};
use crate::application::tool_guard::{GuardContext, GuardVerdict, ToolGuard};
use crate::application::tool_registry::{ToolEntry, ToolSource};
use crate::domain::tool::{ToolAnnotations, ToolCall, ToolOutcome, ToolSpec};
use crate::domain::web_policy::{WebDomainRules, WebDomainVerdict, host_from_url};

/// 搜索端点未配置时给模型和证据账本看的统一原因。
const SEARCH_ENDPOINT_MISSING: &str =
    "搜索端点未配置：请设置 QCH_WEB_SEARCH_ENDPOINT，模板中用 {query} 占位符";

/// URL 的最大长度。HTTP 请求行会被写进 socket，限制长度可避免异常长输入
/// 把错误推迟到连接层。
const MAX_URL_BYTES: usize = 8192;

/// 一次网络请求的种类。真实 transport 只发普通 GET；测试替身据此区分
/// 抓取与搜索的固定响应，避免把假搜索结果伪装成语义搜索能力。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebRequestKind {
    /// 抓取一个网页 URL。
    Fetch,
    /// 请求已配置的搜索端点。
    Search,
}

/// 一次网络请求。URL 是最终请求地址，搜索端点模板展开后也落在这里。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebRequest {
    /// 最终请求 URL。
    pub url: String,
    /// 请求种类，供 transport 记录或适配不同来源用。
    pub kind: WebRequestKind,
}

/// 一次网络请求的结果事实。
///
/// 字节数和耗时由 transport 测量后交给工具层；工具层不再读时钟，证据里
/// 的 `bytes` / `elapsed_ms` 才能与这次请求一一对应。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebResponse {
    /// 实际响应的 URL。工具层会核对它是否与请求一致。
    pub url: String,
    /// 响应体原始字节。
    pub bytes: Vec<u8>,
    /// 请求耗时，单位毫秒。
    pub elapsed_ms: u64,
}

/// 网络请求端口：所有 Web 工具只通过这个端口触达外部世界。
///
/// 放在应用层是为了让装配方能注入本机 HTTP 网关或测试替身；infrastructure
/// 提供真实 TCP 实现。实现必须自行报告真实字节数与耗时，不得返回空成功。
pub trait WebTransport: Send + Sync {
    /// 发起一次请求。失败原因原样进入工具观察，不得静默转成空结果。
    fn fetch<'a>(&'a self, request: &'a WebRequest) -> BoxFuture<'a, Result<WebResponse, String>>;
}

/// 域名策略端口。守卫只依赖这个判定，规则是 TOML 表还是内存表由装配方决定。
pub trait WebPolicy: Send + Sync {
    /// 判定已提取的 host。
    fn evaluate(&self, host: &str) -> WebDomainVerdict;
}

impl WebPolicy for WebDomainRules {
    fn evaluate(&self, host: &str) -> WebDomainVerdict {
        self.classify(host)
    }
}

/// 纯字符串解析得到的 HTTP 连接目标。
///
/// domain 的 `host_from_url` 只负责策略归属；TCP 客户端还需要端口、Host 头
/// 和请求路径。这里一次解析出全部字段，避免守卫、工具与客户端各自解释 URL。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebHttpTarget {
    /// 用于 DNS 解析的 host；IPv6 字面量不带方括号。
    pub host: String,
    /// TCP 端口。
    pub port: u16,
    /// 原样用于 Host 头的 authority，保留方括号与非默认端口。
    pub authority: String,
    /// HTTP 请求行的 path + query，片段已去掉。
    pub target: String,
}

/// 解析并校验一个仅允许明文 HTTP 的 URL。
///
/// HTTPS 明确报错，不降级成 HTTP；URL 内嵌凭据、CRLF、非 ASCII 字符和非法
/// 端口同样拒绝。这里不做 DNS、不连 socket，保持应用层可测试。
pub fn parse_http_target(url: &str) -> Result<WebHttpTarget, String> {
    if url.len() > MAX_URL_BYTES {
        return Err(format!("URL 超过 {MAX_URL_BYTES} 字节上限"));
    }
    if !url.is_ascii() {
        return Err("URL 必须是 ASCII；非 ASCII 字符请百分号编码".to_owned());
    }
    if url
        .bytes()
        .any(|byte| byte.is_ascii_control() || byte == b' ')
    {
        return Err("URL 含空白或控制字符".to_owned());
    }
    if url.starts_with("https://") {
        return Err(
            "HTTPS 未实现：当前 WebTransport 仅支持 http://；请配置本机 HTTP 网关，不要使用 https:// URL".to_owned(),
        );
    }
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| "仅支持 http:// URL".to_owned())?;
    let without_fragment = rest.split('#').next().unwrap_or(rest);
    let cut = without_fragment
        .find(['/', '?'])
        .unwrap_or(without_fragment.len());
    let authority = &without_fragment[..cut];
    let path_and_query = &without_fragment[cut..];
    if authority.is_empty() {
        return Err("URL 缺少 host".to_owned());
    }
    if authority.contains('@') {
        return Err("Web 工具不支持 URL 内嵌凭据".to_owned());
    }

    let (host, port) = if authority.starts_with('[') {
        let close = authority
            .find(']')
            .ok_or_else(|| "IPv6 URL 缺少右方括号".to_owned())?;
        let host = &authority[1..close];
        let suffix = &authority[close + 1..];
        let port = if suffix.is_empty() {
            80
        } else {
            let raw = suffix
                .strip_prefix(':')
                .ok_or_else(|| "IPv6 URL 端口格式不合法".to_owned())?;
            parse_port(raw)?
        };
        (host.to_owned(), port)
    } else {
        let (host, raw_port) = authority.split_once(':').unwrap_or((authority, ""));
        (host.to_owned(), parse_port(raw_port)?)
    };
    if host.is_empty()
        || !host
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b':' | b'_'))
    {
        return Err("URL host 含非法字符".to_owned());
    }

    let target = if path_and_query.is_empty() {
        "/".to_owned()
    } else if path_and_query.starts_with('?') {
        format!("/{path_and_query}")
    } else {
        path_and_query.to_owned()
    };
    if target
        .bytes()
        .any(|byte| byte.is_ascii_control() || byte == b' ')
    {
        return Err("HTTP 请求路径含空白或控制字符".to_owned());
    }
    Ok(WebHttpTarget {
        host,
        port,
        authority: authority.to_owned(),
        target,
    })
}

fn parse_port(raw: &str) -> Result<u16, String> {
    if raw.is_empty() {
        return Ok(80);
    }
    let port = raw
        .parse::<u16>()
        .map_err(|_| format!("URL 端口 {raw} 不合法"))?;
    if port == 0 {
        return Err("URL 端口不能为 0".to_owned());
    }
    Ok(port)
}

/// 把搜索端点模板与查询词合成最终 URL。
///
/// 只做字符串替换和百分号编码，不解析搜索结果、不生成摘要。端点可自带
/// `{query}`；没有占位符时追加 `q=<query>`。模型不能通过参数改端点，否则
/// 域名白名单会被绕过。
pub fn build_search_url(endpoint: &str, query: &str) -> Result<String, String> {
    if endpoint.is_empty() {
        return Err(SEARCH_ENDPOINT_MISSING.to_owned());
    }
    if query.is_empty() {
        return Err("web_search 的 query 不能为空".to_owned());
    }
    if endpoint.contains('#') {
        return Err("搜索端点模板不得包含 # 片段".to_owned());
    }
    let encoded = percent_encode(query);
    let url = if endpoint.contains("{query}") {
        endpoint.replace("{query}", &encoded)
    } else {
        let separator = if endpoint.contains('?') { '&' } else { '?' };
        format!("{endpoint}{separator}q={encoded}")
    };
    parse_http_target(&url)?;
    Ok(url)
}

fn percent_encode(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len() * 3);
    for byte in value.as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(*byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(*byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

/// 域名白/黑名单守卫。
///
/// 它只处理两个 Web 工具，对其他工具放行；对 Web 工具采取 fail-closed：
/// URL 无法解析、HTTPS 未实现、端点未配置、黑名单命中或未列入白名单都直接
/// 拒绝。审批是额外防线，不能用来替代明确的域名拒绝。
pub struct WebDomainGuard<P: WebPolicy> {
    policy: P,
    search_endpoint: Option<String>,
}

impl<P: WebPolicy> WebDomainGuard<P> {
    /// 用域名策略构造守卫。
    pub fn new(policy: P) -> Self {
        Self {
            policy,
            search_endpoint: None,
        }
    }

    /// 注入搜索端点，让守卫能预先检查端点域名。
    pub fn with_search_endpoint(mut self, endpoint: Option<String>) -> Self {
        self.search_endpoint = endpoint;
        self
    }
}

impl<P: WebPolicy> ToolGuard for WebDomainGuard<P> {
    fn name(&self) -> &str {
        "web-domain"
    }

    fn inspect(&self, call: &ToolCall, _ctx: &GuardContext<'_>) -> GuardVerdict {
        match call.name.as_str() {
            "web_fetch" => {
                let Some(url) = call.arguments.get("url").and_then(Value::as_str) else {
                    return GuardVerdict::Deny("web_fetch 需要 url 参数".to_owned());
                };
                self.verdict_for_url(url)
            }
            "web_search" => {
                // 端点来自装配配置而非模型参数；否则模型可用任意 URL 绕过域名表。
                let Some(endpoint) = self.search_endpoint.as_deref() else {
                    return GuardVerdict::Deny(SEARCH_ENDPOINT_MISSING.to_owned());
                };
                let Some(query) = call.arguments.get("query").and_then(Value::as_str) else {
                    return GuardVerdict::Deny("web_search 需要 query 参数".to_owned());
                };
                match build_search_url(endpoint, query) {
                    Ok(url) => self.verdict_for_url(&url),
                    Err(reason) => GuardVerdict::Deny(reason),
                }
            }
            _ => GuardVerdict::Pass,
        }
    }
}

impl<P: WebPolicy> WebDomainGuard<P> {
    fn verdict_for_url(&self, url: &str) -> GuardVerdict {
        if let Err(reason) = parse_http_target(url) {
            return GuardVerdict::Deny(reason);
        }
        let Some(host) = host_from_url(url) else {
            return GuardVerdict::Deny("无法从 URL 解析域名".to_owned());
        };
        match self.policy.evaluate(&host) {
            WebDomainVerdict::Allow => GuardVerdict::Pass,
            WebDomainVerdict::DenyBlacklisted => {
                GuardVerdict::Deny(format!("域名 {host} 命中黑名单，禁止访问"))
            }
            WebDomainVerdict::DenyUnlisted => {
                GuardVerdict::Deny(format!("域名 {host} 不在白名单，禁止访问"))
            }
        }
    }
}

/// WebFetch 工具：抓取一个明文 HTTP URL 的原始响应。
///
/// 工具实现也检查域名表，防止装配方漏挂守卫时把拒绝域名送进 transport；
/// 正常路径的拒绝仍由 `WebDomainGuard` 在审批与执行前产生。
pub struct WebFetchTool {
    transport: Arc<dyn WebTransport>,
    rules: WebDomainRules,
}

impl WebFetchTool {
    /// 用网络端口构造；默认空白名单即拒绝全部域名。
    pub fn new(transport: Arc<dyn WebTransport>) -> Self {
        Self {
            transport,
            rules: WebDomainRules::default(),
        }
    }

    /// 注入工具执行侧的同一份域名规则。
    pub fn with_rules(mut self, rules: WebDomainRules) -> Self {
        self.rules = rules;
        self
    }
}

impl ToolInvoker for WebFetchTool {
    fn invoke<'a>(&'a self, call: &'a ToolCall) -> BoxFuture<'a, ToolOutcome> {
        Box::pin(async move {
            let Some(url) = call.arguments.get("url").and_then(Value::as_str) else {
                return ToolOutcome::failed("web_fetch 需要 url 参数");
            };
            let url = url.to_owned();
            if let Err(outcome) = enforce_web_rules(&url, &self.rules) {
                return outcome;
            }
            let request = WebRequest {
                url,
                kind: WebRequestKind::Fetch,
            };
            execute(&*self.transport, request).await
        })
    }
}

/// WebSearch 工具：请求已配置的 HTTP 搜索端点并原样返回响应体。
///
/// 端点未配置时明确失败。工具不解析 JSON、不补造结果列表，搜索语义由
/// 端点服务负责。
pub struct WebSearchTool {
    transport: Arc<dyn WebTransport>,
    endpoint: Option<String>,
    rules: WebDomainRules,
}

impl WebSearchTool {
    /// 用网络端口与端点模板构造；默认空白名单即拒绝全部域名。
    pub fn new(transport: Arc<dyn WebTransport>, endpoint: Option<String>) -> Self {
        Self {
            transport,
            endpoint,
            rules: WebDomainRules::default(),
        }
    }

    /// 已配置的端点模板，供守卫与诊断使用。
    pub fn endpoint(&self) -> Option<&str> {
        self.endpoint.as_deref()
    }

    /// 注入工具执行侧的同一份域名规则。
    pub fn with_rules(mut self, rules: WebDomainRules) -> Self {
        self.rules = rules;
        self
    }
}

impl ToolInvoker for WebSearchTool {
    fn invoke<'a>(&'a self, call: &'a ToolCall) -> BoxFuture<'a, ToolOutcome> {
        Box::pin(async move {
            let Some(query) = call.arguments.get("query").and_then(Value::as_str) else {
                return ToolOutcome::failed("web_search 需要 query 参数");
            };
            let Some(endpoint) = self.endpoint.as_deref() else {
                return ToolOutcome::failed(SEARCH_ENDPOINT_MISSING);
            };
            let url = match build_search_url(endpoint, query) {
                Ok(url) => url,
                Err(reason) => {
                    return ToolOutcome::failed(format!("endpoint={endpoint}\n{reason}"));
                }
            };
            if let Err(outcome) = enforce_web_rules(&url, &self.rules) {
                return outcome;
            }
            let request = WebRequest {
                url,
                kind: WebRequestKind::Search,
            };
            execute(&*self.transport, request).await
        })
    }
}

fn enforce_web_rules(url: &str, rules: &WebDomainRules) -> Result<(), ToolOutcome> {
    parse_http_target(url).map_err(|reason| ToolOutcome::failed(format!("url={url}\n{reason}")))?;
    let Some(host) = host_from_url(url) else {
        return Err(ToolOutcome::failed(format!(
            "url={url}\n无法从 URL 解析域名"
        )));
    };
    match rules.classify(&host) {
        WebDomainVerdict::Allow => Ok(()),
        WebDomainVerdict::DenyBlacklisted => Err(ToolOutcome::refused(format!(
            "url={url}\n域名 {host} 命中黑名单，拒绝访问"
        ))),
        WebDomainVerdict::DenyUnlisted => Err(ToolOutcome::refused(format!(
            "url={url}\n域名 {host} 不在白名单，拒绝访问"
        ))),
    }
}

/// 执行请求并把 transport 报告的事实写进工具观察。
///
/// `ToolObserved.content` 因此包含 URL、响应字节数和耗时；`ToolRequested`
/// 另存模型给出的原始参数。两者共同构成证据账本，不必新增 AuditEvent。
async fn execute(transport: &dyn WebTransport, request: WebRequest) -> ToolOutcome {
    match transport.fetch(&request).await {
        Ok(response) => {
            if response.url != request.url {
                return ToolOutcome::failed(format!(
                    "url={}\nWebTransport 返回 URL 不一致：{}",
                    request.url, response.url
                ));
            }
            ToolOutcome::success(format!(
                "url={}\nbytes={}\nelapsed_ms={}\nbody=\n{}",
                request.url,
                response.bytes.len(),
                response.elapsed_ms,
                String::from_utf8_lossy(&response.bytes)
            ))
        }
        Err(error) => ToolOutcome::failed(format!("url={}\nWeb 请求失败：{error}", request.url)),
    }
}

/// WebFetch / WebSearch 的工具规格。
///
/// 非只读、开放世界的注解让默认策略给出 `RequireApproval`，外部 IO 必须
/// 经过审批闸门；`idempotent` 只表示相同 GET 可重复，不表示免审。
pub struct WebToolSpecs;

impl WebToolSpecs {
    /// WebFetch 的工具定义。
    pub fn web_fetch() -> ToolSpec {
        ToolSpec {
            name: "web_fetch".to_owned(),
            description:
                "抓取指定 http:// URL 的原始内容。HTTPS 未实现；域名受白/黑名单与审批约束。"
                    .to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "url": { "type": "string", "description": "仅支持 http:// 的完整 URL" }
                },
                "required": ["url"],
                "additionalProperties": false
            }),
            annotations: ToolAnnotations::new(false, false, true, true),
        }
    }

    /// WebSearch 的工具定义。
    pub fn web_search() -> ToolSpec {
        ToolSpec {
            name: "web_search".to_owned(),
            description: "调用启动时配置的 HTTP 搜索端点并原样返回响应；未配置端点时拒绝。"
                .to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "查询词，会被百分号编码后填入端点模板" }
                },
                "required": ["query"],
                "additionalProperties": false
            }),
            annotations: ToolAnnotations::new(false, false, true, true),
        }
    }
}

/// 两个 Web 工具的注册来源。
///
/// 规格由调用方给出，执行入口固定与工具名配对；装配漏掉一个规格时显式
/// 失败，不静默提供半套工具清单。
#[derive(Clone)]
pub struct WebToolSource {
    rules: WebDomainRules,
    search_endpoint: Option<String>,
    entries: Vec<ToolEntry>,
}

impl WebToolSource {
    /// 装配两个 Web 工具。`specs` 必须包含 `web_fetch` 与 `web_search`。
    ///
    /// 非法装配是启动期错误；这里用 panic 终止，避免工具清单残缺后继续跑。
    pub fn new(
        fetch: WebFetchTool,
        search: WebSearchTool,
        specs: Vec<ToolSpec>,
        whitelist: Vec<String>,
        blacklist: Vec<String>,
    ) -> Self {
        Self::try_new(fetch, search, specs, whitelist, blacklist)
            .unwrap_or_else(|error| panic!("Web 工具装配失败：{error}"))
    }

    /// 可检查失败的装配入口，供 main 在启动阶段转成用户可见错误。
    pub fn try_new(
        fetch: WebFetchTool,
        search: WebSearchTool,
        specs: Vec<ToolSpec>,
        whitelist: Vec<String>,
        blacklist: Vec<String>,
    ) -> Result<Self, String> {
        Self::try_from_rules(
            fetch,
            search,
            specs,
            WebDomainRules::new(whitelist, blacklist),
        )
    }

    /// 用已加载的规则表装配，供 main 把 TOML 策略与工具守卫共用一份事实。
    pub fn try_from_rules(
        fetch: WebFetchTool,
        search: WebSearchTool,
        specs: Vec<ToolSpec>,
        rules: WebDomainRules,
    ) -> Result<Self, String> {
        for required in ["web_fetch", "web_search"] {
            if !specs.iter().any(|spec| spec.name == required) {
                return Err(format!("Web 工具规格缺少 {required}"));
            }
        }
        let search_endpoint = search.endpoint().map(str::to_owned);
        let fetch = Arc::new(fetch.with_rules(rules.clone())) as Arc<dyn ToolInvoker>;
        let search = Arc::new(search.with_rules(rules.clone())) as Arc<dyn ToolInvoker>;
        let mut entries = Vec::with_capacity(specs.len());
        for spec in specs {
            let invoker = match spec.name.as_str() {
                "web_fetch" => Arc::clone(&fetch),
                "web_search" => Arc::clone(&search),
                other => return Err(format!("未知 Web 工具规格：{other}")),
            };
            entries.push(ToolEntry { spec, invoker });
        }
        Ok(Self {
            rules,
            search_endpoint,
            entries,
        })
    }

    /// 生成与工具侧规则一致的守卫，供 `AgentDependencies::additional_guards` 使用。
    pub fn guard(&self) -> WebDomainGuard<WebDomainRules> {
        WebDomainGuard::new(self.rules.clone()).with_search_endpoint(self.search_endpoint.clone())
    }

    /// 已配置的搜索端点模板。
    pub fn search_endpoint(&self) -> Option<&str> {
        self.search_endpoint.as_deref()
    }
}

impl ToolSource for WebToolSource {
    fn name(&self) -> &str {
        "web-tools"
    }

    fn tools(&self) -> Vec<ToolEntry> {
        self.entries.clone()
    }
}

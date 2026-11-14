//! Web 工具的基础设施实现：最小 HTTP 客户端与域名策略 TOML。
//!
//! 只实现明文 `http://` 的 HTTP/1.0 GET；HTTPS 明确拒绝，不用明文连接
//! 冒充加密连接。TOML 读取与 socket IO 都留在本层，领域规则仍是纯函数。

use std::path::{Path, PathBuf};
use std::time::Instant;

use serde::Deserialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::{Duration, timeout};

use crate::application::port::BoxFuture;
use crate::application::web_tools::{
    WebHttpTarget, WebRequest, WebResponse, WebTransport, parse_http_target,
};
use crate::domain::web_policy::WebDomainRules;

/// 单次连接与读取的时限。网页抓取不能因为对端不响应而卡住整个 ReAct 循环。
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// 响应体上限。超出时返回明确错误，不静默截断成看似完整的网页。
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;

/// 真实 TCP 网络端口：支持 `http://` 的普通 GET。
///
/// 返回原始响应体，不跟随重定向、不解析 chunked、不支持 HTTPS；所有失败
/// 都带上下文返回给工具层，再由 `ToolObserved` 进入证据账本。
#[derive(Debug, Clone, Copy, Default)]
pub struct TcpWebTransport;

impl TcpWebTransport {
    /// 构造一个无状态的 TCP transport。
    pub fn new() -> Self {
        Self
    }
}

impl WebTransport for TcpWebTransport {
    fn fetch<'a>(&'a self, request: &'a WebRequest) -> BoxFuture<'a, Result<WebResponse, String>> {
        Box::pin(async move { fetch_http(request).await })
    }
}

async fn fetch_http(request: &WebRequest) -> Result<WebResponse, String> {
    let target = parse_http_target(&request.url)
        .map_err(|reason| format!("url={} {reason}", request.url))?;
    let started = Instant::now();
    let raw = timeout(REQUEST_TIMEOUT, request_http(&target))
        .await
        .map_err(|_| {
            format!(
                "url={} 请求超过 {} 秒",
                request.url,
                REQUEST_TIMEOUT.as_secs()
            )
        })??;
    let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let body = parse_response(&raw, &request.url)?;
    Ok(WebResponse {
        url: request.url.clone(),
        bytes: body,
        elapsed_ms,
    })
}

async fn request_http(target: &WebHttpTarget) -> Result<Vec<u8>, String> {
    let address = if target.host.contains(':') {
        format!("[{}]:{}", target.host, target.port)
    } else {
        format!("{}:{}", target.host, target.port)
    };
    let mut stream = TcpStream::connect(&address)
        .await
        .map_err(|error| format!("连接 {address} 失败：{error}"))?;
    let request = format!(
        "GET {} HTTP/1.0\r\nHost: {}\r\nUser-Agent: QCodeHarness/1.0\r\nConnection: close\r\n\r\n",
        target.target, target.authority
    );
    stream
        .write_all(request.as_bytes())
        .await
        .map_err(|error| format!("向 {address} 发送请求失败：{error}"))?;
    let mut raw = Vec::new();
    stream
        .take((MAX_RESPONSE_BYTES + 64 * 1024) as u64)
        .read_to_end(&mut raw)
        .await
        .map_err(|error| format!("读取 {address} 响应失败：{error}"))?;
    if raw.len() > MAX_RESPONSE_BYTES + 64 * 1024 {
        return Err(format!("响应超过 {MAX_RESPONSE_BYTES} 字节上限"));
    }
    Ok(raw)
}

fn parse_response(raw: &[u8], url: &str) -> Result<Vec<u8>, String> {
    let split = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or_else(|| format!("url={url} HTTP 响应缺少头部结束标记"))?;
    let head = std::str::from_utf8(&raw[..split])
        .map_err(|error| format!("url={url} HTTP 头部不是 ASCII：{error}"))?;
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .ok_or_else(|| format!("url={url} HTTP 响应缺少状态行"))?;
    let status_code = status
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| format!("url={url} HTTP 状态行不合法：{status}"))?;
    if status_code != "200" {
        return Err(format!(
            "url={url} HTTP 状态 {status}；不支持重定向与非 200 响应"
        ));
    }
    if lines.any(|line| line.eq_ignore_ascii_case("transfer-encoding: chunked")) {
        return Err(format!("url={url} 不支持 Transfer-Encoding: chunked"));
    }
    let body = raw[split + 4..].to_vec();
    if body.len() > MAX_RESPONSE_BYTES {
        return Err(format!(
            "url={url} 响应体 {} 字节，超过 {MAX_RESPONSE_BYTES} 字节上限",
            body.len()
        ));
    }
    Ok(body)
}

/// 域名策略 TOML 的顶层结构。
///
/// `deny_unknown_fields` 与 `policy.example.toml` 的解析口径一致：手写配置
/// 拼错字段时立即报错，不让一条失效的安全规则被当成空规则继续运行。
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WebPolicyConfig {
    /// 允许的域名及其子域；为空时全部拒绝。
    #[serde(default)]
    whitelist: Vec<String>,
    /// 明确禁止的域名及其子域；优先于白名单。
    #[serde(default)]
    blacklist: Vec<String>,
    /// 规则表，用于让配置与 policy.example.toml 的 `[[rules]]` 结构保持一致。
    #[serde(default)]
    rules: Vec<WebPolicyRuleConfig>,
}

/// 一条域名规则：`action = "allow" / "deny"`，`domain = "example.com"`。
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WebPolicyRuleConfig {
    /// 规则作用，只允许 allow 或 deny。
    action: String,
    /// 域名，不含协议、端口、路径。
    domain: String,
    /// 规则说明，便于人排查配置意图；由 serde 校验存在性，不参与判定。
    #[serde(default, rename = "description")]
    _description: String,
}

/// 把 TOML 文本解析为领域规则表。
///
/// 同时支持顶层数组与 `[[rules]]` 两种写法；未知 action 视为 deny，避免
/// 拼写错误扩大访问范围。`description` 只为人服务，不影响判定结果。
pub fn parse_web_policy(text: &str) -> Result<WebDomainRules, toml::de::Error> {
    let config: WebPolicyConfig = toml::from_str(text)?;
    let mut whitelist = config.whitelist;
    let mut blacklist = config.blacklist;
    for rule in config.rules {
        match rule.action.trim().to_ascii_lowercase().as_str() {
            "allow" => whitelist.push(rule.domain),
            _ => blacklist.push(rule.domain),
        }
    }
    Ok(WebDomainRules::new(whitelist, blacklist))
}

/// 加载域名策略配置。
///
/// 未给路径时使用空白名单（所有 Web 请求拒绝）；给路径但读不到或内容不
/// 合法时返回带路径的错误，不降级成"没有规则"。
pub fn load_web_policy(path: Option<&Path>) -> Result<WebDomainRules, WebPolicyConfigError> {
    let Some(path) = path else {
        return Ok(WebDomainRules::default());
    };
    let text = std::fs::read_to_string(path).map_err(|source| WebPolicyConfigError::Read {
        path: path.to_path_buf(),
        source: Box::new(source),
    })?;
    parse_web_policy(&text).map_err(|source| WebPolicyConfigError::Parse {
        path: path.to_path_buf(),
        source: Box::new(source),
    })
}

/// Web 域名配置的加载错误。
#[derive(Debug, thiserror::Error)]
pub enum WebPolicyConfigError {
    /// 配置文件读不到。
    #[error("读取 Web 域名策略 {path} 失败: {source}")]
    Read {
        /// 配置文件路径。
        path: PathBuf,
        /// 底层 IO 错误。
        source: Box<std::io::Error>,
    },
    /// 配置文件内容不合法。
    #[error("解析 Web 域名策略 {path} 失败: {source}")]
    Parse {
        /// 配置文件路径。
        path: PathBuf,
        /// TOML 解析错误，自带行列号。
        source: Box<toml::de::Error>,
    },
}

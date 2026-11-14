//! Web 域名规则的纯领域判定。
//!
//! 本模块只处理字符串和规则列表，不做 DNS、文件读取或网络访问。把域名
//! 判定放在领域层，是为了让 `ToolGuard`、审批界面与配置解析共用同一套
//! 语义：子域归属、大小写和黑名单优先级不会因调用点不同而漂移。

/// 域名规则裁决。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebDomainVerdict {
    /// 命中白名单，可继续走策略与审批链。
    Allow,
    /// 命中黑名单，必须在工具执行前拒绝。
    DenyBlacklisted,
    /// 未列入白名单，按 fail-closed 拒绝。
    DenyUnlisted,
}

/// 一组域名白/黑名单。
///
/// 规则值只保存域名，不保存端口、协议或路径；端口由 URL 解析层剥离。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WebDomainRules {
    whitelist: Vec<String>,
    blacklist: Vec<String>,
}

impl WebDomainRules {
    /// 构造规则表，并统一域名大小写与末尾点。
    pub fn new(whitelist: Vec<String>, blacklist: Vec<String>) -> Self {
        let normalize = |list: Vec<String>| {
            list.into_iter()
                .map(|domain| normalize_host(&domain))
                .filter(|domain| !domain.is_empty())
                .collect()
        };
        Self {
            whitelist: normalize(whitelist),
            blacklist: normalize(blacklist),
        }
    }

    /// 判定一个已解析的 host；黑名单优先于白名单。
    pub fn classify(&self, host: &str) -> WebDomainVerdict {
        classify_domain(host, &self.whitelist, &self.blacklist)
    }
}

/// 判断 host 是否属于规则中的域名或其子域。
///
/// 按标签边界比较，避免 `example.com` 误匹配 `notexample.com` 或
/// `example.com.evil.net`。IPv6 字面量（如 `[::1]`）只做精确匹配。
fn matches_rule(host: &str, rule: &str) -> bool {
    if host == rule {
        return true;
    }
    if host.starts_with('[') || rule.starts_with('[') {
        return false;
    }
    host.strip_suffix(rule)
        .is_some_and(|prefix| prefix.ends_with('.') && !prefix[..prefix.len() - 1].is_empty())
}

/// 对 host 给出域名规则裁决。
///
/// 参数是已归一化的规则列表。黑名单先判，避免"白名单覆盖父域、黑名单
/// 指定子域"时把明确禁止的站点放进审批链。白名单为空时一律拒绝：开放
/// 世界工具不能靠缺省规则联网。
pub fn classify_domain(host: &str, whitelist: &[String], blacklist: &[String]) -> WebDomainVerdict {
    let host = normalize_host(host);
    let normalized = |list: &[String]| {
        list.iter()
            .map(|rule| normalize_host(rule))
            .filter(|rule| !rule.is_empty())
            .collect::<Vec<_>>()
    };
    let blacklist = normalized(blacklist);
    if blacklist.iter().any(|rule| matches_rule(&host, rule)) {
        return WebDomainVerdict::DenyBlacklisted;
    }
    let whitelist = normalized(whitelist);
    if whitelist.iter().any(|rule| matches_rule(&host, rule)) {
        return WebDomainVerdict::Allow;
    }
    WebDomainVerdict::DenyUnlisted
}

/// 从 HTTP/HTTPS URL 提取 host，转小写并去掉端口、路径、查询与片段。
///
/// 只做策略判定所需的字符串解析，不验证 URL 的全部语法；IPv6 字面量
/// 保留方括号，避免与端口冒号混淆。
pub fn host_from_url(url: &str) -> Option<String> {
    let rest = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.is_empty() {
        return None;
    }
    let host_port = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    if host_port.is_empty() {
        return None;
    }
    let host = if let Some(close) = host_port.rfind(']') {
        if !host_port.starts_with('[') {
            return None;
        }
        &host_port[..=close]
    } else {
        host_port.split(':').next().unwrap_or("")
    };
    Some(normalize_host(host))
}

fn normalize_host(host: &str) -> String {
    host.trim().trim_end_matches('.').to_ascii_lowercase()
}

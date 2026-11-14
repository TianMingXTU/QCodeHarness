//! Web 域名策略纯函数行为测试。
//!
//! 这些用例覆盖 URL 解析与规则匹配的实际边界：大小写、端口、子域、
//! 以及容易误判的前缀。测试只通过公开纯函数观察结论，不依赖 IO 或配置解析。

use qcode_harness::domain::web_policy::{WebDomainVerdict, classify_domain, host_from_url};

fn rules() -> (Vec<String>, Vec<String>) {
    (
        vec!["example.com".to_owned(), "Example.ORG".to_owned()],
        vec![
            "blocked.example.com".to_owned(),
            "evil.example.net".to_owned(),
        ],
    )
}

#[test]
fn host_extraction_is_case_insensitive_and_drops_port_and_path() {
    assert_eq!(
        host_from_url("http://EXAMPLE.com:8080/docs?q=1"),
        Some("example.com".to_owned())
    );
    assert_eq!(
        host_from_url("http://user:pass@Sub.Example.ORG:443/a"),
        Some("sub.example.org".to_owned())
    );
    assert_eq!(
        host_from_url("https://example.com"),
        Some("example.com".to_owned())
    );
    assert_eq!(
        host_from_url("http://[::1]:8080/a"),
        Some("[::1]".to_owned())
    );
    assert_eq!(host_from_url("not-a-url"), None);
    assert_eq!(host_from_url("http:///missing-host"), None);
}

#[test]
fn whitelist_matches_exact_host_or_its_subdomains_case_insensitively() {
    let (whitelist, blacklist) = rules();
    assert_eq!(
        classify_domain("EXAMPLE.com", &whitelist, &blacklist),
        WebDomainVerdict::Allow
    );
    assert_eq!(
        classify_domain("a.b.example.com", &whitelist, &blacklist),
        WebDomainVerdict::Allow
    );
    assert_eq!(
        classify_domain("deep.example.org", &whitelist, &blacklist),
        WebDomainVerdict::Allow
    );
}

#[test]
fn port_prefix_and_suffix_lookalikes_do_not_match_a_domain_rule() {
    let (whitelist, blacklist) = rules();
    assert_eq!(
        classify_domain("example.com:8080", &whitelist, &blacklist),
        WebDomainVerdict::DenyUnlisted
    );
    assert_eq!(
        classify_domain("notexample.com", &whitelist, &blacklist),
        WebDomainVerdict::DenyUnlisted
    );
    assert_eq!(
        classify_domain("example.com.evil.net", &whitelist, &blacklist),
        WebDomainVerdict::DenyUnlisted
    );
    assert_eq!(
        classify_domain("example.comx", &whitelist, &blacklist),
        WebDomainVerdict::DenyUnlisted
    );
    assert_eq!(
        classify_domain("example.org.evil.net", &whitelist, &blacklist),
        WebDomainVerdict::DenyUnlisted
    );
}

#[test]
fn blacklist_overrides_whitelist_and_covers_subdomains() {
    let (whitelist, blacklist) = rules();
    assert_eq!(
        classify_domain("BLOCKED.example.com", &whitelist, &blacklist),
        WebDomainVerdict::DenyBlacklisted
    );
    assert_eq!(
        classify_domain("x.blocked.example.com", &whitelist, &blacklist),
        WebDomainVerdict::DenyBlacklisted
    );
    assert_eq!(
        classify_domain("sub.evil.example.net", &whitelist, &blacklist),
        WebDomainVerdict::DenyBlacklisted
    );
}

#[test]
fn empty_whitelist_denies_every_unlisted_domain() {
    assert_eq!(
        classify_domain("example.com", &[], &[]),
        WebDomainVerdict::DenyUnlisted
    );
    assert_eq!(
        classify_domain("", &[], &[]),
        WebDomainVerdict::DenyUnlisted
    );
}

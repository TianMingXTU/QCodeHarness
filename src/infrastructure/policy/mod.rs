//! 策略基础设施：从 TOML 文件加载规则。
//!
//! 读文件是 IO，所以这一层属于 infrastructure；解析出的 `PolicyEngine`
//! 是领域类型，纯函数、无 IO。分界线就在 `load` 这个函数上。

use std::path::Path;

use serde::Deserialize;

use crate::domain::policy::{PolicyEngine, PolicyRule, PolicyScope, PolicyVerdict};

/// 加载策略配置。
///
/// 路径为 `None` 时返回内置默认策略（空规则表，走注解判断）；
/// 为 `Some` 时读取并解析该文件。
///
/// # Errors
///
/// 文件不存在、内容不是合法 TOML、或字段取值不在允许范围内时返回
/// 带行号的错误——配置文件是手写的，报错必须能直接定位到那一行。
pub fn load(path: Option<&Path>) -> Result<PolicyEngine, PolicyConfigError> {
    let Some(path) = path else {
        return Ok(PolicyEngine::new());
    };
    let text = std::fs::read_to_string(path).map_err(|source| PolicyConfigError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    parse(&text).map_err(|source| PolicyConfigError::Parse {
        path: path.to_path_buf(),
        source,
    })
}

/// 解析 TOML 文本为策略引擎。
///
/// 独立成 pub 函数是为了测试：不需要临时文件就能验证解析规则。
pub fn parse(text: &str) -> Result<PolicyEngine, toml::de::Error> {
    let config: PolicyConfig = toml::from_str(text)?;
    Ok(PolicyEngine::with_rules(
        config.rules.into_iter().map(PolicyRule::from).collect(),
    ))
}

/// TOML 侧的配置结构。
///
/// 与领域类型分开定义而不是直接 `Deserialize` 到 `PolicyRule`：
/// 配置里用字符串表达裁决（`verdict = "deny"`），而领域里用枚举；
/// 这层翻译同时承担了"字符串 → 枚举"的校验，非法值在这里就能报错。
#[derive(Debug, Deserialize)]
struct PolicyConfig {
    /// 规则列表，按数组顺序匹配，第一条命中的生效。
    #[serde(default)]
    rules: Vec<RuleConfig>,
}

/// TOML 侧的一条规则。
#[derive(Debug, Deserialize)]
struct RuleConfig {
    /// 匹配的工具名。缺省表示匹配所有工具。
    tool: Option<String>,
    /// 匹配时给出的裁决：`allow` / `require_approval` / `deny`。
    verdict: String,
    /// 规则说明，进入证据和给模型的解释。
    #[serde(default)]
    description: String,
}

impl From<RuleConfig> for PolicyRule {
    fn from(config: RuleConfig) -> Self {
        PolicyRule {
            tool: config.tool,
            // P1 不支持参数级作用域，配置里也没有对应字段。
            scope: None,
            verdict: PolicyVerdict::from_str(&config.verdict),
            description: config.description,
        }
    }
}

/// 把配置里的裁决字符串翻译成领域枚举。
///
/// 无法识别的值落到 `RequireApproval`：宁可多问一句，
/// 也不能因为一个拼写错误就把高危工具放过去。
impl PolicyVerdict {
    fn from_str(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "allow" => Self::Allow,
            "deny" => Self::Deny,
            _ => Self::RequireApproval,
        }
    }
}

/// 让 `PolicyScope` 的预留字段不会因为"从未构造"而报警。
///
/// P2 加参数级作用域时会用上它；现在先让类型完整可序列化。
#[allow(dead_code)]
fn scope_placeholder() -> Option<PolicyScope> {
    None
}

/// 策略配置的加载错误。
#[derive(Debug, thiserror::Error)]
pub enum PolicyConfigError {
    /// 配置文件读不到。
    #[error("读取策略配置 {path} 失败: {source}")]
    Read {
        /// 配置文件路径。
        path: std::path::PathBuf,
        /// 底层 IO 错误。
        source: std::io::Error,
    },
    /// 配置文件内容不合法。
    #[error("解析策略配置 {path} 失败: {source}")]
    Parse {
        /// 配置文件路径。
        path: std::path::PathBuf,
        /// TOML 解析错误，自带行列号。
        source: toml::de::Error,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::ids::CallId;
    use crate::domain::tool::{ToolAnnotations, ToolCall, ToolSpec};
    use serde_json::json;

    fn call(name: &str) -> ToolCall {
        ToolCall {
            id: CallId::new("c"),
            name: name.to_owned(),
            arguments: json!({}),
        }
    }

    fn spec(read_only: bool) -> ToolSpec {
        ToolSpec {
            name: "x".to_owned(),
            description: "x".to_owned(),
            parameters: json!({}),
            annotations: ToolAnnotations::new(read_only, false, false, false),
        }
    }

    #[test]
    fn no_path_means_default_policy() {
        let engine = load(None).expect("无路径应返回默认策略");

        assert!(engine.rules.is_empty());
        assert_eq!(
            engine.evaluate(&call("write_file"), &spec(false)).verdict,
            PolicyVerdict::RequireApproval
        );
    }

    #[test]
    fn parses_a_single_rule() {
        let engine = parse(
            r#"
[[rules]]
tool = "shell"
verdict = "deny"
description = "P1 未实现 shell 工具，先禁掉"
"#,
        )
        .expect("解析");

        assert_eq!(engine.rules.len(), 1);
        let decision = engine.evaluate(&call("shell"), &spec(false));
        assert_eq!(decision.verdict, PolicyVerdict::Deny);
        assert_eq!(decision.rule_index, Some(0));
        assert!(decision.reason.contains("P1"));
    }

    #[test]
    fn rules_keep_their_file_order() {
        let engine = parse(
            r#"
[[rules]]
tool = "read_file"
verdict = "allow"
description = "读文件免审"

[[rules]]
tool = "write_file"
verdict = "require_approval"
description = "写文件要审"

[[rules]]
verdict = "deny"
description = "其余全禁"
"#,
        )
        .expect("解析");

        assert_eq!(engine.rules.len(), 3);
        assert_eq!(
            engine.evaluate(&call("read_file"), &spec(true)).verdict,
            PolicyVerdict::Allow
        );
        assert_eq!(
            engine.evaluate(&call("write_file"), &spec(false)).verdict,
            PolicyVerdict::RequireApproval
        );
        // 第三条没写 tool，是通配规则。
        assert_eq!(
            engine.evaluate(&call("anything_else"), &spec(true)).verdict,
            PolicyVerdict::Deny
        );
    }

    #[test]
    fn verdict_strings_are_case_insensitive() {
        let engine = parse(
            r#"
[[rules]]
tool = "a"
verdict = "ALLOW"
description = "大写也行"
"#,
        )
        .expect("解析");

        assert_eq!(
            engine.evaluate(&call("a"), &spec(false)).verdict,
            PolicyVerdict::Allow
        );
    }

    #[test]
    fn unknown_verdict_fails_closed_to_require_approval() {
        // 拼错成 deny 的近义词不应静默放行。
        let engine = parse(
            r#"
[[rules]]
tool = "write_file"
verdict = "forbidden"
description = "手滑打错"
"#,
        )
        .expect("解析不应失败，未知值有兜底");

        assert_eq!(
            engine.evaluate(&call("write_file"), &spec(false)).verdict,
            PolicyVerdict::RequireApproval
        );
    }

    #[test]
    fn malformed_toml_reports_an_error() {
        let result = parse("[[rules]\nverdict = \"deny\"\n");

        assert!(result.is_err(), "缺右括号应报错");
    }

    #[test]
    fn missing_verdict_field_reports_an_error() {
        let result = parse(
            r#"
[[rules]]
tool = "write_file"
description = "忘了写 verdict"
"#,
        );

        assert!(result.is_err(), "缺必需字段应报错");
    }

    #[test]
    fn empty_file_yields_default_policy() {
        let engine = parse("").expect("空配置合法");

        assert!(engine.rules.is_empty());
    }

    #[test]
    fn error_message_names_the_file() {
        let dir = std::env::temp_dir().join(format!("qch-policy-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("建临时目录");
        let path = dir.join("policy.toml");
        std::fs::write(&path, "这不是 toml {{{").expect("写坏配置");

        let error = load(Some(&path)).expect_err("坏配置应报错");
        let message = error.to_string();

        assert!(message.contains("policy.toml"), "错误应指明文件: {message}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

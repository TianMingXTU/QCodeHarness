//! 策略基础设施：从 TOML 文件加载规则。
//!
//! 读文件属于 IO，因此放在 infrastructure。解析得到的 `PolicyEngine` 是
//! 领域类型，纯函数、无 IO。分界在 `load`。

use std::path::Path;

use serde::Deserialize;

use crate::domain::policy::{PolicyEngine, PolicyRule, PolicyScope, PolicyVerdict};

/// 加载策略配置。
///
/// 路径为 `None` 时返回内置默认策略，自带一份免审基线（见
/// [`crate::domain::policy::PolicyEngine::new`]）。为 `Some` 时读取并
/// 解析该文件。
///
/// 显式配置整体取代基线，不与其合并。提供配置文件即表示由用户定义规则，
/// 基线中的 `cargo test *` 等不再生效。需要免审的命令必须写进自己的
/// 配置，`policy.example.toml` 已按此方式编写。空文件比不提供文件更严，
/// 表示声明了没有任何规则。
///
/// # Errors
///
/// 文件不存在时返回 [`PolicyConfigError::Read`]；内容不是合法 TOML 时返回
/// 带行列号的 [`PolicyConfigError::Parse`]，由用户手写的配置需要能直接
/// 定位到行。
pub fn load(path: Option<&Path>) -> Result<PolicyEngine, PolicyConfigError> {
    let Some(path) = path else {
        return Ok(PolicyEngine::new());
    };
    let text = std::fs::read_to_string(path).map_err(|source| PolicyConfigError::Read {
        path: path.to_path_buf(),
        source: Box::new(source),
    })?;
    parse(&text).map_err(|source| PolicyConfigError::Parse {
        path: path.to_path_buf(),
        source: Box::new(source),
    })
}

/// 把 TOML 文本解析为策略引擎。
///
/// 独立为 pub 函数以便测试，无需临时文件即可验证解析规则。
pub fn parse(text: &str) -> Result<PolicyEngine, toml::de::Error> {
    let config: PolicyConfig = toml::from_str(text)?;
    Ok(PolicyEngine::with_rules(
        config.rules.into_iter().map(PolicyRule::from).collect(),
    ))
}

/// TOML 侧的配置结构。
///
/// 与领域类型分开定义，因为配置用字符串表示裁决（`verdict = "deny"`），
/// 领域类型用枚举。无法识别的裁决字符串按 fail-closed 处理：规则落到
/// `RequireApproval`（见 `PolicyVerdict::from_str`），不在此报错。
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PolicyConfig {
    /// 规则列表，按数组顺序匹配，第一条命中的生效。
    #[serde(default)]
    rules: Vec<RuleConfig>,
}

/// TOML 侧的一条规则。
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RuleConfig {
    /// 匹配的工具名。缺省表示匹配所有工具。
    tool: Option<String>,
    /// 匹配时给出的裁决：`allow` / `require_approval` / `deny`。
    verdict: String,
    /// 规则说明，写入证据与给模型的解释。
    #[serde(default)]
    description: String,
    /// 参数级作用域。缺省时只按工具名匹配，不检查参数。
    #[serde(default)]
    scope: Option<ScopeConfig>,
}

/// TOML 侧的作用域。
///
/// `deny_unknown_fields` 是安全要求：拼错的字段若被忽略，作用域会整体
/// 失效，一条本意只放行 cargo test 的规则会扩大为放行所有 run_command。
/// 配置由用户手写，拼写错误必须立即报错。
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ScopeConfig {
    /// 命令头条件，尾部的 `*` 表示前缀匹配。
    #[serde(default)]
    command_prefix: Option<String>,
    /// 路径前缀条件，尾部的 `*` 表示前缀匹配。
    #[serde(default)]
    path_prefix: Option<String>,
}

impl From<ScopeConfig> for PolicyScope {
    fn from(config: ScopeConfig) -> Self {
        PolicyScope {
            command_prefix: config.command_prefix,
            path_prefix: config.path_prefix,
        }
    }
}

impl From<RuleConfig> for PolicyRule {
    fn from(config: RuleConfig) -> Self {
        PolicyRule {
            tool: config.tool,
            scope: config.scope.map(PolicyScope::from),
            verdict: PolicyVerdict::from_str(&config.verdict),
            description: config.description,
        }
    }
}

/// 把配置中的裁决字符串转换为领域枚举。
///
/// 无法识别的值按 `RequireApproval` 处理，避免拼写错误导致高危工具
/// 被放行。
impl PolicyVerdict {
    fn from_str(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "allow" => Self::Allow,
            "deny" => Self::Deny,
            _ => Self::RequireApproval,
        }
    }
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
        source: Box<std::io::Error>,
    },
    /// 配置文件内容不合法。
    #[error("解析策略配置 {path} 失败: {source}")]
    Parse {
        /// 配置文件路径。
        path: std::path::PathBuf,
        /// TOML 解析错误，自带行列号。
        source: Box<toml::de::Error>,
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

    fn call_with(name: &str, arguments: serde_json::Value) -> ToolCall {
        ToolCall {
            id: CallId::new("c"),
            name: name.to_owned(),
            arguments,
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

        // 默认策略带内置基线（cargo test/build、git status/diff 免审）。
        // 断言非空：空规则表会让 cargo test 每次都要人工确认。
        assert!(!engine.rules.is_empty(), "默认策略应带内置基线");
        assert_eq!(
            engine
                .evaluate(&call_with("run_command", json!({"command": "cargo test"})), &spec(false))
                .verdict,
            PolicyVerdict::Allow,
            "cargo test 应免审"
        );
        assert_eq!(
            engine
                .evaluate(&call_with("run_command", json!({"command": "git push"})), &spec(false))
                .verdict,
            PolicyVerdict::RequireApproval,
            "基线之外的命令仍要审批"
        );
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
description = "shell 工具被禁用"
"#,
        )
        .expect("解析");

        assert_eq!(engine.rules.len(), 1);
        let decision = engine.evaluate(&call("shell"), &spec(false));
        assert_eq!(decision.verdict, PolicyVerdict::Deny);
        assert_eq!(decision.rule_index, Some(0));
        assert!(decision.reason.contains("shell 工具被禁用"));
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
        // 拼成 deny 的近义词不应被放行。
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

    // ── 作用域（scope）────────────────────────────────────────────────

    /// `scope = { command_prefix = "cargo test *" }` 把针对 `run_command`
    /// 的放行限制在 cargo test 一类命令。
    #[test]
    fn scope_with_command_prefix_narrows_the_rule() {
        let engine = parse(
            r#"
[[rules]]
tool = "run_command"
verdict = "allow"
description = "跑测试免审"
scope = { command_prefix = "cargo test *" }
"#,
        )
        .expect("解析");

        let spec = spec(false);
        assert_eq!(
            engine
                .evaluate(&call_with("run_command", json!({"command": "cargo test --nocapture"})), &spec)
                .verdict,
            PolicyVerdict::Allow
        );
        assert_eq!(
            engine
                .evaluate(&call_with("run_command", json!({"command": "rm -rf /"})), &spec)
                .verdict,
            PolicyVerdict::RequireApproval,
            "作用域外的命令不得被放行"
        );
    }

    /// `path_prefix` 同理，只对 `path` 参数生效。
    #[test]
    fn scope_with_path_prefix_narrows_the_rule() {
        let engine = parse(
            r#"
[[rules]]
tool = "write_file"
verdict = "allow"
description = "src/ 下的写入免审"
scope = { path_prefix = "src/*" }
"#,
        )
        .expect("解析");

        let spec = spec(false);
        assert_eq!(
            engine
                .evaluate(&call_with("write_file", json!({"path": "src/main.rs"})), &spec)
                .verdict,
            PolicyVerdict::Allow
        );
        assert_eq!(
            engine
                .evaluate(&call_with("write_file", json!({"path": "docs/x.md"})), &spec)
                .verdict,
            PolicyVerdict::RequireApproval
        );
    }

    /// 空 `scope = {}` 与未配置作用域相同，规则仍只按工具名匹配。
    #[test]
    fn empty_scope_table_behaves_like_no_scope() {
        let engine = parse(
            r#"
[[rules]]
tool = "run_command"
verdict = "allow"
description = "空作用域"
scope = {}
"#,
        )
        .expect("解析");

        assert_eq!(
            engine
                .evaluate(&call_with("run_command", json!({"command": "随便"})), &spec(false))
                .verdict,
            PolicyVerdict::Allow
        );
    }

    /// 拼错作用域字段必须报错，不能被忽略。
    ///
    /// `command_pretix` 若按"没有作用域"处理，本意只放行 cargo test 的
    /// 规则会扩大为放行所有 run_command。
    #[test]
    fn misspelled_scope_field_is_rejected() {
        let result = parse(
            r#"
[[rules]]
tool = "run_command"
verdict = "allow"
description = "手滑打错一个字母"
scope = { command_pretix = "cargo test *" }
"#,
        );

        assert!(result.is_err(), "拼错的作用域字段应报错");
    }

    /// 规则级的拼错同样要报错，理由同上。
    #[test]
    fn misspelled_rule_field_is_rejected() {
        let result = parse(
            r#"
[[rules]]
tool = "run_command"
verdict = "allow"
descriptoin = "少了一个 i"
"#,
        );

        assert!(result.is_err(), "拼错的规则字段应报错");
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

    /// 仓库中的示例配置必须能被当前解析器解析。
    ///
    /// 该文件是用户抄写用的模板，也被使用手册引用。示例与解析器不一致时
    /// 测试仍然通过，用户抄写后才会报错，因此用示例文件做解析器的回归
    /// 测试。
    #[test]
    fn the_shipped_example_config_parses() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("policy.example.toml");

        let engine = load(Some(&path)).expect("policy.example.toml 应能解析");

        assert!(
            engine.rules.len() >= 9,
            "示例应覆盖九个内置工具，实际 {} 条",
            engine.rules.len()
        );
    }

    /// 示例中带 scope 的 allow 必须排在兜底 require_approval 之前。
    ///
    /// README 的 `write_file` 示例曾只写"src/ 下的写入免审"而缺少 scope：
    /// 描述承诺了范围，规则没有，用户照抄会得到"所有 write_file 都免审"。
    /// 这类漂移不会让测试失败。本测试同时固定顺序：allow 在前，
    /// require_approval 兜底在后；顺序反了 cargo test 会需要审批。
    #[test]
    fn the_scoped_allow_rule_comes_before_the_catch_all() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("policy.example.toml");

        let engine = load(Some(&path)).expect("policy.example.toml 应能解析");
        let spec = spec(false);

        let cargo_test = engine.evaluate(&call_with("run_command", json!({"command": "cargo test"})), &spec);
        let other = engine.evaluate(&call_with("run_command", json!({"command": "git status"})), &spec);

        assert_eq!(cargo_test.verdict, PolicyVerdict::Allow, "cargo test 应免审");
        assert_eq!(
            other.verdict,
            PolicyVerdict::RequireApproval,
            "其余 shell 命令应落到兜底审批"
        );
    }
}

//! 本地 hooks 配置兼容测试：Claude Code / Codex 格式只做显式映射。
//!
//! 未识别字段、未识别事件和未识别配置段必须进入 UnmappedConfig 报告，
//! 不得按字段名猜语义。

use qcode_harness::application::hooks_compat::{
    HookConfigFormat, HookEventKind, parse_hook_config,
};

const CLAUDE_SETTINGS: &str = r#"{
  "hooks": {
    "PreToolUse": [
      {"matcher": "Bash", "hooks": [{"type": "command", "command": "echo deny"}]}
    ],
    "PostToolUse": [
      {"matcher": "Write", "hooks": [{"type": "command", "command": "echo done"}]}
    ]
  }
}"#;

const CODEX_CONFIG: &str = r#"[hooks]
PreToolUse = [{ command = "echo deny" }]
PostToolUse = [{ command = "echo done" }]
"#;

#[test]
fn claude_code_settings_map_known_events_and_commands() {
    let report = parse_hook_config(HookConfigFormat::ClaudeCode, CLAUDE_SETTINGS)
        .expect("Claude Code 样例应被解析");
    let mappings: Vec<_> = report.mappings.iter().collect();
    assert_eq!(mappings.len(), 2);
    assert_eq!(mappings[0].event, HookEventKind::PreToolUse);
    assert_eq!(mappings[0].command, "echo deny");
    assert_eq!(mappings[1].event, HookEventKind::PostToolUse);
    assert_eq!(mappings[1].command, "echo done");
    assert!(report.unmapped.is_empty(), "已知字段不应产生映射报告");
}

#[test]
fn codex_config_maps_known_events_and_commands() {
    let report = parse_hook_config(HookConfigFormat::Codex, CODEX_CONFIG)
        .expect("Codex 样例应被解析");
    assert_eq!(report.mappings.len(), 2);
    assert_eq!(report.mappings[0].event, HookEventKind::PreToolUse);
    assert_eq!(report.mappings[1].event, HookEventKind::PostToolUse);
    assert!(report.unmapped.is_empty());
}

#[test]
fn unknown_fields_events_and_sections_are_explicitly_reported() {
    let text = r#"{
  "hooks": {
    "PreToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "echo deny", "future": true}]}],
    "UnknownEvent": [{"hooks": []}]
  },
  "futureFeature": {"enabled": true}
}"#;
    let report = parse_hook_config(HookConfigFormat::ClaudeCode, text)
        .expect("解析本身成功，但必须显式报告未映射配置");
    assert!(report.mappings.is_empty(), "含未映射项时不得猜测为有效映射");
    assert!(report.unmapped.iter().any(|item| item.path == "hooks.PreToolUse[0].hooks[0].future"));
    assert!(report.unmapped.iter().any(|item| item.path == "hooks.UnknownEvent"));
    assert!(report.unmapped.iter().any(|item| item.path == "futureFeature"));
}

#[test]
fn unsupported_command_type_is_not_guessed_as_external_command() {
    let text = r#"{"hooks":{"PreToolUse":[{"hooks":[{"type":"prompt","prompt":"allow?"}]}]}}"#;
    let report = parse_hook_config(HookConfigFormat::ClaudeCode, text)
        .expect("解析本身成功，但必须显式报告未映射配置");
    assert!(report.mappings.is_empty());
    assert!(report
        .unmapped
        .iter()
        .any(|item| item.reason.contains("hook type")));
}

#[test]
fn malformed_json_and_toml_report_parse_failure_not_swallowed() {
    assert!(parse_hook_config(HookConfigFormat::ClaudeCode, "{").is_err());
    assert!(parse_hook_config(HookConfigFormat::Codex, "[hooks\n").is_err());
}

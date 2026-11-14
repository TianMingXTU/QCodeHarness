//! Claude Code / Codex hooks 配置到内部事件的显式映射。
//!
//! domain 不读取文件：这里只负责解析字符串、校验已知字段，并把不能解释的
//! 配置原样写入 UnmappedConfig。只要报告非空，调用方必须整体失败。

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::Value;
use thiserror::Error;
use toml::Value as TomlValue;


/// 内部 hook 事件表；只列出与现有 RunHooks 链有明确重合点的事件。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HookEventKind {
    PreToolUse,
    PostToolUse,
}

/// 受支持的配置格式。未知格式不尝试按内容自动猜测。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HookConfigFormat {
    ClaudeCode,
    Codex,
}

/// 已确认语义的单条映射。`command` 是可直接执行的命令字符串。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HookMapping {
    pub event: HookEventKind,
    pub command: String,
    pub source: String,
}

/// 未映射配置；`path` 是稳定点分路径，`reason` 说明为什么不能映射。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnmappedConfig {
    pub path: String,
    pub reason: String,
}

/// 映射结果。调用方应在 `unmapped` 非空时拒绝启用兼容配置。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HookConfigReport {
    pub mappings: Vec<HookMapping>,
    pub unmapped: Vec<UnmappedConfig>,
}

#[derive(Debug, Error)]
pub enum HookConfigError {
    #[error("Claude Code hooks 配置 JSON 解析失败：{0}")]
    Json(#[source] serde_json::Error),
    #[error("Codex hooks 配置 TOML 解析失败：{0}")]
    Toml(#[source] toml::de::Error),
}

/// 解析外部 hooks 配置为内部事件表。只接受已明确定义的字段与 command 类型。
pub fn parse_hook_config(
    format: HookConfigFormat,
    text: &str,
) -> Result<HookConfigReport, HookConfigError> {
    match format {
        HookConfigFormat::ClaudeCode => parse_claude_code(text),
        HookConfigFormat::Codex => parse_codex(text),
    }
}

#[derive(Deserialize)]
struct ClaudeSettings {
    #[serde(default)]
    hooks: BTreeMap<String, Vec<ClaudeMatcher>>,
    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
}

#[derive(Deserialize)]
struct ClaudeMatcher {
    #[serde(default)]
    matcher: Option<String>,
    hooks: Vec<ClaudeHook>,
    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
}

#[derive(Deserialize)]
struct ClaudeHook {
    #[serde(rename = "type")]
    hook_type: String,
    #[serde(default)]
    command: Option<String>,
    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
}

fn parse_claude_code(text: &str) -> Result<HookConfigReport, HookConfigError> {
    let settings: ClaudeSettings =
        serde_json::from_str(text).map_err(HookConfigError::Json)?;
    let mut report = HookConfigReport::default();
    for key in settings.extra.keys() {
        report.unmapped.push(UnmappedConfig {
            path: key.clone(),
            reason: "未识别的 settings.json 顶层字段".to_owned(),
        });
    }
    let mut entries: Vec<_> = settings.hooks.iter().collect();
    entries.sort_by(|(left, _), (right, _)| hook_event_order(left).cmp(&hook_event_order(right)));
    for (event_name, matchers) in entries {
        let Some(event) = hook_event(event_name) else {
            report.unmapped.push(UnmappedConfig {
                path: format!("hooks.{event_name}"),
                reason: "未识别的 hook 事件".to_owned(),
            });
            continue;
        };
        for (index, matcher) in matchers.iter().enumerate() {
            let prefix = format!("hooks.{event_name}[{index}]");
            for key in matcher.extra.keys() {
                report.unmapped.push(UnmappedConfig {
                    path: format!("{prefix}.{key}"),
                    reason: "未识别的 matcher 字段".to_owned(),
                });
            }
            for (hook_index, hook) in matcher.hooks.iter().enumerate() {
                let path = format!("{prefix}.hooks[{hook_index}]");
                if hook.hook_type != "command" {
                    report.unmapped.push(UnmappedConfig {
                        path: path.clone(),
                        reason: format!("未支持的 hook type：{}", hook.hook_type),
                    });
                    continue;
                }
                let Some(command) = hook.command.clone() else {
                    report.unmapped.push(UnmappedConfig {
                        path,
                        reason: "command hook 缺少 command 字段".to_owned(),
                    });
                    continue;
                };
                for key in hook.extra.keys() {
                    report.unmapped.push(UnmappedConfig {
                        path: format!("{path}.{key}"),
                        reason: "未识别的 hook 字段".to_owned(),
                    });
                }
                let source = matcher
                    .matcher
                    .as_deref()
                    .unwrap_or("*")
                    .to_owned();
                report.mappings.push(HookMapping {
                    event,
                    command,
                    source,
                });
            }
        }
    }
    if !report.unmapped.is_empty() {
        report.mappings.clear();
    }
    Ok(report)
}

fn parse_codex(text: &str) -> Result<HookConfigReport, HookConfigError> {
    let root: TomlValue = toml::from_str(text).map_err(HookConfigError::Toml)?;
    let mut report = HookConfigReport::default();
    let Some(table) = root.as_table() else {
        report.unmapped.push(UnmappedConfig {
            path: "$".to_owned(),
            reason: "Codex 配置根节点必须是表".to_owned(),
        });
        return Ok(report);
    };
    for (key, _) in table {
        if key != "hooks" {
            report.unmapped.push(UnmappedConfig {
                path: key.clone(),
                reason: "未识别的 Codex 顶层配置段".to_owned(),
            });
        }
    }
    let Some(hooks) = table.get("hooks").and_then(TomlValue::as_table) else {
        report.unmapped.push(UnmappedConfig {
            path: "hooks".to_owned(),
            reason: "hooks 段必须是表".to_owned(),
        });
        return Ok(report);
    };
    let mut entries: Vec<_> = hooks.iter().collect();
    entries.sort_by(|(left, _), (right, _)| hook_event_order(left).cmp(&hook_event_order(right)));
    for (event_name, value) in entries {
        let Some(event) = hook_event(event_name) else {
            report.unmapped.push(UnmappedConfig {
                path: format!("hooks.{event_name}"),
                reason: "未识别的 hook 事件".to_owned(),
            });
            continue;
        };
        let Some(entries) = value.as_array() else {
            report.unmapped.push(UnmappedConfig {
                path: format!("hooks.{event_name}"),
                reason: "hook 事件必须是数组表".to_owned(),
            });
            continue;
        };
        for (index, entry) in entries.iter().enumerate() {
            let path = format!("hooks.{event_name}[{index}]");
            let Some(entry) = entry.as_table() else {
                report.unmapped.push(UnmappedConfig {
                    path,
                    reason: "hook 条目必须是表".to_owned(),
                });
                continue;
            };
            for key in entry.keys() {
                if key != "command" {
                    report.unmapped.push(UnmappedConfig {
                        path: format!("{path}.{key}"),
                        reason: "未识别的 Codex hook 字段".to_owned(),
                    });
                }
            }
            let Some(command) = entry.get("command").and_then(TomlValue::as_str) else {
                report.unmapped.push(UnmappedConfig {
                    path,
                    reason: "hook 条目缺少 command 字符串".to_owned(),
                });
                continue;
            };
            report.mappings.push(HookMapping {
                event,
                command: command.to_owned(),
                source: "*".to_owned(),
            });
        }
    }
    if !report.unmapped.is_empty() {
        report.mappings.clear();
    }
    Ok(report)
}

fn hook_event_order(name: &str) -> u8 {
    match name {
        "PreToolUse" => 0,
        "PostToolUse" => 1,
        _ => 2,
    }
}

fn hook_event(name: &str) -> Option<HookEventKind> {
    match name {
        "PreToolUse" => Some(HookEventKind::PreToolUse),
        "PostToolUse" => Some(HookEventKind::PostToolUse),
        _ => None,
    }
}

//! SKILL.md 兼容解析：把 Claude Code 技能文件的 YAML frontmatter 转成
//! 既有 TOML 扩展清单的字段。只做文本解析与渲染，不做文件 IO；
//! 安装链路与本地技能市场见 `infrastructure::skill_md`。

use serde::Serialize;

/// 渲染目标：与 `infrastructure::extensions::Manifest` 的 TOML 字段同构。
/// Manifest 带 `deny_unknown_fields`，多一个键都会被安装链路拒绝，
/// 因此渲染结构只允许出现这四个字段。
#[derive(Serialize)]
struct ManifestToml<'a> {
    id: &'a str,
    description: &'a str,
    instructions: &'a str,
    permissions: &'a [String],
}

/// SKILL.md 解析出的技能声明，字段与既有 TOML 清单一一对应。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillManifest {
    /// 来自 frontmatter `name`，同时用作扩展 ID 与清单文件名。
    pub id: String,
    pub description: String,
    /// frontmatter 之后的 Markdown 正文，作为 Skill 指令原文。
    pub instructions: String,
    /// `allowed-tools` 中可映射为本仓权限格式的条目。
    pub permissions: Vec<String>,
    /// `allowed-tools` 中无法映射的条目，显式留给调用方报告，不静默丢弃。
    pub unmapped_permissions: Vec<String>,
    /// frontmatter 中本仓未识别的键，显式留给调用方报告。
    pub unrecognized_keys: Vec<String>,
}

/// 解析 SKILL.md 文本；`source` 仅用于错误消息定位。
///
/// 识别的 frontmatter 键：`name`（扩展 ID）、`description`、`allowed-tools`
/// （逗号分隔）。其余键原样记入 `unrecognized_keys`，由调用方显式报告；
/// frontmatter 之后的 Markdown 正文作为 Skill 指令原文，不允许为空。
/// `name` 的字符集校验交给安装链路的清单校验，这里只查非空。
pub fn parse_skill_markdown(text: &str, source: &str) -> Result<SkillManifest, String> {
    let all: Vec<&str> = text.lines().collect();
    if all.is_empty() || all[0].trim_end() != "---" {
        return Err(format!("{source}: SKILL.md 必须以 `---` frontmatter 开头"));
    }
    let Some(close) = all[1..].iter().position(|line| line.trim_end() == "---") else {
        return Err(format!("{source}: frontmatter 未闭合"));
    };
    let mut id = String::new();
    let mut description = String::new();
    let mut permissions = Vec::new();
    let mut unmapped = Vec::new();
    let mut unrecognized = Vec::new();
    for line in &all[1..1 + close] {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Some((key, value)) = trimmed.split_once(':') else {
            return Err(format!("{source}: frontmatter 行无效：`{trimmed}`"));
        };
        let value = unquote(value);
        match key.trim() {
            "name" => id = value.to_owned(),
            "description" => description = value.to_owned(),
            "allowed-tools" => {
                // Claude Code 的 allowed-tools 语法（如 Bash(git status:*)）
                // 与本仓权限字符规则不一致的条目不能悄悄丢掉，逐条记入
                // unmapped，由安装链路决定是否拒绝。
                for entry in value.split(',') {
                    let entry = entry.trim();
                    if entry.is_empty() {
                        continue;
                    }
                    if mappable_permission(entry) {
                        permissions.push(entry.to_owned());
                    } else {
                        unmapped.push(entry.to_owned());
                    }
                }
            }
            other => unrecognized.push(other.to_owned()),
        }
    }
    // 正文从闭合 `---` 的下一行开始；frontmatter 与闭合行之间不能插入正文。
    let instructions = all[1 + close + 1..].join("\n");
    let instructions = instructions.trim().to_owned();

    if id.is_empty() {
        return Err(format!("{source}: 缺少 name"));
    }
    if description.is_empty() {
        return Err(format!("{source}: 缺少 description"));
    }
    if instructions.is_empty() {
        return Err(format!("{source}: 缺少正文指令"));
    }
    Ok(SkillManifest {
        id,
        description,
        instructions,
        permissions,
        unmapped_permissions: unmapped,
        unrecognized_keys: unrecognized,
    })
}

impl SkillManifest {
    /// 渲染为与 `infrastructure::extensions::Manifest` 同构的 TOML 清单文本。
    ///
    /// permissions 永远输出：目标 Manifest 的该字段没有 `serde(default)`，
    /// 空列表也必须显式写出才能通过 `deny_unknown_fields` 解析。
    pub fn to_manifest_toml(&self) -> String {
        toml::to_string_pretty(&ManifestToml {
            id: &self.id,
            description: &self.description,
            instructions: &self.instructions,
            permissions: &self.permissions,
        })
        .expect("清单字段均为可序列化类型")
    }
}

/// 去掉一对匹配的单/双引号；不成对时原样返回。
fn unquote(value: &str) -> &str {
    let value = value.trim();
    if value.len() >= 2
        && ((value.starts_with('"') && value.ends_with('"'))
            || (value.starts_with('\'') && value.ends_with('\'')))
    {
        &value[1..value.len() - 1]
    } else {
        value
    }
}

/// 与 `domain::extension` 的权限字符规则保持一致：非空、不超过 128 字节、
/// 仅 ASCII 字母数字与下划线/连字符。是否为真实 backing 工具仍由安装
/// 链路的清单校验复核，这里只做格式初筛。
fn mappable_permission(entry: &str) -> bool {
    !entry.is_empty()
        && entry.len() <= 128
        && entry
            .bytes()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == b'_' || ch == b'-')
}

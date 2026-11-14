//! SKILL.md 兼容：frontmatter 解析进既有 TOML 清单，经安装链路默认禁用。

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use qcode_harness::application::extension::ExtensionRegistry;
use qcode_harness::application::skill_md::parse_skill_markdown;
use qcode_harness::application::tool_registry::ToolRegistry;
use qcode_harness::infrastructure::extensions::LocalExtensionStore;
use qcode_harness::infrastructure::skill_md::install_skill_md;

static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

/// 每个测试独占的临时工作区；清单装进工作区，授权写在旁边独立目录。
struct Workspace(PathBuf);

impl Workspace {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "qch-skill-md-{}-{}",
            std::process::id(),
            NEXT_DIR.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&root).unwrap();
        Self(root)
    }

    /// 相对路径可含子目录，父目录按需创建。
    fn file(&self, relative: &str, text: &str) -> PathBuf {
        let path = self.0.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&path, text).unwrap();
        path
    }

    fn dir(&self, name: &str) -> PathBuf {
        let path = self.0.join(name);
        std::fs::create_dir(&path).unwrap();
        path
    }

    /// 授权状态放在工作区外的独立目录，测试之间互不影响，也不会写进
    /// 真实用户配置目录。
    fn store(&self) -> LocalExtensionStore {
        LocalExtensionStore::with_state_directory(&self.0, Some(self.0.join("user-config")))
    }

    /// 安装只校验清单自身声明，不要求 backing 工具在场，空注册表即可。
    fn registry(&self) -> ExtensionRegistry {
        ExtensionRegistry::new(&ToolRegistry::new(Vec::new()))
    }
}

/// 组装一份 SKILL.md；`extra` 追加额外 frontmatter 键值对。
fn skill_md(name: &str, description: &str, extra: &[(&str, &str)], body: &str) -> String {
    let mut text = format!("---\nname: {name}\ndescription: {description}\n");
    for (key, value) in extra {
        text.push_str(&format!("{key}: {value}\n"));
    }
    text.push_str("---\n\n");
    text.push_str(body);
    text
}

#[test]
fn parse_renders_manifest_toml_and_installs_disabled() {
    let workspace = Workspace::new();
    let text = skill_md(
        "pdf-tools",
        "提取 PDF 文本为分页清单。",
        &[("allowed-tools", "Read, Grep")],
        "用 Read 与 Grep 分段提取 PDF 文本，禁止整文件载入。",
    );
    let manifest = parse_skill_markdown(&text, "SKILL.md").unwrap();
    assert_eq!(manifest.id, "pdf-tools");
    assert_eq!(manifest.description, "提取 PDF 文本为分页清单。");
    assert!(manifest.instructions.contains("禁止整文件载入"));
    assert_eq!(manifest.permissions, vec!["Read", "Grep"]);
    assert!(manifest.unmapped_permissions.is_empty());
    assert!(manifest.unrecognized_keys.is_empty());

    let scratch = workspace.dir("scratch");
    let manifest_path = scratch.join("pdf-tools.toml");
    std::fs::write(&manifest_path, manifest.to_manifest_toml()).unwrap();

    let store = workspace.store();
    let registry = workspace.registry();
    let descriptor = store.install(&registry, &manifest_path).unwrap();
    assert_eq!(descriptor.id, "pdf-tools");
    assert_eq!(descriptor.permissions, vec!["Read", "Grep"]);

    let installed = &registry.list().unwrap()[0];
    assert!(!installed.enabled);
    assert!(installed.has_skill);
    assert!(!installed.has_hook);
}

#[test]
fn install_reports_unrecognized_frontmatter_keys() {
    let workspace = Workspace::new();
    let path = workspace.file(
        "skills/pdf/SKILL.md",
        &skill_md("pdf-tools", "说明。", &[("license", "MIT")], "正文指令。"),
    );
    let store = workspace.store();
    let registry = workspace.registry();
    let report = install_skill_md(&store, &registry, &path, &workspace.dir("scratch")).unwrap();
    assert_eq!(report.unrecognized_keys, vec!["license"]);
    assert_eq!(report.descriptor.id, "pdf-tools");
    assert!(!registry.list().unwrap()[0].enabled);
}

#[test]
fn unmapped_allowed_tools_block_install() {
    let workspace = Workspace::new();
    let text = skill_md(
        "shell-helper",
        "包装 shell 调用。",
        &[("allowed-tools", "Read, Bash(git *)")],
        "正文指令。",
    );
    let manifest = parse_skill_markdown(&text, "SKILL.md").unwrap();
    assert_eq!(manifest.permissions, vec!["Read"]);
    assert_eq!(manifest.unmapped_permissions, vec!["Bash(git *)"]);

    let path = workspace.file("SKILL.md", &text);
    let store = workspace.store();
    let registry = workspace.registry();
    let error = install_skill_md(&store, &registry, &path, &workspace.dir("scratch"))
        .unwrap_err()
        .to_string();
    assert!(error.contains("Bash(git *)"), "实际错误: {error}");
}

#[test]
fn missing_description_rejected() {
    let text = "---\nname: bare\n---\n\n正文指令。\n";
    let error = parse_skill_markdown(text, "SKILL.md").unwrap_err();
    assert!(error.contains("description"), "实际错误: {error}");
}

#[test]
fn invalid_name_rejected_by_install_chain() {
    let workspace = Workspace::new();
    let path = workspace.file(
        "SKILL.md",
        &skill_md("PDF Tools", "说明。", &[], "正文指令。"),
    );
    let store = workspace.store();
    let registry = workspace.registry();
    let error = install_skill_md(&store, &registry, &path, &workspace.dir("scratch"))
        .unwrap_err()
        .to_string();
    assert!(error.contains("仅允许"), "实际错误: {error}");
}

#[test]
fn oversized_skill_md_rejected() {
    let workspace = Workspace::new();
    // "# 大技能\n" 约 12 字节，重复 12000 次远超 64 KiB 上限。
    let body = "# 大技能\n".repeat(12_000);
    let path = workspace.file("SKILL.md", &skill_md("big-skill", "说明。", &[], &body));
    let store = workspace.store();
    let registry = workspace.registry();
    let error = install_skill_md(&store, &registry, &path, &workspace.dir("scratch"))
        .unwrap_err()
        .to_string();
    assert!(error.contains("64 KiB"), "实际错误: {error}");
}

#[test]
fn missing_frontmatter_rejected() {
    let error = parse_skill_markdown("# 只是说明\n", "SKILL.md").unwrap_err();
    assert!(error.contains("frontmatter"), "实际错误: {error}");
}

#[test]
fn empty_body_rejected() {
    let text = "---\nname: quiet\ndescription: 说明。\n---\n";
    let error = parse_skill_markdown(text, "SKILL.md").unwrap_err();
    assert!(error.contains("正文"), "实际错误: {error}");
}

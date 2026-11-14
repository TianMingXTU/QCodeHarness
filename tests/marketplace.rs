//! 本地技能市场：目录索引与自定义 TOML 索引，按 id 安装、默认禁用。

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use qcode_harness::application::extension::ExtensionRegistry;
use qcode_harness::application::tool_registry::ToolRegistry;
use qcode_harness::infrastructure::extensions::LocalExtensionStore;
use qcode_harness::infrastructure::skill_md::{install_from_marketplace, load_marketplace_index};

static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

/// 每个测试独占的临时工作区，市场目录、索引与授权状态都放在里面。
struct Workspace(PathBuf);

impl Workspace {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "qch-marketplace-{}-{}",
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

    fn store(&self) -> LocalExtensionStore {
        LocalExtensionStore::with_state_directory(&self.0, Some(self.0.join("user-config")))
    }

    fn registry(&self) -> ExtensionRegistry {
        ExtensionRegistry::new(&ToolRegistry::new(Vec::new()))
    }
}

/// 在指定子目录写入一份最小 SKILL.md。
fn write_skill(workspace: &Workspace, relative_dir: &str, name: &str) {
    workspace.file(
        &format!("{relative_dir}/SKILL.md"),
        &format!(
            "---\nname: {name}\ndescription: {name} 的技能说明。\n---\n\n{name} 的正文指令。\n"
        ),
    );
}

#[test]
fn directory_market_lists_and_installs_by_id() {
    let workspace = Workspace::new();
    write_skill(&workspace, "market/alpha", "alpha");
    write_skill(&workspace, "market/beta", "beta");

    let market = workspace.0.join("market");
    let entries = load_marketplace_index(&market).unwrap();
    let ids: Vec<&str> = entries.iter().map(|entry| entry.id.as_str()).collect();
    assert_eq!(ids, vec!["alpha", "beta"]);

    let store = workspace.store();
    let registry = workspace.registry();
    let scratch = workspace.dir("scratch");
    let report = install_from_marketplace(&store, &registry, &market, "beta", &scratch).unwrap();
    assert_eq!(report.descriptor.id, "beta");
    assert!(!registry.list().unwrap()[0].enabled);

    let again = install_from_marketplace(&store, &registry, &market, "beta", &scratch)
        .unwrap_err()
        .to_string();
    assert!(again.contains("已安装"), "实际错误: {again}");
}

#[test]
fn custom_toml_index_resolves_relative_and_absolute_paths() {
    let workspace = Workspace::new();
    write_skill(&workspace, "market/alpha", "alpha");
    let delta_dir = workspace.dir("elsewhere");
    workspace.file(
        "elsewhere/SKILL.md",
        "---\nname: delta\ndescription: delta 说明。\n---\n\n正文。\n",
    );

    let index_path = workspace.file(
        "market/idx.toml",
        &format!(
            "[[skills]]\nid = \"alpha\"\npath = \"alpha/SKILL.md\"\n\n[[skills]]\nid = \"delta\"\npath = '{}'\n",
            delta_dir.join("SKILL.md").display()
        ),
    );

    let store = workspace.store();
    let registry = workspace.registry();
    let scratch = workspace.dir("scratch");
    let report =
        install_from_marketplace(&store, &registry, &index_path, "delta", &scratch).unwrap();
    assert_eq!(report.descriptor.id, "delta");

    let ghost = install_from_marketplace(&store, &registry, &index_path, "ghost", &scratch)
        .unwrap_err()
        .to_string();
    assert!(
        ghost.contains("没有技能") && ghost.contains("alpha"),
        "实际错误: {ghost}"
    );
}

#[test]
fn index_id_must_match_skill_name() {
    let workspace = Workspace::new();
    write_skill(&workspace, "market/alpha", "alpha");
    let index_path = workspace.file(
        "market/idx.toml",
        "[[skills]]\nid = \"wrong\"\npath = \"alpha/SKILL.md\"\n",
    );

    let store = workspace.store();
    let registry = workspace.registry();
    let error = install_from_marketplace(
        &store,
        &registry,
        &index_path,
        "wrong",
        &workspace.dir("scratch"),
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("不一致"), "实际错误: {error}");
}

#[test]
fn duplicate_index_ids_rejected() {
    let workspace = Workspace::new();
    write_skill(&workspace, "market/alpha", "alpha");
    let index_path = workspace.file(
        "market/idx.toml",
        "[[skills]]\nid = \"alpha\"\npath = \"alpha/SKILL.md\"\n\n[[skills]]\nid = \"alpha\"\npath = \"alpha/SKILL.md\"\n",
    );
    let error = load_marketplace_index(&index_path).unwrap_err().to_string();
    assert!(error.contains("重复"), "实际错误: {error}");
}

#[test]
fn broken_skill_fails_directory_scan() {
    let workspace = Workspace::new();
    workspace.file("market/gamma/SKILL.md", "---\nname: gamma\n---\n\n正文。\n");
    let market = workspace.0.join("market");
    let error = load_marketplace_index(&market).unwrap_err().to_string();
    assert!(error.contains("description"), "实际错误: {error}");
}

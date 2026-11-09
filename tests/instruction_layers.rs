//! 分层指令的装配验收；只读取选定任务目录的祖先链。

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use qcode_harness::domain::platform::Platform;
use qcode_harness::infrastructure::instructions::{
    load_layered_instructions, load_project_instructions,
};

const FILE_LIMIT: usize = 8 * 1024;
const TOTAL_LIMIT: usize = 32 * 1024;

struct TestTree(PathBuf);

impl TestTree {
    fn new(name: &str) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "qch-layered-{}-{name}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).expect("建临时目录");
        Self(path)
    }

    fn dir(&self, relative: &str) -> PathBuf {
        let path: PathBuf = self.0.join(relative).components().collect();
        fs::create_dir_all(&path).expect("建测试目录");
        path
    }
}

impl Drop for TestTree {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).expect("清理测试目录");
    }
}

fn write_instruction(directory: &Path, relative: &str, body: impl AsRef<[u8]>) -> PathBuf {
    let path = directory.join(relative);
    fs::create_dir_all(path.parent().unwrap()).expect("建指令目录");
    fs::write(&path, body).expect("写测试指令");
    path
}

fn link_dir(link: &Path, target: &Path) {
    if Platform::current() == Platform::Windows {
        let link: PathBuf = link.components().collect();
        let target: PathBuf = target.components().collect();
        let output = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .output()
            .expect("建 junction");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    } else {
        #[cfg(unix)]
        std::os::unix::fs::symlink(target, link).expect("建符号链接");
    }
}

#[test]
fn layers_are_ordered_from_user_to_root_to_closest_directory() {
    let tree = TestTree::new("order");
    let user = tree.dir("config/qcodeharness");
    let workspace = tree.dir("workspace");
    let middle = tree.dir("workspace/crates");
    let scope = tree.dir("workspace/crates/parser");
    let expected = vec![
        write_instruction(&user, "QCODEHARNESS.md", "USER_LAYER"),
        write_instruction(&workspace, "QCODEHARNESS.md", "ROOT_LAYER"),
        write_instruction(&middle, "QCODEHARNESS.md", "MIDDLE_LAYER"),
        write_instruction(&scope, "QCODEHARNESS.md", "CLOSEST_LAYER"),
    ];

    let loaded = load_layered_instructions(Some(&user), &workspace, &scope).unwrap();

    assert_eq!(loaded.sources, expected);
    let positions: Vec<_> = ["USER_LAYER", "ROOT_LAYER", "MIDDLE_LAYER", "CLOSEST_LAYER"]
        .map(|body| loaded.text.find(body).expect("正文应进入提示词"))
        .into();
    assert!(positions.windows(2).all(|pair| pair[0] < pair[1]));
    for source in &loaded.sources {
        assert!(loaded.text.contains(&source.display().to_string()));
    }
    assert!(loaded.text.contains("优先级"));
    assert!(loaded.text.contains("仅适用于"));
    assert!(loaded.text.contains("工具权限"));
    assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);
}

#[test]
fn fallback_locations_are_chosen_once_per_layer() {
    let tree = TestTree::new("fallback");
    let user = tree.dir("config/qcodeharness");
    let workspace = tree.dir("workspace");
    let scope = tree.dir("workspace/module");
    let user_source = write_instruction(&user, "instructions.md", "USER_FALLBACK");
    let root_source =
        write_instruction(&workspace, ".qcodeharness/instructions.md", "ROOT_FALLBACK");
    let scope_source = write_instruction(&scope, ".qcodeharness/instructions.md", "SCOPE_FALLBACK");
    write_instruction(&user, "qcodeharness/instructions.md", "WRONG_CONFIG_BASE");

    let fallback = load_layered_instructions(Some(&user), &workspace, &scope).unwrap();
    assert_eq!(
        fallback.sources,
        vec![user_source, root_source, scope_source]
    );
    assert!(!fallback.text.contains("WRONG_CONFIG_BASE"));

    let preferred = vec![
        write_instruction(&user, "QCODEHARNESS.md", "USER_PRIMARY"),
        write_instruction(&workspace, "QCODEHARNESS.md", "ROOT_PRIMARY"),
        write_instruction(&scope, "QCODEHARNESS.md", "SCOPE_PRIMARY"),
    ];
    let loaded = load_layered_instructions(Some(&user), &workspace, &scope).unwrap();
    assert_eq!(loaded.sources, preferred);
    assert!(!loaded.text.contains("FALLBACK"));
}

#[test]
fn root_scope_is_loaded_only_once_and_missing_optional_files_are_empty() {
    let tree = TestTree::new("optional");
    let workspace = tree.dir("workspace");
    let missing_user = tree.0.join("missing-user-dir");
    let empty = load_layered_instructions(Some(&missing_user), &workspace, &workspace).unwrap();
    assert!(empty.text.is_empty());
    assert!(empty.sources.is_empty());
    assert!(empty.warnings.is_empty());

    let source = write_instruction(&workspace, "QCODEHARNESS.md", "ROOT_ONCE");
    let loaded = load_layered_instructions(None, &workspace, &workspace).unwrap();
    assert_eq!(loaded.sources, vec![source]);
    assert_eq!(loaded.text.matches("ROOT_ONCE").count(), 1);
}

#[test]
fn explicit_user_directory_is_inherited_by_child_workspace_without_parent_or_siblings() {
    let tree = TestTree::new("child");
    let user = tree.dir("config/qcodeharness");
    let parent = tree.dir("parent");
    let child = tree.dir("parent/worktrees/child");
    let scope = tree.dir("parent/worktrees/child/src");
    let sibling = tree.dir("parent/worktrees/sibling");
    let child_sibling = tree.dir("parent/worktrees/child/tests");
    let sources = vec![
        write_instruction(&user, "QCODEHARNESS.md", "INHERITED_USER"),
        write_instruction(&child, "QCODEHARNESS.md", "CHILD_ROOT"),
        write_instruction(&scope, "QCODEHARNESS.md", "CHILD_SCOPE"),
    ];
    write_instruction(&parent, "QCODEHARNESS.md", "PARENT_EXCLUDED");
    write_instruction(&sibling, "QCODEHARNESS.md", "WORKTREE_SIBLING_EXCLUDED");
    write_instruction(&child_sibling, "QCODEHARNESS.md", "TASK_SIBLING_EXCLUDED");

    let loaded = load_layered_instructions(Some(&user), &child, &scope).unwrap();
    assert_eq!(loaded.sources, sources);
    assert!(!loaded.text.contains("EXCLUDED"));
}

#[test]
fn relative_scope_and_current_directory_components_are_normalized() {
    let tree = TestTree::new("normalized");
    let workspace = tree.dir("workspace");
    let scope = tree.dir("workspace/src/parser");
    let source = write_instruction(&scope, "QCODEHARNESS.md", "NORMALIZED_SCOPE");

    let loaded =
        load_layered_instructions(None, &workspace.join("."), Path::new("./src/./parser/."))
            .unwrap();
    assert_eq!(loaded.sources, vec![source]);
}

#[test]
fn absolute_scope_case_aliases_follow_the_platform_path_rules() {
    if !Platform::current().has_case_insensitive_paths() {
        return;
    }
    let tree = TestTree::new("path-case");
    let workspace = tree.dir("workspace");
    let scope = tree.dir("workspace/src");
    write_instruction(&scope, "QCODEHARNESS.md", "CASE_NORMALIZED_SCOPE");
    let workspace_alias = PathBuf::from(workspace.to_str().unwrap().to_uppercase());
    let scope_alias = PathBuf::from(scope.to_str().unwrap().to_uppercase());

    for (root, selected) in [(&workspace, &scope_alias), (&workspace_alias, &scope)] {
        let loaded = load_layered_instructions(None, root, selected).unwrap();
        assert!(loaded.text.contains("CASE_NORMALIZED_SCOPE"));
    }
}

#[test]
fn canonical_and_regular_absolute_paths_can_select_the_same_scope() {
    let tree = TestTree::new("canonical-inputs");
    let workspace = tree.dir("workspace");
    let scope = tree.dir("workspace/src");
    write_instruction(&scope, "QCODEHARNESS.md", "CANONICAL_SCOPE");
    let canonical_workspace = fs::canonicalize(&workspace).unwrap();
    let canonical_scope = fs::canonicalize(&scope).unwrap();

    for (root, selected) in [
        (&canonical_workspace, &scope),
        (&workspace, &canonical_scope),
    ] {
        let loaded = load_layered_instructions(None, root, selected).unwrap();
        assert!(loaded.text.contains("CANONICAL_SCOPE"));
    }
}

#[test]
fn traversal_and_absolute_scopes_outside_workspace_are_rejected() {
    let tree = TestTree::new("lexical");
    let workspace = tree.dir("workspace");
    let outside = tree.dir("workspace-suffix");
    tree.dir("workspace/child");

    for scope in [
        Path::new("../workspace-suffix"),
        Path::new("child/.."),
        &outside,
    ] {
        assert!(
            load_layered_instructions(None, &workspace, scope).is_err(),
            "{scope:?}"
        );
    }
    assert!(load_layered_instructions(None, &workspace.join("child/.."), &workspace).is_err());
}

#[test]
fn workspace_and_scope_must_be_existing_directories() {
    let tree = TestTree::new("directories");
    let workspace = tree.dir("workspace");
    let file = write_instruction(&workspace, "regular-file", "not a directory");
    let missing = workspace.join("missing");
    for invalid in [&file, &missing] {
        assert!(load_layered_instructions(None, invalid, invalid).is_err());
        assert!(load_layered_instructions(None, &workspace, invalid).is_err());
    }
}

#[test]
fn oversized_utf8_files_are_capped_individually_with_source_warnings() {
    let tree = TestTree::new("file-limit");
    let workspace = tree.dir("workspace");
    let scope = tree.dir("workspace/src");
    let source = write_instruction(&workspace, "QCODEHARNESS.md", "界".repeat(FILE_LIMIT));
    write_instruction(&scope, "QCODEHARNESS.md", "CLOSEST_STILL_PRESENT");

    let loaded = load_layered_instructions(None, &workspace, &scope).unwrap();
    assert_eq!(
        loaded.text.matches('界').count(),
        FILE_LIMIT / '界'.len_utf8()
    );
    assert!(!loaded.text.contains('\u{FFFD}'));
    assert!(loaded.text.contains("CLOSEST_STILL_PRESENT"));
    assert!(loaded.text.contains("截断"));
    assert!(loaded.warnings.iter().any(
        |warning| warning.contains("8 KiB") && warning.contains(&source.display().to_string())
    ));
}

#[test]
fn bytes_beyond_the_file_cap_are_not_decoded_or_loaded() {
    let tree = TestTree::new("bounded-read");
    let workspace = tree.dir("workspace");
    let mut body = vec![b'x'; FILE_LIMIT];
    body.extend([0xff; 1024]);
    write_instruction(&workspace, "QCODEHARNESS.md", body);

    let loaded = load_layered_instructions(None, &workspace, &workspace).unwrap();
    assert!(loaded.text.contains(&"x".repeat(FILE_LIMIT)));
    assert!(
        loaded
            .warnings
            .iter()
            .any(|warning| warning.contains("8 KiB"))
    );
    assert!(
        !loaded
            .warnings
            .iter()
            .any(|warning| warning.contains("读不了"))
    );
}

#[test]
fn exact_file_limit_does_not_warn_or_truncate() {
    let tree = TestTree::new("exact-file-limit");
    let workspace = tree.dir("workspace");
    write_instruction(&workspace, "QCODEHARNESS.md", "§".repeat(FILE_LIMIT / 2));

    let loaded = load_layered_instructions(None, &workspace, &workspace).unwrap();
    assert_eq!(loaded.text.matches('§').count(), FILE_LIMIT / 2);
    assert!(loaded.warnings.is_empty());
}

#[test]
fn combined_prompt_is_utf8_safe_and_bounded_including_headers_and_notice() {
    let tree = TestTree::new("combined-limit");
    let user = tree.dir("config/qcodeharness");
    let workspace = tree.dir("workspace");
    let mut scope = workspace.clone();
    write_instruction(&user, "QCODEHARNESS.md", "约".repeat(FILE_LIMIT));
    write_instruction(&workspace, "QCODEHARNESS.md", "规".repeat(FILE_LIMIT));
    for depth in 0..8 {
        scope.push(format!("level-{depth}"));
        fs::create_dir(&scope).unwrap();
        write_instruction(&scope, "QCODEHARNESS.md", "定".repeat(FILE_LIMIT));
    }

    let loaded = load_layered_instructions(Some(&user), &workspace, &scope).unwrap();
    assert!(loaded.text.len() <= TOTAL_LIMIT, "{}", loaded.text.len());
    assert!(loaded.text.contains("32 KiB"));
    assert!(loaded.text.contains("截断"));
    assert!(!loaded.text.contains('\u{FFFD}'));
    assert!(
        loaded
            .warnings
            .iter()
            .any(|warning| warning.contains("32 KiB"))
    );
    assert!(
        loaded.sources.len() < 10,
        "总上限后不继续读入祖先链后续文件"
    );
}

#[test]
fn malformed_utf8_is_visible_and_does_not_fall_back_or_hide_other_layers() {
    let tree = TestTree::new("invalid-utf8");
    let workspace = tree.dir("workspace");
    let scope = tree.dir("workspace/src");
    let bad = write_instruction(&workspace, "QCODEHARNESS.md", [0xff, 0xfe]);
    write_instruction(
        &workspace,
        ".qcodeharness/instructions.md",
        "FALLBACK_MUST_NOT_HIDE_FAILURE",
    );
    write_instruction(&scope, "QCODEHARNESS.md", "VALID_NESTED");

    let loaded = load_layered_instructions(None, &workspace, &scope).unwrap();
    assert!(
        loaded
            .warnings
            .iter()
            .any(|warning| warning.contains(&bad.display().to_string())
                && warning.contains("读不了"))
    );
    assert!(loaded.text.contains("VALID_NESTED"));
    assert!(!loaded.text.contains("FALLBACK_MUST_NOT_HIDE_FAILURE"));
}

#[test]
fn an_instruction_path_that_is_a_directory_is_visible_as_a_warning() {
    let tree = TestTree::new("not-a-file");
    let workspace = tree.dir("workspace");
    let invalid = tree.dir("workspace/QCODEHARNESS.md");

    let loaded = load_layered_instructions(None, &workspace, &workspace).unwrap();
    assert!(
        loaded
            .warnings
            .iter()
            .any(|warning| warning.contains(&invalid.display().to_string()))
    );
}

#[test]
fn scope_directory_link_cannot_escape_workspace() {
    let tree = TestTree::new("scope-link");
    let workspace = tree.dir("workspace");
    let outside = tree.dir("outside");
    write_instruction(&outside, "QCODEHARNESS.md", "OUTSIDE_SECRET");
    let link = workspace.join("linked-scope");
    link_dir(&link, &outside);

    assert!(load_layered_instructions(None, &workspace, &link).is_err());
}

#[test]
fn fallback_directory_link_cannot_escape_even_when_target_instruction_is_missing() {
    let tree = TestTree::new("fallback-link");
    let workspace = tree.dir("workspace");
    let outside = tree.dir("outside");
    link_dir(&workspace.join(".qcodeharness"), &outside);

    assert!(load_layered_instructions(None, &workspace, &workspace).is_err());
}

#[test]
fn directory_links_inside_workspace_remain_usable() {
    let tree = TestTree::new("inside-link");
    let workspace = tree.dir("workspace");
    let target = tree.dir("workspace/actual");
    write_instruction(&target, "QCODEHARNESS.md", "SAFE_LINKED_SCOPE");
    let link = workspace.join("linked-scope");
    link_dir(&link, &target);

    let loaded = load_layered_instructions(None, &workspace, &link).unwrap();
    assert!(loaded.text.contains("SAFE_LINKED_SCOPE"));
    assert!(loaded.warnings.is_empty());
}

#[test]
fn a_large_file_is_read_only_through_the_capped_utf8_prefix() {
    use std::io::{Seek, SeekFrom, Write};

    let tree = TestTree::new("bounded-io");
    let workspace = tree.dir("workspace");
    let source = write_instruction(&workspace, "QCODEHARNESS.md", "§".repeat(FILE_LIMIT / 2));
    let mut file = fs::OpenOptions::new().write(true).open(&source).unwrap();
    file.set_len(128 * 1024 * 1024).unwrap();
    file.seek(SeekFrom::Start(FILE_LIMIT as u64)).unwrap();
    file.write_all(&[0xff]).unwrap();
    drop(file);

    let loaded = load_layered_instructions(None, &workspace, &workspace).unwrap();
    assert_eq!(loaded.text.matches('§').count(), FILE_LIMIT / 2);
    assert!(
        loaded
            .warnings
            .iter()
            .any(|warning| warning.contains("8 KiB"))
    );
    assert!(
        !loaded
            .warnings
            .iter()
            .any(|warning| warning.contains("读不了"))
    );
}

fn link_file(link: &Path, target: &Path) -> bool {
    #[cfg(windows)]
    let result = std::os::windows::fs::symlink_file(target, link);
    #[cfg(unix)]
    let result = std::os::unix::fs::symlink(target, link);
    match result {
        Ok(()) => true,
        Err(error)
            if Platform::current() == Platform::Windows
                && error.kind() == std::io::ErrorKind::PermissionDenied =>
        {
            eprintln!("当前进程没有创建文件符号链接的权限；目录 junction 越界用例仍执行：{error}");
            false
        }
        Err(error) => panic!("建文件符号链接失败：{error}"),
    }
}

#[test]
fn instruction_file_links_cannot_escape_workspace() {
    let tree = TestTree::new("file-link");
    let workspace = tree.dir("workspace");
    let outside = tree.dir("outside");
    let target = write_instruction(&outside, "secret.md", "OUTSIDE_SECRET");
    if link_file(&workspace.join("QCODEHARNESS.md"), &target) {
        assert!(load_layered_instructions(None, &workspace, &workspace).is_err());
    }
}

#[test]
fn broken_instruction_links_are_visible() {
    let tree = TestTree::new("broken-file-link");
    let workspace = tree.dir("workspace");
    if !link_file(
        &workspace.join("QCODEHARNESS.md"),
        &workspace.join("missing.md"),
    ) {
        return;
    }
    match load_layered_instructions(None, &workspace, &workspace) {
        Ok(loaded) => assert!(!loaded.warnings.is_empty()),
        Err(error) => assert!(!error.is_empty()),
    }
}

#[test]
fn legacy_api_keeps_its_raw_body_and_truncation_notice() {
    let tree = TestTree::new("legacy");
    let workspace = tree.dir("workspace");
    let source = write_instruction(&workspace, "QCODEHARNESS.md", "x".repeat(FILE_LIMIT * 2));

    let loaded = load_project_instructions(&workspace).unwrap();
    assert_eq!(loaded.source, source);
    assert!(loaded.truncated);
    assert!(loaded.text.starts_with(&"x".repeat(FILE_LIMIT)));
    assert!(loaded.text.contains("截断"));
}

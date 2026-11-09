//! 真实 Git 仓库中的隔离、批量合并和失败原子性验收；缺少 Git 时直接失败。

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

use qcode_harness::application::isolation::{
    ChangeSet, IsolationWorkspace, WorkspaceIsolation, WorkspaceVerifier,
};
use qcode_harness::application::port::BoxFuture;
use qcode_harness::domain::platform::Platform;
use qcode_harness::domain::sandbox::PlatformCapabilities;
use qcode_harness::domain::workspace::WorkspaceRoot;
use qcode_harness::infrastructure::git_worktree::GitWorktreeManager;

struct Repo(PathBuf);

impl Repo {
    fn new(label: &str) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "qch-isolation-{}-{label}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).expect("建独立测试目录");
        git(&root, &["init", "-q"]);
        git(&root, &["config", "core.autocrlf", "false"]);
        git(&root, &["config", "core.quotePath", "true"]);
        write(&root, ".gitignore", b"target/\n.env\n");
        write(&root, "alpha.txt", b"alpha baseline\n");
        write(&root, "beta.txt", b"beta baseline\n");
        write(&root, ".env.example", b"BASELINE_SECRET=do-not-copy\n");
        git(&root, &["add", "."]);
        git(&root, &["commit", "-qm", "baseline"]);
        Self(fs::canonicalize(root).expect("规范化测试仓库根"))
    }

    async fn manager(&self) -> GitWorktreeManager {
        GitWorktreeManager::new(WorkspaceRoot::new(&self.0))
            .await
            .expect("构造隔离器")
    }

    fn head(&self) -> String {
        String::from_utf8(git(&self.0, &["rev-parse", "HEAD"]))
            .unwrap()
            .trim()
            .to_owned()
    }

    fn index(&self) -> Vec<u8> {
        fs::read(self.0.join(".git/index")).expect("读父索引")
    }

    fn worktrees(&self) -> Vec<u8> {
        git(&self.0, &["worktree", "list", "--porcelain", "-z"])
    }
}

impl Drop for Repo {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_dir_all(&self.0)
            && !std::thread::panicking()
        {
            panic!("清理 {} 失败: {error}", self.0.display());
        }
    }
}

fn git_output(root: &Path, args: &[&str]) -> Output {
    let mut command = Command::new("git");
    command
        .args([
            "-c",
            "user.name=isolation-test",
            "-c",
            "user.email=isolation@example.invalid",
            "-c",
            "commit.gpgSign=false",
            "-c",
            "tag.gpgSign=false",
            "-c",
            "core.hooksPath=",
        ])
        .args(args)
        .current_dir(root)
        .stdin(Stdio::null());
    for (name, _) in std::env::vars_os() {
        if name
            .to_string_lossy()
            .to_ascii_uppercase()
            .starts_with("GIT_")
        {
            command.env_remove(name);
        }
    }
    command.env("GIT_OPTIONAL_LOCKS", "0");
    command.output().expect("必须安装 Git 并加入 PATH")
}

fn git(root: &Path, args: &[&str]) -> Vec<u8> {
    let output = git_output(root, args);
    assert!(
        output.status.success(),
        "git {args:?} 失败: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

fn write(root: &Path, path: &str, bytes: &[u8]) {
    let destination = root.join(path);
    fs::create_dir_all(destination.parent().unwrap()).expect("建测试文件目录");
    fs::write(destination, bytes).expect("写测试文件");
}

struct Accept;

impl WorkspaceVerifier for Accept {
    fn verify<'a>(&'a self, _root: &'a Path) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }
}

struct Reject;

impl WorkspaceVerifier for Reject {
    fn verify<'a>(&'a self, _root: &'a Path) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async { Err("candidate tests failed".to_owned()) })
    }
}

async fn changed(
    manager: &GitWorktreeManager,
    id: &str,
    base: &str,
    body: &[u8],
) -> (IsolationWorkspace, ChangeSet) {
    let workspace = manager.create(id, base).await.unwrap();
    write(&workspace.root, "alpha.txt", body);
    let patch = manager.capture(&workspace).await.unwrap();
    (workspace, patch)
}

#[tokio::test]
async fn isolated_roots_share_only_committed_safe_files_and_are_detached() {
    let repo = Repo::new("roots");
    write(&repo.0, ".env", b"LOCAL_SECRET=never-copy\n");
    write(&repo.0, ".qcodeharness/sessions/run.json", b"state\n");
    let first_manager = repo.manager().await;
    let second_manager = repo.manager().await;
    let base = first_manager.prepare().await.unwrap();
    assert_eq!(base, repo.head());
    let first = first_manager.create("same-id", &base).await.unwrap();
    let second = second_manager.create("same-id", &base).await.unwrap();
    assert_ne!(first.root, second.root);
    assert_ne!(first.root.parent(), second.root.parent());
    assert!(
        first
            .root
            .starts_with(repo.0.join(".qcodeharness/worktrees"))
    );
    assert_eq!(first.base_revision, base);
    for workspace in [&first, &second] {
        assert_eq!(
            fs::read(workspace.root.join("alpha.txt")).unwrap(),
            b"alpha baseline\n"
        );
        assert!(!workspace.root.join(".env").exists());
        assert!(!workspace.root.join(".env.example").exists());
        assert!(
            !workspace
                .root
                .join(".qcodeharness/sessions/run.json")
                .exists()
        );
        assert_eq!(
            git_output(&workspace.root, &["symbolic-ref", "-q", "HEAD"])
                .status
                .code(),
            Some(1)
        );
    }
    write(&first.root, "alpha.txt", b"first only\n");
    assert_eq!(
        fs::read(second.root.join("alpha.txt")).unwrap(),
        b"alpha baseline\n"
    );
    assert_eq!(
        fs::read(repo.0.join("alpha.txt")).unwrap(),
        b"alpha baseline\n"
    );
    let one = first_manager.capture(&first).await.unwrap();
    assert!(!String::from_utf8_lossy(&one.patch).contains("BASELINE_SECRET"));
    assert_eq!(
        git(
            &repo.0,
            &["for-each-ref", "--format=%(refname)", "refs/heads"]
        )
        .split(|b| *b == b'\n')
        .filter(|x| !x.is_empty())
        .count(),
        1
    );
    first_manager.remove(&first).await.unwrap();
    second_manager.remove(&second).await.unwrap();
    assert_eq!(
        repo.worktrees()
            .split(|b| *b == 0)
            .filter(|entry| entry.starts_with(b"worktree "))
            .count(),
        1
    );
}

struct InspectCandidate<'a> {
    parent: &'a Path,
    called: AtomicUsize,
}

impl WorkspaceVerifier for InspectCandidate<'_> {
    fn verify<'a>(&'a self, root: &'a Path) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            self.called.fetch_add(1, Ordering::Relaxed);
            assert_ne!(root, self.parent);
            assert_eq!(
                fs::read(root.join("alpha.txt")).unwrap(),
                b"changed alpha\n"
            );
            assert!(!root.join("beta.txt").exists());
            assert_eq!(
                fs::read(root.join("new folder/文件.bin")).unwrap(),
                [0, 255, 1, 0, 42]
            );
            assert_eq!(
                fs::read(root.join("second.txt")).unwrap(),
                b"second new file\n"
            );
            assert_eq!(
                fs::read(self.parent.join("alpha.txt")).unwrap(),
                b"alpha baseline\n"
            );
            Ok(())
        })
    }
}

#[tokio::test]
async fn verified_batch_merges_new_modified_deleted_and_binary_files_without_staging_parent() {
    let repo = Repo::new("batch");
    let manager = repo.manager().await;
    let base = manager.prepare().await.unwrap();
    let first = manager.create("first", &base).await.unwrap();
    let second = manager.create("second", &base).await.unwrap();
    write(&first.root, "alpha.txt", b"changed alpha\n");
    write(&first.root, "new folder/文件.bin", &[0, 255, 1, 0, 42]);
    fs::remove_file(second.root.join("beta.txt")).unwrap();
    write(&second.root, "second.txt", b"second new file\n");
    let patches = [
        manager.capture(&first).await.unwrap(),
        manager.capture(&second).await.unwrap(),
    ];
    assert!(
        patches[0]
            .patch
            .windows(b"GIT binary patch".len())
            .any(|part| part == b"GIT binary patch")
    );
    let index = repo.index();
    let worktrees = repo.worktrees();
    let verifier = InspectCandidate {
        parent: &repo.0,
        called: AtomicUsize::new(0),
    };

    manager.merge(&base, &patches, &verifier).await.unwrap();

    assert_eq!(verifier.called.load(Ordering::Relaxed), 1);
    assert_eq!(
        fs::read(repo.0.join("alpha.txt")).unwrap(),
        b"changed alpha\n"
    );
    assert!(!repo.0.join("beta.txt").exists());
    assert_eq!(
        fs::read(repo.0.join("new folder/文件.bin")).unwrap(),
        [0, 255, 1, 0, 42]
    );
    assert_eq!(
        fs::read(repo.0.join("second.txt")).unwrap(),
        b"second new file\n"
    );
    assert_eq!(repo.index(), index);
    assert_eq!(repo.head(), base);
    assert!(git(&repo.0, &["diff", "--cached", "--exit-code"]).is_empty());
    assert_eq!(repo.worktrees(), worktrees);
    assert_eq!(
        fs::read(repo.0.join(".env.example")).unwrap(),
        b"BASELINE_SECRET=do-not-copy\n"
    );
    manager.remove(&first).await.unwrap();
    manager.remove(&second).await.unwrap();
}

#[tokio::test]
async fn overlapping_changes_fail_without_any_parent_or_index_change_and_clean_integration() {
    let repo = Repo::new("conflict");
    let manager = repo.manager().await;
    let base = manager.prepare().await.unwrap();
    let (first, left) = changed(&manager, "left", &base, b"left\n").await;
    let (second, right) = changed(&manager, "right", &base, b"right\n").await;
    let worktrees = repo.worktrees();
    let index = repo.index();

    assert!(manager.merge(&base, &[left, right], &Accept).await.is_err());

    assert_eq!(
        fs::read(repo.0.join("alpha.txt")).unwrap(),
        b"alpha baseline\n"
    );
    assert_eq!(repo.index(), index);
    assert_eq!(repo.worktrees(), worktrees);
    manager.remove(&first).await.unwrap();
    manager.remove(&second).await.unwrap();
}

#[tokio::test]
async fn rejected_candidate_leaves_parent_bytes_and_index_untouched() {
    let repo = Repo::new("verification");
    let manager = repo.manager().await;
    let base = manager.prepare().await.unwrap();
    let (workspace, patch) = changed(&manager, "candidate", &base, b"candidate\n").await;
    let worktrees = repo.worktrees();
    let index = repo.index();

    let error = manager.merge(&base, &[patch], &Reject).await.unwrap_err();

    assert!(error.contains("candidate tests failed"), "{error}");
    assert_eq!(
        fs::read(repo.0.join("alpha.txt")).unwrap(),
        b"alpha baseline\n"
    );
    assert_eq!(repo.index(), index);
    assert_eq!(repo.worktrees(), worktrees);
    manager.remove(&workspace).await.unwrap();
}

#[tokio::test]
async fn dirty_tracked_staged_and_untracked_parent_files_are_rejected() {
    let repo = Repo::new("dirty");
    let manager = repo.manager().await;
    let base = manager.prepare().await.unwrap();
    let (workspace, patch) = changed(&manager, "candidate", &base, b"candidate\n").await;
    for staged in [false, true] {
        write(&repo.0, "alpha.txt", b"user edit\n");
        if staged {
            git(&repo.0, &["add", "alpha.txt"]);
        }
        let index = repo.index();
        assert!(manager.prepare().await.is_err());
        assert!(
            manager
                .merge(&base, std::slice::from_ref(&patch), &Accept)
                .await
                .is_err()
        );
        assert_eq!(repo.index(), index);
        assert_eq!(fs::read(repo.0.join("alpha.txt")).unwrap(), b"user edit\n");
        git(&repo.0, &["reset", "--hard", "HEAD"]);
    }
    write(&repo.0, "user-untracked.txt", b"user-owned\n");
    assert!(manager.prepare().await.is_err());
    assert!(manager.merge(&base, &[patch], &Accept).await.is_err());
    assert_eq!(
        fs::read(repo.0.join("user-untracked.txt")).unwrap(),
        b"user-owned\n"
    );
    manager.remove(&workspace).await.unwrap();
}

#[tokio::test]
async fn moved_parent_head_is_rejected_even_when_clean() {
    let repo = Repo::new("moved-head");
    let manager = repo.manager().await;
    let base = manager.prepare().await.unwrap();
    let (workspace, patch) = changed(&manager, "candidate", &base, b"candidate\n").await;
    git(&repo.0, &["commit", "--allow-empty", "-qm", "user commit"]);
    let head = repo.head();
    let index = repo.index();
    assert!(manager.merge(&base, &[patch], &Accept).await.is_err());
    assert_eq!(repo.head(), head);
    assert_eq!(repo.index(), index);
    assert_eq!(
        fs::read(repo.0.join("alpha.txt")).unwrap(),
        b"alpha baseline\n"
    );
    manager.remove(&workspace).await.unwrap();
}

struct ChangeParent<'a>(&'a Path);

impl WorkspaceVerifier for ChangeParent<'_> {
    fn verify<'a>(&'a self, _root: &'a Path) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            write(self.0, "beta.txt", b"user change during verification\n");
            Ok(())
        })
    }
}

#[tokio::test]
async fn parent_is_rechecked_after_verification() {
    let repo = Repo::new("final-check");
    let manager = repo.manager().await;
    let base = manager.prepare().await.unwrap();
    let (workspace, patch) = changed(&manager, "candidate", &base, b"candidate\n").await;
    let index = repo.index();
    let worktrees = repo.worktrees();
    assert!(
        manager
            .merge(&base, &[patch], &ChangeParent(&repo.0))
            .await
            .is_err()
    );
    assert_eq!(
        fs::read(repo.0.join("alpha.txt")).unwrap(),
        b"alpha baseline\n"
    );
    assert_eq!(
        fs::read(repo.0.join("beta.txt")).unwrap(),
        b"user change during verification\n"
    );
    assert_eq!(repo.index(), index);
    assert_eq!(repo.worktrees(), worktrees);
    manager.remove(&workspace).await.unwrap();
}

struct ChangeCandidate;

impl WorkspaceVerifier for ChangeCandidate {
    fn verify<'a>(&'a self, root: &'a Path) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            write(root, "alpha.txt", b"validation rewrote source\n");
            Ok(())
        })
    }
}

#[tokio::test]
async fn verifier_cannot_change_the_candidate_that_will_be_applied() {
    let repo = Repo::new("candidate-mutation");
    let manager = repo.manager().await;
    let base = manager.prepare().await.unwrap();
    let (workspace, patch) = changed(&manager, "candidate", &base, b"candidate\n").await;
    let index = repo.index();
    assert!(
        manager
            .merge(&base, &[patch], &ChangeCandidate)
            .await
            .is_err()
    );
    assert_eq!(
        fs::read(repo.0.join("alpha.txt")).unwrap(),
        b"alpha baseline\n"
    );
    assert_eq!(repo.index(), index);
    manager.remove(&workspace).await.unwrap();
}

#[tokio::test]
async fn rejects_discovery_from_a_subdirectory_and_invalid_ids_or_revisions() {
    let repo = Repo::new("input");
    fs::create_dir(repo.0.join("nested")).unwrap();
    assert!(
        GitWorktreeManager::new(WorkspaceRoot::new(repo.0.join("nested")))
            .await
            .is_err()
    );
    let manager = repo.manager().await;
    let base = manager.prepare().await.unwrap();
    for id in [
        "",
        ".",
        "..",
        "../escape",
        "a/b",
        "a\\b",
        "C:escape",
        ".hidden",
        "-flag",
    ] {
        assert!(manager.create(id, &base).await.is_err(), "id={id:?}");
    }
    assert!(manager.create(&"a".repeat(65), &base).await.is_err());
    for revision in [
        "HEAD",
        "--help",
        "abcd",
        "ffffffffffffffffffffffffffffffffffffffff",
    ] {
        assert!(
            manager.create("valid", revision).await.is_err(),
            "base={revision:?}"
        );
    }
}

#[tokio::test]
async fn forged_workspace_cannot_capture_or_remove_parent_or_other_owned_children() {
    let repo = Repo::new("forged");
    let manager = repo.manager().await;
    let base = manager.prepare().await.unwrap();
    let workspace = manager.create("owned", &base).await.unwrap();
    let mut forged = workspace.clone();
    forged.root = repo.0.clone();
    assert!(manager.capture(&forged).await.is_err());
    assert!(manager.remove(&forged).await.is_err());
    forged = workspace.clone();
    forged.base_revision = "f".repeat(40);
    assert!(manager.capture(&forged).await.is_err());
    assert!(manager.remove(&forged).await.is_err());
    assert!(repo.0.join(".git/index").exists());
    manager.remove(&workspace).await.unwrap();
    assert!(!workspace.root.exists());
}

#[tokio::test]
async fn protected_child_files_and_nested_git_repositories_are_rejected_but_state_is_excluded() {
    let repo = Repo::new("protected-child");
    let manager = repo.manager().await;
    let base = manager.prepare().await.unwrap();
    let workspace = manager.create("protected", &base).await.unwrap();
    write(&workspace.root, ".env.local", b"credential\n");
    assert!(manager.capture(&workspace).await.is_err());
    fs::remove_file(workspace.root.join(".env.local")).unwrap();
    write(
        &workspace.root,
        "nested/.env.production",
        b"nested credential\n",
    );
    assert!(manager.capture(&workspace).await.is_err());
    fs::remove_file(workspace.root.join("nested/.env.production")).unwrap();
    let nested = workspace.root.join("nested");
    git(&nested, &["init", "-q"]);
    write(&nested, "file.txt", b"nested repository\n");
    git(&nested, &["add", "."]);
    git(&nested, &["commit", "-qm", "nested baseline"]);
    assert!(manager.capture(&workspace).await.is_err());
    fs::remove_dir_all(&nested).unwrap();
    write(
        &workspace.root,
        ".qcodeharness/sessions/state.json",
        b"allowed state\n",
    );
    write(&workspace.root, "alpha.txt", b"allowed source\n");
    let change = manager.capture(&workspace).await.unwrap();
    assert!(!String::from_utf8_lossy(&change.patch).contains("state.json"));
    manager.remove(&workspace).await.unwrap();
}

fn new_file_patch(path: &str) -> Vec<u8> {
    format!(
        "diff --git a/{path} b/{path}\nnew file mode 100644\n--- /dev/null\n\
         +++ b/{path}\n@@ -0,0 +1 @@\n+unsafe\n"
    )
    .into_bytes()
}

#[tokio::test]
async fn malicious_changesets_cannot_write_protected_or_escaping_paths() {
    let repo = Repo::new("malicious-patch");
    let manager = repo.manager().await;
    let base = manager.prepare().await.unwrap();
    let index = repo.index();
    let worktrees = repo.worktrees();
    for path in [
        ".env.local",
        "nested/.git/config",
        "nested/.qcodeharness/state",
        "../escape",
        ".ENV.production",
    ] {
        let change = ChangeSet {
            id: "untrusted".to_owned(),
            patch: new_file_patch(path),
        };
        assert!(
            manager.merge(&base, &[change], &Accept).await.is_err(),
            "path={path}"
        );
        assert_eq!(repo.index(), index);
        assert_eq!(repo.worktrees(), worktrees);
    }
    assert!(!repo.0.join(".env.local").exists());
    assert!(!repo.0.join("nested").exists());
}

fn link_directory(link: &Path, target: &Path) {
    if PlatformCapabilities::detect().platform == Platform::Windows {
        let link: PathBuf = link.components().collect();
        let target: PathBuf = target.components().collect();
        let output = Command::new("cmd")
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

#[tokio::test]
async fn symlink_or_junction_paths_and_staged_submodules_are_rejected() {
    let repo = Repo::new("links");
    let outside = Repo::new("outside");
    let manager = repo.manager().await;
    let base = manager.prepare().await.unwrap();
    let workspace = manager.create("links", &base).await.unwrap();
    let linked = workspace.root.join("linked");
    link_directory(&linked, &outside.0);
    assert!(manager.capture(&workspace).await.is_err());
    if PlatformCapabilities::detect().platform == Platform::Windows {
        fs::remove_dir(&linked).unwrap();
    } else {
        fs::remove_file(&linked).unwrap();
    }
    git(
        &workspace.root,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{base},submodule"),
        ],
    );
    assert!(manager.capture(&workspace).await.is_err());
    git(
        &workspace.root,
        &["update-index", "--force-remove", "submodule"],
    );
    let blob = String::from_utf8(git(&repo.0, &["rev-parse", "HEAD:alpha.txt"])).unwrap();
    git(
        &workspace.root,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("120000,{},symlink", blob.trim()),
        ],
    );
    assert!(manager.capture(&workspace).await.is_err());
    assert_eq!(
        fs::read(outside.0.join("alpha.txt")).unwrap(),
        b"alpha baseline\n"
    );
    manager.remove(&workspace).await.unwrap();
}

#[tokio::test]
async fn materializing_a_skipped_protected_baseline_path_is_rejected() {
    let repo = Repo::new("protected-baseline");
    let manager = repo.manager().await;
    let base = manager.prepare().await.unwrap();
    let workspace = manager.create("worker", &base).await.unwrap();
    write(&workspace.root, ".env.example", b"changed secret\n");
    assert!(manager.capture(&workspace).await.is_err());
    assert_eq!(
        fs::read(repo.0.join(".env.example")).unwrap(),
        b"BASELINE_SECRET=do-not-copy\n"
    );
    manager.remove(&workspace).await.unwrap();
}

#[tokio::test]
async fn incoming_symlink_and_gitlink_modes_are_rejected() {
    let repo = Repo::new("malicious-modes");
    let manager = repo.manager().await;
    let base = manager.prepare().await.unwrap();
    let index = repo.index();
    let worktrees = repo.worktrees();
    let patches = [
        String::from_utf8(new_file_patch("link"))
            .unwrap()
            .replace("100644", "120000"),
        format!(
            "diff --git a/module b/module\nnew file mode 160000\nindex 0000000..{base}\n\
             --- /dev/null\n+++ b/module\n@@ -0,0 +1 @@\n+Subproject commit {base}\n"
        ),
    ];
    for patch in patches {
        let change = ChangeSet {
            id: "mode".to_owned(),
            patch: patch.into_bytes(),
        };
        assert!(manager.merge(&base, &[change], &Accept).await.is_err());
        assert_eq!(repo.index(), index);
        assert_eq!(repo.worktrees(), worktrees);
    }
    assert!(!repo.0.join("link").exists());
    assert!(!repo.0.join("module").exists());
}

#[tokio::test]
async fn inherited_git_environment_cannot_redirect_the_managers_repository_or_index() {
    const CHILD_MARKER: &str = "QCH_ISOLATION_ENV_TEST_CHILD";
    if std::env::var_os(CHILD_MARKER).is_some() {
        let repo = Repo::new("env-child");
        let manager = repo.manager().await;
        let base = manager.prepare().await.unwrap();
        let (workspace, change) = changed(&manager, "worker", &base, b"safe target\n").await;
        let index = repo.index();
        manager.merge(&base, &[change], &Accept).await.unwrap();
        assert_eq!(
            fs::read(repo.0.join("alpha.txt")).unwrap(),
            b"safe target\n"
        );
        assert_eq!(repo.index(), index);
        manager.remove(&workspace).await.unwrap();
        return;
    }
    let outside = Repo::new("env-outside");
    let index = outside.index();
    let head = outside.head();
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "inherited_git_environment_cannot_redirect_the_managers_repository_or_index",
            "--nocapture",
        ])
        .env(CHILD_MARKER, "1")
        .env("GIT_DIR", outside.0.join(".git"))
        .env("GIT_COMMON_DIR", outside.0.join(".git"))
        .env("GIT_WORK_TREE", &outside.0)
        .env("GIT_INDEX_FILE", outside.0.join(".git/index"))
        .env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", "core.bare")
        .env("GIT_CONFIG_VALUE_0", "true")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(outside.index(), index);
    assert_eq!(outside.head(), head);
    assert_eq!(
        fs::read(outside.0.join("alpha.txt")).unwrap(),
        b"alpha baseline\n"
    );
}

#[tokio::test]
async fn isolation_never_runs_checkout_index_hooks_or_creates_signed_commits() {
    let repo = Repo::new("hooks");
    for hook in ["post-checkout", "post-index-change"] {
        let path = format!(".git/hooks/{hook}");
        write(
            &repo.0,
            &path,
            b"#!/bin/sh\nprintf triggered > hook-ran.txt\n",
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(repo.0.join(&path), fs::Permissions::from_mode(0o755)).unwrap();
        }
    }
    let hooks = repo.0.join(".git/hooks");
    git(
        &repo.0,
        &["config", "core.hooksPath", hooks.to_str().unwrap()],
    );
    git(&repo.0, &["config", "commit.gpgSign", "true"]);
    git(
        &repo.0,
        &[
            "config",
            "gpg.program",
            "nonexistent-isolation-signing-program",
        ],
    );
    let manager = repo.manager().await;
    let base = manager.prepare().await.unwrap();
    let (workspace, change) = changed(&manager, "worker", &base, b"safe change\n").await;
    assert!(!workspace.root.join("hook-ran.txt").exists());
    manager.merge(&base, &[change], &Accept).await.unwrap();
    assert!(!repo.0.join("hook-ran.txt").exists());
    assert_eq!(repo.head(), base);
    manager.remove(&workspace).await.unwrap();
}

#[tokio::test]
async fn nul_delimited_paths_preserve_git_quoted_names() {
    let repo = Repo::new("quoted-paths");
    let manager = repo.manager().await;
    let base = manager.prepare().await.unwrap();
    let workspace = manager.create("names", &base).await.unwrap();
    let file = if PlatformCapabilities::detect().platform == Platform::Windows {
        "space dir/文件 name.txt"
    } else {
        "space dir/文件\t\n\" name.txt"
    };
    write(&workspace.root, file, b"named file\n");
    let change = manager.capture(&workspace).await.unwrap();
    manager.merge(&base, &[change], &Accept).await.unwrap();
    assert_eq!(fs::read(repo.0.join(file)).unwrap(), b"named file\n");
    manager.remove(&workspace).await.unwrap();
}

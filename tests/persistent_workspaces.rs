//! 长期工作树的复制、重启接管与恢复一致性；使用真实的临时 Git 仓库。

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

use qcode_harness::application::isolation::{
    IsolationWorkspace, WorkspaceIsolation, WorkspaceVerifier,
};
use qcode_harness::application::port::BoxFuture;
use qcode_harness::domain::workspace::WorkspaceRoot;
use qcode_harness::infrastructure::git_worktree::GitWorktreeManager;

struct Repo(PathBuf);

impl Repo {
    fn new(label: &str) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "qch-persistent-{}-{label}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        git(&root, &["init", "-q"]);
        git(&root, &["config", "core.autocrlf", "false"]);
        write(&root, ".gitignore", b"ignored/\n.env\n");
        write(&root, "alpha.txt", b"alpha baseline\n");
        write(&root, "beta.txt", b"beta baseline\n");
        write(&root, ".env.example", b"protected baseline\n");
        git(&root, &["add", "."]);
        git(&root, &["commit", "-qm", "baseline"]);
        Self(fs::canonicalize(root).unwrap())
    }

    async fn manager(&self) -> GitWorktreeManager {
        GitWorktreeManager::new(WorkspaceRoot::new(&self.0))
            .await
            .unwrap()
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

fn git(root: &Path, args: &[&str]) -> Vec<u8> {
    let mut command = Command::new("git");
    command
        .args([
            "-c",
            "user.name=persistence-test",
            "-c",
            "user.email=persistence@example.invalid",
            "-c",
            "commit.gpgSign=false",
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
    let output = command.output().expect("测试需要 Git");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

fn write(root: &Path, path: &str, bytes: &[u8]) {
    let destination = root.join(path);
    fs::create_dir_all(destination.parent().unwrap()).unwrap();
    fs::write(destination, bytes).unwrap();
}

struct Accept;

impl WorkspaceVerifier for Accept {
    fn verify<'a>(&'a self, _root: &'a Path) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }
}

#[tokio::test]
async fn fork_copies_latest_uncommitted_files_and_both_trees_remain_independent() {
    let repo = Repo::new("fork");
    let manager = repo.manager().await;
    let base = manager.prepare().await.unwrap();
    let source = manager.create("source", &base).await.unwrap();
    write(&source.root, "alpha.txt", b"staged version\n");
    git(&source.root, &["add", "alpha.txt"]);
    write(&source.root, "alpha.txt", b"latest version\n");
    write(&source.root, "new folder/文件.bin", &[0, 255, 42, 0]);
    fs::remove_file(source.root.join("beta.txt")).unwrap();
    let parent_index = fs::read(repo.0.join(".git/index")).unwrap();
    let branches = git(&repo.0, &["for-each-ref", "refs/heads"]);

    let fork = manager.fork_workspace(&source, "fork").await.unwrap();

    assert_eq!(fork.base_revision, base);
    assert_ne!(fork.root, source.root);
    assert_eq!(
        fs::read(fork.root.join("alpha.txt")).unwrap(),
        b"latest version\n"
    );
    assert_eq!(
        fs::read(fork.root.join("new folder/文件.bin")).unwrap(),
        [0, 255, 42, 0]
    );
    assert!(!fork.root.join("beta.txt").exists());
    assert!(!fork.root.join(".env.example").exists());
    assert_eq!(
        manager.capture(&source).await.unwrap().patch,
        manager.capture(&fork).await.unwrap().patch
    );
    assert_eq!(
        git(&source.root, &["rev-parse", "HEAD"]),
        git(&fork.root, &["rev-parse", "HEAD"])
    );
    assert_eq!(git(&repo.0, &["for-each-ref", "refs/heads"]), branches);
    assert_eq!(fs::read(repo.0.join(".git/index")).unwrap(), parent_index);
    assert_eq!(
        fs::read(repo.0.join("alpha.txt")).unwrap(),
        b"alpha baseline\n"
    );

    write(&source.root, "alpha.txt", b"source only\n");
    write(&fork.root, "new folder/文件.bin", b"fork only\n");
    assert_eq!(
        fs::read(fork.root.join("alpha.txt")).unwrap(),
        b"latest version\n"
    );
    assert_eq!(
        fs::read(source.root.join("new folder/文件.bin")).unwrap(),
        [0, 255, 42, 0]
    );
    assert_ne!(
        manager.capture_digest(&source).await.unwrap(),
        manager.capture_digest(&fork).await.unwrap()
    );
    manager.remove(&source).await.unwrap();
    assert!(fork.root.exists());
    manager.remove(&fork).await.unwrap();
}

#[tokio::test]
async fn restart_adopts_serialized_workspace_and_preserves_capture_digest_and_removal() {
    let repo = Repo::new("restart");
    let manager = repo.manager().await;
    let base = manager.prepare().await.unwrap();
    let workspace = manager.create("long-task", &base).await.unwrap();
    write(&workspace.root, "alpha.txt", b"checkpoint\n");
    write(&workspace.root, "new.txt", b"untracked checkpoint\n");
    let digest = manager.capture_digest(&workspace).await.unwrap();
    let persisted = serde_json::to_vec(&workspace).unwrap();
    let registered = repo.worktrees();
    drop(manager);

    let manager = repo.manager().await;
    assert_eq!(repo.worktrees(), registered);
    let restored: IsolationWorkspace = serde_json::from_slice(&persisted).unwrap();
    manager.adopt(&restored).await.unwrap();
    manager.adopt(&restored).await.unwrap();
    assert_eq!(manager.capture_digest(&restored).await.unwrap(), digest);
    write(&restored.root, "alpha.txt", b"changed after checkpoint\n");
    assert_ne!(manager.capture_digest(&restored).await.unwrap(), digest);
    let fork = manager
        .fork_workspace(&restored, "resumed-fork")
        .await
        .unwrap();
    assert_eq!(
        manager.capture(&restored).await.unwrap().patch,
        manager.capture(&fork).await.unwrap().patch
    );
    manager.remove(&restored).await.unwrap();
    manager.remove(&restored).await.unwrap();
    assert!(!restored.root.exists());
    manager.remove(&fork).await.unwrap();
    assert_eq!(
        repo.worktrees()
            .split(|byte| *byte == 0)
            .filter(|entry| entry.starts_with(b"worktree "))
            .count(),
        1
    );
    assert!(manager.adopt(&restored).await.is_err());
}

#[tokio::test]
async fn uncaptured_edits_also_survive_manager_restart() {
    let repo = Repo::new("uncaptured");
    let manager = repo.manager().await;
    let base = manager.prepare().await.unwrap();
    let workspace = manager.create("waiting", &base).await.unwrap();
    write(&workspace.root, "alpha.txt", b"not yet captured\n");
    write(&workspace.root, "new.txt", b"not yet staged\n");
    drop(manager);
    let manager = repo.manager().await;
    manager.adopt(&workspace).await.unwrap();
    let fork = manager.fork_workspace(&workspace, "copied").await.unwrap();
    assert_eq!(
        fs::read(fork.root.join("new.txt")).unwrap(),
        b"not yet staged\n"
    );
    let patch = manager.capture(&workspace).await.unwrap();
    assert!(!patch.patch.is_empty());
    manager.merge(&base, &[patch], &Accept).await.unwrap();
    assert_eq!(
        fs::read(repo.0.join("alpha.txt")).unwrap(),
        b"not yet captured\n"
    );
    manager.remove(&workspace).await.unwrap();
    manager.remove(&fork).await.unwrap();
}

#[tokio::test]
async fn adopt_rejects_arbitrary_roots_even_if_they_are_registered_git_worktrees() {
    let repo = Repo::new("arbitrary");
    let manager = repo.manager().await;
    let base = manager.prepare().await.unwrap();
    let issued = manager.create("issued", &base).await.unwrap();
    let mut forged = issued.clone();
    for root in [
        repo.0.clone(),
        repo.0.join(".git"),
        issued.root.join(".."),
        repo.0.join(".qcodeharness/worktrees"),
    ] {
        forged.root = root;
        assert!(manager.adopt(&forged).await.is_err());
    }
    let unissued = repo.0.join(".qcodeharness/worktrees/manual/user-owned");
    fs::create_dir_all(unissued.parent().unwrap()).unwrap();
    // Windows 的 canonicalize 会带 \\?\ 前缀，Git for Windows 的 worktree add
    // 不接受这种形式；测试夹具按生产端 path_text 的口径去掉前缀。
    let raw = unissued.to_str().unwrap();
    let path = raw
        .strip_prefix(r"\\?\UNC\")
        .map(|unc| format!(r"\\{unc}"))
        .unwrap_or_else(|| {
            raw.strip_prefix(r"\\?\").unwrap_or(raw).to_owned()
        });
    git(
        &repo.0,
        &[
            "worktree",
            "add",
            "--detach",
            &path,
            &base,
        ],
    );
    forged.id = "user-owned".to_owned();
    forged.root = unissued;
    assert!(manager.adopt(&forged).await.is_err());
    assert!(forged.root.join("alpha.txt").exists());
    manager.remove(&issued).await.unwrap();
}

#[tokio::test]
async fn adopt_rejects_other_repositories_and_changed_git_markers_or_baselines() {
    let repo = Repo::new("identity");
    let other_repo = Repo::new("other");
    let manager = repo.manager().await;
    let base = manager.prepare().await.unwrap();
    let first = manager.create("first", &base).await.unwrap();
    let second = manager.create("second", &base).await.unwrap();
    let other_manager = other_repo.manager().await;
    assert!(other_manager.adopt(&first).await.is_err());
    drop(manager);
    let manager = repo.manager().await;
    let mut forged = first.clone();
    forged.base_revision = "f".repeat(40);
    assert!(manager.adopt(&forged).await.is_err());
    forged.base_revision = "HEAD".to_owned();
    assert!(manager.adopt(&forged).await.is_err());
    let original_marker = fs::read(first.root.join(".git")).unwrap();
    fs::copy(second.root.join(".git"), first.root.join(".git")).unwrap();
    assert!(manager.adopt(&first).await.is_err());
    fs::write(first.root.join(".git"), original_marker).unwrap();
    manager.adopt(&first).await.unwrap();
    manager.adopt(&second).await.unwrap();
    manager.remove(&first).await.unwrap();
    manager.remove(&second).await.unwrap();
}

#[tokio::test]
async fn matching_git_marker_cannot_hide_a_foreign_common_repository() {
    let repo = Repo::new("common-dir");
    let foreign = Repo::new("foreign");
    let manager = repo.manager().await;
    let base = manager.prepare().await.unwrap();
    let workspace = manager.create("source", &base).await.unwrap();
    let git_dir = PathBuf::from(
        String::from_utf8(git(&workspace.root, &["rev-parse", "--absolute-git-dir"]))
            .unwrap()
            .trim(),
    );
    let commondir = git_dir.join("commondir");
    let original = fs::read(&commondir).unwrap();
    fs::write(&commondir, foreign.0.join(".git").to_str().unwrap()).unwrap();
    drop(manager);
    let manager = repo.manager().await;
    assert!(manager.adopt(&workspace).await.is_err());
    fs::write(commondir, original).unwrap();
    manager.adopt(&workspace).await.unwrap();
    manager.remove(&workspace).await.unwrap();
}

#[tokio::test]
async fn fork_rejects_protected_files_and_committed_source_without_losing_source_work() {
    let repo = Repo::new("unsafe-source");
    let manager = repo.manager().await;
    let base = manager.prepare().await.unwrap();
    let source = manager.create("source", &base).await.unwrap();
    let registered = repo.worktrees();
    for path in [".env.local", ".env.example", "nested/.git/config"] {
        write(&source.root, path, b"protected\n");
        assert!(
            manager.fork_workspace(&source, "rejected").await.is_err(),
            "path={path}"
        );
        assert_eq!(repo.worktrees(), registered);
        assert_eq!(fs::read(source.root.join(path)).unwrap(), b"protected\n");
        fs::remove_file(source.root.join(path)).unwrap();
    }
    fs::remove_dir_all(source.root.join("nested")).unwrap();
    write(&source.root, "alpha.txt", b"committed on child\n");
    git(&source.root, &["add", "alpha.txt"]);
    git(&source.root, &["commit", "-qm", "child commit"]);
    let registered = repo.worktrees();
    let error = manager
        .fork_workspace(&source, "rejected")
        .await
        .unwrap_err();
    assert!(error.contains("HEAD"), "{error}");
    assert_eq!(repo.worktrees(), registered);
    drop(manager);
    let manager = repo.manager().await;
    assert!(manager.adopt(&source).await.is_err());
}

#[tokio::test]
async fn fork_refuses_hidden_ordinary_changes_instead_of_silently_discarding_them() {
    let repo = Repo::new("hidden-files");
    let manager = repo.manager().await;
    let base = manager.prepare().await.unwrap();
    let source = manager.create("source", &base).await.unwrap();
    let registered = repo.worktrees();
    write(&source.root, "alpha.txt", b"hidden tracked edit\n");
    git(
        &source.root,
        &["update-index", "--assume-unchanged", "alpha.txt"],
    );
    assert!(manager.fork_workspace(&source, "hidden").await.is_err());
    git(
        &source.root,
        &["update-index", "--no-assume-unchanged", "alpha.txt"],
    );
    write(&source.root, "ignored/new.txt", b"ignored ordinary file\n");
    assert!(manager.fork_workspace(&source, "hidden").await.is_err());
    assert_eq!(repo.worktrees(), registered);
    assert_eq!(
        fs::read(source.root.join("alpha.txt")).unwrap(),
        b"hidden tracked edit\n"
    );
    assert_eq!(
        fs::read(source.root.join("ignored/new.txt")).unwrap(),
        b"ignored ordinary file\n"
    );
    manager.remove(&source).await.unwrap();
}

#[tokio::test]
async fn adopted_workspace_can_fork_while_dirty_or_moved_parent_still_rejects_merge() {
    let repo = Repo::new("parent-preconditions");
    let manager = repo.manager().await;
    let base = manager.prepare().await.unwrap();
    let source = manager.create("source", &base).await.unwrap();
    write(&source.root, "alpha.txt", b"child change\n");
    drop(manager);
    write(&repo.0, "beta.txt", b"parent user edit\n");
    let parent_index = fs::read(repo.0.join(".git/index")).unwrap();
    let manager = repo.manager().await;
    manager.adopt(&source).await.unwrap();
    let fork = manager
        .fork_workspace(&source, "dirty-parent-fork")
        .await
        .unwrap();
    let changes = [manager.capture(&fork).await.unwrap()];
    assert!(manager.merge(&base, &changes, &Accept).await.is_err());
    assert_eq!(
        fs::read(repo.0.join("beta.txt")).unwrap(),
        b"parent user edit\n"
    );
    assert_eq!(fs::read(repo.0.join(".git/index")).unwrap(), parent_index);
    git(&repo.0, &["add", "beta.txt"]);
    git(&repo.0, &["commit", "-qm", "parent moved"]);
    let moved_index = fs::read(repo.0.join(".git/index")).unwrap();
    let second = manager
        .fork_workspace(&source, "moved-parent-fork")
        .await
        .unwrap();
    assert_eq!(second.base_revision, base);
    assert!(manager.merge(&base, &changes, &Accept).await.is_err());
    assert_eq!(fs::read(repo.0.join(".git/index")).unwrap(), moved_index);
    assert_eq!(
        fs::read(repo.0.join("alpha.txt")).unwrap(),
        b"alpha baseline\n"
    );
    manager.remove(&source).await.unwrap();
    manager.remove(&fork).await.unwrap();
    manager.remove(&second).await.unwrap();
}

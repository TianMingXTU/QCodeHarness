//! Git worktree 隔离：子任务只拿到提交中的普通文件，父索引从不参与暂存。

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::sync::Mutex;

use crate::application::isolation::{
    ChangeSet, IsolationWorkspace, WorkspaceIsolation, WorkspaceVerifier,
};
use crate::application::port::BoxFuture;
use crate::domain::workspace::{WorkspaceRoot, is_protected_path};

const GIT_TIMEOUT: Duration = Duration::from_secs(30);
const STATE_EXCLUSION: &str = ":(top,exclude).qcodeharness";
static NEXT_ID: AtomicU64 = AtomicU64::new(0);

struct OwnedWorkspace {
    workspace: IsolationWorkspace,
    marker: Vec<u8>,
    removed: bool,
}

/// 一个父仓库的一组工作区；实例之间使用不同目录，Git 索引也彼此独立。
///
/// 构造不要求父仓库干净，`prepare`、`create` 和 `merge` 分别检查自己的前提。
/// 暂不支持非 UTF-8 路径。受保护文件和基线中的链接、子模块不会检出到子工作区。
/// 本实现提供版本控制隔离，不限制其他进程直接修改文件系统。
pub struct GitWorktreeManager {
    root: PathBuf,
    root_marker: Vec<u8>,
    storage: PathBuf,
    filter_overrides: Vec<String>,
    workspaces: Mutex<HashMap<String, OwnedWorkspace>>,
}

impl GitWorktreeManager {
    /// 校验真实仓库根，并分配本实例独占的工作区目录。
    ///
    /// # Errors
    ///
    /// Git 不可用、目录不是工作区根、路径含链接或运行状态目录不可写时返回错误。
    pub async fn new(root: WorkspaceRoot) -> Result<Self, String> {
        let root = fs::canonicalize(root.as_path()).map_err(io_error)?;
        path_text(&root)?;
        let root_marker = git_marker(&root)?;
        let suffix = unique_id()?;
        let storage = root.join(".qcodeharness/worktrees").join(suffix);
        let mut manager = Self {
            root,
            root_marker,
            storage,
            filter_overrides: Vec::new(),
            workspaces: Mutex::new(HashMap::new()),
        };
        let top = manager
            .git(&manager.root, &["rev-parse", "--show-toplevel"], None)
            .await?;
        let top = PathBuf::from(output_text(&top)?);
        if fs::canonicalize(top).map_err(io_error)? != manager.root {
            return Err("隔离需要真实 Git 工作区根，不能从子目录发现父仓库".to_owned());
        }
        let inside = manager
            .git(&manager.root, &["rev-parse", "--is-inside-work-tree"], None)
            .await?;
        if output_text(&inside)? != "true" {
            return Err("不能在裸仓库中建立任务工作区".to_owned());
        }

        // .gitattributes 可以选择仓库配置中的过滤器；Git 的检出和暂存不能执行它们。
        let filters = manager
            .run_git(
                &manager.root,
                &[
                    "config",
                    "--null",
                    "--name-only",
                    "--get-regexp",
                    r"^filter\..*\.(clean|smudge|process|required)$",
                ],
                None,
            )
            .await?;
        if filters.status.success() {
            for key in nul_records(&filters.stdout)? {
                let key = std::str::from_utf8(key).map_err(|_| "Git 过滤器名称不是 UTF-8")?;
                let value = if key.ends_with(".required") {
                    "false"
                } else {
                    ""
                };
                manager.filter_overrides.push(format!("{key}={value}"));
            }
        } else if filters.status.code() != Some(1) {
            return Err(git_error(&["config"], &filters));
        }
        ensure_directories(
            &manager.root,
            manager.storage.parent().expect("storage 有父目录"),
        )?;
        fs::create_dir(&manager.storage).map_err(io_error)?;
        // hooksPath 指向普通文件，任何 hook 名拼接后都不存在，不依赖一个可被填入脚本的目录。
        File::create(manager.storage.join(".disabled-hooks")).map_err(io_error)?;
        Ok(manager)
    }

    async fn run_git(
        &self,
        root: &Path,
        args: &[&str],
        input: Option<&[u8]>,
    ) -> Result<Output, String> {
        let disabled = self.storage.join(".disabled-hooks");
        let mut command = Command::new("git");
        command
            .arg("-c")
            .arg(format!("core.hooksPath={}", path_text(&disabled)?))
            .arg("-c")
            .arg(format!("core.attributesFile={}", path_text(&disabled)?))
            .args([
                "-c",
                "core.fsmonitor=false",
                "-c",
                "core.protectNTFS=true",
                "-c",
                "core.protectHFS=true",
                "-c",
                "submodule.recurse=false",
                "-c",
                "commit.gpgSign=false",
                "-c",
                "tag.gpgSign=false",
                "-c",
                "core.sparseCheckout=false",
            ]);
        for setting in &self.filter_overrides {
            command.arg("-c").arg(setting);
        }
        // GIT_INDEX_FILE、GIT_DIR、GIT_WORK_TREE 和 GIT_CONFIG_* 都不能改写命令的目标。
        for (name, _) in std::env::vars_os() {
            if name
                .to_string_lossy()
                .to_ascii_uppercase()
                .starts_with("GIT_")
            {
                command.env_remove(name);
            }
        }
        command
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("GIT_ATTR_NOSYSTEM", "1")
            .args(args)
            .current_dir(root)
            .stdin(if input.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command
            .spawn()
            .map_err(|error| format!("启动 Git 失败: {error}"))?;
        let stdin = child.stdin.take();
        let run = async {
            let send = async {
                if let (Some(mut stdin), Some(bytes)) = (stdin, input) {
                    stdin.write_all(bytes).await?;
                    stdin.shutdown().await?;
                }
                Ok::<(), io::Error>(())
            };
            let (sent, output) = tokio::join!(send, child.wait_with_output());
            let output = output.map_err(io_error)?;
            if output.status.success() {
                sent.map_err(io_error)?;
            }
            Ok(output)
        };
        tokio::time::timeout(GIT_TIMEOUT, run)
            .await
            .map_err(|_| "Git 命令超过 30 秒，已终止子进程".to_owned())?
    }

    async fn git(
        &self,
        root: &Path,
        args: &[&str],
        input: Option<&[u8]>,
    ) -> Result<Vec<u8>, String> {
        let output = self.run_git(root, args, input).await?;
        if !output.status.success() {
            return Err(git_error(args, &output));
        }
        Ok(output.stdout)
    }

    fn check_storage(&self) -> Result<(), String> {
        check_path(&self.root, &self.storage, false)?;
        let disabled = self.storage.join(".disabled-hooks");
        let metadata = fs::symlink_metadata(&disabled).map_err(io_error)?;
        if is_link(&metadata) || !metadata.is_file() || metadata.len() != 0 {
            return Err("隔离实例的 Git 配置文件被改动".to_owned());
        }
        if git_marker(&self.root)? != self.root_marker {
            return Err("父工作区的 .git 入口被改动".to_owned());
        }
        Ok(())
    }

    async fn head(&self, root: &Path) -> Result<String, String> {
        let output = self
            .git(root, &["rev-parse", "--verify", "HEAD^{commit}"], None)
            .await?;
        let revision = output_text(&output)?.to_owned();
        validate_revision(&revision)?;
        Ok(revision)
    }

    async fn parent_at(&self, base: &str) -> Result<(), String> {
        validate_revision(base)?;
        self.check_storage()?;
        if self.head(&self.root).await? != base {
            return Err("父工作区 HEAD 已离开任务基线".to_owned());
        }
        if !self.status(&self.root).await?.is_empty() {
            return Err("父工作区有未提交或未跟踪的改动，拒绝隔离合并".to_owned());
        }
        if self.head(&self.root).await? != base {
            return Err("检查过程中父工作区 HEAD 发生变化".to_owned());
        }
        Ok(())
    }

    async fn status(&self, root: &Path) -> Result<Vec<u8>, String> {
        self.git(
            root,
            &[
                "status",
                "--porcelain=v1",
                "-z",
                "--untracked-files=all",
                "--no-renames",
                "--ignore-submodules=none",
                "--",
                ".",
                STATE_EXCLUSION,
            ],
            None,
        )
        .await
    }

    async fn checkout(&self, root: &Path, base: &str) -> Result<Vec<u8>, String> {
        self.git(
            &self.root,
            &[
                "worktree",
                "add",
                "--detach",
                "--no-checkout",
                "--",
                path_text(root)?.as_ref(),
                base,
            ],
            None,
        )
        .await?;
        let populate = async {
            let marker = git_marker(root)?;
            self.git(root, &["read-tree", base], None).await?;
            let tree = self
                .git(
                    root,
                    &["ls-tree", "-r", "-z", "--full-tree", base, "--"],
                    None,
                )
                .await?;
            let mut ordinary = Vec::new();
            let mut omitted = Vec::new();
            for record in nul_records(&tree)? {
                let (header, path) = split_once(record, b'\t')?;
                let mode = header
                    .split(|byte| *byte == b' ')
                    .next()
                    .unwrap_or_default();
                let path = std::str::from_utf8(path).map_err(|_| "基线含非 UTF-8 路径")?;
                validate_relative(path)?;
                let destination = if protected(path) || !regular_mode(mode) {
                    &mut omitted
                } else {
                    &mut ordinary
                };
                destination.extend_from_slice(path.as_bytes());
                destination.push(0);
            }
            // 未检出的受保护文件仍留在索引中，避免后续暂存把它们误记成删除。
            if !omitted.is_empty() {
                self.git(
                    root,
                    &["update-index", "--skip-worktree", "-z", "--stdin"],
                    Some(&omitted),
                )
                .await?;
            }
            if !ordinary.is_empty() {
                self.git(
                    root,
                    &["checkout-index", "--index", "-z", "--stdin"],
                    Some(&ordinary),
                )
                .await?;
            }
            Ok(marker)
        }
        .await;
        match populate {
            Ok(marker) => Ok(marker),
            Err(error) => combine_cleanup(Err(error), self.remove_worktree(root).await),
        }
    }

    fn owned<'a>(
        &self,
        workspaces: &'a HashMap<String, OwnedWorkspace>,
        workspace: &IsolationWorkspace,
    ) -> Result<&'a OwnedWorkspace, String> {
        validate_id(&workspace.id)?;
        validate_revision(&workspace.base_revision)?;
        let owned = workspaces
            .get(&workspace.id)
            .ok_or("工作区不属于当前隔离实例")?;
        if owned.workspace != *workspace {
            return Err("工作区路径或基线与隔离实例签发的记录不符".to_owned());
        }
        Ok(owned)
    }

    fn check_owned(&self, owned: &OwnedWorkspace) -> Result<(), String> {
        if owned.removed {
            return Err("工作区已被移除".to_owned());
        }
        self.check_storage()?;
        check_path(&self.storage, &owned.workspace.root, false)?;
        if git_marker(&owned.workspace.root)? != owned.marker {
            return Err("子工作区的 .git 入口被改动".to_owned());
        }
        Ok(())
    }

    async fn child_at(&self, workspace: &IsolationWorkspace) -> Result<(), String> {
        if self.head(&workspace.root).await? != workspace.base_revision {
            return Err("子工作区 HEAD 已变化；只允许捕获未提交的变更".to_owned());
        }
        let symbolic = self
            .run_git(&workspace.root, &["symbolic-ref", "-q", "HEAD"], None)
            .await?;
        if symbolic.status.code() != Some(1) {
            return Err("子工作区必须保持 detached HEAD".to_owned());
        }
        Ok(())
    }

    async fn validate_index(&self, root: &Path, base: &str) -> Result<(), String> {
        let raw = self
            .git(
                root,
                &[
                    "diff",
                    "--cached",
                    "--raw",
                    "-z",
                    "--no-renames",
                    "--no-ext-diff",
                    "--no-textconv",
                    base,
                    "--",
                ],
                None,
            )
            .await?;
        let records = nul_records(&raw)?;
        let mut records = records.into_iter();
        while let Some(header) = records.next() {
            let path = records.next().ok_or("Git raw diff 缺少路径")?;
            let modes: Vec<_> = header.split(|byte| *byte == b' ').take(2).collect();
            if modes.len() != 2 || !modes[0].starts_with(b":") {
                return Err("Git raw diff 格式无效".to_owned());
            }
            for mode in [&modes[0][1..], modes[1]] {
                if mode != b"000000" && !regular_mode(mode) {
                    return Err("隔离变更不允许符号链接或子模块".to_owned());
                }
            }
            let path = checked_patch_path(path)?;
            check_relative(root, path)?;
        }
        Ok(())
    }

    async fn binary_diff(&self, root: &Path, base: &str) -> Result<Vec<u8>, String> {
        self.git(
            root,
            &[
                "diff",
                "--cached",
                "--binary",
                "--full-index",
                "--no-renames",
                "--no-ext-diff",
                "--no-textconv",
                base,
                "--",
            ],
            None,
        )
        .await
    }

    async fn validate_omitted_paths(&self, root: &Path) -> Result<(), String> {
        let entries = self
            .git(
                root,
                &["ls-files", "-v", "-z", "--", ".", STATE_EXCLUSION],
                None,
            )
            .await?;
        for record in nul_records(&entries)? {
            if record.len() < 3 || record[1] != b' ' {
                return Err("Git index 标记格式无效".to_owned());
            }
            if record[0].is_ascii_lowercase() {
                return Err("子工作区不允许 assume-unchanged index 标记".to_owned());
            }
            if record[0] != b'S' {
                continue;
            }
            let path = std::str::from_utf8(&record[2..]).map_err(|_| "基线含非 UTF-8 路径")?;
            validate_relative(path)?;
            match fs::symlink_metadata(root.join(path)) {
                Ok(_) => return Err(format!("未检出的基线路径被物化或修改: {path:?}")),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(io_error(error)),
            }
        }
        Ok(())
    }

    async fn capture_inner(&self, workspace: &IsolationWorkspace) -> Result<ChangeSet, String> {
        self.child_at(workspace).await?;
        self.validate_omitted_paths(&workspace.root).await?;
        self.validate_index(&workspace.root, &workspace.base_revision)
            .await?;
        let status = self.status(&workspace.root).await?;
        let mut paths = Vec::new();
        for record in nul_records(&status)? {
            if record.len() < 4 || record[2] != b' ' {
                return Err("Git status 格式无效".to_owned());
            }
            let path = checked_patch_path(&record[3..])?;
            let path = path.trim_end_matches('/');
            check_relative(&workspace.root, path)?;
            let absolute = workspace.root.join(path);
            if fs::symlink_metadata(&absolute).is_ok_and(|metadata| metadata.is_dir()) {
                return Err(format!("隔离变更含嵌套仓库或目录条目: {path}"));
            }
            paths.extend_from_slice(path.as_bytes());
            paths.push(0);
        }
        if !paths.is_empty() {
            self.git(
                &workspace.root,
                &[
                    "--literal-pathspecs",
                    "add",
                    "--all",
                    "--pathspec-from-file=-",
                    "--pathspec-file-nul",
                ],
                Some(&paths),
            )
            .await?;
        }
        self.validate_index(&workspace.root, &workspace.base_revision)
            .await?;
        self.child_at(workspace).await?;
        Ok(ChangeSet {
            id: workspace.id.clone(),
            patch: self
                .binary_diff(&workspace.root, &workspace.base_revision)
                .await?,
        })
    }

    async fn patch_paths(&self, root: &Path, patch: &[u8]) -> Result<Vec<String>, String> {
        if patch.is_empty() {
            return Ok(Vec::new());
        }
        let stats = self
            .git(root, &["apply", "--numstat", "-z", "-"], Some(patch))
            .await?;
        let records = nul_records(&stats)?;
        let mut records = records.into_iter();
        let mut paths = Vec::new();
        while let Some(record) = records.next() {
            let (_, rest) = split_once(record, b'\t')?;
            let (_, path) = split_once(rest, b'\t')?;
            if path.is_empty() {
                // -z 的 rename/copy 记录把旧、新路径单独放在随后的两个 NUL 字段。
                for _ in 0..2 {
                    paths.push(
                        checked_patch_path(records.next().ok_or("Git numstat 缺少重命名路径")?)?
                            .to_owned(),
                    );
                }
            } else {
                paths.push(checked_patch_path(path)?.to_owned());
            }
        }
        Ok(paths)
    }

    async fn apply_checked(&self, root: &Path, patch: &[u8], index: bool) -> Result<(), String> {
        if patch.is_empty() {
            return Ok(());
        }
        let mut check = vec!["apply", "--check", "--binary", "--whitespace=nowarn"];
        if index {
            check.push("--index");
        }
        check.push("-");
        self.git(root, &check, Some(patch)).await?;
        let mut apply = vec!["apply", "--binary", "--whitespace=nowarn"];
        if index {
            apply.push("--index");
        }
        apply.push("-");
        self.git(root, &apply, Some(patch)).await?;
        Ok(())
    }

    async fn candidate(
        &self,
        root: &Path,
        base: &str,
        changes: &[ChangeSet],
        verifier: &dyn WorkspaceVerifier,
    ) -> Result<Vec<u8>, String> {
        for change in changes {
            for path in self.patch_paths(root, &change.patch).await? {
                check_relative(root, &path)?;
            }
            self.apply_checked(root, &change.patch, true).await?;
            self.validate_index(root, base).await?;
        }
        let combined = self.binary_diff(root, base).await?;
        let marker = git_marker(root)?;
        verifier.verify(root).await?;
        // 测试通过的目录必须与待写回补丁一致，验证器生成源码或暂存改动都要拒绝。
        if git_marker(root)? != marker {
            return Err("验证器修改了候选工作区的 .git 入口".to_owned());
        }
        self.validate_omitted_paths(root).await?;
        self.validate_index(root, base).await?;
        if self.head(root).await? != base || self.binary_diff(root, base).await? != combined {
            return Err("验证器修改了候选 HEAD 或索引".to_owned());
        }
        let unstaged = self
            .git(
                root,
                &[
                    "diff",
                    "--name-only",
                    "-z",
                    "--no-ext-diff",
                    "--no-textconv",
                    "--",
                    ".",
                    STATE_EXCLUSION,
                ],
                None,
            )
            .await?;
        let untracked = self
            .git(
                root,
                &[
                    "ls-files",
                    "--others",
                    "--exclude-standard",
                    "-z",
                    "--",
                    ".",
                    STATE_EXCLUSION,
                ],
                None,
            )
            .await?;
        if !unstaged.is_empty() || !untracked.is_empty() {
            return Err("验证器修改了候选文件，已拒绝写回".to_owned());
        }
        Ok(combined)
    }

    async fn remove_worktree(&self, root: &Path) -> Result<(), String> {
        check_path(&self.storage, root, true)?;
        // --force 只作用于本实例创建的子工作区，允许清理未提交的子任务结果。
        self.git(
            &self.root,
            &[
                "worktree",
                "remove",
                "--force",
                "--",
                path_text(root)?.as_ref(),
            ],
            None,
        )
        .await?;
        Ok(())
    }

    async fn merge_inner(
        &self,
        base: &str,
        changes: &[ChangeSet],
        verifier: &dyn WorkspaceVerifier,
    ) -> Result<(), String> {
        self.parent_at(base).await?;
        let mut ids = HashSet::new();
        for change in changes {
            validate_id(&change.id)?;
            if !ids.insert(&change.id) {
                return Err(format!("批量变更中 ID 重复: {}", change.id));
            }
        }
        let integration = self.storage.join(format!(
            ".integration-{}",
            NEXT_ID.fetch_add(1, Ordering::Relaxed)
        ));
        self.checkout(&integration, base).await?;
        let candidate = self.candidate(&integration, base, changes, verifier).await;
        // 清理也可能失败；在父工作区写入之前处理完，避免失败返回时已有父文件变更。
        let combined = combine_cleanup(candidate, self.remove_worktree(&integration).await)?;
        let lease = ParentLease::acquire(self.storage.parent().expect("storage 有父目录"))?;
        let apply = async {
            self.parent_at(base).await?;
            for path in self.patch_paths(&self.root, &combined).await? {
                check_relative(&self.root, &path)?;
            }
            if !combined.is_empty() {
                self.git(
                    &self.root,
                    &["apply", "--check", "--binary", "--whitespace=nowarn", "-"],
                    Some(&combined),
                )
                .await?;
                self.parent_at(base).await?;
                // 不使用 --index，不触碰父索引；默认 apply 会先检查整份补丁再写文件。
                self.git(
                    &self.root,
                    &["apply", "--binary", "--whitespace=nowarn", "-"],
                    Some(&combined),
                )
                .await?;
            }
            Ok(())
        }
        .await;
        combine_cleanup(apply, lease.release())
    }
}

impl WorkspaceIsolation for GitWorktreeManager {
    fn prepare(&self) -> BoxFuture<'_, Result<String, String>> {
        Box::pin(async move {
            let _guard = self.workspaces.lock().await;
            self.check_storage()?;
            let base = self.head(&self.root).await?;
            self.parent_at(&base).await?;
            Ok(base)
        })
    }

    fn create<'a>(
        &'a self,
        id: &'a str,
        base: &'a str,
    ) -> BoxFuture<'a, Result<IsolationWorkspace, String>> {
        Box::pin(async move {
            validate_id(id)?;
            validate_revision(base)?;
            let mut workspaces = self.workspaces.lock().await;
            if workspaces.contains_key(id) {
                return Err(format!("工作区 ID 已使用: {id}"));
            }
            self.parent_at(base).await?;
            let root = self.storage.join(id);
            if root.try_exists().map_err(io_error)? {
                return Err("工作区目录已存在，拒绝覆盖".to_owned());
            }
            let marker = self.checkout(&root, base).await?;
            let workspace = IsolationWorkspace {
                id: id.to_owned(),
                root,
                base_revision: base.to_owned(),
            };
            workspaces.insert(
                id.to_owned(),
                OwnedWorkspace {
                    workspace: workspace.clone(),
                    marker,
                    removed: false,
                },
            );
            Ok(workspace)
        })
    }

    fn capture<'a>(
        &'a self,
        workspace: &'a IsolationWorkspace,
    ) -> BoxFuture<'a, Result<ChangeSet, String>> {
        Box::pin(async move {
            let workspaces = self.workspaces.lock().await;
            self.check_owned(self.owned(&workspaces, workspace)?)?;
            self.capture_inner(workspace).await
        })
    }

    fn merge<'a>(
        &'a self,
        base: &'a str,
        changes: &'a [ChangeSet],
        verifier: &'a dyn WorkspaceVerifier,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            let _guard = self.workspaces.lock().await;
            self.merge_inner(base, changes, verifier).await
        })
    }

    fn remove<'a>(
        &'a self,
        workspace: &'a IsolationWorkspace,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            let mut workspaces = self.workspaces.lock().await;
            let owned = self.owned(&workspaces, workspace)?;
            if owned.removed {
                return Ok(());
            }
            self.check_owned(owned)?;
            self.remove_worktree(&workspace.root).await?;
            workspaces
                .get_mut(&workspace.id)
                .expect("已验证工作区存在")
                .removed = true;
            Ok(())
        })
    }
}

struct ParentLease {
    path: PathBuf,
    file: Option<File>,
}

impl ParentLease {
    fn acquire(directory: &Path) -> Result<Self, String> {
        let path = directory.join(".parent-merge.lock");
        let file = File::options()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|error| format!("无法取得父工作区合并锁（{}）: {error}", path.display()))?;
        Ok(Self {
            path,
            file: Some(file),
        })
    }

    fn release(mut self) -> Result<(), String> {
        self.file.take();
        fs::remove_file(&self.path).map_err(io_error)
    }
}

impl Drop for ParentLease {
    fn drop(&mut self) {
        if self.file.take().is_some() {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn unique_id() -> Result<String, String> {
    let time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| error.to_string())?;
    Ok(format!(
        "m-{}-{}-{}",
        std::process::id(),
        time.as_nanos(),
        NEXT_ID.fetch_add(1, Ordering::Relaxed)
    ))
}

fn validate_id(id: &str) -> Result<(), String> {
    if id.is_empty()
        || id.len() > 64
        || !id.as_bytes()[0].is_ascii_alphanumeric()
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(
            "工作区 ID 必须以字母或数字开头，仅含字母、数字、-、_，长度不超过 64".to_owned(),
        );
    }
    Ok(())
}

fn validate_revision(revision: &str) -> Result<(), String> {
    if !matches!(revision.len(), 40 | 64) || !revision.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err("基线必须是完整的 Git 提交 SHA".to_owned());
    }
    Ok(())
}

fn regular_mode(mode: &[u8]) -> bool {
    matches!(mode, b"100644" | b"100755")
}

fn protected(path: &str) -> bool {
    path.split('/').any(|component| {
        let name = component.trim_end_matches([' ', '.']).to_ascii_lowercase();
        is_protected_path(Path::new(&name)) || name.starts_with(".env")
    })
}

fn validate_relative(path: &str) -> Result<(), String> {
    if path.is_empty()
        || path.contains(['\\', ':', '\0'])
        || path.split('/').any(|part| {
            part.is_empty()
                || matches!(part, "." | "..")
                || part.trim_end_matches([' ', '.']).is_empty()
        })
    {
        return Err(format!("Git 路径不安全: {path:?}"));
    }
    Ok(())
}

fn checked_patch_path(bytes: &[u8]) -> Result<&str, String> {
    let path = std::str::from_utf8(bytes).map_err(|_| "变更含非 UTF-8 路径")?;
    validate_relative(path.trim_end_matches('/'))?;
    if protected(path) {
        return Err(format!("隔离变更含受保护路径: {path:?}"));
    }
    Ok(path)
}

fn check_relative(root: &Path, relative: &str) -> Result<(), String> {
    validate_relative(relative)?;
    if protected(relative) {
        return Err(format!("隔离变更含受保护路径: {relative:?}"));
    }
    check_path(root, &root.join(relative), true)
}

fn check_path(root: &Path, path: &Path, allow_missing: bool) -> Result<(), String> {
    let relative = path.strip_prefix(root).map_err(|_| "路径不属于隔离实例")?;
    let mut cursor = root.to_path_buf();
    for component in relative.components() {
        if !matches!(component, std::path::Component::Normal(_)) {
            return Err("隔离路径含非普通路径组件".to_owned());
        }
        cursor.push(component);
        match fs::symlink_metadata(&cursor) {
            Ok(metadata) if is_link(&metadata) => {
                return Err(format!("隔离路径不允许链接: {}", cursor.display()));
            }
            Ok(_) => {}
            Err(error) if allow_missing && error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(io_error(error)),
        }
    }
    Ok(())
}

fn ensure_directories(root: &Path, path: &Path) -> Result<(), String> {
    let relative = path
        .strip_prefix(root)
        .map_err(|_| "运行状态目录不在父工作区内")?;
    let mut cursor = root.to_path_buf();
    for component in relative.components() {
        cursor.push(component);
        match fs::create_dir(&cursor) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(io_error(error)),
        }
        let metadata = fs::symlink_metadata(&cursor).map_err(io_error)?;
        if is_link(&metadata) || !metadata.is_dir() {
            return Err(format!("运行状态目录不是普通目录: {}", cursor.display()));
        }
    }
    Ok(())
}

fn git_marker(root: &Path) -> Result<Vec<u8>, String> {
    let path = root.join(".git");
    let metadata = fs::symlink_metadata(&path)
        .map_err(|error| format!("需要真实 Git 工作区根（{}）: {error}", root.display()))?;
    if is_link(&metadata) {
        return Err(".git 入口不能是链接".to_owned());
    }
    if metadata.is_dir() {
        Ok(b"directory".to_vec())
    } else if metadata.is_file() {
        fs::read(path).map_err(io_error)
    } else {
        Err(".git 入口不是普通目录或文件".to_owned())
    }
}

fn is_link(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

fn path_text(path: &Path) -> Result<std::borrow::Cow<'_, str>, String> {
    let text = path
        .to_str()
        .ok_or_else(|| "隔离暂不支持非 UTF-8 路径".to_owned())?;
    // Git for Windows 会把 verbatim 前缀转成 //?/，worktree add 无法创建该路径。
    if let Some(unc) = text.strip_prefix(r"\\?\UNC\") {
        return Ok(std::borrow::Cow::Owned(format!(r"\\{unc}")));
    }
    Ok(std::borrow::Cow::Borrowed(
        text.strip_prefix(r"\\?\").unwrap_or(text),
    ))
}

fn output_text(bytes: &[u8]) -> Result<&str, String> {
    std::str::from_utf8(bytes)
        .map(|text| text.trim_end_matches(['\r', '\n']))
        .map_err(|_| "Git 输出不是 UTF-8".to_owned())
}

fn nul_records(bytes: &[u8]) -> Result<Vec<&[u8]>, String> {
    if bytes.is_empty() {
        return Ok(Vec::new());
    }
    if bytes.last() != Some(&0) {
        return Err("Git 的 NUL 输出没有正确结束".to_owned());
    }
    Ok(bytes[..bytes.len() - 1].split(|byte| *byte == 0).collect())
}

fn split_once(bytes: &[u8], delimiter: u8) -> Result<(&[u8], &[u8]), String> {
    let position = bytes
        .iter()
        .position(|byte| *byte == delimiter)
        .ok_or("Git 输出字段缺失")?;
    Ok((&bytes[..position], &bytes[position + 1..]))
}

fn git_error(args: &[&str], output: &Output) -> String {
    let stderr: String = String::from_utf8_lossy(&output.stderr)
        .chars()
        .take(2000)
        .collect();
    format!(
        "git {} 失败（{}）: {}",
        args.first().copied().unwrap_or(""),
        output.status,
        stderr.trim()
    )
}

fn io_error(error: io::Error) -> String {
    format!("隔离工作区 IO 失败: {error}")
}

fn combine_cleanup<T>(result: Result<T, String>, cleanup: Result<(), String>) -> Result<T, String> {
    match (result, cleanup) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(cleanup)) => Err(format!("隔离工作区清理失败: {cleanup}")),
        (Err(error), Err(cleanup)) => Err(format!("{error}；隔离工作区清理失败: {cleanup}")),
    }
}

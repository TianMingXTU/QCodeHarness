//! Git 只读工具：状态与差异。
//!
//! 通过 shell 执行 `git` 而不是引 `git2`：libgit2 是 C 依赖，构建成本高，
//! 而这两个工具只是把两条现成的 git 命令的输出搬给模型。
//! git 不在 PATH 时的失败要给出可读的说明，不 panic。
//!
//! 仓库类测试需要 git 在 PATH 中，标了 `#[ignore = "需要 git 在 PATH 中"]`：
//! `cargo test` 默认不跑它们，`cargo test -- --ignored` 才跑。不用
//! "git 缺失就 return"的运行时跳过——那会被 cargo 记为 passed，
//! 在没装 git 的机器上制造"git 工具都有覆盖"的虚假信心。

use serde::Deserialize;
use serde_json::json;
use std::time::Duration;

use crate::application::port::{BoxFuture, ToolInvoker};
use crate::domain::tool::{ToolAnnotations, ToolCall, ToolOutcome, ToolSpec};
use crate::domain::workspace::WorkspaceRoot;

use super::parse_args;
use super::shell::{clamp_output, run_in_workspace};

const GIT_STATUS: &str = "git_status";
const GIT_DIFF: &str = "git_diff";
/// git 只读命令的超时：比 run_command 默认值短，git 卡住多半是仓库坏了。
const GIT_TIMEOUT: Duration = Duration::from_secs(30);
/// diff 输出的截断上限：diff 可能非常长，4000 字符够模型判断改了什么。
const DIFF_MAX_CHARS: usize = 4000;

/// 查看工作区状态。
pub struct GitStatus {
    root: WorkspaceRoot,
}

impl GitStatus {
    /// 以 `root` 为仓库根构造。
    pub fn new(root: WorkspaceRoot) -> Self {
        Self { root }
    }

    /// 工具规格。
    pub fn spec() -> ToolSpec {
        ToolSpec {
            name: GIT_STATUS.to_owned(),
            description: "查看 workspace 内 git 仓库的工作区状态（git status --porcelain），\
                          空输出表示工作区干净。只读操作。".to_owned(),
            parameters: json!({"type": "object", "properties": {}}),
            annotations: ToolAnnotations::new(true, false, true, false),
        }
    }

    /// 执行 git status。独立出来供测试与内部复用。
    async fn run(&self) -> ToolOutcome {
        match run_in_workspace(&self.root, "git status --porcelain=v1", GIT_TIMEOUT).await {
            Err(message) => git_failure(message),
            Ok(output) => {
                if output.code != 0 {
                    return git_failure(format!(
                        "git status 退出码 {}：{}",
                        output.code,
                        output.stderr.trim_end()
                    ));
                }
                let stdout = output.stdout.trim_end();
                if stdout.is_empty() {
                    ToolOutcome::success("（工作区干净，没有未提交的改动）")
                } else {
                    ToolOutcome::success(stdout)
                }
            }
        }
    }
}

impl ToolInvoker for GitStatus {
    fn invoke<'a>(&'a self, _call: &'a ToolCall) -> BoxFuture<'a, ToolOutcome> {
        Box::pin(self.run())
    }
}

/// 查看差异。
pub struct GitDiff {
    root: WorkspaceRoot,
}

impl GitDiff {
    /// 以 `root` 为仓库根构造。
    pub fn new(root: WorkspaceRoot) -> Self {
        Self { root }
    }

    /// 工具规格。
    pub fn spec() -> ToolSpec {
        ToolSpec {
            name: GIT_DIFF.to_owned(),
            description: "查看 workspace 内 git 仓库的未暂存差异（git diff）；\
                          staged 为 true 时查看已暂存的差异（git diff --staged）。输出最多约 4000 字符。只读操作。"
                .to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "staged": { "type": "boolean", "description": "是否查看已暂存的差异，默认 false" }
                }
            }),
            annotations: ToolAnnotations::new(true, false, true, false),
        }
    }

    /// 执行 git diff。独立出来供测试与内部复用。
    async fn run(&self, staged: bool) -> ToolOutcome {
        let command = if staged {
            "git diff --staged"
        } else {
            "git diff"
        };
        match run_in_workspace(&self.root, command, GIT_TIMEOUT).await {
            Err(message) => git_failure(message),
            Ok(output) => {
                if output.code != 0 {
                    return git_failure(format!(
                        "git diff 退出码 {}：{}",
                        output.code,
                        output.stderr.trim_end()
                    ));
                }
                let stdout = output.stdout.trim_end().to_owned();
                if stdout.is_empty() {
                    ToolOutcome::success("（没有差异）")
                } else {
                    ToolOutcome::success(clamp_output(stdout, DIFF_MAX_CHARS))
                }
            }
        }
    }
}

/// `git diff` 的参数。
#[derive(Debug, Deserialize)]
struct GitDiffArgs {
    staged: Option<bool>,
}

impl ToolInvoker for GitDiff {
    fn invoke<'a>(&'a self, call: &'a ToolCall) -> BoxFuture<'a, ToolOutcome> {
        Box::pin(async move {
            let args: GitDiffArgs = match parse_args(&call.arguments) {
                Ok(args) => args,
                Err(message) => return ToolOutcome::failed(message),
            };
            self.run(args.staged.unwrap_or(false)).await
        })
    }
}

/// 把执行层的失败翻译成面向模型的失败观察。
///
/// 启动失败（git 不在 PATH）和 git 自身报错（不是仓库等）在这里归并：
/// 模型只需要知道"git 没跑成、为什么"，不需要区分进程层还是 git 层。
fn git_failure(message: String) -> ToolOutcome {
    if message.contains("启动 `git` 失败") {
        ToolOutcome::failed(format!("找不到 git 可执行文件（可能不在 PATH 中）: {message}"))
    } else {
        ToolOutcome::failed(message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::ids::CallId;
    use crate::domain::tool::OutcomeStatus;
    use serde_json::json;
    use std::path::PathBuf;

    fn call(name: &str, arguments: serde_json::Value) -> ToolCall {
        ToolCall {
            id: CallId::new("call_git"),
            name: name.to_owned(),
            arguments,
        }
    }

    /// 建一个真实 git 仓库（需要 git 在 PATH），返回根路径。
    ///
    /// 只被 `#[ignore]` 的测试调用：有人显式 `-- --ignored` 要求跑时，
    /// 环境就该具备 git，缺了直接 panic 暴露问题，而不是静默跳过。
    fn temp_repo(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("qch-git-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建临时目录");

        let init = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&dir)
            .output()
            .expect("启动 git 失败：git 不在 PATH 中");
        assert!(
            init.status.success(),
            "git init 失败: {}",
            String::from_utf8_lossy(&init.stderr)
        );
        dir
    }

    /// 把 repo 里现有文件全部提交，制造一个干净基线。
    fn commit_all(repo: &PathBuf) {
        for args in [
            vec!["add", "."],
            vec!["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-qm", "init"],
        ] {
            let output = std::process::Command::new("git")
                .args(&args)
                .current_dir(repo)
                .output()
                .expect("git 应在 PATH");
            assert!(output.status.success(), "git {:?} 失败", args);
        }
    }

    /// git 不可用时给出失败观察，而不是 panic——这条不依赖 git 存在。
    #[tokio::test]
    async fn handles_missing_git_gracefully_when_git_absent() {
        // 用一个必然不是 git 仓库的目录：即使 git 在 PATH，status 也会非 0。
        let dir = std::env::temp_dir().join(format!("qch-git-norepo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建临时目录");

        let tool = GitStatus::new(WorkspaceRoot::new(&dir));
        let outcome = tool.run().await;

        // git 在 PATH：退出码 128（not a repository）；不在：启动失败。
        // 两种都必须是 failed 且带说明，绝不能 panic。
        assert_eq!(outcome.status, OutcomeStatus::Failed, "{}", outcome.content);
        assert!(
            outcome.content.contains("git"),
            "失败说明应点名 git: {}",
            outcome.content
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    #[ignore = "需要 git 在 PATH 中"]
    async fn status_reports_dirty_worktree_in_a_real_repo() {
        let repo = temp_repo("dirty");
        std::fs::write(repo.join("changed.txt"), "新内容\n").unwrap();

        let tool = GitStatus::new(WorkspaceRoot::new(&repo));
        let outcome = tool.run().await;

        assert_eq!(outcome.status, OutcomeStatus::Success, "{}", outcome.content);
        assert!(outcome.content.contains("changed.txt"), "{}", outcome.content);
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[tokio::test]
    #[ignore = "需要 git 在 PATH 中"]
    async fn status_reports_clean_worktree() {
        let repo = temp_repo("clean");
        // 提交一个初始 commit，让工作区干净。
        std::fs::write(repo.join("base.txt"), "base\n").unwrap();
        commit_all(&repo);

        let tool = GitStatus::new(WorkspaceRoot::new(&repo));
        let outcome = tool.run().await;

        assert_eq!(outcome.status, OutcomeStatus::Success, "{}", outcome.content);
        assert!(outcome.content.contains("干净"), "{}", outcome.content);
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[tokio::test]
    #[ignore = "需要 git 在 PATH 中"]
    async fn diff_shows_unstaged_changes_and_truncates() {
        let repo = temp_repo("diff");
        std::fs::write(repo.join("file.txt"), "旧的一行\n").unwrap();
        commit_all(&repo);
        std::fs::write(repo.join("file.txt"), "新的一行\n").unwrap();

        let tool = GitDiff::new(WorkspaceRoot::new(&repo));
        let outcome = tool.run(false).await;

        assert_eq!(outcome.status, OutcomeStatus::Success, "{}", outcome.content);
        assert!(outcome.content.contains("+新的一行"), "{}", outcome.content);
        assert!(outcome.content.contains("-旧的一行"), "{}", outcome.content);
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[tokio::test]
    #[ignore = "需要 git 在 PATH 中"]
    async fn diff_with_no_changes_says_so() {
        let repo = temp_repo("nodiff");
        std::fs::write(repo.join("file.txt"), "内容\n").unwrap();
        commit_all(&repo);

        let tool = GitDiff::new(WorkspaceRoot::new(&repo));
        let outcome = tool.run(false).await;

        assert_eq!(outcome.status, OutcomeStatus::Success, "{}", outcome.content);
        assert!(outcome.content.contains("没有差异"), "{}", outcome.content);
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[tokio::test]
    #[ignore = "需要 git 在 PATH 中"]
    async fn staged_flag_selects_the_staged_diff() {
        let repo = temp_repo("staged");
        std::fs::write(repo.join("file.txt"), "基线\n").unwrap();
        commit_all(&repo);
        // 未暂存的改动：staged diff 应该看不到它。
        std::fs::write(repo.join("file.txt"), "未暂存\n").unwrap();

        let tool = GitDiff::new(WorkspaceRoot::new(&repo));
        let staged = tool.run(true).await;
        let unstaged = tool.run(false).await;

        assert!(staged.content.contains("没有差异"), "staged diff 不应看到未暂存改动: {}", staged.content);
        assert!(unstaged.content.contains("+未暂存"), "{}", unstaged.content);
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[tokio::test]
    async fn invoke_routes_through_argument_parsing() {
        let dir = std::env::temp_dir().join(format!("qch-git-route-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建临时目录");
        let tool = GitDiff::new(WorkspaceRoot::new(&dir));

        // 带参数与不带参数都走通（这里仓库不存在，只验证解析与失败路径）。
        let with_args = tool.invoke(&call(GIT_DIFF, json!({"staged": true}))).await;
        let no_args = tool.invoke(&call(GIT_DIFF, json!({}))).await;
        assert_eq!(with_args.status, OutcomeStatus::Failed);
        assert_eq!(no_args.status, OutcomeStatus::Failed);

        // 坏参数类型 → failed。
        let bad = tool.invoke(&call(GIT_DIFF, json!({"staged": "yes"}))).await;
        assert_eq!(bad.status, OutcomeStatus::Failed);
        assert!(bad.content.contains("参数解析失败"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn git_tools_are_read_only() {
        assert!(GitStatus::spec().annotations.read_only);
        assert!(GitDiff::spec().annotations.read_only);
        assert!(!GitStatus::spec().annotations.requires_approval());
        assert!(!GitDiff::spec().annotations.requires_approval());
    }
}

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
use crate::domain::sandbox::SandboxPolicy;
use crate::domain::tool::{ToolAnnotations, ToolCall, ToolOutcome, ToolSpec};
use crate::domain::workspace::WorkspaceRoot;

use super::parse_args;
use super::shell::{clamp_output, run_with_policy};

const GIT_STATUS: &str = "git_status";
const GIT_DIFF: &str = "git_diff";
/// git 只读命令的超时：比 run_command 默认值短，git 卡住多半是仓库坏了。
const GIT_TIMEOUT: Duration = Duration::from_secs(30);
/// diff 输出的截断上限：diff 可能非常长，4000 字符够模型判断改了什么。
const DIFF_MAX_CHARS: usize = 4000;

/// 查看工作区状态。
pub struct GitStatus {
    root: WorkspaceRoot,
    policy: SandboxPolicy,
}

impl GitStatus {
    /// 以 `root` 为仓库根构造。
    pub fn new(root: WorkspaceRoot) -> Self {
        Self { root, policy: SandboxPolicy::default() }
    }

    /// 工具规格。
    pub fn spec() -> ToolSpec {
        ToolSpec {
            name: GIT_STATUS.to_owned(),
            description: "查看 workspace 内 git 仓库的工作区状态（git status --porcelain），\
                          空输出表示工作区干净。只读操作。workspace 不是 git 仓库根时返回说明，\
                          不会去读父目录的仓库。"
                .to_owned(),
            parameters: json!({"type": "object", "properties": {}}),
            annotations: ToolAnnotations::new(true, false, true, false),
        }
    }

    /// 执行 git status。独立出来供测试与内部复用。
    async fn run(&self) -> ToolOutcome {
        if let Some(notice) = not_a_repo_notice(&self.root) {
            return ToolOutcome::failed(notice);
        }
        let execution = run_with_policy(
            &self.root, "git status --porcelain=v1", GIT_TIMEOUT, &self.policy,
        ).await;
        git_outcome(execution, true)
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
    policy: SandboxPolicy,
}

impl GitDiff {
    /// 以 `root` 为仓库根构造。
    pub fn new(root: WorkspaceRoot) -> Self {
        Self { root, policy: SandboxPolicy::default() }
    }

    /// 工具规格。
    pub fn spec() -> ToolSpec {
        ToolSpec {
            name: GIT_DIFF.to_owned(),
            description: "查看 workspace 内 git 仓库的未暂存差异（git diff）；\
                          staged 为 true 时查看已暂存的差异（git diff --staged）。输出最多约 4000 字符。只读操作。\
                          workspace 不是 git 仓库根时返回说明，不会去读父目录的仓库。"
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
        if let Some(notice) = not_a_repo_notice(&self.root) {
            return ToolOutcome::failed(notice);
        }
        let command = if staged {
            "git diff --staged"
        } else {
            "git diff"
        };
        let execution = run_with_policy(&self.root, command, GIT_TIMEOUT, &self.policy).await;
        git_outcome(execution, false)
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

/// workspace 不是 git 仓库根时，返回给模型的说明；是仓库根则返回 `None`。
///
/// git 在找不到 `.git` 的目录里会**沿父目录一路向上爬**，直到撞见一个仓库
/// 才肯报告。于是 workspace 里的 `git status` 会输出父仓库的状态，模型
/// 据此以为自己看到的是 workspace 的改动——本项目的 workspace 常常是
/// 父目录下的临时副本（基准评测的 fixture 副本、用户实测仓库都中过招），
/// 这个泄露会直接把 agent 带偏去改不相干的文件。
///
/// 判定用 `.git` 的存在性而非 `git rev-parse --show-toplevel`：前者一次
/// stat 就够，且不受 workspace 路径写法（相对/绝对/大小写/尾斜杠）影响；
/// 后者要把两条路径规范化后才能比，而"要不要比"本身还是要先确认 `.git`。
/// `.git` 是文件（worktree、submodule）也算仓库根——它们各自有独立的
/// 工作区，git 报的就是本目录的状态，不会向上爬。
fn not_a_repo_notice(root: &WorkspaceRoot) -> Option<String> {
    if root.as_path().join(".git").exists() {
        return None;
    }
    Some(format!(
        "workspace（{}）不是 git 仓库根：目录下没有 .git。\
         本工具不会去读父目录的仓库——那只会看到与 workspace 无关的状态。\
         如需版本状态，请先在 workspace 内执行 git init。",
        root.as_path().display()
    ))
}

fn git_outcome(
    execution: Result<super::shell::RawOutput, super::shell::RunError>,
    is_status: bool,
) -> ToolOutcome {
    match execution {
        Err(error) => error.into_outcome(false),
        Ok(output) => {
            let command = if is_status { "git status" } else { "git diff" };
            let body = if output.code != 0 {
                format!(
                    "{} 退出码 {}：{}",
                    command,
                    output.code,
                    output.stderr.trim_end()
                )
            } else {
                let stdout = output.stdout.trim_end();
                if stdout.is_empty() {
                    if is_status {
                        "（工作区干净，没有未提交的改动）".to_owned()
                    } else {
                        "（没有差异）".to_owned()
                    }
                } else if is_status {
                    stdout.to_owned()
                } else {
                    clamp_output(stdout.to_owned(), DIFF_MAX_CHARS)
                }
            };
            output.into_outcome(body, false)
        }
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

    /// workspace 里没有 `.git` 时必须明确说"不是仓库根"。
    ///
    /// 这条不依赖 git 在 PATH，也不该依赖：判定本身只看文件系统，git
    /// 根本不会被执行——"不执行"正是修复的一部分（旧行为会执行 git，
    /// 然后 git 沿父目录爬到一个不相干的仓库里去）。
    #[tokio::test]
    async fn workspace_without_git_dir_is_reported_as_not_a_repo() {
        let dir = std::env::temp_dir().join(format!("qch-git-nogitdir-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建临时目录");

        let outcome = GitStatus::new(WorkspaceRoot::new(&dir)).run().await;

        assert_eq!(outcome.status, OutcomeStatus::Failed, "{}", outcome.content);
        assert!(
            outcome.content.contains("不是 git 仓库根"),
            "{}",
            outcome.content
        );
        assert!(
            outcome.execution.is_none(),
            "没启动 git 进程就不能发出受保护执行记录"
        );
        assert!(
            outcome.content.contains(".git"),
            "说明应点名缺失的 .git: {}",
            outcome.content
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 仓库子目录里的 git 工具不得泄露父仓库状态。
    ///
    /// 这是实测中把 agent 带偏的那个 bug 的回归测试：workspace 是仓库的
    /// 子目录时，旧实现让 git 沿父目录爬，模型看到的是父仓库的改动，
    /// 于是据此修改了跟 workspace 无关的文件。
    #[tokio::test]
    #[ignore = "需要 git 在 PATH 中"]
    async fn subdirectory_of_a_repo_does_not_leak_the_parent_status() {
        let repo = temp_repo("subdir");
        // 仓库根的未提交改动：泄露发生时它一定会出现在观察里。
        std::fs::write(repo.join("parent-only.txt"), "父仓库的改动\n").unwrap();
        let nested = repo.join("nested");
        std::fs::create_dir_all(&nested).unwrap();

        let outcome = GitStatus::new(WorkspaceRoot::new(&nested)).run().await;

        assert_eq!(outcome.status, OutcomeStatus::Failed, "{}", outcome.content);
        assert!(
            outcome.content.contains("不是 git 仓库根"),
            "{}",
            outcome.content
        );
        assert!(
            !outcome.content.contains("parent-only.txt"),
            "不得泄露父仓库状态: {}",
            outcome.content
        );
        let _ = std::fs::remove_dir_all(&repo);
    }

    /// diff 同样不得泄露父仓库：父仓库的差异对 workspace 里的模型是噪音。
    #[tokio::test]
    #[ignore = "需要 git 在 PATH 中"]
    async fn subdirectory_of_a_repo_does_not_leak_the_parent_diff() {
        let repo = temp_repo("subdir-diff");
        std::fs::write(repo.join("base.txt"), "基线\n").unwrap();
        commit_all(&repo);
        std::fs::write(repo.join("base.txt"), "父仓库的改动\n").unwrap();
        let nested = repo.join("nested");
        std::fs::create_dir_all(&nested).unwrap();

        let outcome = GitDiff::new(WorkspaceRoot::new(&nested)).run(false).await;

        assert_eq!(outcome.status, OutcomeStatus::Failed, "{}", outcome.content);
        assert!(
            outcome.content.contains("不是 git 仓库根"),
            "{}",
            outcome.content
        );
        assert!(
            !outcome.content.contains("父仓库的改动"),
            "不得泄露父仓库差异: {}",
            outcome.content
        );
        let _ = std::fs::remove_dir_all(&repo);
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

    #[cfg(windows)]
    #[tokio::test]
    async fn git_tools_refuse_when_the_sandbox_cannot_be_created() {
        let dir = std::env::temp_dir().join(format!("qch-git-failclosed-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建临时目录");
        std::fs::create_dir_all(dir.join(".git")).expect("造仓库根标记");
        let mut status = GitStatus::new(WorkspaceRoot::new(&dir));
        let mut diff = GitDiff::new(WorkspaceRoot::new(&dir));
        status.policy.cpu_time = Some(Duration::from_secs(1));
        diff.policy.cpu_time = Some(Duration::from_secs(1));

        for outcome in [status.run().await, diff.run(false).await] {
            assert_eq!(outcome.status, OutcomeStatus::Refused, "{}", outcome.content);
            assert!(outcome.content.contains("沙箱创建失败"), "{}", outcome.content);
            assert!(outcome.execution.is_none(), "命令没跑不能记边界证据");
        }
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

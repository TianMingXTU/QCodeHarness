//! 文本搜索工具：在 workspace 内递归查找子串。
//!
//! 不引 `ignore`/`ripgrep` crate：P2 的目标是让循环能用上搜索，
//! `.gitignore` 语义属于后续优化，多一个重依赖要先证明它不可替代。
//! 这里用 std 遍历 + 硬编码跳过列表（`.git`/`target`/`node_modules`/
//! `.qcodeharness`），已覆盖日常开发的绝大多数噪音。

use serde::Deserialize;
use serde_json::json;
use std::path::Path;

use crate::application::port::{BoxFuture, ToolInvoker};
use crate::domain::tool::{ToolAnnotations, ToolCall, ToolOutcome, ToolSpec};
use crate::domain::workspace::WorkspaceRoot;

use super::{parse_args, resolve_in};

const SEARCH_TEXT: &str = "search_text";

/// 默认返回条数上限。
const DEFAULT_MAX_RESULTS: usize = 50;
/// `max_results` 的硬上限：再大也只会拖长响应，对模型没有额外价值。
const HARD_MAX_RESULTS: usize = 200;

/// 递归遍历时要跳过的目录名。任意深度下命中都跳过。
const SKIPPED_DIRS: [&str; 4] = [".git", "target", "node_modules", ".qcodeharness"];

/// 凭据文件名：遍历时整个跳过，不当成普通文本读。
///
/// P8-1 把 `.env` / `.env.*` 列为受保护路径，`read_file` 直接拒。但搜索走
/// 的是"按跳过名单遍历 + 读每行"这条路，`read_file` 的保护够不着它——
/// `search_text({"pattern":"API_KEY"})` 会把 `.env` 里的凭据行连同行号
/// 一起打印出来。同一份秘密换条路就能读到，保护就是半套。
///
/// 大小写不敏感：Windows 上 `.ENV` 与 `.env` 是同一个文件。
fn is_credential_file(name: &str) -> bool {
    let lowered = name.to_ascii_lowercase();
    lowered == ".env" || lowered.starts_with(".env.")
}

/// 在 workspace 内递归搜索一段文本。
pub struct SearchText {
    root: WorkspaceRoot,
}

impl SearchText {
    /// 以 `root` 为工作区构造。
    pub fn new(root: WorkspaceRoot) -> Self {
        Self { root }
    }

    /// 工具规格。
    pub fn spec() -> ToolSpec {
        ToolSpec {
            name: SEARCH_TEXT.to_owned(),
            description: "在 workspace 内递归搜索一段文本（区分大小写的子串匹配），\
                          输出 `相对路径:行号:该行内容`。跳过 .git、target、node_modules、.qcodeharness \
                          目录与 .env / .env.* 凭据文件。"
                .to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "要搜索的子串" },
                    "path": { "type": "string", "description": "搜索起点，workspace 内相对路径，默认工作区根" },
                    "max_results": { "type": "integer", "description": "最多返回多少条匹配，默认 50，上限 200" }
                },
                "required": ["pattern"]
            }),
            annotations: ToolAnnotations::new(true, false, true, false),
        }
    }
}

/// `search_text` 的参数。
#[derive(Debug, Deserialize)]
struct SearchArgs {
    pattern: String,
    path: Option<String>,
    max_results: Option<u64>,
}

/// 一次遍历的累计结果。
#[derive(Debug, Default)]
struct WalkResult {
    /// 已收集的匹配行（受上限约束）。
    lines: Vec<String>,
    /// 匹配总数（不受上限约束，用于截断说明）。
    total: usize,
}

impl ToolInvoker for SearchText {
    fn invoke<'a>(&'a self, call: &'a ToolCall) -> BoxFuture<'a, ToolOutcome> {
        Box::pin(async move {
            let args: SearchArgs = match parse_args(&call.arguments) {
                Ok(args) => args,
                Err(message) => return ToolOutcome::failed(message),
            };
            if args.pattern.is_empty() {
                return ToolOutcome::failed("pattern 不能为空");
            }
            let start = match resolve_in(&self.root, args.path.as_deref().unwrap_or(".")) {
                Ok(path) => path,
                Err(outcome) => return outcome,
            };
            let cap = args
                .max_results
                .unwrap_or(DEFAULT_MAX_RESULTS as u64)
                .clamp(1, HARD_MAX_RESULTS as u64) as usize;

            // 同步遍历放到阻塞线程池：递归走整个目录树是典型的阻塞操作，
            // 占着异步线程会拖慢同进程里的其他任务。
            let root = self.root.as_path().to_path_buf();
            let start_dir = start.as_path().to_path_buf();
            let pattern = args.pattern;
            let walk_root = root.clone();
            let result = tokio::task::spawn_blocking(move || {
                // 起点的相对路径用 strip_prefix 算，而不是信原始参数——
                // "./src" 和 "src" 应该给出同样的显示路径。
                let rel = start_dir
                    .strip_prefix(&walk_root)
                    .map(|p| p.to_string_lossy().replace('\\', "/"))
                    .unwrap_or_default();
                let mut result = WalkResult::default();
                walk(&start_dir, &rel, &pattern, cap, &mut result);
                result
            })
            .await;

            let result = match result {
                Ok(result) => result,
                Err(error) => {
                    return ToolOutcome::failed(format!("搜索任务失败: {error}"));
                }
            };

            if result.total == 0 {
                return ToolOutcome::success("未找到匹配");
            }
            if result.lines.len() < result.total {
                return ToolOutcome::success(format!(
                    "{}\n…（已截断，共匹配 {} 处，显示前 {} 处）",
                    result.lines.join("\n"),
                    result.total,
                    result.lines.len()
                ));
            }
            ToolOutcome::success(result.lines.join("\n"))
        })
    }
}

/// 递归遍历 `dir`，把匹配行写进 `result`。
///
/// 单个文件/目录读不动（权限、非 UTF-8）时静默跳过：搜索是尽力而为的
/// 探索性操作，一个读不了的文件不该让整次搜索失败。
///
/// 达到条数上限后不提前退出：截断说明要报"共匹配 N 处"，
/// 数完总数模型才知道自己漏看了多少。
fn walk(dir: &Path, relative: &str, pattern: &str, cap: usize, result: &mut WalkResult) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        let child_rel = if relative.is_empty() {
            name.clone()
        } else {
            format!("{relative}/{name}")
        };
        if is_dir {
            if SKIPPED_DIRS.contains(&name.as_str()) {
                continue;
            }
            walk(&entry.path(), &child_rel, pattern, cap, result);
        } else if is_credential_file(&name) {
            // 凭据文件整份跳过：读进来的每一行都可能带着密钥，而这一行
            // 会原样进提示词。搜索是"找代码"的工具，不是"读秘密"的工具。
            continue;
        } else if let Ok(content) = std::fs::read_to_string(entry.path()) {
            scan_file(&content, &child_rel, pattern, cap, result);
        }
    }
}

/// 扫描单个文件的每一行。
fn scan_file(content: &str, relative: &str, pattern: &str, cap: usize, result: &mut WalkResult) {
    for (index, line) in content.lines().enumerate() {
        if line.contains(pattern) {
            result.total += 1;
            if result.lines.len() < cap {
                result
                    .lines
                    .push(format!("{relative}:{}:{line}", index + 1));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::ids::CallId;
    use crate::domain::tool::OutcomeStatus;
    use std::fs;
    use std::path::PathBuf;

    /// 建一个临时 workspace，返回根路径。测试结束由调用方清理。
    fn temp_workspace(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("qch-search-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("建临时目录");
        dir
    }

    fn call(arguments: serde_json::Value) -> ToolCall {
        ToolCall {
            id: CallId::new("call_search"),
            name: SEARCH_TEXT.to_owned(),
            arguments,
        }
    }

    /// 布置一个典型 workspace：两个文件有匹配，target 与 .git 里的不碰。
    fn plant(root: &Path) {
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join("target")).unwrap();
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::write(root.join("src/main.rs"), "fn main() {}\n// TODO fix this\n").unwrap();
        fs::write(root.join("src/lib.rs"), "TODO fix\nfn x() {}\nTODO again\n").unwrap();
        fs::write(root.join("target/generated.rs"), "TODO fix\n").unwrap();
        fs::write(root.join(".git/config"), "TODO fix\n").unwrap();
        fs::write(root.join("README.md"), "没有匹配的内容\n").unwrap();
    }

    /// 凭据文件不能被全文搜索读到。
    ///
    /// P8-1 把 `.env` 列为受保护路径，`read_file` 直接拒。但 `search_text`
    /// 走的是另一条路——它按跳过名单遍历，而名单里原本没有 `.env`，于是
    /// `search_text({"pattern":"API_KEY"})` 能把 `.env` 里的凭据行连同行号
    /// 一起打印出来。同一份秘密，换条路就能读到，那保护就是半套。
    ///
    /// 大小写同样要不敏感：Windows 上 `.ENV` 与 `.env` 是同一个文件。
    #[tokio::test]
    async fn search_never_reads_credential_files() {
        let root = temp_workspace("env");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join(".env"), "API_KEY={{SECRET_0a71ea2c}}\n").unwrap();
        fs::write(root.join(".ENV.local"), "DB_PASSWORD={{SECRET_d34bace8}}\n").unwrap();
        fs::write(root.join("src/main.rs"), "const API_KEY: &str = \"\";\n").unwrap();
        let tool = SearchText::new(WorkspaceRoot::new(&root));

        let outcome = tool.invoke(&call(json!({"pattern": "KEY"}))).await;

        assert_eq!(outcome.status, OutcomeStatus::Success, "{}", outcome.content);
        assert!(
            outcome.content.contains("src/main.rs:1"),
            "workspace 内的正常文件仍要能搜到: {}",
            outcome.content
        );
        assert!(
            !outcome.content.contains("{{SECRET_0a71ea2c}}"),
            ".env 里的凭据不能被搜出来: {}",
            outcome.content
        );
        assert!(
            !outcome.content.contains("{{SECRET_d34bace8}}"),
            ".ENV.local 里的凭据不能被搜出来: {}",
            outcome.content
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn finds_matches_with_relative_paths_and_line_numbers() {
        let root = temp_workspace("basic");
        plant(&root);
        let tool = SearchText::new(WorkspaceRoot::new(&root));

        let outcome = tool.invoke(&call(json!({"pattern": "TODO"}))).await;

        assert_eq!(outcome.status, OutcomeStatus::Success, "{}", outcome.content);
        // 相对路径用 / 分隔，行号从 1 开始。
        assert!(outcome.content.contains("src/main.rs:2:// TODO fix this"), "{}", outcome.content);
        assert!(outcome.content.contains("src/lib.rs:1:TODO fix"), "{}", outcome.content);
        assert!(outcome.content.contains("src/lib.rs:3:TODO again"), "{}", outcome.content);
        let _ = fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn skips_build_and_vcs_directories() {
        let root = temp_workspace("skip");
        plant(&root);
        let tool = SearchText::new(WorkspaceRoot::new(&root));

        let outcome = tool.invoke(&call(json!({"pattern": "TODO"}))).await;

        assert!(!outcome.content.contains("target/"), "{}", outcome.content);
        assert!(!outcome.content.contains(".git/"), "{}", outcome.content);
        let _ = fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn truncates_and_reports_the_total() {
        let root = temp_workspace("cap");
        plant(&root);
        let tool = SearchText::new(WorkspaceRoot::new(&root));

        let outcome = tool
            .invoke(&call(json!({"pattern": "TODO", "max_results": 2})))
            .await;

        assert_eq!(outcome.status, OutcomeStatus::Success);
        // 共 3 处匹配（target 与 .git 已跳过），只显示 2 条，截断说明里带总数。
        assert!(outcome.content.contains("已截断，共匹配 3 处"), "{}", outcome.content);
        assert!(outcome.content.contains("显示前 2 处"), "{}", outcome.content);
        let _ = fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn zero_matches_is_success_not_error() {
        let root = temp_workspace("empty");
        plant(&root);
        let tool = SearchText::new(WorkspaceRoot::new(&root));

        let outcome = tool.invoke(&call(json!({"pattern": "XYZZY_NOT_THERE"}))).await;

        assert_eq!(outcome.status, OutcomeStatus::Success);
        assert_eq!(outcome.content, "未找到匹配");
        assert!(!outcome.is_error());
        let _ = fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn empty_pattern_fails() {
        let root = temp_workspace("emptypat");
        let tool = SearchText::new(WorkspaceRoot::new(&root));

        let outcome = tool.invoke(&call(json!({"pattern": ""}))).await;

        assert_eq!(outcome.status, OutcomeStatus::Failed);
        assert!(outcome.content.contains("不能为空"));
        let _ = fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn escaping_path_is_refused() {
        let root = temp_workspace("escape");
        plant(&root);
        let tool = SearchText::new(WorkspaceRoot::new(&root));

        let outcome = tool
            .invoke(&call(json!({"pattern": "TODO", "path": "../elsewhere"})))
            .await;

        assert_eq!(outcome.status, OutcomeStatus::Refused);
        let _ = fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn path_argument_narrows_the_search_root() {
        let root = temp_workspace("narrow");
        plant(&root);
        let tool = SearchText::new(WorkspaceRoot::new(&root));

        let outcome = tool
            .invoke(&call(json!({"pattern": "TODO", "path": "src"})))
            .await;

        assert!(outcome.content.contains("src/lib.rs"), "{}", outcome.content);
        assert!(!outcome.content.contains("README"), "{}", outcome.content);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn walk_counts_matches_without_emitting_past_the_cap() {
        // 按行匹配：每行要么算一处，要么不算——行内的多次出现不重复计。
        let mut result = WalkResult::default();
        scan_file("x1\nplain\nx2\nx3\n", "f.txt", "x", 2, &mut result);

        assert_eq!(result.total, 3, "总数不受上限影响");
        assert_eq!(result.lines.len(), 2, "收集数受上限约束");
        assert_eq!(result.lines[0], "f.txt:1:x1");
        assert_eq!(result.lines[1], "f.txt:3:x2");
    }
}

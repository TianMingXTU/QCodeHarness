//! 文本搜索工具：在 workspace 内递归查找文本，支持正则与扩展名过滤。
//!
//! 遍历使用 `ignore` crate，自动读取 workspace 根与嵌套目录的
//! `.gitignore`，被 ignore 的文件不搜索。另加显式目录名单兜底
//! （`.git`/`.qcodeharness`/`target`/`node_modules`）：`.git` 不受
//! gitignore 规则管理；`target` 是否被 ignore 取决于项目的 .gitignore，
//! 不能依赖。
//!
//! `.gitignore` 只影响搜索范围，不影响读权限：`read_file` 仍可读取被
//! ignore 的文件。ignore 状态用于降低全文搜索的噪声，不是额外的读权限
//! 检查。

use ignore::WalkBuilder;
use regex::Regex;
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

/// 递归遍历时显式跳过的目录名，任意深度命中都跳过。
///
/// `.git` 不在 gitignore 规则覆盖范围内（git 从不跟踪它），
/// `.qcodeharness` 存放会话证据。`target`/`node_modules` 通常被 ignore，
/// 但项目可能没有写这条规则，全文搜索会产生大量无关结果，这里显式
/// 兜底。
const SKIPPED_DIRS: [&str; 4] = [".git", "target", "node_modules", ".qcodeharness"];

/// 凭据文件名：遍历时整体跳过，不当作普通文本读取。
///
/// `.env` / `.env.*` 在 `read_file` 侧是受保护路径，直接拒绝。但搜索
/// 走"遍历 + 读每行"的路径，`read_file` 的保护覆盖不到：
/// `search_text({"pattern":"API_KEY"})` 会把 `.env` 里的凭据行连同行号
/// 一起输出。同一份秘密换条路径仍能读到，保护就不完整。
///
/// 这条判断优先于其他所有过滤：.gitignore 感知与扩展名过滤都不能跳过
/// 凭据检查。`.env` 通常不被 git 跟踪，常落在 ignore 名单之外，凭据
/// 保护依靠文件名本身。
///
/// 大小写不敏感：所有平台都按不敏感比较。Windows 与 macOS 是文件系统
/// 语义使然，Linux 上是主动多拒几个形似 `.env` 的路径（代价是一次误拒，
/// 漏拒的代价是泄露凭据）。
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
            description: "在 workspace 内递归搜索文本，输出 `相对路径:行号:该行内容`。\
                          默认区分大小写的子串匹配；设 `regex: true` 按 Rust 正则匹配。\
                          可用 `include`/`exclude` 按扩展名过滤（如 [\"rs\"]；同时给时 include 优先）。\
                          自动遵循 .gitignore；无论 ignore 状态如何，.env/.env.* 凭据文件一律跳过。\
                          已知文件路径要读内容用 read_file（多个文件用 read_files）；\
                          已知要改什么时，改一处用 edit_file，改多处用 apply_patch。"
                .to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "要搜索的文本；regex 为 true 时是正则表达式" },
                    "path": { "type": "string", "description": "搜索起点，workspace 内相对路径，默认工作区根" },
                    "max_results": { "type": "integer", "description": "最多返回多少条匹配，默认 50，上限 200" },
                    "regex": { "type": "boolean", "description": "true 时 pattern 按正则解释（默认 false，子串匹配）" },
                    "include": { "type": "array", "items": { "type": "string" }, "description": "只搜这些扩展名的文件，如 [\"rs\",\"toml\"]" },
                    "exclude": { "type": "array", "items": { "type": "string" }, "description": "跳过这些扩展名的文件；与 include 同时给时 include 优先" }
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
    /// 使用布尔开关而非 `pattern_kind` 字符串枚举：多数调用仍是子串匹配，
    /// 布尔缺省即原语义，模型也少一个需要记对的字面量。
    #[serde(default)]
    regex: bool,
    #[serde(default)]
    include: Vec<String>,
    #[serde(default)]
    exclude: Vec<String>,
}

/// 规范化一个扩展名参数：`".rs"` / `"rs"` / `"RS"` 统一成小写无点形式。
fn normalize_ext(raw: &str) -> String {
    raw.trim_start_matches('.').to_ascii_lowercase()
}

/// 扩展名过滤器。
///
/// include 与 exclude 同时给出时 include 优先：include 表达要找的内容，
/// 正向且具体；exclude 表达要排除的噪声，负向。冲突时以具体的正向选择
/// 为准，否则两张名单叠加后行为难以解释。
#[derive(Debug, Default)]
struct ExtFilter {
    include: Vec<String>,
    exclude: Vec<String>,
}

impl ExtFilter {
    /// 没给任何名单时放行一切。
    fn is_active(&self) -> bool {
        !self.include.is_empty() || !self.exclude.is_empty()
    }

    /// `include` 只搜这些扩展名，`exclude` 跳过这些，冲突时 include 优先。
    ///
    /// 无扩展名文件（Makefile、LICENSE、Dockerfile）：给出 include 时排除，
    /// 因为 include 按扩展名收窄，无扩展名文件不属于任何扩展名；只给
    /// exclude 时放行，用户排除的是具体扩展名，无扩展名文件不在其中。
    fn allows(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .map(|e| e.to_string_lossy().to_ascii_lowercase());
        if !self.include.is_empty() {
            return ext.is_some() && self.include.iter().any(|want| ext.as_deref() == Some(want.as_str()));
        }
        !ext.is_some_and(|actual| self.exclude.contains(&actual))
    }
}

/// 一次遍历的累计结果。
#[derive(Debug, Default)]
struct WalkResult {
    /// 已收集的匹配行（受上限约束）。
    lines: Vec<String>,
    /// 匹配总数（不受上限约束，用于截断说明）。
    total: usize,
}

/// 行匹配器：子串与正则共用同一判定口径，即"这一行是否匹配"。
///
/// 子串使用 `contains`，行内多次出现只算一处；正则使用 `Regex::is_match`，
/// 同样按行整体判定。两种模式的计数口径一致，截断说明含义相同。
enum Matcher {
    Substring(String),
    Regex(Regex),
}

impl Matcher {
    fn matches_line(&self, line: &str) -> bool {
        match self {
            Matcher::Substring(needle) => line.contains(needle.as_str()),
            Matcher::Regex(re) => re.is_match(line),
        }
    }
}

/// 编译正则。失败时返回模型可自行修正的错误：原模式与 regex 库的
/// 定位说明。
fn compile_regex(pattern: &str) -> Result<Matcher, String> {
    Regex::new(pattern)
        .map(Matcher::Regex)
        .map_err(|error| {
            format!(
                "正则编译失败: pattern `{pattern}` 不是合法的正则表达式（{error}）；\
                 若只想搜普通文本，请去掉 regex 参数按子串匹配"
            )
        })
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
            let matcher = if args.regex {
                match compile_regex(&args.pattern) {
                    Ok(matcher) => matcher,
                    Err(message) => return ToolOutcome::failed(message),
                }
            } else {
                Matcher::Substring(args.pattern.clone())
            };
            let filter = ExtFilter {
                include: args.include.iter().map(String::as_str).map(normalize_ext).collect(),
                exclude: args.exclude.iter().map(String::as_str).map(normalize_ext).collect(),
            };
            let start = match resolve_in(&self.root, args.path.as_deref().unwrap_or(".")) {
                Ok(path) => path,
                Err(outcome) => return outcome,
            };
            let cap = args
                .max_results
                .unwrap_or(DEFAULT_MAX_RESULTS as u64)
                .clamp(1, HARD_MAX_RESULTS as u64) as usize;

            // 同步遍历放入阻塞线程池：递归整个目录树是阻塞操作，占用异步
            // 线程会拖慢同进程的其他任务。
            let root = self.root.as_path().to_path_buf();
            let start_dir = start.as_path().to_path_buf();
            let result = tokio::task::spawn_blocking(move || {
                let mut result = WalkResult::default();
                walk(&root, &start_dir, &matcher, &filter, cap, &mut result);
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

/// 遍历 `workspace` 全树，把 `start` 子树内文件的匹配行写入 `result`。
///
/// 使用 `ignore::WalkBuilder` 遍历整个 workspace，它自动读取 workspace
/// 根与各嵌套目录的 `.gitignore`（子目录规则只约束自己的子树）。搜索
/// 起点是子目录时按前缀裁剪。`hidden(false)` 让 `.gitignore` 这类点开头
/// 文件仍可被搜索；隐藏文件的跳过交由显式目录名单处理。`.git` 不在
/// gitignore 规则覆盖范围内，必须在这里拦截。
///
/// 单线程遍历：文件系统操作本身需要排队，多线程遍历对小 workspace 只
/// 增加复杂度，还会让结果顺序不确定。
///
/// 单个文件或目录读不了（权限、非 UTF-8）时静默跳过。搜索是尽力而为的
/// 探索操作，一个文件读不了不应让整次搜索失败。
///
/// 达到条数上限后不提前退出：截断说明需要报告"共匹配 N 处"，数完总数
/// 模型才知道漏看了多少。
fn walk(
    workspace: &Path,
    start: &Path,
    matcher: &Matcher,
    filter: &ExtFilter,
    cap: usize,
    result: &mut WalkResult,
) {
    let walker = WalkBuilder::new(workspace)
        .hidden(false)
        .git_ignore(true)
        .parents(true)
        // 测试环境与临时目录通常没有 .git，而 ignore crate 默认
        // require_git(true)，不在 git 仓库中就不读 .gitignore。搜索的目标
        // 是"被 ignore 的文件不搜索"，这条语义不应依赖仓库是否存在。
        .require_git(false)
        .build();
    let Some(start_prefix) = start.strip_prefix(workspace).ok() else {
        return;
    };
    for entry in walker.flatten() {
        let path = entry.path();
        let Ok(relative) = path.strip_prefix(workspace) else {
            continue;
        };
        if !relative.starts_with(start_prefix) {
            continue;
        }
        if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let mut components = relative.components();
        if components.any(|c| SKIPPED_DIRS.contains(&c.as_os_str().to_string_lossy().as_ref())) {
            continue;
        }
        let child_rel = relative.to_string_lossy().replace('\\', "/");
        if is_credential_file(name) {
            // 凭据文件整体跳过：其中每一行都可能带密钥，而匹配行会原样进入
            // 提示词。搜索用于找代码，不用于读取秘密。
            continue;
        } else if filter.is_active() && !filter.allows(path) {
            continue;
        } else if let Ok(content) = std::fs::read_to_string(path) {
            scan_file(&content, &child_rel, matcher, cap, result);
        }
    }
}

/// 扫描单个文件的每一行。
fn scan_file(content: &str, relative: &str, matcher: &Matcher, cap: usize, result: &mut WalkResult) {
    for (index, line) in content.lines().enumerate() {
        if matcher.matches_line(line) {
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

    /// 布置一个典型 workspace：两个文件含匹配，target 与 .git 中的不参与。
    ///
    /// 不写 .gitignore，用于验证没有 .gitignore 时行为与启用 gitignore
    /// 感知时一致。
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

    /// 凭据文件不能被全文搜索读取。
    ///
    /// `.env` 在 `read_file` 侧被直接拒绝，但 `search_text` 按遍历读取
    /// 每行，`search_text({"pattern":"API_KEY"})` 会输出 `.env` 中的凭据
    /// 行连同其行号。保护必须覆盖这条路径。
    ///
    /// 大小写不敏感：所有平台都按不敏感比较。Windows 与 macOS 是文件系统
    /// 语义使然，Linux 上是主动多拒几个形似 `.env` 的路径（代价是一次误拒，
    /// 漏拒的代价是泄露凭据）。
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

    /// 凭据跳过不受 .gitignore 影响：.env 不在 ignore 名单里也跳过，嵌套
    /// 目录中的 .env 同样受保护。
    #[tokio::test]
    async fn credential_files_are_skipped_even_when_not_gitignored() {
        let root = temp_workspace("env-deep");
        fs::create_dir_all(root.join("src/nested")).unwrap();
        fs::write(root.join(".env"), "API_KEY={{SECRET_aa11bb22}}\n").unwrap();
        fs::write(root.join("src/nested/.env"), "DB_PASSWORD={{SECRET_cc33dd44}}\n").unwrap();
        fs::write(root.join("src/main.rs"), "const X = 1;\n").unwrap();
        let tool = SearchText::new(WorkspaceRoot::new(&root));

        let outcome = tool.invoke(&call(json!({"pattern": "SECRET"}))).await;

        assert_eq!(outcome.status, OutcomeStatus::Success, "{}", outcome.content);
        assert_eq!(outcome.content, "未找到匹配", "凭据行任何深度都搜不出来: {}", outcome.content);
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
        // 共 3 处匹配（target 与 .git 已跳过），只显示 2 条，截断说明含总数。
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
        // 按行匹配：每行计一处，行内多次出现不重复计数。
        let mut result = WalkResult::default();
        scan_file("x1\nplain\nx2\nx3\n", "f.txt", &Matcher::Substring("x".to_owned()), 2, &mut result);

        assert_eq!(result.total, 3, "总数不受上限影响");
        assert_eq!(result.lines.len(), 2, "收集数受上限约束");
        assert_eq!(result.lines[0], "f.txt:1:x1");
        assert_eq!(result.lines[1], "f.txt:3:x2");
    }

    // ── .gitignore 感知 ────────────────────────────────────────────────

    /// 根目录 .gitignore 里的文件不被搜到；被 ignore 的目录整体跳过。
    #[tokio::test]
    async fn gitignored_files_are_not_searched() {
        let root = temp_workspace("gi-root");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join("build")).unwrap();
        fs::write(root.join(".gitignore"), "secret_key.rs\n/build/\n").unwrap();
        fs::write(root.join("src/main.rs"), "TODO real\n").unwrap();
        fs::write(root.join("secret_key.rs"), "TODO hidden\n").unwrap();
        fs::write(root.join("build/output.rs"), "TODO hidden\n").unwrap();
        let tool = SearchText::new(WorkspaceRoot::new(&root));

        let outcome = tool.invoke(&call(json!({"pattern": "TODO"}))).await;

        assert_eq!(outcome.status, OutcomeStatus::Success, "{}", outcome.content);
        assert!(outcome.content.contains("src/main.rs"), "{}", outcome.content);
        assert!(
            !outcome.content.contains("TODO hidden"),
            "gitignore 里的文件与被 ignore 的目录都不该被搜到: {}",
            outcome.content
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// 嵌套目录的 .gitignore 同样生效：ignore crate 从文件所在目录向上
    /// 收集规则链，子目录规则只约束子目录。
    #[tokio::test]
    async fn nested_gitignore_applies_to_its_subtree() {
        let root = temp_workspace("gi-nested");
        fs::create_dir_all(root.join("src/generated")).unwrap();
        fs::write(root.join("src/main.rs"), "TODO real\n").unwrap();
        fs::write(root.join("src/generated/.gitignore"), "*.rs\n").unwrap();
        fs::write(root.join("src/generated/auto.rs"), "TODO hidden\n").unwrap();
        let tool = SearchText::new(WorkspaceRoot::new(&root));

        let outcome = tool.invoke(&call(json!({"pattern": "TODO"}))).await;

        assert_eq!(outcome.status, OutcomeStatus::Success, "{}", outcome.content);
        assert!(outcome.content.contains("src/main.rs"), "{}", outcome.content);
        assert!(
            !outcome.content.contains("auto.rs"),
            "嵌套 .gitignore 应作用于自己的子目录: {}",
            outcome.content
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// 没有 .gitignore 时行为与升级前一致，所有普通文件照常可搜。
    /// 使用 `plant` 的布置（不带 .gitignore）。
    #[tokio::test]
    async fn without_gitignore_behaviour_is_unchanged() {
        let root = temp_workspace("gi-absent");
        plant(&root);
        let tool = SearchText::new(WorkspaceRoot::new(&root));

        let outcome = tool.invoke(&call(json!({"pattern": "TODO", "max_results": 200}))).await;

        assert_eq!(outcome.status, OutcomeStatus::Success, "{}", outcome.content);
        assert!(outcome.content.contains("src/main.rs:2"), "{}", outcome.content);
        assert!(outcome.content.contains("src/lib.rs:1"), "{}", outcome.content);
        assert!(outcome.content.contains("src/lib.rs:3"), "{}", outcome.content);
        assert!(!outcome.content.contains("target/"), "{}", outcome.content);
        assert!(!outcome.content.contains(".git/"), "{}", outcome.content);
        let _ = fs::remove_dir_all(&root);
    }

    /// gitignore 只影响搜索，不影响读权限：`read_file` 仍可读取被 ignore 的
    /// 文件。这条测试固定两个工具的语义分工（见模块注释）。
    #[tokio::test]
    async fn read_file_still_allows_gitignored_files() {
        let root = temp_workspace("gi-read");
        fs::write(root.join(".gitignore"), "secret_key.rs\n").unwrap();
        fs::write(root.join("secret_key.rs"), "not really a secret\n").unwrap();
        let tool = super::super::file::ReadFile::new(WorkspaceRoot::new(&root));
        let outcome = tool.invoke(&call(json!({"path": "secret_key.rs"}))).await;

        assert_eq!(
            outcome.status,
            OutcomeStatus::Success,
            "gitignore 不该拦 read_file: {}",
            outcome.content
        );
        let _ = fs::remove_dir_all(&root);
    }

    // ── 正则 ──────────────────────────────────────────────────────────

    /// `regex=true` 时按正则匹配，默认仍是子串。`TODO|todo` 作为子串
    /// 搜不到，作为正则可以搜到。
    #[tokio::test]
    async fn regex_mode_matches_pattern_syntax() {
        let root = temp_workspace("regex");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/main.rs"), "TODO fix\nfn main() {}\ntodo later\n").unwrap();
        let tool = SearchText::new(WorkspaceRoot::new(&root));

        // 子串语义下 `TODO|todo` 是字面量，且大小写敏感，搜不到。
        let substring = tool.invoke(&call(json!({"pattern": "TODO|todo"}))).await;
        assert_eq!(substring.content, "未找到匹配", "不带 regex 时应按子串字面匹配");

        let regex_outcome = tool.invoke(&call(json!({"pattern": "TODO|todo", "regex": true}))).await;
        assert_eq!(regex_outcome.status, OutcomeStatus::Success, "{}", regex_outcome.content);
        assert!(regex_outcome.content.contains("src/main.rs:1:TODO fix"), "{}", regex_outcome.content);
        assert!(regex_outcome.content.contains("src/main.rs:3:todo later"), "{}", regex_outcome.content);
        let _ = fs::remove_dir_all(&root);
    }

    /// 正则编译失败时返回原模式与修正建议，模型可以自行修改。
    #[tokio::test]
    async fn invalid_regex_fails_with_actionable_message() {
        let root = temp_workspace("badregex");
        let tool = SearchText::new(WorkspaceRoot::new(&root));

        let outcome = tool.invoke(&call(json!({"pattern": "TODO(", "regex": true}))).await;

        assert_eq!(outcome.status, OutcomeStatus::Failed);
        assert!(outcome.content.contains("TODO("), "错误信息应带原模式: {}", outcome.content);
        assert!(outcome.content.contains("正则编译失败"), "{}", outcome.content);
        assert!(outcome.content.contains("regex"), "应提示可去掉 regex 参数: {}", outcome.content);
        let _ = fs::remove_dir_all(&root);
    }

    // ── 扩展名过滤 ─────────────────────────────────────────────────────

    /// include 只搜指定扩展名；exclude 跳过指定扩展名；大小写不敏感。
    #[tokio::test]
    async fn include_and_exclude_filter_by_extension() {
        let root = temp_workspace("ext");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/main.rs"), "MARK code\n").unwrap();
        fs::write(root.join("notes.md"), "MARK doc\n").unwrap();
        fs::write(root.join("Cargo.toml"), "MARK cfg\n").unwrap();
        let tool = SearchText::new(WorkspaceRoot::new(&root));

        let only_rs = tool.invoke(&call(json!({"pattern": "MARK", "include": ["rs"]}))).await;
        assert_eq!(only_rs.status, OutcomeStatus::Success, "{}", only_rs.content);
        assert!(only_rs.content.contains("src/main.rs"), "{}", only_rs.content);
        assert!(!only_rs.content.contains("notes.md"), "include 应排除非 rs 文件: {}", only_rs.content);
        assert!(!only_rs.content.contains("Cargo.toml"), "{}", only_rs.content);

        // 大小写不敏感：`.RS` 与 `rs` 匹配同一批文件。
        let upper = tool.invoke(&call(json!({"pattern": "MARK", "include": [".RS"]}))).await;
        assert!(upper.content.contains("src/main.rs"), "扩展名匹配应大小写不敏感: {}", upper.content);

        let no_md = tool.invoke(&call(json!({"pattern": "MARK", "exclude": ["md"]}))).await;
        assert!(no_md.content.contains("src/main.rs"), "{}", no_md.content);
        assert!(no_md.content.contains("Cargo.toml"), "exclude 只应排除 md: {}", no_md.content);
        assert!(!no_md.content.contains("notes.md"), "exclude 应排除 md 文件: {}", no_md.content);

        // 带点写法（`.md`）与不带点写法（`md`）等价。
        let dotted = tool.invoke(&call(json!({"pattern": "MARK", "exclude": [".md"]}))).await;
        assert!(!dotted.content.contains("notes.md"), "{}", dotted.content);
        let _ = fs::remove_dir_all(&root);
    }

    /// include 与 exclude 同时给出时 include 优先：正向选择更具体，
    /// exclude 不能拦掉 include 已放行的扩展名。
    #[tokio::test]
    async fn include_wins_over_exclude() {
        let root = temp_workspace("ext-both");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/main.rs"), "MARK code\n").unwrap();
        fs::write(root.join("notes.md"), "MARK doc\n").unwrap();
        let tool = SearchText::new(WorkspaceRoot::new(&root));

        let outcome = tool
            .invoke(&call(json!({"pattern": "MARK", "include": ["rs"], "exclude": ["rs"]})))
            .await;

        assert_eq!(outcome.status, OutcomeStatus::Success, "{}", outcome.content);
        assert!(
            outcome.content.contains("src/main.rs"),
            "冲突时 include 优先，rs 文件应仍被搜出: {}",
            outcome.content
        );
        assert!(!outcome.content.contains("notes.md"), "{}", outcome.content);
        let _ = fs::remove_dir_all(&root);
    }

    /// 无扩展名文件：有 include 时排除（不属于任何扩展名），只有 exclude
    /// 时放行（不在被点名排除的扩展名之列）。
    #[test]
    fn extensionless_files_follow_include_narrows_exclude_passes() {
        let only_ext = ExtFilter {
            include: vec!["rs".to_owned()],
            exclude: vec![],
        };
        assert!(!only_ext.allows(Path::new("Makefile")), "include 收窄时应排除无扩展名文件");
        assert!(only_ext.allows(Path::new("main.rs")));

        let skip_ext = ExtFilter {
            include: vec![],
            exclude: vec!["log".to_owned()],
        };
        assert!(skip_ext.allows(Path::new("Makefile")), "只 exclude 时无扩展名文件应放行");
        assert!(!skip_ext.allows(Path::new("app.log")));

        let inactive = ExtFilter::default();
        assert!(inactive.allows(Path::new("anything.bin")), "没给名单时放行一切");
    }
}

//! 文件工具：读取、列目录、整体写入。
//!
//! 三个工具各自独立成结构体，配置都只有 workspace 根。共享的路径校验
//! 与参数解析在 [`super`] 里，这里不复述。
//!
//! `write_file` 与 `edit_file`（见 `super::edit`）的分工要在描述里说清：
//! 前者创建新文件或整体覆盖，后者只替换已有文件中的一段——让模型
//! 在该用哪个上不靠猜。

use serde::Deserialize;
use serde_json::json;

use crate::application::port::{BoxFuture, ToolInvoker};
use crate::domain::tool::{OutcomeStatus, ToolAnnotations, ToolCall, ToolOutcome, ToolSpec};
use crate::domain::workspace::WorkspaceRoot;

use super::{parse_args, resolve_in};

const READ_FILE: &str = "read_file";
const READ_FILES: &str = "read_files";
const LIST_DIR: &str = "list_dir";
const WRITE_FILE: &str = "write_file";

/// `read_file` 默认显示的行数。与 search_text/run_command 的截断口径对齐：
/// read_file 是只读工具，默认策略下不问人就执行，一旦读进一个超长文件，
/// 整份内容会直接吃掉大量上下文——所以它不能是唯一"裸奔"的工具。
const READ_FILE_DEFAULT_LINES: usize = 400;
/// `max_lines` 的硬上限。再大的文件也该用 search_text 定位后分块读。
const READ_FILE_HARD_MAX_LINES: usize = 2000;

// ── read_file ────────────────────────────────────────────────────────

/// 读取一个文本文件的完整内容。
pub struct ReadFile {
    root: WorkspaceRoot,
}

impl ReadFile {
    /// 以 `root` 为工作区构造。
    pub fn new(root: WorkspaceRoot) -> Self {
        Self { root }
    }

    /// 工具规格。
    pub fn spec() -> ToolSpec {
        ToolSpec {
            name: READ_FILE.to_owned(),
            description: format!(
                "读取 workspace 内指定文本文件的内容，返回 UTF-8 文本。\
                 默认最多显示前 {READ_FILE_DEFAULT_LINES} 行，超长时只返回开头并附总行数；\
                 大文件请先用 search_text 定位目标位置，再调大 max_lines（上限 {READ_FILE_HARD_MAX_LINES}）读取。\
                 已经知道要读哪几个文件时改用 read_files 一次读回，省掉逐个调用的往返。\
                 要改代码时用 apply_patch，不要用 write_file 整体覆盖。"
            ),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "workspace 内的相对路径，例如 src/main.rs" },
                    "max_lines": {
                        "type": "integer",
                        "description": format!("最多读取的行数，默认 {READ_FILE_DEFAULT_LINES}，上限 {READ_FILE_HARD_MAX_LINES}")
                    }
                },
                "required": ["path"]
            }),
            annotations: ToolAnnotations::new(true, false, true, false),
        }
    }
}

/// `read_file` 的参数。
#[derive(Debug, Deserialize)]
struct ReadFileArgs {
    path: String,
    max_lines: Option<usize>,
}

impl ToolInvoker for ReadFile {
    fn invoke<'a>(&'a self, call: &'a ToolCall) -> BoxFuture<'a, ToolOutcome> {
        Box::pin(async move {
            let args: ReadFileArgs = match parse_args(&call.arguments) {
                Ok(args) => args,
                Err(message) => return ToolOutcome::failed(message),
            };
            read_one_file(&self.root, &args.path, args.max_lines).await
        })
    }
}

/// 读一个文件并按行数上限渲染。
///
/// `read_file` 与 `read_files` 共用这一条路径：两条路对同一个文件必须给出
/// 逐字节相同的结果（含截断说明的措辞），否则模型对"这个文件有多长"的
/// 判断会取决于它是一次读的还是批量读的——而它没有任何依据去做这个区分。
async fn read_one_file(
    root: &WorkspaceRoot,
    raw_path: &str,
    max_lines: Option<usize>,
) -> ToolOutcome {
    let path = match resolve_in(root, raw_path) {
        Ok(path) => path,
        Err(outcome) => return outcome,
    };
    let content = match tokio::fs::read_to_string(path.as_path()).await {
        Ok(content) => content,
        Err(error) => {
            // InvalidData 即"内容不是合法 UTF-8"：多半是二进制文件。
            // 不点破的话模型会以为是权限或路径问题，反复重试同一个文件。
            let hint = if error.kind() == std::io::ErrorKind::InvalidData {
                "（文件可能不是 UTF-8 文本，例如二进制文件；本工具只能读文本文件）"
            } else {
                ""
            };
            return ToolOutcome::failed(format!("读取 `{raw_path}` 失败: {error}{hint}"));
        }
    };
    render_with_line_cap(&content, max_lines)
}

// ── read_files ───────────────────────────────────────────────────────

/// 一次读取多个文本文件。
///
/// P9-4 的吞吐手段：Harness 一轮只执行一条工具调用（见
/// `application::agent_run` 的 `step()`），所以"读 5 个文件"如果拆成
/// 5 次 `read_file`，就是 5 轮模型往返。已知路径时一次拿回全部内容，
/// 轮数直接除以文件数——这是单项收益最大的提速。
pub struct ReadFiles {
    root: WorkspaceRoot,
}

impl ReadFiles {
    /// 以 `root` 为工作区构造。
    pub fn new(root: WorkspaceRoot) -> Self {
        Self { root }
    }

    /// 工具规格。
    pub fn spec() -> ToolSpec {
        ToolSpec {
            name: READ_FILES.to_owned(),
            description: format!(
                "一次读取 workspace 内多个文本文件，按 paths 的顺序分节返回，每节带路径标题。\
                 适合已经知道要读哪几个文件的场景：一次调用拿回全部内容，省掉逐个 read_file 的往返。\
                 单个文件失败不影响其他文件，失败的那一节内说明原因；\
                 每个文件的行数上限与 read_file 相同（默认 {READ_FILE_DEFAULT_LINES}，上限 {READ_FILE_HARD_MAX_LINES}），\
                 超长文件同样只返回开头并附总行数。\
                 还不确定有哪些文件时，先用 list_dir 看目录结构、用 search_text 定位内容，不要用它碰运气。\
                 要改代码时用 apply_patch，不要用 write_file 整体覆盖。"
            ),
            parameters: json!({
                "type": "object",
                "properties": {
                    "paths": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "workspace 内的相对路径列表，例如 [\"src/main.rs\", \"Cargo.toml\"]；结果按这个顺序分节返回"
                    },
                    "max_lines": {
                        "type": "integer",
                        "description": format!("每个文件最多读取的行数，默认 {READ_FILE_DEFAULT_LINES}，上限 {READ_FILE_HARD_MAX_LINES}")
                    }
                },
                "required": ["paths"]
            }),
            // 与 read_file 同权：只读、无破坏、幂等、不碰外部世界。
            // 标成只读还有一层实际意义——默认策略下它不问人就执行，
            // 批量读几个文件不该让人逐个点头。
            annotations: ToolAnnotations::new(true, false, true, false),
        }
    }
}

/// `read_files` 的参数。
#[derive(Debug, Deserialize)]
struct ReadFilesArgs {
    paths: Vec<String>,
    max_lines: Option<usize>,
}

impl ToolInvoker for ReadFiles {
    fn invoke<'a>(&'a self, call: &'a ToolCall) -> BoxFuture<'a, ToolOutcome> {
        Box::pin(async move {
            let args: ReadFilesArgs = match parse_args(&call.arguments) {
                Ok(args) => args,
                Err(message) => return ToolOutcome::failed(message),
            };
            if args.paths.is_empty() {
                return ToolOutcome::failed(
                    "read_files 需要至少一个路径（paths 不能是空数组）；只读一个文件请直接用 read_file",
                );
            }
            let mut sections = Vec::with_capacity(args.paths.len());
            let mut read_any = false;
            // 逐个读而不是并发读：路径校验要碰文件系统，而 Harness 对工具
            // 执行的假设是"一次一个"（审计事件、审批、沙箱都按这个来）。
            // 这里的收益本就不在读取本身，而在省下的模型往返。
            for raw in &args.paths {
                let outcome = read_one_file(&self.root, raw, args.max_lines).await;
                read_any |= outcome.status == OutcomeStatus::Success;
                sections.push(format!("{}\n{}", section_header(raw), outcome.content));
            }
            let body = sections.join("\n\n");
            // 整批的成败看"有没有读到任何一个"：全部失败时报 failed，
            // 否则模型会以为内容已经拿到手，而实际上手里只有几条错误说明。
            if read_any {
                ToolOutcome::success(body)
            } else {
                ToolOutcome::failed(body)
            }
        })
    }
}

/// 批量读取的分节标题。
///
/// 用模型当初请求的那个路径原文，不做规范化：模型是靠"我传了什么"把
/// 内容和文件对上的，换成别的写法它反而认不出这一节是谁。
fn section_header(path: &str) -> String {
    format!("── {path} ──")
}

/// 按行数上限渲染读取结果，超限截断并附说明。
///
/// 说明必须点名 search_text 与 max_lines：模型看到截断后最常犯的错是
/// 原样重读同一文件，要告诉它出路在哪。
///
/// 行切分用 `split_inclusive('\n')` 而不是 `lines()`：后者会把 CRLF 的
/// `\r` 剥掉，截断路径下 Windows 文件的行尾就被静默改写了——模型据此
/// 构造的 edit_file old_string 会匹配不上原始文件。
fn render_with_line_cap(content: &str, max_lines: Option<usize>) -> ToolOutcome {
    let max_lines = max_lines
        .unwrap_or(READ_FILE_DEFAULT_LINES)
        .min(READ_FILE_HARD_MAX_LINES);
    let lines: Vec<&str> = content.split_inclusive('\n').collect();
    let total_lines = lines.len();
    if total_lines <= max_lines {
        return ToolOutcome::success(content);
    }
    let shown: String = lines[..max_lines].concat();
    ToolOutcome::success(format!(
        "{shown}\n\n……（共 {total_lines} 行，仅显示前 {max_lines} 行；\
         请用 search_text 定位目标内容，或调大 max_lines（上限 {READ_FILE_HARD_MAX_LINES}）重新读取本文件。）"
    ))
}

// ── list_dir ─────────────────────────────────────────────────────────

/// 列出一个目录的内容。
pub struct ListDir {
    root: WorkspaceRoot,
}

impl ListDir {
    /// 以 `root` 为工作区构造。
    pub fn new(root: WorkspaceRoot) -> Self {
        Self { root }
    }

    /// 工具规格。
    pub fn spec() -> ToolSpec {
        ToolSpec {
            name: LIST_DIR.to_owned(),
            description:
                "列出 workspace 内指定目录的内容（子目录以 / 结尾）。path 省略时列出根目录。\
                 用它摸清目录结构；选中要读的文件后，用 read_files 一次全部读出。"
                    .to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "workspace 内的相对路径，可省略" }
                }
            }),
            annotations: ToolAnnotations::new(true, false, true, false),
        }
    }
}

/// `list_dir` 的参数，`path` 省略时列出 workspace 根目录。
#[derive(Debug, Deserialize)]
struct ListDirArgs {
    path: Option<String>,
}

impl ToolInvoker for ListDir {
    fn invoke<'a>(&'a self, call: &'a ToolCall) -> BoxFuture<'a, ToolOutcome> {
        Box::pin(async move {
            let args: ListDirArgs = match parse_args(&call.arguments) {
                Ok(args) => args,
                Err(message) => return ToolOutcome::failed(message),
            };
            // 两种来源统一成 PathBuf：校验过的相对路径，或 workspace 根本身。
            let path: std::path::PathBuf = match args.path {
                Some(raw) => match resolve_in(&self.root, &raw) {
                    Ok(path) => path.as_path().to_path_buf(),
                    Err(outcome) => return outcome,
                },
                None => self.root.as_path().to_path_buf(),
            };

            let mut entries = Vec::new();
            let mut reader = match tokio::fs::read_dir(path.as_path()).await {
                Ok(reader) => reader,
                Err(error) => {
                    return ToolOutcome::failed(format!(
                        "读取目录 `{}` 失败: {error}",
                        path.display()
                    ));
                }
            };
            while let Some(entry) = match reader.next_entry().await {
                Ok(entry) => entry,
                Err(error) => {
                    return ToolOutcome::failed(format!(
                        "遍历目录 `{}` 失败: {error}",
                        path.display()
                    ));
                }
            } {
                let name = entry.file_name().to_string_lossy().into_owned();
                let is_dir = entry
                    .file_type()
                    .await
                    .map(|file_type| file_type.is_dir())
                    .unwrap_or(false);
                entries.push(if is_dir { format!("{name}/") } else { name });
            }
            if entries.is_empty() {
                return ToolOutcome::success("（空目录）");
            }
            entries.sort();
            ToolOutcome::success(entries.join("\n"))
        })
    }
}

// ── write_file ───────────────────────────────────────────────────────

/// 把文本整体写入一个文件。
pub struct WriteFile {
    root: WorkspaceRoot,
}

impl WriteFile {
    /// 以 `root` 为工作区构造。
    pub fn new(root: WorkspaceRoot) -> Self {
        Self { root }
    }

    /// 工具规格。
    pub fn spec() -> ToolSpec {
        ToolSpec {
            name: WRITE_FILE.to_owned(),
            description: "把文本内容写入 workspace 内的指定文件（覆盖写入，父目录必须已存在）。\
                          适合创建新文件（或用 apply_patch 的 Add File）；修改已有文件中的一段请改用 edit_file，\
                          一次改多处用 apply_patch。"
                .to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "workspace 内的相对路径" },
                    "content": { "type": "string", "description": "要写入的完整文件内容" }
                },
                "required": ["path", "content"]
            }),
            // 覆盖写入不可逆，且不幂等；只作用于已知的本地文件，所以不是开放世界。
            annotations: ToolAnnotations::new(false, true, false, false),
        }
    }
}

/// `write_file` 的参数。
#[derive(Debug, Deserialize)]
struct WriteFileArgs {
    path: String,
    content: String,
}

impl ToolInvoker for WriteFile {
    fn invoke<'a>(&'a self, call: &'a ToolCall) -> BoxFuture<'a, ToolOutcome> {
        Box::pin(async move {
            let args: WriteFileArgs = match parse_args(&call.arguments) {
                Ok(args) => args,
                Err(message) => return ToolOutcome::failed(message),
            };
            let path = match resolve_in(&self.root, &args.path) {
                Ok(path) => path,
                Err(outcome) => return outcome,
            };
            match tokio::fs::write(path.as_path(), args.content.as_bytes()).await {
                Ok(()) => ToolOutcome::success(format!(
                    "已写入 {} 字节到 `{}`",
                    args.content.len(),
                    args.path
                )),
                Err(error) => ToolOutcome::failed(format!("写入 `{}` 失败: {error}", args.path)),
            }
        })
    }
}

/// 工具名常量，供组装与测试引用。
pub const READ_FILE_NAME: &str = READ_FILE;
pub const READ_FILES_NAME: &str = READ_FILES;
pub const LIST_DIR_NAME: &str = LIST_DIR;
pub const WRITE_FILE_NAME: &str = WRITE_FILE;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::ids::CallId;
    use crate::domain::tool::OutcomeStatus;
    use crate::domain::workspace::WorkspaceRoot;
    use std::path::PathBuf;

    fn workspace() -> WorkspaceRoot {
        WorkspaceRoot::new("D:/tmp/demo-workspace")
    }

    fn call(name: &str, arguments: serde_json::Value) -> ToolCall {
        ToolCall {
            id: CallId::new(format!("call_{name}")),
            name: name.to_owned(),
            arguments,
        }
    }

    #[tokio::test]
    async fn read_file_rejects_escaping_path() {
        let tools = ReadFile::new(workspace());
        let outcome = tools
            .invoke(&call(READ_FILE, json!({"path": "../secrets.txt"})))
            .await;

        assert_eq!(outcome.status, OutcomeStatus::Refused);
        assert!(outcome.content.contains("只允许 workspace 内的相对路径"));
    }

    #[tokio::test]
    async fn unknown_tool_name_is_refused_by_the_registry() {
        // P0 时这个兜底在 FileToolSet 的 dispatch 里；P2 起路由收归注册表，
        // 行为不变：模型指名一个没登记的工具 → refused，不 panic。
        use std::sync::Arc;
        use crate::application::tool_registry::{ToolEntry, ToolRegistry};
        let registry = ToolRegistry::new(vec![ToolEntry {
            spec: WriteFile::spec(),
            invoker: Arc::new(WriteFile::new(workspace())),
        }]);
        let outcome = registry
            .invoke(&call(READ_FILE, json!({"path": "a.txt"})))
            .await;

        assert_eq!(outcome.status, OutcomeStatus::Refused);
        assert!(outcome.content.contains("未注册"), "{}", outcome.content);
    }

    #[tokio::test]
    async fn missing_argument_reports_failure() {
        let tools = ReadFile::new(workspace());
        let outcome = tools.invoke(&call(READ_FILE, json!({}))).await;

        assert_eq!(outcome.status, OutcomeStatus::Failed);
        assert!(outcome.content.starts_with("参数解析失败"));
    }

    // ── read_file 行数截断（P3）────────────────────────────────────────
    //
    // 这些测试用真实临时文件：截断逻辑操作的是读进来的字符串，
    // 只有走完整的 invoke 路径才能验证"读 → 截 → 附说明"串起来没错。

    /// 在共享临时目录里造一个文件，返回目录路径。
    fn temp_dir_with(name: &str, content: &[u8]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("qch-file-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("建临时目录");
        std::fs::write(dir.join(name), content).expect("写临时文件");
        dir
    }

    fn lines(count: usize) -> String {
        (1..=count)
            .map(|index| format!("第{index}行\n"))
            .collect()
    }

    #[tokio::test]
    async fn read_file_returns_the_full_file_within_the_cap() {
        let dir = temp_dir_with("short.txt", lines(3).as_bytes());
        let tools = ReadFile::new(WorkspaceRoot::new(&dir));
        let outcome = tools
            .invoke(&call(READ_FILE, json!({"path": "short.txt"})))
            .await;

        assert_eq!(outcome.status, OutcomeStatus::Success);
        assert_eq!(outcome.content, lines(3));
        assert!(!outcome.content.contains("仅显示"), "未超限不应有截断说明");
    }

    #[tokio::test]
    async fn read_file_truncates_and_reports_the_total_line_count() {
        let dir = temp_dir_with("long.txt", lines(10).as_bytes());
        let tools = ReadFile::new(WorkspaceRoot::new(&dir));
        let outcome = tools
            .invoke(&call(READ_FILE, json!({"path": "long.txt", "max_lines": 3})))
            .await;

        assert_eq!(outcome.status, OutcomeStatus::Success);
        assert!(outcome.content.starts_with("第1行\n第2行\n第3行"), "{}", outcome.content);
        assert!(outcome.content.contains("共 10 行"), "{}", outcome.content);
        assert!(outcome.content.contains("仅显示前 3 行"), "{}", outcome.content);
        // 截断说明要给模型指路：定位工具与调参出口都得提到。
        assert!(outcome.content.contains("search_text"), "{}", outcome.content);
        assert!(outcome.content.contains("max_lines"), "{}", outcome.content);
    }

    #[tokio::test]
    async fn read_file_truncation_preserves_crlf_line_endings() {
        // CRLF 文件在截断路径下必须逐字节保留 \r\n：模型拿截断输出构造
        // edit_file 的 old_string 时才不会和磁盘上的原始内容对不上。
        let dir = temp_dir_with("crlf.txt", b"line1\r\nline2\r\nline3\r\n");
        let tools = ReadFile::new(WorkspaceRoot::new(&dir));
        let outcome = tools
            .invoke(&call(READ_FILE, json!({"path": "crlf.txt", "max_lines": 2})))
            .await;

        assert_eq!(outcome.status, OutcomeStatus::Success);
        assert!(
            outcome.content.starts_with("line1\r\nline2\r\n"),
            "CRLF 必须原样保留: {:?}",
            outcome.content
        );
        assert!(outcome.content.contains("共 3 行"), "{}", outcome.content);
    }

    #[tokio::test]
    async fn read_file_defaults_to_400_lines() {
        let dir = temp_dir_with("default.txt", lines(401).as_bytes());
        let tools = ReadFile::new(WorkspaceRoot::new(&dir));
        let outcome = tools
            .invoke(&call(READ_FILE, json!({"path": "default.txt"})))
            .await;

        assert!(outcome.content.contains("仅显示前 400 行"), "{}", outcome.content);
        assert!(!outcome.content.contains("第401行"), "默认上限之后的内容不应出现");
    }

    #[tokio::test]
    async fn read_file_clamps_max_lines_to_the_hard_cap() {
        let dir = temp_dir_with("huge.txt", lines(2500).as_bytes());
        let tools = ReadFile::new(WorkspaceRoot::new(&dir));
        let outcome = tools
            .invoke(&call(READ_FILE, json!({"path": "huge.txt", "max_lines": 99_999})))
            .await;

        assert!(outcome.content.contains("仅显示前 2000 行"), "{}", outcome.content);
        assert!(!outcome.content.contains("第2500行"), "硬上限之后的内容不应出现");
    }

    #[tokio::test]
    async fn read_file_reports_non_utf8_content_as_failure() {
        let dir = temp_dir_with("blob.bin", &[0xFF, 0xFE, 0x00, 0x01]);
        let tools = ReadFile::new(WorkspaceRoot::new(&dir));
        let outcome = tools
            .invoke(&call(READ_FILE, json!({"path": "blob.bin"})))
            .await;

        assert_eq!(outcome.status, OutcomeStatus::Failed);
        assert!(
            outcome.content.contains("UTF-8"),
            "失败说明应指出非文本文件: {}",
            outcome.content
        );
    }

    #[test]
    fn only_write_file_is_destructive() {
        let specs = vec![ReadFile::spec(), ListDir::spec(), WriteFile::spec()];
        let destructive: Vec<&str> = specs
            .iter()
            .filter(|spec| spec.annotations.destructive)
            .map(|spec| spec.name.as_str())
            .collect();

        assert_eq!(destructive, vec![WRITE_FILE]);
    }

    #[test]
    fn write_file_description_mentions_edit_file() {
        // 分工说明是给模型看的：不提 edit_file，模型就会用整体覆盖做小修改。
        let description = WriteFile::spec().description;
        assert!(description.contains("edit_file"));
    }

    /// 路径解析辅助函数的直接验证：越界返回拒绝观察。
    #[test]
    fn resolve_in_maps_escape_to_refusal() {
        let root = workspace();
        let Err(refused) = resolve_in(&root, "../outside") else {
            panic!("越界应返回拒绝观察");
        };
        assert_eq!(refused.status, OutcomeStatus::Refused);
    }

    // ── read_files 批量读取（P9-4）─────────────────────────────────────

    /// 造一个带若干普通文件、一份 `.env` 和一个二进制文件的 workspace。
    ///
    /// 每类失败各来一个（不存在、非 UTF-8、受保护），用来证明它们互不影响。
    fn batch_workspace(name: &str) -> PathBuf {
        let dir = protected_dir(name);
        std::fs::write(dir.join("a.txt"), "AAA\nBBB\n").expect("写 a.txt");
        std::fs::write(dir.join("b.txt"), "BBB only\n").expect("写 b.txt");
        std::fs::write(dir.join(".env"), "TOKEN=supersecret\n").expect("写 .env");
        std::fs::write(dir.join("blob.bin"), [0xFF, 0xFE, 0x00]).expect("写二进制");
        dir
    }

    /// 批量读取按 `paths` 的顺序分节返回，每节带路径标题。
    #[tokio::test]
    async fn read_files_returns_every_file_labelled_with_its_path() {
        let dir = batch_workspace("batch-ok");
        let tools = ReadFiles::new(WorkspaceRoot::new(&dir));
        let outcome = tools
            .invoke(&call(READ_FILES, json!({"paths": ["a.txt", "b.txt"]})))
            .await;

        assert_eq!(outcome.status, OutcomeStatus::Success, "{}", outcome.content);
        let a_at = outcome.content.find("a.txt").expect("应有 a.txt 的分节标题");
        let b_at = outcome.content.find("b.txt").expect("应有 b.txt 的分节标题");
        assert!(
            a_at < b_at,
            "分节顺序应跟随 paths 的顺序: {}",
            outcome.content
        );
        assert!(outcome.content.contains("AAA\nBBB"), "{}", outcome.content);
        assert!(outcome.content.contains("BBB only"), "{}", outcome.content);
    }

    /// 单个文件失败不影响其他文件——这是 `read_files` 存在的全部理由。
    ///
    /// 不存在、非 UTF-8、受保护三种失败同时出现，读得到的两个文件
    /// 必须照常回来，每种失败也都要各自说明原因。
    #[tokio::test]
    async fn read_files_one_file_failing_does_not_affect_the_others() {
        let dir = batch_workspace("batch-partial");
        let tools = ReadFiles::new(WorkspaceRoot::new(&dir));
        let outcome = tools
            .invoke(&call(
                READ_FILES,
                json!({"paths": ["missing.txt", "blob.bin", "a.txt", ".env", "b.txt"]}),
            ))
            .await;

        // 有读成的，整批就是成功：模型要能拿到读到的那些，而不是整批作废。
        assert_eq!(outcome.status, OutcomeStatus::Success, "{}", outcome.content);
        assert!(
            outcome.content.contains("AAA\nBBB"),
            "读得到的文件必须回来: {}",
            outcome.content
        );
        assert!(
            outcome.content.contains("BBB only"),
            "读得到的文件必须回来: {}",
            outcome.content
        );
        // 失败逐个点名，不合并成一句含糊的"部分失败"。
        assert!(outcome.content.contains("missing.txt"), "{}", outcome.content);
        assert!(outcome.content.contains("UTF-8"), "{}", outcome.content);
        assert!(outcome.content.contains("受保护"), "{}", outcome.content);
        // 失败原因不得夹带任何读到的内容。
        assert!(
            !outcome.content.contains("supersecret"),
            "凭据不得进观察: {}",
            outcome.content
        );
    }

    /// 一个都没读成时不能报 success：那会让模型以为内容已经拿到手。
    #[tokio::test]
    async fn read_files_reports_failure_when_nothing_was_read() {
        let dir = batch_workspace("batch-allfail");
        let tools = ReadFiles::new(WorkspaceRoot::new(&dir));
        let outcome = tools
            .invoke(&call(READ_FILES, json!({"paths": ["nope.txt", ".env"]})))
            .await;

        assert_eq!(outcome.status, OutcomeStatus::Failed, "{}", outcome.content);
        assert!(outcome.content.contains("受保护"), "{}", outcome.content);
    }

    /// 空 `paths` 是参数错误，不是"读零个文件并且成功了"。
    #[tokio::test]
    async fn read_files_rejects_an_empty_path_list() {
        let dir = batch_workspace("batch-empty");
        let tools = ReadFiles::new(WorkspaceRoot::new(&dir));
        let outcome = tools.invoke(&call(READ_FILES, json!({"paths": []}))).await;

        assert_eq!(outcome.status, OutcomeStatus::Failed);
        assert!(
            outcome.content.contains("read_file"),
            "应指路单文件工具: {}",
            outcome.content
        );
    }

    /// 截断口径与 `read_file` 逐字一致，且每个文件各自判定。
    ///
    /// 同一个批量调用里，超限的那个附说明，没超限的那个原样返回——
    /// 混为一谈会让模型以为短文件也被截了。
    #[tokio::test]
    async fn read_files_applies_the_same_line_cap_as_read_file() {
        let dir = batch_workspace("batch-cap");
        std::fs::write(dir.join("long.txt"), lines(10).as_bytes()).expect("写长文件");
        let tools = ReadFiles::new(WorkspaceRoot::new(&dir));
        let outcome = tools
            .invoke(&call(
                READ_FILES,
                json!({"paths": ["long.txt", "a.txt"], "max_lines": 3}),
            ))
            .await;

        assert_eq!(outcome.status, OutcomeStatus::Success, "{}", outcome.content);
        assert!(outcome.content.contains("共 10 行"), "{}", outcome.content);
        assert!(outcome.content.contains("仅显示前 3 行"), "{}", outcome.content);
        assert!(
            !outcome.content.contains("第4行"),
            "超限部分不应出现: {}",
            outcome.content
        );
        assert!(
            outcome.content.contains("AAA\nBBB"),
            "未超限的文件要原样回来: {}",
            outcome.content
        );
        assert!(
            !outcome.content.contains("共 2 行"),
            "未超限的文件不该有截断说明: {}",
            outcome.content
        );
    }

    /// 受保护路径在批量读取里同样逐个拒绝，且内容不进观察。
    ///
    /// 每条路径都独立走 `resolve_in`，所以 junction 变形（P8 用户验收
    /// 打穿的那两种）在这里和单文件读取一样被拦下。
    #[tokio::test]
    async fn read_files_refuses_protected_paths_per_file() {
        let dir = batch_workspace("batch-protected");
        std::fs::create_dir_all(dir.join(".git")).expect("建 .git");
        std::fs::write(dir.join(".git/config"), "[core]\n").expect("写 .git/config");
        let tools = ReadFiles::new(WorkspaceRoot::new(&dir));
        let outcome = tools
            .invoke(&call(
                READ_FILES,
                json!({"paths": [".git/config", ".env", "a.txt"]}),
            ))
            .await;

        assert_eq!(
            outcome.status,
            OutcomeStatus::Success,
            "a.txt 读成了，整批算成功: {}",
            outcome.content
        );
        assert!(outcome.content.contains("受保护"), "{}", outcome.content);
        assert!(
            !outcome.content.contains("supersecret"),
            "凭据不得进观察: {}",
            outcome.content
        );
        assert!(
            !outcome.content.contains("[core]"),
            "版本库配置不得进观察: {}",
            outcome.content
        );
    }

    /// 批量工具是只读的，且描述里点明它和 `read_file` 的分工。
    #[test]
    fn read_files_is_read_only_and_describes_when_to_use_it() {
        let spec = ReadFiles::spec();

        assert!(spec.annotations.read_only, "批量读取不得被标成有副作用");
        assert!(!spec.annotations.destructive);
        assert!(spec.annotations.idempotent);
        assert!(
            spec.description.contains("read_file"),
            "描述要说清与单文件工具的分工: {}",
            spec.description
        );
        assert!(
            spec.description.contains("paths"),
            "描述要点名参数，模型才知道怎么传: {}",
            spec.description
        );
    }

    /// `read_file` 的描述要指路 `read_files`。
    ///
    /// 不指路的后果是模型继续一个个读——那正是这个工具要省掉的往返，
    /// 而省往返是 P9-4 的全部目的。
    #[test]
    fn read_file_description_points_at_the_batch_tool() {
        assert!(
            ReadFile::spec().description.contains("read_files"),
            "{}",
            ReadFile::spec().description
        );
    }

    /// `list_dir` 的描述要把"看完结构之后怎么办"说完。
    ///
    /// 停在"列出目录"的话，模型下一步仍是一个个 `read_file`；
    /// 把 `read_files` 接在后面，一次摸结构的代价就换来一次批量读。
    #[test]
    fn list_dir_description_leads_into_the_batch_reader() {
        assert!(
            ListDir::spec().description.contains("read_files"),
            "{}",
            ListDir::spec().description
        );
    }

    /// 受保护路径测试专用的临时目录。
    ///
    /// 不复用 `temp_dir_with` 的共享目录：那些测试并行跑，末尾会
    /// `remove_dir_all` 整个目录，把这边刚建好的 `.git` 一起删掉。
    fn protected_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("qch-protected-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建临时目录");
        dir
    }

    // ── P8-1 受保护路径（工具层）──────────────────────────────────────

    /// `write_file(".git/config")` 必须在策略之前就被拒绝。
    ///
    /// 这条测试刻意不碰策略引擎：保护是 workspace 路径解析的固有行为，
    /// 任何"放行写入"的配置都不该让它失效。工具层的拒绝观察就是契约——
    /// 模型看到的是 refused，不是"审批一下就能写"。
    #[tokio::test]
    async fn write_file_refuses_protected_paths_without_consulting_policy() {
        let dir = protected_dir("write");
        // 造一个真的 .git/config：拒绝必须发生在文件系统之前，
        // 不能让"文件不存在"成为一种假通过。
        std::fs::create_dir_all(dir.join(".git")).expect("建 .git");
        std::fs::write(dir.join(".git/config"), "[core]\n").expect("写 .git/config");
        let tools = WriteFile::new(WorkspaceRoot::new(&dir));

        for path in [".git/config", "./.git/config", ".env", ".qcodeharness/sessions/x.jsonl"] {
            let outcome = tools
                .invoke(&call(WRITE_FILE, json!({"path": path, "content": "恶意覆盖"})))
                .await;
            assert_eq!(outcome.status, OutcomeStatus::Refused, "`{path}`: {}", outcome.content);
            assert!(outcome.content.contains("受保护"), "`{path}`: {}", outcome.content);
        }

        assert_eq!(
            std::fs::read_to_string(dir.join(".git/config")).unwrap(),
            "[core]\n",
            "受保护文件必须保持原样"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 读与写同权：agent 也不能通过 `read_file` 摸 `.git` 和 `.env`。
    #[tokio::test]
    async fn read_file_refuses_protected_paths() {
        let dir = protected_dir("read");
        std::fs::create_dir_all(dir.join(".git")).expect("建 .git");
        std::fs::write(dir.join(".git/config"), "[core]\n").expect("写 .git/config");
        std::fs::write(dir.join(".env"), "TOKEN=secret\n").expect("写 .env");
        let tools = ReadFile::new(WorkspaceRoot::new(&dir));

        for path in [".git/config", ".env"] {
            let outcome = tools.invoke(&call(READ_FILE, json!({"path": path}))).await;
            assert_eq!(outcome.status, OutcomeStatus::Refused, "`{path}`: {}", outcome.content);
            assert!(!outcome.content.contains("secret"), "凭据不得出现在观察里: {}", outcome.content);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `list_dir(".qcodeharness")` 同样拒绝：目录列表会泄露会话文件名，
    /// 那是审计证据的索引，不该进模型的上下文。
    #[tokio::test]
    async fn list_dir_refuses_protected_directories() {
        let dir = protected_dir("list");
        std::fs::create_dir_all(dir.join(".qcodeharness/sessions")).expect("建目录");
        let tools = ListDir::new(WorkspaceRoot::new(&dir));

        let outcome = tools
            .invoke(&call(LIST_DIR, json!({"path": ".qcodeharness"})))
            .await;

        assert_eq!(outcome.status, OutcomeStatus::Refused, "{}", outcome.content);
        assert!(outcome.content.contains("受保护"), "{}", outcome.content);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// WorkspacePath 校验通过时 resolve_in 返回路径本身。
    #[test]
    fn resolve_in_returns_validated_path() {
        let root = workspace();
        let path = resolve_in(&root, "src/main.rs").expect("合法路径应通过");
        assert!(path.as_path().starts_with(root.as_path()));
    }
}

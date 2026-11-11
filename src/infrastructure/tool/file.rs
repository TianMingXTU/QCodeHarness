//! 文件工具：读取、列目录、整体写入。
//!
//! 每个工具独立成结构体，配置都只有 workspace 根。共享的路径校验与
//! 参数解析在 [`super`] 中，此处不重复。
//!
//! `write_file` 与 `edit_file`（见 `super::edit`）的分工写在工具描述里：
//! 前者创建新文件或整体覆盖，后者只替换已有文件中的一段，让模型明确
//! 该用哪个。

use serde::Deserialize;
use serde_json::json;

use crate::application::port::{BoxFuture, ToolInvoker};
use crate::domain::tool::{OutcomeStatus, ToolAnnotations, ToolCall, ToolOutcome, ToolSpec};
use crate::domain::workspace::WorkspaceRoot;

use super::line_ending::LineEnding;
use super::{parse_args, resolve_in};

const READ_FILE: &str = "read_file";
const READ_FILES: &str = "read_files";
const LIST_DIR: &str = "list_dir";
const WRITE_FILE: &str = "write_file";

/// `read_file` 默认显示的行数。与 search_text/run_command 一样，截断时附说明。
/// read_file 是只读工具，默认策略下无需审批就执行，读取超长文件会占用
/// 大量上下文。
const READ_FILE_DEFAULT_LINES: usize = 400;
/// `max_lines` 的硬上限。更大的文件应先用 search_text 定位再分块读取。
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

/// 读取一个文件并按行数上限渲染。
///
/// `read_file` 与 `read_files` 共用该路径，同一文件必须得到逐字节相同的
/// 结果（包括截断说明的措辞）。否则模型对文件长度的判断会随单读或批量读
/// 而变化，而模型没有依据做这种区分。
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
            // InvalidData 表示内容不是合法 UTF-8，常见于二进制文件。
            // 不说明原因时，模型可能误以为权限或路径问题而反复重试。
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
/// 模型也可以在一条回复里发多条 `read_file`（一轮内顺序执行，见
/// `application::agent_run` 的 `record_and_advance`），但那需要模型逐个
/// 发起；已知路径时一次调用取回全部内容，意图更直接，工具定义也更省。
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
            // 与 read_file 权限相同：只读、无破坏、幂等、不访问外部世界。
            // 标为只读还有一个作用：默认策略下无需审批即可执行，批量读
            // 文件不必逐个确认。
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
            // 逐个读取，不并发。路径校验需要访问文件系统，Harness 也假设
            // 工具一次执行一个（审计、审批、沙箱都基于此）。收益来自减少
            // 模型往返，不在读取本身。
            for raw in &args.paths {
                let outcome = read_one_file(&self.root, raw, args.max_lines).await;
                read_any |= outcome.status == OutcomeStatus::Success;
                sections.push(format!("{}\n{}", section_header(raw), outcome.content));
            }
            let body = sections.join("\n\n");
            // 整批成败取决于是否读到至少一个文件。全部失败时报 failed，
            // 否则模型会以为已拿到内容，实际只有错误说明。
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
/// 使用模型请求时的路径原文，不做规范化。模型依靠自己传入的路径将内容
/// 与文件对应，改写后反而难以识别。
fn section_header(path: &str) -> String {
    format!("── {path} ──")
}

/// 按行数上限渲染读取结果，超限时截断并附说明。
///
/// 说明中必须提到 search_text 与 max_lines。模型看到截断后常会原样重读
/// 同一文件，需要告知可行的处理方式。
///
/// 行切分使用 `split_inclusive('\n')` 而非 `lines()`。后者会去掉 CRLF
/// 中的 `\r`，截断路径下 Windows 文件的行尾被改写，模型据此构造的
/// edit_file old_string 将无法匹配原始文件。
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
            // 两种情况统一为 PathBuf：校验后的相对路径，或 workspace 根目录本身。
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
            // 覆盖写入不可逆且不幂等。只作用于已知本地文件，不属于开放世界。
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
            // 覆盖已有文件时沿用它的行尾风格。Windows 上 git 检出的文本是
            // CRLF，模型写出的内容是 LF，直接落盘会让整文件在 git 里变成
            // 一行行改动的假 diff。新建文件没有原风格可循，用模型给的内容。
            let ending = LineEnding::detect_in_file(path.as_path()).await;
            let content = if ending == LineEnding::CrLf {
                LineEnding::restore(&LineEnding::to_lf(&args.content), ending)
            } else {
                args.content.clone()
            };
            match tokio::fs::write(path.as_path(), content.as_bytes()).await {
                Ok(()) => ToolOutcome::success(format!(
                    "已写入 {} 字节到 `{}`",
                    content.len(),
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
        // 路由由注册表负责，兜底拒绝也由注册表给出：模型指定未注册的
        // 工具时返回 refused，不 panic。
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

    // ── read_file 行数截断 ─────────────────────────────────────────
    //
    // 测试使用真实临时文件。截断逻辑处理读入的字符串，只有走完整的
    // invoke 路径才能验证读取、截断、附说明连起来是否正确。

    /// 在共享临时目录中创建一个文件，返回目录路径。
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
        // 截断说明需提到定位工具与调参入口。
        assert!(outcome.content.contains("search_text"), "{}", outcome.content);
        assert!(outcome.content.contains("max_lines"), "{}", outcome.content);
    }

    #[tokio::test]
    async fn read_file_truncation_preserves_crlf_line_endings() {
        // 截断路径必须逐字节保留 CRLF 的 \r\n，否则模型用截断输出构造的
        // edit_file old_string 会与磁盘内容不一致。
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
        let specs = [ReadFile::spec(), ListDir::spec(), WriteFile::spec()];
        let destructive: Vec<&str> = specs
            .iter()
            .filter(|spec| spec.annotations.destructive)
            .map(|spec| spec.name.as_str())
            .collect();

        assert_eq!(destructive, vec![WRITE_FILE]);
    }

    #[test]
    fn write_file_description_mentions_edit_file() {
        // 描述面向模型：不提到 edit_file 时，模型会用整体覆盖做小修改。
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

    // ── read_files 批量读取 ──────────────────────────────────────────

    /// 创建包含普通文件、`.env` 与二进制文件的 workspace。
    ///
    /// 每类失败各准备一个（不存在、非 UTF-8、受保护），验证它们互不影响。
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

    /// 单个文件失败不影响其他文件。
    ///
    /// 不存在、非 UTF-8、受保护三种失败同时出现时，能读到的两个文件必须
    /// 正常返回，每种失败分别说明原因。
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

        // 有文件读成功，整批即为成功，模型需要拿到已读到的内容。
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
        // 每个失败单独列出，不合并为"部分失败"。
        assert!(outcome.content.contains("missing.txt"), "{}", outcome.content);
        assert!(outcome.content.contains("UTF-8"), "{}", outcome.content);
        assert!(outcome.content.contains("受保护"), "{}", outcome.content);
        // 失败原因不得包含读到的内容。
        assert!(
            !outcome.content.contains("supersecret"),
            "凭据不得进观察: {}",
            outcome.content
        );
    }

    /// 全部未读成时必须报失败，否则模型会以为已拿到内容。
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

    /// 空 `paths` 是参数错误，不能视为读零个文件成功。
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

    /// 截断口径与 `read_file` 逐字一致，且每个文件独立判定。
    ///
    /// 同一次批量调用中，超限文件附截断说明，未超限文件原样返回。
    /// 两者混淆会让模型以为短文件也被截断。
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

    /// 受保护路径在批量读取中同样逐个拒绝，内容不进入观察。
    ///
    /// 每条路径独立经过 `resolve_in`，因此 junction 变形（词法合法但
    /// 物理上落入受保护目录）与单文件读取一样会被拦截。
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

    /// `read_file` 的描述要指向 `read_files`。
    ///
    /// 不指向时模型会继续逐个读取，批量读取因此失去作用。
    #[test]
    fn read_file_description_points_at_the_batch_tool() {
        assert!(
            ReadFile::spec().description.contains("read_files"),
            "{}",
            ReadFile::spec().description
        );
    }

    /// `list_dir` 的描述要说明列出结构之后的下一步。
    ///
    /// 只写"列出目录"时，模型接下来仍会逐个 `read_file`。接上
    /// `read_files` 后，查看一次目录即可进行一次批量读取。
    #[test]
    fn list_dir_description_leads_into_the_batch_reader() {
        assert!(
            ListDir::spec().description.contains("read_files"),
            "{}",
            ListDir::spec().description
        );
    }

    /// 覆盖已有文件时沿用它的行尾风格。
    ///
    /// Windows 上 git 检出的文本是 CRLF，模型写出的内容是 LF。直接落盘
    /// 会让整文件在 git 里变成一行行改动的假 diff，实际改动却只有一处。
    #[tokio::test]
    async fn write_file_keeps_the_existing_line_endings() {
        let dir = protected_dir("write-crlf");
        std::fs::write(dir.join("crlf.txt"), "old\r\nlines\r\n").expect("写 CRLF 文件");
        let tools = WriteFile::new(WorkspaceRoot::new(&dir));

        let outcome = tools
            .invoke(&call(
                WRITE_FILE,
                json!({"path": "crlf.txt", "content": "new\nlines\n"}),
            ))
            .await;

        assert_eq!(outcome.status, OutcomeStatus::Success, "{}", outcome.content);
        assert_eq!(
            std::fs::read_to_string(dir.join("crlf.txt")).unwrap(),
            "new\r\nlines\r\n"
        );
        // 字节数要报实际落盘的大小：CRLF 比 LF 每行多一个字节。
        assert!(
            outcome.content.contains("已写入 12 字节"),
            "字节数应是落盘后的大小: {}",
            outcome.content
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 新建文件没有原风格可循，用模型给的内容。
    #[tokio::test]
    async fn write_file_creates_a_new_file_with_the_given_content() {
        let dir = protected_dir("write-new");
        let tools = WriteFile::new(WorkspaceRoot::new(&dir));

        let outcome = tools
            .invoke(&call(
                WRITE_FILE,
                json!({"path": "fresh.txt", "content": "a\nb\n"}),
            ))
            .await;

        assert_eq!(outcome.status, OutcomeStatus::Success, "{}", outcome.content);
        assert_eq!(
            std::fs::read_to_string(dir.join("fresh.txt")).unwrap(),
            "a\nb\n"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 受保护路径测试专用的临时目录。
    ///
    /// 独立命名，不与 `temp_dir_with` 的共享目录混用：这些测试各自先
    /// 清空再重建目录，共享目录里的既有测试文件会被误删。
    fn protected_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("qch-protected-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建临时目录");
        dir
    }

    // ── 受保护路径（工具层）──────────────────────────────────────

    /// `write_file(".git/config")` 必须在策略检查之前被拒绝。
    ///
    /// 本测试不涉及策略引擎。保护是 workspace 路径解析的固有行为，
    /// 任何放行写入的配置都不应使其失效。模型看到的观察是 refused，
    /// 不会进入审批。
    #[tokio::test]
    async fn write_file_refuses_protected_paths_without_consulting_policy() {
        let dir = protected_dir("write");
        // 创建真实的 .git/config，确保拒绝发生在文件系统访问之前。
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

    /// 读与写权限相同：`read_file` 也不能访问 `.git` 和 `.env`。
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
    /// 而会话文件是审计证据的索引，不应进入模型上下文。
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

    /// 路径校验通过时 `resolve_in` 返回该路径。
    #[test]
    fn resolve_in_returns_validated_path() {
        let root = workspace();
        let path = resolve_in(&root, "src/main.rs").expect("合法路径应通过");
        assert!(path.as_path().starts_with(root.as_path()));
    }
}

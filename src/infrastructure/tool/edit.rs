//! 查找替换式编辑工具。
//!
//! 核心是唯一性校验。`old_string` 匹配多处时，替换第一处会产生难以发现
//! 的错误。此时应返回失败并说明匹配数量，让模型补充更长的上下文后重试。
//!
//! 比较前把文件内容与模型给出的原文统一成 LF，写回时还原成文件原来的
//! 行尾风格。Windows 上 git 默认检出 CRLF，不统一口径的话模型给的
//! `old_string` 与磁盘内容永远对不上。

use serde::Deserialize;
use serde_json::json;

use crate::application::port::{BoxFuture, ToolInvoker};
use crate::domain::tool::{ToolAnnotations, ToolCall, ToolOutcome, ToolSpec};
use crate::domain::workspace::WorkspaceRoot;

use super::line_ending::LineEnding;
use super::{parse_args, resolve_in};

const EDIT_FILE: &str = "edit_file";

/// 对已有文件做一处精确替换。
pub struct EditFile {
    root: WorkspaceRoot,
}

impl EditFile {
    /// 以 `root` 为工作区构造。
    pub fn new(root: WorkspaceRoot) -> Self {
        Self { root }
    }

    /// 工具规格。
    pub fn spec() -> ToolSpec {
        ToolSpec {
            name: EDIT_FILE.to_owned(),
            description: "在 workspace 内的已有文件中做一次精确替换：`old_string` 必须在文件中恰好出现一次，\
                          否则拒绝执行。适合小段修改；一次改多处用 apply_patch，\
                          创建新文件或整体重写请用 write_file。"
                .to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "workspace 内的相对路径" },
                    "old_string": { "type": "string", "description": "要被替换掉的原文，必须在文件中唯一" },
                    "new_string": { "type": "string", "description": "替换成什么" }
                },
                "required": ["path", "old_string", "new_string"]
            }),
            // 修改文件内容不可逆，重复执行不是空操作（当 `new_string` 含
            // `old_string` 时重复替换会继续增长），因此不幂等。
            annotations: ToolAnnotations::new(false, true, false, false),
        }
    }
}

/// `edit_file` 的参数。
#[derive(Debug, Deserialize)]
struct EditArgs {
    path: String,
    old_string: String,
    new_string: String,
}

impl ToolInvoker for EditFile {
    fn invoke<'a>(&'a self, call: &'a ToolCall) -> BoxFuture<'a, ToolOutcome> {
        Box::pin(async move {
            let args: EditArgs = match parse_args(&call.arguments) {
                Ok(args) => args,
                Err(message) => return ToolOutcome::failed(message),
            };
            let path = match resolve_in(&self.root, &args.path) {
                Ok(path) => path,
                Err(outcome) => return outcome,
            };
            if args.old_string.is_empty() {
                return ToolOutcome::failed("old_string 不能为空");
            }
            if args.old_string == args.new_string {
                return ToolOutcome::failed("新旧内容相同，无需修改");
            }

            let content = match tokio::fs::read_to_string(path.as_path()).await {
                Ok(content) => content,
                Err(error) => {
                    return ToolOutcome::failed(format!("读取 `{}` 失败: {error}", args.path));
                }
            };

            // 行尾统一成 LF 再比较，写回时还原成原风格。模型给出的
            // `old_string` 用 LF，Windows 上 git 检出的文件是 CRLF，两边
            // 逐字比对会永远找不到。统一口径后，模型给的原文不必带 `\r`。
            let ending = LineEnding::detect(&content);
            let normalized = LineEnding::to_lf(&content);
            let old_string = LineEnding::to_lf(&args.old_string);
            let new_string = LineEnding::to_lf(&args.new_string);

            let occurrences = occurrence_count(&normalized, &old_string);
            match occurrences {
                0 => ToolOutcome::failed(format!(
                    "在 `{}` 中找不到要替换的文本（old_string 与文件内容不一致）",
                    args.path
                )),
                1 => {
                    let replaced = normalized.replacen(&old_string, &new_string, 1);
                    let updated = LineEnding::restore(&replaced, ending);
                    match tokio::fs::write(path.as_path(), updated.as_bytes()).await {
                        Ok(()) => ToolOutcome::success(format!(
                            "已替换 `{}` 中的 1 处文本（{} → {} 字节）",
                            args.path,
                            args.old_string.len(),
                            args.new_string.len()
                        )),
                        Err(error) => {
                            ToolOutcome::failed(format!("写回 `{}` 失败: {error}", args.path))
                        }
                    }
                }
                n => ToolOutcome::failed(format!(
                    "`{}` 中匹配到 {n} 处，请提供更长的上下文以唯一定位",
                    args.path
                )),
            }
        })
    }
}

/// `needle` 在 `haystack` 中出现的次数，包含重叠位置。
///
/// `str::matches` 只返回不重叠的匹配，`"aaa"` 中的 `"aa"` 会被数成 1。
/// 唯一性校验必须把重叠位置也算上，否则会放行一次有歧义的替换。
fn occurrence_count(haystack: &str, needle: &str) -> usize {
    if needle.is_empty() {
        return 0;
    }
    let step = needle.chars().next().map_or(1, char::len_utf8);
    let mut count = 0;
    let mut start = 0;
    while let Some(offset) = haystack[start..].find(needle) {
        count += 1;
        start += offset + step;
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::ids::CallId;
    use crate::domain::tool::OutcomeStatus;
    use std::fs;
    use std::path::PathBuf;

    fn temp_workspace(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("qch-edit-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("建临时目录");
        dir
    }

    fn tool(root: &PathBuf) -> EditFile {
        EditFile::new(WorkspaceRoot::new(root))
    }

    fn call(arguments: serde_json::Value) -> ToolCall {
        ToolCall {
            id: CallId::new("call_edit"),
            name: EDIT_FILE.to_owned(),
            arguments,
        }
    }

    fn edit_args(path: &str, old: &str, new: &str) -> serde_json::Value {
        json!({"path": path, "old_string": old, "new_string": new})
    }

    #[tokio::test]
    async fn replaces_a_unique_occurrence() {
        let root = temp_workspace("unique");
        fs::write(root.join("lib.rs"), "fn a() {}\nfn b() {}\n").unwrap();
        let file = tool(&root);

        let outcome = file
            .invoke(&call(edit_args("lib.rs", "fn b() {}", "fn renamed() {}")))
            .await;

        assert_eq!(outcome.status, OutcomeStatus::Success, "{}", outcome.content);
        let updated = fs::read_to_string(root.join("lib.rs")).unwrap();
        assert_eq!(updated, "fn a() {}\nfn renamed() {}\n");
        let _ = fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn multiple_matches_fail_instead_of_silent_replace() {
        let root = temp_workspace("multi");
        fs::write(root.join("lib.rs"), "let x = 1;\nlet x = 2;\n").unwrap();
        let file = tool(&root);

        let outcome = file
            .invoke(&call(edit_args("lib.rs", "let x", "let y")))
            .await;

        assert_eq!(outcome.status, OutcomeStatus::Failed);
        assert!(outcome.content.contains("匹配到 2 处"), "{}", outcome.content);
        assert!(
            outcome.content.contains("更长的上下文"),
            "应提示模型补充上下文: {}",
            outcome.content
        );
        // 文件必须原封不动。
        assert_eq!(
            fs::read_to_string(root.join("lib.rs")).unwrap(),
            "let x = 1;\nlet x = 2;\n"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn overlapping_matches_fail_instead_of_picking_the_first() {
        let root = temp_workspace("overlap");
        fs::write(root.join("lib.rs"), "aaa\n").unwrap();
        let file = tool(&root);

        let outcome = file
            .invoke(&call(edit_args("lib.rs", "aa", "b")))
            .await;

        assert_eq!(outcome.status, OutcomeStatus::Failed, "两个重叠位置同样不唯一");
        assert!(outcome.content.contains("匹配到 2 处"), "{}", outcome.content);
        assert_eq!(fs::read_to_string(root.join("lib.rs")).unwrap(), "aaa\n");
        let _ = fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn zero_matches_fail() {
        let root = temp_workspace("zero");
        fs::write(root.join("lib.rs"), "hello\n").unwrap();
        let file = tool(&root);

        let outcome = file
            .invoke(&call(edit_args("lib.rs", "不存在的内容", "任意")))
            .await;

        assert_eq!(outcome.status, OutcomeStatus::Failed);
        assert!(outcome.content.contains("找不到要替换的文本"), "{}", outcome.content);
        let _ = fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn identical_old_and_new_fails() {
        let root = temp_workspace("same");
        fs::write(root.join("lib.rs"), "hello\n").unwrap();
        let file = tool(&root);

        let outcome = file
            .invoke(&call(edit_args("lib.rs", "hello", "hello")))
            .await;

        assert_eq!(outcome.status, OutcomeStatus::Failed);
        assert!(outcome.content.contains("无需修改"));
        let _ = fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn empty_old_string_fails() {
        let root = temp_workspace("emptyold");
        fs::write(root.join("lib.rs"), "hello\n").unwrap();
        let file = tool(&root);

        let outcome = file.invoke(&call(edit_args("lib.rs", "", "x"))).await;

        assert_eq!(outcome.status, OutcomeStatus::Failed);
        assert!(outcome.content.contains("old_string 不能为空"));
        let _ = fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn missing_file_fails() {
        let root = temp_workspace("missing");
        let file = tool(&root);

        let outcome = file
            .invoke(&call(edit_args("ghost.rs", "a", "b")))
            .await;

        assert_eq!(outcome.status, OutcomeStatus::Failed);
        assert!(outcome.content.contains("读取"), "{}", outcome.content);
        let _ = fs::remove_dir_all(&root);
    }

    /// CRLF 文件要能被模型给的 LF 原文改到。
    ///
    /// Windows 上 git 默认把文本检出成 CRLF，模型从 `read_file` 看到的内容
    /// 与给出的 `old_string` 未必带 `\r`。比较前不统一行尾，两者永远对不上。
    #[tokio::test]
    async fn replaces_in_a_crlf_file_with_lf_text() {
        let root = temp_workspace("crlf");
        fs::write(root.join("lib.rs"), "fn a() {}\r\nfn b() {}\r\n").unwrap();
        let file = tool(&root);

        let outcome = file
            .invoke(&call(edit_args("lib.rs", "fn b() {}", "fn renamed() {}")))
            .await;

        assert_eq!(outcome.status, OutcomeStatus::Success, "{}", outcome.content);
        assert_eq!(
            fs::read_to_string(root.join("lib.rs")).unwrap(),
            "fn a() {}\r\nfn renamed() {}\r\n",
            "写回时不能把 CRLF 改成 LF"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// 改一处不能动别的行的行尾。
    ///
    /// 整文件换行尾会在 git 里变成满屏 diff：改一行却让每个未改动的行
    /// 都变了一遍。
    #[tokio::test]
    async fn a_crlf_file_keeps_its_line_endings_untouched() {
        let root = temp_workspace("crlf-mixed");
        fs::write(root.join("lib.rs"), "a\r\nb\r\nc\r\n").unwrap();
        let file = tool(&root);

        let outcome = file
            .invoke(&call(edit_args("lib.rs", "b", "B")))
            .await;

        assert_eq!(outcome.status, OutcomeStatus::Success, "{}", outcome.content);
        assert_eq!(
            fs::read_to_string(root.join("lib.rs")).unwrap(),
            "a\r\nB\r\nc\r\n"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn escaping_path_is_refused() {
        let root = temp_workspace("escape");
        let file = tool(&root);

        let outcome = file
            .invoke(&call(edit_args("../outside.rs", "a", "b")))
            .await;

        assert_eq!(outcome.status, OutcomeStatus::Refused);
        let _ = fs::remove_dir_all(&root);
    }
}

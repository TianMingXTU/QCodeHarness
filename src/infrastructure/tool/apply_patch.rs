//! 结构化补丁工具：一次调用改一处或多处。
//!
//! **为什么是独立工具，不增强 `edit_file`。** 三条理由：
//!
//! 1. **统一格式更容易让模型学会**。`edit_file` 的 `old_string` 要模型
//!    逐字抄原文，抄错就失败；补丁格式带上下文行，模型只需指出"改哪
//!    一块"。
//! 2. **一次改多处**。`edit_file` 改 N 处要 N 轮调用，补丁一次调用改
//!    N 处——同时省轮次。
//! 3. **不动 `edit_file` 的既有语义**。它有 7 个测试、语义已被验证
//!    （唯一匹配才改）。`apply_patch` 是新工具，两者并存，让模型自己选。
//!
//! **原子性是这个工具相对 `edit_file` 的最大优势**：全部操作先在内存里
//! 算出新内容，**全部成功才写盘**；多文件时先写全部临时文件，再逐个
//! 改名。任何一步失败，一个文件都不会被改。
//!
//! 写盘用"临时文件 + 改名"：中途崩溃最多个残留一个 `.tmp`，绝不会留下
//! 一个截断的文件顶替上一次的好文件（模式见
//! `infrastructure::session_store::json`）。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::json;

use crate::application::port::{BoxFuture, ToolInvoker};
use crate::domain::tool::{ToolAnnotations, ToolCall, ToolOutcome, ToolSpec};
use crate::domain::workspace::WorkspaceRoot;

use super::patch_format::{self, Operation};
use super::{parse_args, resolve_in};

const APPLY_PATCH: &str = "apply_patch";

/// 应用结构化补丁的工具。
pub struct ApplyPatch {
    root: WorkspaceRoot,
}

impl ApplyPatch {
    /// 以 `root` 为工作区构造。
    pub fn new(root: WorkspaceRoot) -> Self {
        Self { root }
    }

    /// 工具规格。
    pub fn spec() -> ToolSpec {
        ToolSpec {
            name: APPLY_PATCH.to_owned(),
            description: "用结构化补丁一次性修改 workspace 内的文件。补丁格式：\
                          以 `*** Begin Patch` 开头、`*** End Patch` 结尾，中间是若干操作：\
                          `*** Update File: 路径`（后跟改动块，行首 `-` 是删除、`+` 是新增、其余是上下文，\
                          可用 `@@ 定位提示` 限定搜索范围）、\
                          `*** Add File: 路径`（后跟每行以 `+` 开头的文件内容）、\
                          `*** Delete File: 路径`。\
                          所有操作要么全部成功，要么一个文件都不改。\
                          要改一处用 edit_file 更省事；新建文件用 write_file 或 Add File；\
                          整体重写用 write_file。"
                .to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "patch": { "type": "string", "description": "完整的补丁文本，含 Begin/End 标记" }
                },
                "required": ["patch"]
            }),
            // 修改与删除文件内容不可逆，同样的补丁做第二次会失败，不幂等。
            annotations: ToolAnnotations::new(false, true, false, false),
        }
    }
}

/// `apply_patch` 的参数。
#[derive(Debug, Deserialize)]
struct ApplyPatchArgs {
    patch: String,
}

/// 一个文件在补丁应用后的最终状态。
///
/// `content` 为 `None` 表示文件最终不存在（被删了）。
struct PlannedFile {
    /// workspace 内的相对路径（原样回显给模型）。
    display: String,
    /// 落盘的绝对路径。
    path: PathBuf,
    /// 最终内容。`None` = 文件最终不存在。
    content: Option<String>,
    /// 这个文件在补丁开始前是否就存在于磁盘上。
    ///
    /// 用来判断"删除"要不要真删：补丁里"先建后删"的文件从没在磁盘上
    /// 出现过，删它是删一个不存在的东西。
    existed: bool,
}

impl ToolInvoker for ApplyPatch {
    fn invoke<'a>(&'a self, call: &'a ToolCall) -> BoxFuture<'a, ToolOutcome> {
        Box::pin(async move {
            let args: ApplyPatchArgs = match parse_args(&call.arguments) {
                Ok(args) => args,
                Err(message) => return ToolOutcome::failed(message),
            };
            if args.patch.trim().is_empty() {
                return ToolOutcome::failed("patch 不能为空");
            }
            let operations = match patch_format::parse_patch(&args.patch) {
                Ok(operations) => operations,
                Err(message) => return ToolOutcome::failed(format!("补丁格式错误: {message}")),
            };

            // ── 第一阶段：算出每个文件的最终内容 ────────────────────────
            //
            // 路径校验与内容计算都在这里做完，一个文件都不碰盘。
            //
            // **后一个操作看到前一个的结果**：同一个文件在补丁里出现
            // 两次时，第二次的改动块是在第一次的结果上匹配的——否则
            // "先改再改"的补丁会拿原文去匹配第二次的上下文，必然失败。
            let mut planned: Vec<PlannedFile> = Vec::new();
            let mut index_of: HashMap<PathBuf, usize> = HashMap::new();

            for operation in &operations {
                match operation {
                    Operation::Update { path, hunks } => {
                        let resolved = match resolve_in(&self.root, path) {
                            Ok(resolved) => resolved,
                            Err(outcome) => return outcome,
                        };
                        let absolute = resolved.as_path().to_path_buf();
                        // 基准内容：补丁里已经碰过这个文件就用计划中的
                        // 内容，否则读磁盘。
                        let base = match index_of.get(&absolute).copied() {
                            Some(index) => match &planned[index].content {
                                Some(content) => content.clone(),
                                None => {
                                    return ToolOutcome::failed(format!(
                                        "补丁里 `{}` 先被删除又��更新：文件已不存在",
                                        path
                                    ));
                                }
                            },
                            None => match tokio::fs::read_to_string(&absolute).await {
                                Ok(content) => content,
                                Err(error) => {
                                    return ToolOutcome::failed(format!(
                                        "读取 `{path}` 失败: {error}"
                                    ));
                                }
                            },
                        };
                        let updated = match patch_format::apply_hunks(&base, hunks) {
                            Ok(updated) => updated,
                            Err(reason) => {
                                return ToolOutcome::failed(format!("补丁应用失败: {reason}"));
                            }
                        };
                        record(
                            &mut planned,
                            &mut index_of,
                            PlannedFile {
                                display: path.clone(),
                                path: absolute,
                                content: Some(updated),
                                existed: true,
                            },
                        );
                    }
                    Operation::Add { path, lines } => {
                        let resolved = match resolve_in(&self.root, path) {
                            Ok(resolved) => resolved,
                            Err(outcome) => return outcome,
                        };
                        let absolute = resolved.as_path().to_path_buf();
                        // "已存在"要对着**计划中的状态**判：补丁里先删后加
                        // 是合法的（挪走再重建），对着磁盘判会误报。
                        let already_there = match index_of.get(&absolute).copied() {
                            Some(index) => planned[index].content.is_some(),
                            None => absolute.exists(),
                        };
                        if already_there {
                            return ToolOutcome::failed(format!(
                                "`{path}` 已存在：新增文件不能覆盖已有文件（要改它请用 Update File）"
                            ));
                        }
                        let mut content = lines.join("\n");
                        if !content.is_empty() {
                            content.push('\n');
                        }
                        record(
                            &mut planned,
                            &mut index_of,
                            PlannedFile {
                                display: path.clone(),
                                path: absolute,
                                content: Some(content),
                                existed: false,
                            },
                        );
                    }
                    Operation::Delete { path } => {
                        let resolved = match resolve_in(&self.root, path) {
                            Ok(resolved) => resolved,
                            Err(outcome) => return outcome,
                        };
                        let absolute = resolved.as_path().to_path_buf();
                        let exists = match index_of.get(&absolute).copied() {
                            Some(index) => planned[index].content.is_some(),
                            None => absolute.exists(),
                        };
                        if !exists {
                            return ToolOutcome::failed(format!(
                                "`{path}` 不存在：没有可删除的文件"
                            ));
                        }
                        let existed = match index_of.get(&absolute).copied() {
                            Some(index) => planned[index].existed,
                            None => true,
                        };
                        record(
                            &mut planned,
                            &mut index_of,
                            PlannedFile {
                                display: path.clone(),
                                path: absolute,
                                content: None,
                                existed,
                            },
                        );
                    }
                }
            }

            // ── 第二阶段：先写全部临时文件 ──────────────────────────────
            //
            // 全部内容都算好了，写盘失败只可能是磁盘/权限这类环境问题。
            // 先把每个新内容写进临时文件，一个都写不成就一个都不改。
            let mut temps: Vec<(PathBuf, PathBuf)> = Vec::new();
            for file in planned.iter().filter(|file| file.content.is_some()) {
                let content = file.content.as_ref().expect("过滤过有内容的");
                let temp = temp_path(&file.path);
                // 父目录可能还不存在（新建 `src/a/b.rs` 时 `src/a` 是新的）。
                // 目录建不出来就报在这里——临时文件还没写，工作区仍然
                // 一个字节都没变。
                if let Some(parent) = temp.parent() {
                    if let Err(error) = tokio::fs::create_dir_all(parent).await {
                        for (written, _) in &temps {
                            let _ = tokio::fs::remove_file(written).await;
                        }
                        return ToolOutcome::failed(format!(
                            "创建 `{}` 的目录失败: {error}（未改动任何文件）",
                            file.display
                        ));
                    }
                }
                if let Err(error) = write_temp(&temp, content).await {
                    // 已经写好的临时文件清掉：留着它们只会污染工作区。
                    for (temp, _) in &temps {
                        let _ = tokio::fs::remove_file(temp).await;
                    }
                    return ToolOutcome::failed(format!(
                        "写入 `{}` 的临时文件失败: {error}（未改动任何文件）",
                        file.display
                    ));
                }
                temps.push((temp, file.path.clone()));
            }

            // ── 第三阶段：逐个改名 ──────────────────────────────────────
            //
            // 改名失败不回滚：回滚本身也可能失败（磁盘满、文件被占用），
            // "部分成功 + 如实报告"比"回滚失败导致状态更乱"诚实。报告里
            // 必须逐个列出已改与未改的文件，让模型知道真实状态。
            let mut changed: Vec<String> = Vec::new();
            let mut unchanged: Vec<String> = Vec::new();
            for (index, (temp, target)) in temps.iter().enumerate() {
                let display = planned
                    .iter()
                    .filter(|file| file.content.is_some())
                    .nth(index)
                    .map(|file| file.display.clone())
                    .unwrap_or_default();
                if let Err(error) = tokio::fs::rename(temp, target).await {
                    // 剩下的临时文件清掉，已改的如实报。
                    for (rest_temp, _) in &temps[index + 1..] {
                        let _ = tokio::fs::remove_file(rest_temp).await;
                    }
                    let mut report = format!(
                        "部分成功：已改动 {} 个文件，未改动 {} 个。改名 `{display}` 失败: {error}",
                        changed.len(),
                        unchanged.len() + 1
                    );
                    if !changed.is_empty() {
                        report.push_str(&format!("\n已改动：{}", changed.join("、")));
                    }
                    return ToolOutcome::failed(report);
                }
                changed.push(display);
            }
            for file in planned.iter().filter(|file| file.content.is_some()) {
                if !changed.contains(&file.display) {
                    unchanged.push(file.display.clone());
                }
            }

            // 删除放在最后：改都改成功了，删失败只影响那一个文件。
            let mut deleted: Vec<String> = Vec::new();
            let mut delete_failed: Vec<String> = Vec::new();
            for file in planned.iter().filter(|file| file.content.is_none()) {
                // 补丁里"先建后删"的文件从没在磁盘上出现过，删它是删一个
                // 不存在的东西——那不是失败，是本来就不用删。
                if !file.existed {
                    continue;
                }
                match tokio::fs::remove_file(&file.path).await {
                    Ok(()) => deleted.push(file.display.clone()),
                    Err(error) => {
                        delete_failed.push(format!("{}（{error}）", file.display));
                    }
                }
            }

            let mut summary = String::new();
            if !changed.is_empty() {
                summary.push_str(&format!(
                    "已改动 {} 个文件：{}\n",
                    changed.len(),
                    changed.join("、")
                ));
            }
            if !deleted.is_empty() {
                summary.push_str(&format!(
                    "已删除 {} 个文件：{}\n",
                    deleted.len(),
                    deleted.join("、")
                ));
            }
            if !delete_failed.is_empty() {
                summary.push_str(&format!(
                    "以下文件删除失败：{}",
                    delete_failed.join("、")
                ));
            }
            if summary.is_empty() {
                summary.push_str("补丁没有产生任何改动");
            }
            let summary = summary.trim_end().to_owned();
            if delete_failed.is_empty() {
                ToolOutcome::success(summary)
            } else {
                ToolOutcome::failed(summary)
            }
        })
    }
}

/// 把一个文件的计划状态记进表里（同一文件的后续操作覆盖前一个）。
fn record(
    planned: &mut Vec<PlannedFile>,
    index_of: &mut HashMap<PathBuf, usize>,
    file: PlannedFile,
) {
    match index_of.get(&file.path).copied() {
        Some(index) => planned[index] = file,
        None => {
            index_of.insert(file.path.clone(), planned.len());
            planned.push(file);
        }
    }
}

/// 临时文件路径：与目标同目录、同名加 `.tmp` 后缀。
///
/// 同目录是必须的：改名（`rename`）要在同一卷上才可靠，跨卷会失败。
fn temp_path(target: &Path) -> PathBuf {
    let mut name = target
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "patch".to_owned());
    name.push_str(".tmp");
    target.with_file_name(name)
}

/// 把内容写进临时文件并刷盘。
///
/// 刷盘（`sync_all`）不能省：改名之前数据必须真的落到盘上，否则断电
/// 之后得到的是一个"存在但内容是空的"文件——比写失败更坏。
async fn write_temp(temp: &Path, content: &str) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt;
    let mut file = tokio::fs::File::create(temp).await?;
    file.write_all(content.as_bytes()).await?;
    file.flush().await?;
    file.sync_all().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::ids::CallId;
    use crate::domain::tool::OutcomeStatus;
    use std::fs;
    use std::path::PathBuf;

    fn temp_workspace(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("qch-patch-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("建临时目录");
        dir
    }

    fn tool(root: &PathBuf) -> ApplyPatch {
        ApplyPatch::new(WorkspaceRoot::new(root))
    }

    fn call(patch: &str) -> ToolCall {
        ToolCall {
            id: CallId::new("call_patch"),
            name: APPLY_PATCH.to_owned(),
            arguments: json!({ "patch": patch }),
        }
    }

    /// 改一处。
    #[tokio::test]
    async fn updates_one_place() {
        let root = temp_workspace("update");
        fs::write(root.join("lib.rs"), "fn main() {\n    let x = 1;\n}\n").unwrap();
        let file = tool(&root);

        let outcome = file
            .invoke(&call(concat!(
                "*** Begin Patch\n",
                "*** Update File: lib.rs\n",
                "@@ fn main()\n",
                " fn main() {\n",
                "-    let x = 1;\n",
                "+    let x = 2;\n",
                " }\n",
                "*** End Patch",
            )))
            .await;

        assert_eq!(outcome.status, OutcomeStatus::Success, "{}", outcome.content);
        assert_eq!(
            fs::read_to_string(root.join("lib.rs")).unwrap(),
            "fn main() {\n    let x = 2;\n}\n"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// 新建文件。
    #[tokio::test]
    async fn adds_a_file() {
        let root = temp_workspace("add");
        let file = tool(&root);

        let outcome = file
            .invoke(&call(concat!(
                "*** Begin Patch\n",
                "*** Add File: src/new.rs\n",
                "+fn new() {}\n",
                "+// 第二行\n",
                "*** End Patch",
            )))
            .await;

        assert_eq!(outcome.status, OutcomeStatus::Success, "{}", outcome.content);
        assert_eq!(
            fs::read_to_string(root.join("src/new.rs")).unwrap(),
            "fn new() {}\n// 第二行\n"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// 删除文件。
    #[tokio::test]
    async fn deletes_a_file() {
        let root = temp_workspace("delete");
        fs::write(root.join("old.rs"), "fn old() {}\n").unwrap();
        let file = tool(&root);

        let outcome = file
            .invoke(&call(concat!(
                "*** Begin Patch\n",
                "*** Delete File: old.rs\n",
                "*** End Patch",
            )))
            .await;

        assert_eq!(outcome.status, OutcomeStatus::Success, "{}", outcome.content);
        assert!(!root.join("old.rs").exists(), "文件应被删除");
        let _ = fs::remove_dir_all(&root);
    }

    /// 上下文不匹配：失败，并附上文件实际内容让模型能纠正。
    #[tokio::test]
    async fn a_context_mismatch_explains_the_actual_content() {
        let root = temp_workspace("mismatch");
        fs::write(root.join("lib.rs"), "fn main() {\n    let x = 1;\n}\n").unwrap();
        let file = tool(&root);

        let outcome = file
            .invoke(&call(concat!(
                "*** Begin Patch\n",
                "*** Update File: lib.rs\n",
                "-    let x = 999;\n",
                "+    let x = 2;\n",
                "*** End Patch",
            )))
            .await;

        assert_eq!(outcome.status, OutcomeStatus::Failed);
        assert!(outcome.content.contains("找不到"), "{}", outcome.content);
        assert!(
            outcome.content.contains("let x = 1;"),
            "应附上文件实际内容: {}",
            outcome.content
        );
        // 文件必须原封不动。
        assert_eq!(
            fs::read_to_string(root.join("lib.rs")).unwrap(),
            "fn main() {\n    let x = 1;\n}\n"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// 上下文有歧义：拒绝改。
    #[tokio::test]
    async fn an_ambiguous_context_is_refused() {
        let root = temp_workspace("ambiguous");
        fs::write(root.join("lib.rs"), "let x = 1;\nlet x = 1;\n").unwrap();
        let file = tool(&root);

        let outcome = file
            .invoke(&call(concat!(
                "*** Begin Patch\n",
                "*** Update File: lib.rs\n",
                "-let x = 1;\n",
                "+let x = 2;\n",
                "*** End Patch",
            )))
            .await;

        assert_eq!(outcome.status, OutcomeStatus::Failed);
        assert!(outcome.content.contains("匹配到 2 处"), "{}", outcome.content);
        assert_eq!(
            fs::read_to_string(root.join("lib.rs")).unwrap(),
            "let x = 1;\nlet x = 1;\n"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// Add 目标已存在：拒绝，不覆盖。
    #[tokio::test]
    async fn adding_over_an_existing_file_fails() {
        let root = temp_workspace("add-exists");
        fs::write(root.join("a.rs"), "原有内容\n").unwrap();
        let file = tool(&root);

        let outcome = file
            .invoke(&call(concat!(
                "*** Begin Patch\n",
                "*** Add File: a.rs\n",
                "+新内容\n",
                "*** End Patch",
            )))
            .await;

        assert_eq!(outcome.status, OutcomeStatus::Failed);
        assert!(outcome.content.contains("已存在"), "{}", outcome.content);
        assert_eq!(
            fs::read_to_string(root.join("a.rs")).unwrap(),
            "原有内容\n",
            "原有文件不能被覆盖"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// Delete 目标不存在：拒绝。
    #[tokio::test]
    async fn deleting_a_missing_file_fails() {
        let root = temp_workspace("delete-missing");
        let file = tool(&root);

        let outcome = file
            .invoke(&call(concat!(
                "*** Begin Patch\n",
                "*** Delete File: ghost.rs\n",
                "*** End Patch",
            )))
            .await;

        assert_eq!(outcome.status, OutcomeStatus::Failed);
        assert!(outcome.content.contains("不存在"), "{}", outcome.content);
        let _ = fs::remove_dir_all(&root);
    }

    /// **原子性：多文件补丁里一个失败，其余文件都不改。**
    ///
    /// 这是补丁相对 `edit_file` 的核心优势。第二个文件的上下文对不上，
    /// 第一个文件一个字节都不能变。
    #[tokio::test]
    async fn one_failing_file_leaves_every_other_file_untouched() {
        let root = temp_workspace("atomic");
        fs::write(root.join("good.rs"), "fn good() {\n    let x = 1;\n}\n").unwrap();
        fs::write(root.join("bad.rs"), "fn bad() {\n    let y = 1;\n}\n").unwrap();
        let file = tool(&root);

        let outcome = file
            .invoke(&call(concat!(
                "*** Begin Patch\n",
                "*** Update File: good.rs\n",
                "-    let x = 1;\n",
                "+    let x = 2;\n",
                "*** Update File: bad.rs\n",
                "-    let z = 999;\n",
                "+    let z = 3;\n",
                "*** End Patch",
            )))
            .await;

        assert_eq!(outcome.status, OutcomeStatus::Failed);
        assert!(
            outcome.content.contains("找不到"),
            "应说清失败原因: {}",
            outcome.content
        );
        assert_eq!(
            fs::read_to_string(root.join("good.rs")).unwrap(),
            "fn good() {\n    let x = 1;\n}\n",
            "第一个文件必须原封不动"
        );
        assert_eq!(
            fs::read_to_string(root.join("bad.rs")).unwrap(),
            "fn bad() {\n    let y = 1;\n}\n"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// **受保护路径在补丁里出现：拒绝，且不写任何文件。**
    ///
    /// `.git/config` 写成补丁照样拒——受保护路径的判定在路径校验层，
    /// 不因为它藏在补丁文本里就绕过。
    #[tokio::test]
    async fn a_protected_path_in_the_patch_is_refused_and_nothing_is_written() {
        let root = temp_workspace("protected");
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::write(root.join(".git/config"), "ORIGINAL\n").unwrap();
        fs::write(root.join("safe.rs"), "fn safe() {}\n").unwrap();
        let file = tool(&root);

        let outcome = file
            .invoke(&call(concat!(
                "*** Begin Patch\n",
                "*** Update File: safe.rs\n",
                "-fn safe() {}\n",
                "+fn safe() { changed }\n",
                "*** Update File: .git/config\n",
                "-ORIGINAL\n",
                "+HACKED\n",
                "*** End Patch",
            )))
            .await;

        assert_eq!(outcome.status, OutcomeStatus::Refused);
        assert!(
            outcome.content.contains("受保护"),
            "应按受保护拒绝: {}",
            outcome.content
        );
        // 两个文件都必须原封不动：受保护路径在补丁的**后面**，
        // 但校验是全部做完才写盘的。
        assert_eq!(
            fs::read_to_string(root.join(".git/config")).unwrap(),
            "ORIGINAL\n"
        );
        assert_eq!(
            fs::read_to_string(root.join("safe.rs")).unwrap(),
            "fn safe() {}\n"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// 越界路径：拒绝。
    #[tokio::test]
    async fn an_escaping_path_is_refused() {
        let root = temp_workspace("escape");
        let file = tool(&root);

        let outcome = file
            .invoke(&call(concat!(
                "*** Begin Patch\n",
                "*** Update File: ../outside.rs\n",
                "-a\n",
                "+b\n",
                "*** End Patch",
            )))
            .await;

        assert_eq!(outcome.status, OutcomeStatus::Refused);
        let _ = fs::remove_dir_all(&root);
    }

    /// 凭据文件：受保护路径的一部分，写不进去。
    #[tokio::test]
    async fn a_credential_file_cannot_be_patched() {
        let root = temp_workspace("credential");
        fs::write(root.join(".env"), "TOKEN=abc\n").unwrap();
        let file = tool(&root);

        let outcome = file
            .invoke(&call(concat!(
                "*** Begin Patch\n",
                "*** Update File: .env\n",
                "-TOKEN=abc\n",
                "+TOKEN=hacked\n",
                "*** End Patch",
            )))
            .await;

        assert_eq!(outcome.status, OutcomeStatus::Refused);
        assert_eq!(
            fs::read_to_string(root.join(".env")).unwrap(),
            "TOKEN=abc\n",
            "凭据文件不能被改"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// 补丁格式错了：明确报错，文件不动。
    #[tokio::test]
    async fn a_malformed_patch_is_reported_and_changes_nothing() {
        let root = temp_workspace("malformed");
        fs::write(root.join("a.rs"), "fn a() {}\n").unwrap();
        let file = tool(&root);

        let outcome = file
            .invoke(&call(concat!(
                "*** Update File: a.rs\n",
                "-fn a() {}\n",
                "+fn a() { }\n",
            )))
            .await;

        assert_eq!(outcome.status, OutcomeStatus::Failed);
        assert!(
            outcome.content.contains("Begin Patch"),
            "应说清格式要求: {}",
            outcome.content
        );
        assert_eq!(fs::read_to_string(root.join("a.rs")).unwrap(), "fn a() {}\n");
        let _ = fs::remove_dir_all(&root);
    }

    /// 一次补丁改多个文件：全部成功。
    #[tokio::test]
    async fn one_patch_can_change_several_files() {
        let root = temp_workspace("multi");
        fs::write(root.join("a.rs"), "fn a() {\n    let x = 1;\n}\n").unwrap();
        fs::write(root.join("b.rs"), "fn b() {\n    let y = 1;\n}\n").unwrap();
        let file = tool(&root);

        let outcome = file
            .invoke(&call(concat!(
                "*** Begin Patch\n",
                "*** Update File: a.rs\n",
                "-    let x = 1;\n",
                "+    let x = 2;\n",
                "*** Update File: b.rs\n",
                "-    let y = 1;\n",
                "+    let y = 3;\n",
                "*** End Patch",
            )))
            .await;

        assert_eq!(outcome.status, OutcomeStatus::Success, "{}", outcome.content);
        assert!(outcome.content.contains("2 个文件"), "{}", outcome.content);
        assert!(fs::read_to_string(root.join("a.rs")).unwrap().contains("let x = 2;"));
        assert!(fs::read_to_string(root.join("b.rs")).unwrap().contains("let y = 3;"));
        let _ = fs::remove_dir_all(&root);
    }

    /// 临时文件不该留在工作区里。
    #[tokio::test]
    async fn no_temporary_file_is_left_behind() {
        let root = temp_workspace("no-temp");
        fs::write(root.join("a.rs"), "fn a() {\n    let x = 1;\n}\n").unwrap();
        let file = tool(&root);

        file.invoke(&call(concat!(
            "*** Begin Patch\n",
            "*** Update File: a.rs\n",
            "-    let x = 1;\n",
            "+    let x = 2;\n",
            "*** End Patch",
        )))
        .await;

        let leftovers: Vec<String> = fs::read_dir(&root)
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "临时文件应被改名掉: {leftovers:?}");
        let _ = fs::remove_dir_all(&root);
    }

    /// 同一个文件在补丁里出现两次：第二次在第一次的结果上匹配。
    ///
    /// 这是"后一个操作看到前一个的结果"的直接后果，也是补丁格式的自然
    /// 语义。第二次的改动块匹配的是第一次改出来的文本——若拿原文去
    /// 匹配，它必然失败。
    #[tokio::test]
    async fn a_file_touched_twice_uses_the_second_change_on_the_first_result() {
        let root = temp_workspace("twice");
        fs::write(root.join("a.rs"), "value = 1\n").unwrap();
        let file = tool(&root);

        let outcome = file
            .invoke(&call(concat!(
                "*** Begin Patch\n",
                "*** Update File: a.rs\n",
                "-value = 1\n",
                "+value = 2\n",
                "*** Update File: a.rs\n",
                "-value = 2\n",
                "+value = 3\n",
                "*** End Patch",
            )))
            .await;

        assert_eq!(outcome.status, OutcomeStatus::Success, "{}", outcome.content);
        assert_eq!(fs::read_to_string(root.join("a.rs")).unwrap(), "value = 3\n");
        let _ = fs::remove_dir_all(&root);
    }

    /// 补丁里先删后加同一个文件：合法（挪走再重建）。
    ///
    /// "已存在"要对着**计划中的状态**判，不能对着磁盘判——对着磁盘判
    /// 会把这个合法的补丁误报成"文件已存在"。
    #[tokio::test]
    async fn deleting_then_adding_the_same_file_works() {
        let root = temp_workspace("delete-then-add");
        fs::write(root.join("a.rs"), "旧内容\n").unwrap();
        let file = tool(&root);

        let outcome = file
            .invoke(&call(concat!(
                "*** Begin Patch\n",
                "*** Delete File: a.rs\n",
                "*** Add File: a.rs\n",
                "+新内容\n",
                "*** End Patch",
            )))
            .await;

        assert_eq!(outcome.status, OutcomeStatus::Success, "{}", outcome.content);
        assert_eq!(
            fs::read_to_string(root.join("a.rs")).unwrap(),
            "新内容\n",
            "文件应被重建为新内容"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// `apply_patch` 是破坏性工具：默认策略必须对它要审批。
    ///
    /// P11 明确"不放松任何安全边界"。`apply_patch` 一次能改多个文件，
    /// 破坏面比 `edit_file` 更大——它必须落进"非只读一律审批"这一档，
    /// 不能因为它是新工具就漏在默认放行里。
    #[test]
    fn the_default_policy_requires_approval_for_a_patch() {
        let engine = crate::domain::policy::PolicyEngine::new();
        let call = ToolCall {
            id: CallId::new("call_patch"),
            name: APPLY_PATCH.to_owned(),
            arguments: json!({"patch": "*** Begin Patch\n*** Add File: a.rs\n+x\n*** End Patch"}),
        };

        let decision = engine.evaluate(&call, &ApplyPatch::spec());

        assert_eq!(
            decision.verdict,
            crate::domain::policy::PolicyVerdict::RequireApproval,
            "补丁是破坏性操作，默认策略必须先问人"
        );
    }

    /// 注解本身也要钉住：破坏性、非只读、非幂等、不开放世界。
    ///
    /// 这四个标记是策略层与审批层的判据。`read_only` 误标成 true 会让
    /// 默认策略直接放行——那等于把一个新工具漏出安全边界之外。
    #[test]
    fn the_annotations_mark_the_patch_as_destructive() {
        let annotations = ApplyPatch::spec().annotations;

        assert!(!annotations.read_only, "补丁不是只读操作");
        assert!(annotations.destructive, "补丁是破坏性操作");
        assert!(!annotations.idempotent, "同一补丁重复执行结果可能不同");
        assert!(!annotations.open_world, "补丁只碰 workspace 内的文件");
    }
}

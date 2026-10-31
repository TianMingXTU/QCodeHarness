//! 工具基础设施：文件操作工具。
//!
//! 把现有 `src/tools.rs` 的三个工具搬过来，但不再包成 rig 的 `DynamicTool`：
//! 执行已经不经过 rig 的工具回调，而是走 `ToolInvoker` 端口，
//! rig 只负责把 `ToolSpec` 序列化给模型看。

use serde::Deserialize;
use serde_json::json;

use crate::application::port::{BoxFuture, ToolInvoker};
use crate::domain::tool::{ToolAnnotations, ToolCall, ToolOutcome, ToolSpec};
use crate::domain::workspace::{WorkspacePath, WorkspaceRoot};

const READ_FILE: &str = "read_file";
const LIST_DIR: &str = "list_dir";
const WRITE_FILE: &str = "write_file";

/// workspace 内的三个文件工具，共用一个执行器。
///
/// 一个执行器而不是三个：三个工具共享同一套路径校验和错误映射，
/// 分开写只会得到三份近似重复的代码。工具名到具体行为的分派在
/// [`FileToolSet::dispatch`] 里一次做完。
#[derive(Debug, Clone)]
pub struct FileToolSet {
    root: WorkspaceRoot,
}

impl FileToolSet {
    /// 以 `root` 为工作区构造工具集。
    pub fn new(root: WorkspaceRoot) -> Self {
        Self { root }
    }

    /// 工具集提供的规格，即模型能看到的能力。
    pub fn specs() -> Vec<ToolSpec> {
        vec![
            ToolSpec {
                name: READ_FILE.to_owned(),
                description: "读取 workspace 内指定文本文件的完整内容，返回 UTF-8 文本。"
                    .to_owned(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "workspace 内的相对路径，例如 src/main.rs" }
                    },
                    "required": ["path"]
                }),
                annotations: ToolAnnotations::new(true, false, true, false),
            },
            ToolSpec {
                name: LIST_DIR.to_owned(),
                description: "列出 workspace 内指定目录的内容（子目录以 / 结尾）。path 省略时列出根目录。"
                    .to_owned(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "workspace 内的相对路径，可省略" }
                    }
                }),
                annotations: ToolAnnotations::new(true, false, true, false),
            },
            ToolSpec {
                name: WRITE_FILE.to_owned(),
                description: "把文本内容写入 workspace 内的指定文件（覆盖写入，父目录必须已存在）。"
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
            },
        ]
    }

    /// 按工具名分派到具体实现。
    async fn dispatch(&self, call: &ToolCall) -> ToolOutcome {
        match call.name.as_str() {
            READ_FILE => self.read_file(call).await,
            LIST_DIR => self.list_dir(call).await,
            WRITE_FILE => self.write_file(call).await,
            // 工具目录已经挡掉未注册的名字；真走到这里就明确拒绝，不静默成功。
            other => ToolOutcome::refused(format!("工具 `{other}` 未注册")),
        }
    }

    async fn read_file(&self, call: &ToolCall) -> ToolOutcome {
        let args: ReadFileArgs = match parse_args(&call.arguments) {
            Ok(args) => args,
            Err(message) => return ToolOutcome::failed(message),
        };
        let path = match self.resolve(&args.path) {
            Ok(path) => path,
            Err(outcome) => return outcome,
        };
        match tokio::fs::read_to_string(path.as_path()).await {
            Ok(content) => ToolOutcome::success(content),
            Err(error) => ToolOutcome::failed(format!("读取 `{}` 失败: {error}", args.path)),
        }
    }

    async fn list_dir(&self, call: &ToolCall) -> ToolOutcome {
        let args: ListDirArgs = match parse_args(&call.arguments) {
            Ok(args) => args,
            Err(message) => return ToolOutcome::failed(message),
        };
        // 两种来源统一成 PathBuf：校验过的相对路径，或 workspace 根本身。
        let path = match args.path {
            Some(raw) => match self.resolve(&raw) {
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
                    path.as_path().display()
                ));
            }
        };
        while let Some(entry) = match reader.next_entry().await {
            Ok(entry) => entry,
            Err(error) => {
                return ToolOutcome::failed(format!(
                    "遍历目录 `{}` 失败: {error}",
                    path.as_path().display()
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
    }

    async fn write_file(&self, call: &ToolCall) -> ToolOutcome {
        let args: WriteFileArgs = match parse_args(&call.arguments) {
            Ok(args) => args,
            Err(message) => return ToolOutcome::failed(message),
        };
        let path = match self.resolve(&args.path) {
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
    }

    /// 解析路径，越界时直接给出拒绝观察。
    fn resolve(&self, raw: &str) -> Result<WorkspacePath, ToolOutcome> {
        WorkspacePath::resolve(&self.root, raw).map_err(|escape| ToolOutcome::refused(escape.to_string()))
    }
}

impl ToolInvoker for FileToolSet {
    fn invoke<'a>(&'a self, call: &'a ToolCall) -> BoxFuture<'a, ToolOutcome> {
        Box::pin(self.dispatch(call))
    }
}

/// `read_file` 的参数。
#[derive(Debug, Deserialize)]
struct ReadFileArgs {
    path: String,
}

/// `list_dir` 的参数，`path` 省略时列出 workspace 根目录。
#[derive(Debug, Deserialize)]
struct ListDirArgs {
    path: Option<String>,
}

/// `write_file` 的参数。
#[derive(Debug, Deserialize)]
struct WriteFileArgs {
    path: String,
    content: String,
}

/// 把模型给出的 JSON 参数反序列化成具体工具的参数结构。
fn parse_args<A: serde::de::DeserializeOwned>(
    arguments: &serde_json::Value,
) -> Result<A, String> {
    serde_json::from_value(arguments.clone()).map_err(|error| format!("参数解析失败: {error}"))
}

/// 仅供测试：构造一次调用。
#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::ids::CallId;

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
        let tools = FileToolSet::new(workspace());
        let outcome = tools
            .invoke(&call(READ_FILE, json!({"path": "../secrets.txt"})))
            .await;

        assert_eq!(outcome.status, crate::domain::tool::OutcomeStatus::Refused);
        assert!(outcome.content.contains("只允许 workspace 内的相对路径"));
    }

    #[tokio::test]
    async fn unknown_tool_name_is_refused_not_panicked() {
        let tools = FileToolSet::new(workspace());
        let outcome = tools.invoke(&call("rm_rf", json!({}))).await;

        assert_eq!(outcome.status, crate::domain::tool::OutcomeStatus::Refused);
    }

    #[tokio::test]
    async fn missing_argument_reports_failure() {
        let tools = FileToolSet::new(workspace());
        let outcome = tools.invoke(&call(READ_FILE, json!({}))).await;

        assert_eq!(outcome.status, crate::domain::tool::OutcomeStatus::Failed);
        assert!(outcome.content.starts_with("参数解析失败"));
    }

    #[test]
    fn write_file_is_the_only_destructive_spec() {
        let specs = FileToolSet::specs();
        let destructive: Vec<&str> = specs
            .iter()
            .filter(|spec| spec.annotations.destructive)
            .map(|spec| spec.name.as_str())
            .collect();

        assert_eq!(destructive, vec![WRITE_FILE]);
    }
}

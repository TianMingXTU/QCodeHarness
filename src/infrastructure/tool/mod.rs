//! 工具基础设施：全部内置工具与组装入口。
//!
//! 一个工具一个结构体，而不是一个大 dispatch：每个工具需要的配置不同
//! （只有 `run_command` 需要超时和输出上限），合成一个结构体就得让所有
//! 工具共享一堆用不上的字段。"加一个工具"只在一处发生——写好结构体，
//! 到 [`all_tools`] 登记一行。

pub mod edit;
pub mod file;
pub mod git;
pub mod search;
pub mod shell;

use std::sync::Arc;

use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::application::tool_registry::ToolEntry;
use crate::domain::tool::ToolOutcome;
use crate::domain::workspace::{WorkspacePath, WorkspaceRoot};

/// 组装全部内置工具。
///
/// 规格与执行入口在这里绑成 [`ToolEntry`]——调用方（`main.rs`、集成测试）
/// 拿到就能直接装进 [`crate::application::tool_registry::ToolRegistry`]，
/// 不可能造出"spec 有但没人执行"的登记项。
pub fn all_tools(root: WorkspaceRoot) -> Vec<ToolEntry> {
    vec![
        // 文件三件套：读、列、整体写。各自独立成结构体，配置只有 root。
        ToolEntry {
            spec: file::ReadFile::spec(),
            invoker: Arc::new(file::ReadFile::new(root.clone())),
        },
        ToolEntry {
            spec: file::ListDir::spec(),
            invoker: Arc::new(file::ListDir::new(root.clone())),
        },
        ToolEntry {
            spec: file::WriteFile::spec(),
            invoker: Arc::new(file::WriteFile::new(root.clone())),
        },
        ToolEntry {
            spec: search::SearchText::spec(),
            invoker: Arc::new(search::SearchText::new(root.clone())),
        },
        ToolEntry {
            spec: edit::EditFile::spec(),
            invoker: Arc::new(edit::EditFile::new(root.clone())),
        },
        ToolEntry {
            spec: shell::RunCommand::spec(),
            invoker: Arc::new(shell::RunCommand::new(root.clone())),
        },
        ToolEntry {
            spec: git::GitStatus::spec(),
            invoker: Arc::new(git::GitStatus::new(root.clone())),
        },
        ToolEntry {
            spec: git::GitDiff::spec(),
            invoker: Arc::new(git::GitDiff::new(root)),
        },
    ]
}

/// 把模型给出的 JSON 参数反序列化成具体工具的参数结构。
///
/// 参数解析失败归为 `failed` 而不是 `refused`：参数错是模型可自行修正的
/// 普通错误，错误信息里带上 serde 的说明，模型重试时知道缺了什么。
pub(crate) fn parse_args<A: DeserializeOwned>(arguments: &Value) -> Result<A, String> {
    serde_json::from_value(arguments.clone()).map_err(|error| format!("参数解析失败: {error}"))
}

/// 解析 workspace 内的相对路径，越界时直接给出拒绝观察。
///
/// 所有碰文件的工具共用这条校验路径，保证"越界即拒绝"只有一种实现。
pub(crate) fn resolve_in(root: &WorkspaceRoot, raw: &str) -> Result<WorkspacePath, ToolOutcome> {
    WorkspacePath::resolve(root, raw).map_err(|escape| ToolOutcome::refused(escape.to_string()))
}

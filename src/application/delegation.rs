//! 子 Agent 委派：请求校验、结果结构，以及子运行所需依赖的装配端口。
//!
//! 这里只放纯数据与校验，不碰工作树、进程或模型。真正的并发编排在
//! 基础设施层，因为它需要 tokio 的定时与任务调度；应用层只规定什么
//! 请求合法、什么结果必须如实上报。

use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::application::agent_run::AgentDependencies;
use crate::application::isolation::IsolationWorkspace;
use crate::domain::agent::AgentRole;
use crate::domain::ids::{SessionId, validate_safe_identifier};
use crate::domain::workspace::is_protected_path;

/// 一批委派允许的子任务上限。
///
/// 上限不是性能参数，而是审批与证据的可读性边界：每个子任务都会产生
/// 一份会话、一份证据和一次合并尝试，数量失控时人无法逐条复核。
pub const MAX_SUBTASKS: usize = 16;

/// 为每个隔离工作区创建独立工具、审批、会话存储和证据汇。
///
/// `scope` 是经过校验的工作区相对路径。角色档案应包含子工作区的项目指令；
/// 不得注入委派工具，也不得共享会被 runner 绑定取消通道的审批闸门。
pub trait ChildAgentFactory: Send + Sync {
    fn dependencies(
        &self,
        workspace: &IsolationWorkspace,
        role: AgentRole,
        session_id: &SessionId,
        scope: &Path,
    ) -> Result<AgentDependencies, String>;
}

impl<F> ChildAgentFactory for F
where
    F: Fn(&IsolationWorkspace, AgentRole, &SessionId, &Path) -> Result<AgentDependencies, String>
        + Send
        + Sync,
{
    fn dependencies(
        &self,
        workspace: &IsolationWorkspace,
        role: AgentRole,
        session_id: &SessionId,
        scope: &Path,
    ) -> Result<AgentDependencies, String> {
        self(workspace, role, session_id, scope)
    }
}

/// 模型提交的整批委派请求。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DelegationRequest {
    pub tasks: Vec<SubtaskRequest>,
}

/// 一个子任务的原始声明。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubtaskRequest {
    pub id: String,
    pub goal: String,
    /// 角色短名；省略时用编码者。只接受稳定角色名，未知角色不会被
    /// 猜测成默认角色。
    #[serde(default)]
    pub role: Option<String>,
    /// 子任务关注的目录，相对子工作区根；省略时用 `.`。
    #[serde(default)]
    pub scope: Option<String>,
}

/// 校验后的子任务声明。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Subtask {
    pub id: String,
    pub goal: String,
    pub role: AgentRole,
    /// 已归一化的相对目录，允许 `.`。
    pub scope: PathBuf,
}

/// 单个子任务的结局。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SubtaskStatus {
    /// 前序失败或整批取消，未开始运行。
    Skipped,
    /// 子运行、捕获与独立验证都成功。
    Completed,
    /// 任一步失败；原因见 `error`。
    Failed,
}

/// 单个子任务的结果。`verified` 只表示独立验证通过，不表示已合并。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubtaskOutcome {
    pub id: String,
    pub status: SubtaskStatus,
    /// 是否在子工作区上跑过外部验证。
    pub verified: bool,
    /// 是否确认移除了子工作区；清理失败时保留路径供人工恢复。
    pub workspace_removed: bool,
    /// 子工作区路径；未创建时为空。
    pub workspace_path: PathBuf,
    /// 子运行的会话 ID。
    pub session_id: SessionId,
    /// 证据 ID；未启动的子任务没有证据。
    pub evidence_id: Option<SessionId>,
    pub error: Option<String>,
}

/// 清理失败的工作区；保留路径，调用方据此提示人工处理。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CleanupFailure {
    pub id: String,
    pub path: PathBuf,
    pub reason: String,
}

/// 一批委派的完整结果。
///
/// 所有分支都返回它并序列化进工具观察，模型与证据看到同一份事实：
/// 哪些任务完成、哪些验证过、整批是否合并、哪些工作区没清理掉。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DelegationResult {
    pub invocation_id: String,
    /// 整批变更是否已写回父工作区。
    pub merged: bool,
    pub tasks: Vec<SubtaskOutcome>,
    pub cleanup_failures: Vec<CleanupFailure>,
    /// 批次级失败原因（准备、创建或合并）；单个任务的失败在任务项里。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl DelegationResult {
    /// 为每个请求项建立占位结果，后续按实际进展改写。
    pub fn placeholder(invocation_id: String, subtasks: &[Subtask]) -> Self {
        Self {
            invocation_id: invocation_id.clone(),
            merged: false,
            tasks: subtasks
                .iter()
                .map(|task| SubtaskOutcome {
                    id: task.id.clone(),
                    status: SubtaskStatus::Skipped,
                    verified: false,
                    workspace_removed: false,
                    workspace_path: PathBuf::new(),
                    session_id: SessionId::new(format!("{invocation_id}-{}", task.id)),
                    evidence_id: None,
                    error: None,
                })
                .collect(),
            cleanup_failures: Vec::new(),
            error: None,
        }
    }
}

/// 解析并校验模型提交的委派参数。
///
/// `max_subtasks` 来自配置，只限制并发数，不限制一批的任务总数；它必须
/// 在 `1..=MAX_SUBTASKS` 范围内。校验失败时不触碰任何工作区或模型；
/// 错误文本会原样回填给模型。
pub fn parse_request(arguments: &Value, max_subtasks: usize) -> Result<Vec<Subtask>, String> {
    if max_subtasks == 0 || max_subtasks > MAX_SUBTASKS {
        return Err(format!(
            "并行子任务上限必须在 1..={MAX_SUBTASKS} 范围内，当前为 {max_subtasks}"
        ));
    }
    let request: DelegationRequest = serde_json::from_value(arguments.clone())
        .map_err(|error| format!("委派参数无效: {error}"))?;
    if request.tasks.is_empty() {
        return Err("至少需要一个子任务".to_owned());
    }
    if request.tasks.len() > MAX_SUBTASKS {
        return Err(format!(
            "子任务数量 {} 超过上限 {MAX_SUBTASKS}；请拆批提交",
            request.tasks.len()
        ));
    }
    let mut ids = BTreeSet::new();
    let mut subtasks = Vec::with_capacity(request.tasks.len());
    for task in request.tasks {
        validate_safe_identifier("子任务 ID", &task.id, 56)?;
        if !ids.insert(task.id.clone()) {
            return Err(format!("子任务 ID 重复: {}", task.id));
        }
        let goal = task.goal.trim();
        if goal.is_empty() {
            return Err(format!("子任务 {} 的 goal 不能为空", task.id));
        }
        let role = match task.role.as_deref() {
            None => AgentRole::Coder,
            Some(name) => AgentRole::all()
                .into_iter()
                .find(|role| role.name() == name)
                .ok_or_else(|| {
                    format!(
                        "子任务 {} 的角色 `{name}` 未知；可用: {}",
                        task.id,
                        AgentRole::all()
                            .iter()
                            .map(|role| role.name())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                })?,
        };
        let scope = validate_scope(task.scope.as_deref().unwrap_or(".")).map_err(|error| {
            format!("子任务 {} 的 scope 无效: {error}", task.id)
        })?;
        subtasks.push(Subtask {
            id: task.id,
            goal: goal.to_owned(),
            role,
            scope,
        });
    }
    Ok(subtasks)
}

/// 把模型给的 scope 归一化成工作区内的相对目录。
///
/// 只做词法校验。物理链接与真实边界由隔离实现和子工作区装配再检查；
/// 这里先拒掉绝对路径、`..`、盘符和受保护目录，避免无意义地创建
/// 工作区后才失败。
fn validate_scope(raw: &str) -> Result<PathBuf, String> {
    if raw.is_empty() {
        return Err("不能为空".to_owned());
    }
    if raw.contains(['\\', ':', '\0']) {
        return Err(format!("`{raw}` 含有不允许的字符"));
    }
    let path = Path::new(raw);
    if path.is_absolute() {
        return Err(format!("`{raw}` 必须是工作区相对路径"));
    }
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(part) => {
                let text = part
                    .to_str()
                    .ok_or_else(|| format!("`{raw}` 含非 UTF-8 路径"))?;
                if text.trim_end_matches([' ', '.']).is_empty() || text != text.trim_end_matches([' ', '.'])
                {
                    return Err(format!("`{raw}` 含以空格或点结尾的路径段"));
                }
                if is_protected_path(Path::new(text)) {
                    return Err(format!("`{raw}` 落在受保护路径内"));
                }
                normalized.push(part);
            }
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(format!("`{raw}` 不允许越出工作区"));
            }
        }
    }
    Ok(if normalized.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        normalized
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tasks(value: Value) -> Value {
        json!({ "tasks": value })
    }

    #[test]
    fn valid_request_defaults_role_and_scope() {
        let parsed = parse_request(
            &tasks(json!([{ "id": "one", "goal": "  do it  " }])),
            2,
        )
        .unwrap();

        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].goal, "do it");
        assert_eq!(parsed[0].role, AgentRole::Coder);
        assert_eq!(parsed[0].scope, Path::new("."));
    }

    #[test]
    fn every_stable_role_is_accepted_and_nested_scope_is_normalized() {
        for role in AgentRole::all() {
            let parsed = parse_request(
                &tasks(json!([{ "id": role.name(), "goal": "work", "role": role.name(), "scope": "./src/parser/." }])),
                4,
            )
            .unwrap();
            assert_eq!(parsed[0].role, role);
            assert_eq!(parsed[0].scope, Path::new("src/parser"));
        }
    }

    #[test]
    fn invalid_requests_are_rejected_before_any_work() {
        for (arguments, max) in [
            (tasks(json!([])), 2),
            (tasks(json!([{ "id": "one", "goal": "work" }])), 0),
            (tasks(json!([{ "id": "one", "goal": "work" }])), 17),
            (tasks(json!([{ "id": "one", "goal": "  " }])), 2),
            (tasks(json!([{ "id": "one", "goal": "work" }, { "id": "one", "goal": "again" }])), 2),
            (tasks(json!([{ "id": "one", "goal": "work", "role": "manager" }])), 2),
            (tasks(json!([{ "id": "one", "goal": "work", "scope": "../escape" }])), 2),
            (tasks(json!([{ "id": "one", "goal": "work", "scope": "/escape" }])), 2),
            (tasks(json!([{ "id": "one", "goal": "work", "scope": ".git" }])), 2),
            (tasks(json!([{ "id": "one", "goal": "work", "scope": "" }])), 2),
            (tasks(json!([{ "id": "one", "goal": "work", "verify_command": "skip" }])), 2),
            (json!({ "tasks": [{ "id": "one", "goal": "work" }], "verify_command": "skip" }), 2),
            (tasks(json!([{ "id": "CON", "goal": "work" }])), 2),
            (tasks(json!([{ "id": "a/b", "goal": "work" }])), 2),
        ] {
            assert!(parse_request(&arguments, max).is_err(), "{arguments}");
        }
    }

    /// `max_subtasks` 只限制并发，不限制一批的任务总数。
    ///
    /// 一批 4 个任务、并发上限 2 是正常用法：调度器分两批跑完。把并发
    /// 上限当成批次上限会让模型拆出毫无意义的多次调用。
    #[test]
    fn concurrency_limit_does_not_cap_the_batch_size() {
        let request = tasks(json!([
            { "id": "one", "goal": "work" },
            { "id": "two", "goal": "work" },
        ]));
        assert_eq!(parse_request(&request, 1).unwrap().len(), 2);
        assert_eq!(parse_request(&request, 2).unwrap().len(), 2);

        let full = tasks(json!((0..MAX_SUBTASKS)
            .map(|index| json!({ "id": format!("task-{index}"), "goal": "work" }))
            .collect::<Vec<_>>()));
        assert_eq!(parse_request(&full, 1).unwrap().len(), MAX_SUBTASKS);
    }
}

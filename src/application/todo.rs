//! Todo 工具的应用层契约与纯统计。
//!
//! 工具通过 `TodoStore` 访问持久化，结构化操作通过 `TodoEvent` 编码进
//! 既有 `ToolRequested`/`ToolObserved` 证据。这里不读文件、不驱动进程，
//! 使装配方式可以替换且 `--report` 能按纯函数复算完成率。

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;

use crate::application::port::{BoxFuture, EvidenceSink, ToolInvoker};
use crate::application::tool_registry::{ToolEntry, ToolSource};
use crate::domain::evidence::{AuditEvent, Evidence};
use crate::domain::ids::CallId;
use crate::domain::todo::{TodoError, TodoItem, TodoList, TodoStatus};
use crate::domain::tool::{OutcomeStatus, ToolAnnotations, ToolCall, ToolOutcome, ToolSpec};

/// 单一 Todo 工具名；动作差异都由 `action` 参数表达，避免注册表膨胀。
pub const TODO_TOOL: &str = "todo";

/// 一次待办操作的类型化事实。`snapshot` 是操作后的列表，供完成率重放。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TodoEvent {
    pub operation: TodoOperation,
    pub snapshot: Vec<TodoItem>,
}

impl TodoEvent {
    pub fn new(operation: TodoOperation, snapshot: Vec<TodoItem>) -> Self {
        Self {
            operation,
            snapshot,
        }
    }
}

/// 工具成功时返回给模型的结果。`id` 便于后续 update/remove 引用，
/// `items` 是当前列表，`event` 是写入证据账本的结构化事实。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TodoToolReply {
    pub id: Option<String>,
    pub items: Vec<TodoItem>,
    pub event: TodoEvent,
}

/// 待办操作种类；只记录计划状态变化，不记录凭据或路径等外部细节。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TodoOperation {
    Add {
        id: String,
        content: String,
    },
    Update {
        id: String,
        from: TodoStatus,
        to: TodoStatus,
    },
    Remove {
        id: String,
    },
    Read,
}

/// 完成率计数。无待办时为 0，调用方据此与"未知"区分。
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TodoCompletion {
    pub completed: usize,
    pub total: usize,
    pub rate: f64,
}

impl TodoCompletion {
    pub fn from_items(items: &[TodoItem]) -> Self {
        let total = items.len();
        let completed = items
            .iter()
            .filter(|item| item.status == TodoStatus::Completed)
            .count();
        let rate = if total == 0 {
            0.0
        } else {
            completed as f64 / total as f64
        };
        Self {
            completed,
            total,
            rate,
        }
    }
}

/// 存储失败必须带原因；IO 与解析失败都不允许静默降级为空列表。
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum TodoStoreError {
    #[error("Todo 存储 IO 失败: {0}")]
    Io(String),
    #[error("Todo 存储数据损坏: {0}")]
    Corrupt(String),
}

/// Todo 持久化的扩展点。实现在 infrastructure，应用层只依赖此 trait。
pub trait TodoStore: Send + Sync {
    fn load(&self) -> Result<TodoList, TodoStoreError>;
    fn save(&self, list: &TodoList) -> Result<(), TodoStoreError>;
}

/// 从证据账本还原结构化 Todo 事件。结构化载荷只存在成功的观察里。
pub fn todo_events_from_evidence(events: &[Evidence]) -> Vec<TodoEvent> {
    events
        .iter()
        .filter_map(|evidence| match &evidence.event {
            AuditEvent::ToolObserved { call, outcome, .. }
                if call.name == TODO_TOOL && outcome.status == OutcomeStatus::Success =>
            {
                parse_todo_observation(&outcome.content)
            }
            _ => None,
        })
        .collect()
}

/// 纯完成率函数：按事件流重放，最终状态以最后一份非空快照为准。
pub fn todo_completion_rate(events: &[TodoEvent]) -> TodoCompletion {
    let mut list = TodoList::new();
    for event in events {
        if !event.snapshot.is_empty() {
            list = TodoList::from_items(event.snapshot.clone())
                .unwrap_or_else(|_| replay_snapshot(&event.operation, &event.snapshot));
            continue;
        }
        replay_operation(&mut list, &event.operation);
    }
    TodoCompletion::from_items(list.items())
}

/// 事件只给操作、不给快照时，按单向状态流补齐最小状态。
fn replay_operation(list: &mut TodoList, operation: &TodoOperation) {
    match operation {
        TodoOperation::Add { id, content } if list.get(id).is_none() => {
            let _ = list.add_with_id(id.clone(), content.clone());
        }
        TodoOperation::Update { id, from, to } => {
            if list.get(id).is_none() {
                let _ = list.add_with_id(id.clone(), format!("待办 {id}"));
                let _ = list.set_status(id, *from);
            }
            let _ = list.set_status(id, *to);
        }
        TodoOperation::Remove { id } => {
            let _ = list.remove(id);
        }
        TodoOperation::Read | TodoOperation::Add { .. } => {}
    }
}

/// 快照损坏时重建一份可用状态；正常持久化不会产生这种事件。
fn replay_snapshot(operation: &TodoOperation, snapshot: &[TodoItem]) -> TodoList {
    let mut list = TodoList::new();
    for item in snapshot {
        let _ = list.add_with_id(item.id.clone(), item.content.clone());
        let _ = list.set_status(&item.id, item.status);
    }
    replay_operation(&mut list, operation);
    list
}

fn parse_todo_observation(content: &str) -> Option<TodoEvent> {
    let value: Value = serde_json::from_str(content).ok()?;
    serde_json::from_value::<TodoToolReply>(value)
        .ok()
        .map(|reply| reply.event)
}

/// 单一 Todo 工具来源；非只读注解让标准 ToolGuard 链要求审批。
pub struct TodoToolSource {
    store: Arc<dyn TodoStore>,
    evidence: Option<Arc<dyn EvidenceSink>>,
}

impl TodoToolSource {
    pub fn new(store: Arc<dyn TodoStore>) -> Self {
        Self {
            store,
            evidence: None,
        }
    }

    /// 注入证据账本。工具自身也记录结构化事件，避免只依赖 Runner 的观察。
    pub fn with_evidence(mut self, evidence: Arc<dyn EvidenceSink>) -> Self {
        self.evidence = Some(evidence);
        self
    }
}

impl ToolSource for TodoToolSource {
    fn name(&self) -> &str {
        "todo"
    }

    fn tools(&self) -> Vec<ToolEntry> {
        vec![ToolEntry {
            spec: todo_spec(),
            invoker: Arc::new(TodoInvoker {
                store: Arc::clone(&self.store),
                evidence: self.evidence.clone(),
            }),
        }]
    }
}

/// 工具规格；所有写操作都走同一非只读注解，避免只读 list 绕过审批。
fn todo_spec() -> ToolSpec {
    ToolSpec {
        name: TODO_TOOL.to_owned(),
        description: "维护结构化 Todo 计划。action 为 add/list/update/remove；update 可改 content 或 status。".to_owned(),
        parameters: json!({
            "type": "object",
            "properties": {
                "action": { "type": "string", "enum": ["add", "list", "update", "remove"] },
                "id": { "type": "string" },
                "content": { "type": "string" },
                "status": { "type": "string", "enum": ["pending", "in_progress", "completed"] }
            },
            "required": ["action"],
            "additionalProperties": false
        }),
        annotations: ToolAnnotations::new(false, true, false, false),
    }
}

struct TodoInvoker {
    store: Arc<dyn TodoStore>,
    evidence: Option<Arc<dyn EvidenceSink>>,
}

impl ToolInvoker for TodoInvoker {
    fn invoke<'a>(&'a self, call: &'a ToolCall) -> BoxFuture<'a, ToolOutcome> {
        Box::pin(async move { self.run(call) })
    }
}

impl TodoInvoker {
    fn run(&self, call: &ToolCall) -> ToolOutcome {
        let result = self.apply(call);
        let outcome = match result {
            Ok((id, operation, snapshot)) => {
                let reply = TodoToolReply {
                    id,
                    items: snapshot.clone(),
                    event: TodoEvent::new(operation, snapshot),
                };
                ToolOutcome::success(
                    serde_json::to_string(&reply).expect("TodoToolReply 是可序列化结构"),
                )
            }
            Err(reason) => ToolOutcome::failed(reason),
        };
        if let Some(sink) = &self.evidence {
            // 证据写入复用标准审计事件；结构化载荷在成功观察里。
            // 这里只补一条 ToolObserved，由装配方决定是否注入 sink。
            sink.record(
                std::time::SystemTime::now(),
                AuditEvent::ToolObserved {
                    turn: 0,
                    call: call.clone(),
                    outcome: outcome.clone(),
                },
            );
        }
        outcome
    }

    fn apply(
        &self,
        call: &ToolCall,
    ) -> Result<(Option<String>, TodoOperation, Vec<TodoItem>), String> {
        let action = call
            .arguments
            .get("action")
            .and_then(Value::as_str)
            .ok_or_else(|| "缺少 action 参数".to_owned())?;
        let mut list = self.store.load().map_err(|error| error.to_string())?;
        let (id, operation) = match action {
            "list" => (None, TodoOperation::Read),
            "add" => {
                let content = required_string(&call.arguments, "content")?;
                let item = list.add(content.clone()).map_err(todo_error_text)?;
                (
                    Some(item.id.clone()),
                    TodoOperation::Add {
                        id: item.id,
                        content,
                    },
                )
            }
            "update" => {
                let id = required_string(&call.arguments, "id")?;
                if let Some(content) = optional_string(&call.arguments, "content")? {
                    list.set_content(&id, content).map_err(todo_error_text)?;
                }
                let to = match optional_string(&call.arguments, "status")? {
                    Some(raw) => parse_status(&raw)?,
                    None => {
                        list.get(&id)
                            .ok_or_else(|| TodoError::UnknownId(id.clone()).to_string())?
                            .status
                    }
                };
                let from = list
                    .get(&id)
                    .ok_or_else(|| TodoError::UnknownId(id.clone()).to_string())?
                    .status;
                list.set_status(&id, to).map_err(todo_error_text)?;
                (Some(id.clone()), TodoOperation::Update { id, from, to })
            }
            "remove" => {
                let id = required_string(&call.arguments, "id")?;
                list.remove(&id).map_err(todo_error_text)?;
                (Some(id.clone()), TodoOperation::Remove { id })
            }
            other => return Err(format!("未知 Todo action: {other}")),
        };
        self.store.save(&list).map_err(|error| error.to_string())?;
        Ok((id, operation, list.items().to_vec()))
    }
}

fn required_string(arguments: &Value, key: &str) -> Result<String, String> {
    arguments
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| format!("缺少有效 {key} 参数"))
}

fn optional_string(arguments: &Value, key: &str) -> Result<Option<String>, String> {
    match arguments.get(key) {
        None => Ok(None),
        Some(value) => value
            .as_str()
            .map(str::to_owned)
            .map(Some)
            .ok_or_else(|| format!("{key} 参数必须是字符串")),
    }
}

fn parse_status(raw: &str) -> Result<TodoStatus, String> {
    match raw {
        "pending" => Ok(TodoStatus::Pending),
        "in_progress" => Ok(TodoStatus::InProgress),
        "completed" => Ok(TodoStatus::Completed),
        other => Err(format!("未知 Todo 状态: {other}")),
    }
}

fn todo_error_text(error: TodoError) -> String {
    match error {
        TodoError::IllegalTransition { from, to } => {
            format!("非法状态流转: {} -> {}", from.as_str(), to.as_str())
        }
        other => other.to_string(),
    }
}

/// 测试与装配方可用它构造一致的工具调用 ID，避免硬编码字符串散落。
pub fn todo_call_id(raw: impl Into<String>) -> CallId {
    CallId::new(raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completion_without_items_is_zero_not_unknown() {
        assert_eq!(TodoCompletion::from_items(&[]).rate, 0.0);
    }
}

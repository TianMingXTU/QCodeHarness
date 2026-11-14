//! Todo 列表的纯领域状态机。
//!
//! 这里只维护列表结构与状态流转规则；文件、证据和工具调用都留在上层，
//! 使规则可以脱离 IO 被测试，也让下游 Plan mode 能直接复用同一份类型。

use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Todo 项的三种状态。状态只能单向推进，避免计划完成后被悄然回退。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    #[default]
    Pending,
    InProgress,
    Completed,
}

impl TodoStatus {
    /// 返回状态在证据和工具参数中使用的稳定名称。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::InProgress => "in_progress",
            Self::Completed => "completed",
        }
    }
}

/// 一条结构化计划项。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TodoItem {
    pub id: String,
    pub content: String,
    pub status: TodoStatus,
}

/// 状态机可判定的全部失败原因。错误文本不带 IO 细节，便于上层包装。
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum TodoError {
    #[error("Todo 项不存在: {0}")]
    UnknownId(String),
    #[error("Todo ID 重复: {0}")]
    DuplicateId(String),
    #[error("Todo 内容不能为空")]
    EmptyContent,
    #[error("非法状态流转: {from:?} -> {to:?}")]
    IllegalTransition { from: TodoStatus, to: TodoStatus },
}

/// 内存中的 Todo 列表，ID 由本结构分配并保证唯一。
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TodoList {
    items: Vec<TodoItem>,
    next_id: u64,
    #[serde(default)]
    used_ids: HashSet<String>,
}

impl Default for TodoList {
    fn default() -> Self {
        Self {
            items: Vec::new(),
            next_id: 0,
            used_ids: HashSet::new(),
        }
    }
}

impl TodoList {
    pub fn new() -> Self {
        Self::default()
    }

    /// 从持久化数据重建列表，并校验 ID 唯一这一领域不变量。
    pub fn from_items(items: Vec<TodoItem>) -> Result<Self, TodoError> {
        let mut used_ids = HashSet::new();
        for item in &items {
            if !used_ids.insert(item.id.clone()) {
                return Err(TodoError::DuplicateId(item.id.clone()));
            }
        }
        Ok(Self {
            items,
            next_id: 0,
            used_ids,
        })
    }

    pub fn items(&self) -> &[TodoItem] {
        &self.items
    }

    pub fn get(&self, id: &str) -> Option<&TodoItem> {
        self.items.iter().find(|item| item.id == id)
    }

    /// 追加一条待办项，返回带 ID 的副本。
    pub fn add(&mut self, content: impl Into<String>) -> Result<TodoItem, TodoError> {
        let content = content.into();
        if content.trim().is_empty() {
            return Err(TodoError::EmptyContent);
        }
        let id = loop {
            let candidate = format!("todo-{}", self.next_id + 1);
            self.next_id += 1;
            if self.used_ids.insert(candidate.clone()) {
                break candidate;
            }
        };
        let item = TodoItem {
            id: id.clone(),
            content,
            status: TodoStatus::Pending,
        };
        self.items.push(item.clone());
        Ok(item)
    }

    /// 按外部 ID 追加一条待办项；导入持久化数据时用它保持 ID 稳定。
    pub fn add_with_id(
        &mut self,
        id: impl Into<String>,
        content: impl Into<String>,
    ) -> Result<TodoItem, TodoError> {
        let id = id.into();
        let content = content.into();
        if content.trim().is_empty() {
            return Err(TodoError::EmptyContent);
        }
        if !self.used_ids.insert(id.clone()) {
            return Err(TodoError::DuplicateId(id));
        }
        let item = TodoItem {
            id: id.clone(),
            content,
            status: TodoStatus::Pending,
        };
        self.items.push(item.clone());
        Ok(item)
    }

    /// 推进状态；同状态是幂等操作，跨越 InProgress 或回退都明确拒绝。
    pub fn set_status(&mut self, id: &str, to: TodoStatus) -> Result<TodoItem, TodoError> {
        let item = self
            .items
            .iter_mut()
            .find(|item| item.id == id)
            .ok_or_else(|| TodoError::UnknownId(id.to_owned()))?;
        if !is_transition_allowed(item.status, to) {
            return Err(TodoError::IllegalTransition {
                from: item.status,
                to,
            });
        }
        item.status = to;
        Ok(item.clone())
    }

    /// 修改内容但不改变状态，保证文本编辑不会绕过审批过的状态流。
    pub fn set_content(
        &mut self,
        id: &str,
        content: impl Into<String>,
    ) -> Result<TodoItem, TodoError> {
        let content = content.into();
        if content.trim().is_empty() {
            return Err(TodoError::EmptyContent);
        }
        let item = self
            .items
            .iter_mut()
            .find(|item| item.id == id)
            .ok_or_else(|| TodoError::UnknownId(id.to_owned()))?;
        item.content = content;
        Ok(item.clone())
    }

    pub fn remove(&mut self, id: &str) -> Result<TodoItem, TodoError> {
        let index = self
            .items
            .iter()
            .position(|item| item.id == id)
            .ok_or_else(|| TodoError::UnknownId(id.to_owned()))?;
        Ok(self.items.remove(index))
    }
}

/// 只允许原地保持或向下一阶段推进；Completed 是终态。
fn is_transition_allowed(from: TodoStatus, to: TodoStatus) -> bool {
    matches!(
        (from, to),
        (TodoStatus::Pending, TodoStatus::Pending)
            | (TodoStatus::Pending, TodoStatus::InProgress)
            | (TodoStatus::InProgress, TodoStatus::InProgress)
            | (TodoStatus::InProgress, TodoStatus::Completed)
            | (TodoStatus::Completed, TodoStatus::Completed)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_never_repeat_after_removal() {
        let mut list = TodoList::new();
        let first = list.add("a").unwrap();
        list.remove(&first.id).unwrap();
        let second = list.add("b").unwrap();
        assert_ne!(first.id, second.id);
    }

    #[test]
    fn completed_is_terminal() {
        let mut list = TodoList::new();
        let item = list.add("a").unwrap();
        list.set_status(&item.id, TodoStatus::InProgress).unwrap();
        list.set_status(&item.id, TodoStatus::Completed).unwrap();
        assert_eq!(
            list.set_status(&item.id, TodoStatus::InProgress),
            Err(TodoError::IllegalTransition {
                from: TodoStatus::Completed,
                to: TodoStatus::InProgress,
            })
        );
    }

    #[test]
    fn missing_item_is_reported() {
        let mut list = TodoList::new();
        assert_eq!(
            list.set_status("absent", TodoStatus::Pending),
            Err(TodoError::UnknownId("absent".to_owned()))
        );
    }
}

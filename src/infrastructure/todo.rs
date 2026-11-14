//! Todo 持久化的文件实现。
//!
//! 只在基础设施层触碰文件：把 `TodoList` 序列化为 JSON 存档，载入时校验
//! 损坏（不静默降级为空列表）。应用层通过 `TodoStore` trait 使用本实现。

use std::fs;
use std::path::PathBuf;

use crate::application::todo::{TodoStore, TodoStoreError};
use crate::domain::todo::TodoList;

/// 把待办列表存到单个 JSON 文件。路径父目录不存在时按需创建。
pub struct FileTodoStore {
    path: PathBuf,
}

impl FileTodoStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    fn ensure_parent(&self) -> Result<(), TodoStoreError> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).map_err(|error| TodoStoreError::Io(error.to_string()))?;
        }
        Ok(())
    }
}

impl TodoStore for FileTodoStore {
    fn load(&self) -> Result<TodoList, TodoStoreError> {
        if !self.path.exists() {
            return Ok(TodoList::new());
        }
        let raw = fs::read_to_string(&self.path)
            .map_err(|error| TodoStoreError::Io(error.to_string()))?;
        serde_json::from_str::<TodoList>(&raw)
            .map_err(|error| TodoStoreError::Corrupt(error.to_string()))
    }

    fn save(&self, list: &TodoList) -> Result<(), TodoStoreError> {
        self.ensure_parent()?;
        let text = serde_json::to_string_pretty(list)
            .map_err(|error| TodoStoreError::Io(error.to_string()))?;
        fs::write(&self.path, text).map_err(|error| TodoStoreError::Io(error.to_string()))
    }
}

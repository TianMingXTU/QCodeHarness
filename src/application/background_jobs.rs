//! 后台作业的纯契约。
//!
//! `run_command` 的后台分支与 `job_list` 查询共用这里的 ID、状态和 trait。
//! 生命周期由基础设施实现；本模块不持有进程句柄、不碰平台 API。

use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::application::port::BoxFuture;

/// 作业 ID，由注册器单调递增生成，恢复后不与旧作业混淆。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct JobId(pub u64);

/// 作业状态。超时是终态，避免 `job_list` 把已清理进程误报为运行中。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum JobStatus {
    Running,
    Exited { code: i32 },
    TimedOut,
}

/// 一条只读的作业清单记录。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobRecord {
    pub id: JobId,
    pub command: String,
    pub status: JobStatus,
    pub note: String,
}

/// 后台作业输入。工作目录由调用方解析为 workspace 根，越界检查在前。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackgroundJobSpec {
    pub command: String,
    pub root: PathBuf,
    pub timeout: Duration,
}

/// 启动与查询后台作业的扩展点。
///
/// 只读查询走同一份进程内状态，避免 `job_list` 另起子进程或读未知文件。
pub trait BackgroundJobRegistry: Send + Sync {
    fn spawn(&self, spec: BackgroundJobSpec) -> BoxFuture<'_, Result<JobId, String>>;
    fn list(&self) -> BoxFuture<'_, Result<Vec<JobRecord>, String>>;
}

//! 子任务隔离端口；应用层只传递基线、工作区和二进制补丁。

use std::path::{Path, PathBuf};

use crate::application::port::BoxFuture;

/// 从共同基线建立的独立工作区。由隔离实现签发和校验。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IsolationWorkspace {
    /// 调用方分配的子任务 ID。
    pub id: String,
    /// 供子任务和验证器使用的独立根目录。
    pub root: PathBuf,
    /// 完整基线 revision，捕获期间不得移动。
    pub base_revision: String,
}

/// 相对于基线的完整 Git 二进制补丁，不携带用户提交。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangeSet {
    /// 变更所属的子任务 ID，用于定位冲突来源。
    pub id: String,
    /// 相对共同基线的二进制补丁；合并端仍须校验路径与文件类型。
    pub patch: Vec<u8>,
}

/// 在合并候选工作区运行项目要求的验证。
pub trait WorkspaceVerifier: Send + Sync {
    /// `root` 含整批变更；返回错误时不得把候选写回父工作区。
    fn verify<'a>(&'a self, root: &'a Path) -> BoxFuture<'a, Result<(), String>>;
}

/// 子任务的工作区生命周期与整批合并。
///
/// 所有失败均返回说明文本。调用方负责在成功和失败路径上移除子工作区；
/// 合并实现负责清理自身的临时候选工作区，不提交、不暂存父工作区。
pub trait WorkspaceIsolation: Send + Sync {
    /// 检查父工作区干净，并返回完整基线 revision。
    fn prepare(&self) -> BoxFuture<'_, Result<String, String>>;

    /// 从基线建立独立工作区。ID 只在当前隔离实例内唯一。
    fn create<'a>(
        &'a self,
        id: &'a str,
        base: &'a str,
    ) -> BoxFuture<'a, Result<IsolationWorkspace, String>>;

    /// 捕获子工作区相对基线的全部允许变更，包括新增文件。
    fn capture<'a>(
        &'a self,
        workspace: &'a IsolationWorkspace,
    ) -> BoxFuture<'a, Result<ChangeSet, String>>;

    /// 在共同基线上组合补丁，验证候选，再一次性写回仍然干净的父工作区。
    ///
    /// 冲突、验证失败、基线移动或父工作区变脏时，父文件和 index 保持原样。
    fn merge<'a>(
        &'a self,
        base: &'a str,
        changes: &'a [ChangeSet],
        verifier: &'a dyn WorkspaceVerifier,
    ) -> BoxFuture<'a, Result<(), String>>;

    /// 移除本实例创建的工作区及其 Git 元数据。已移除时再次调用可成功。
    fn remove<'a>(&'a self, workspace: &'a IsolationWorkspace)
    -> BoxFuture<'a, Result<(), String>>;
}

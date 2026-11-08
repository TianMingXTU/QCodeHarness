//! 工作区清单端口：恢复前检查工作区是否被改动。
//!
//! 定义在应用层，避免编排器直接依赖基础设施实现。编排器只关心工作区
//! 当前状态，不需要知道清单如何扫描；扫描涉及文件系统，属于基础设施。

use crate::domain::worktree::WorktreeManifest;

/// 扫描工作区并给出清单。
///
/// 定义为 trait 便于测试注入固定清单，稳定复现"工作区被改过"分支，
/// 无需真的修改文件。
pub trait WorktreeScanner: Send + Sync {
    /// 扫描当前工作区。
    ///
    /// # Errors
    ///
    /// 工作区无法读取时返回说明文本。调用方应将这种情况视为"无法检查"，
    /// 不能当作"没有改动"，两者后果不同。
    fn scan(&self) -> Result<WorktreeManifest, String>;
}

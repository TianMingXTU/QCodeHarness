//! Workspace 抽象：受控执行环境的根目录与路径。
//!
//! 核心不变量：所有文件操作必须限制在 workspace 内，拒绝绝对路径和 `..` 等越界成分。
//! 这个不变量通过 `WorkspacePath` 类型在构造时强制执行——拿到一个
//! `WorkspacePath` 就等于拿到"已经验过界"的保证，下游不必再检查一遍。

use std::path::{Component, Path, PathBuf};

/// 工作区根目录。
///
/// 只做"基准点"，不校验存在性、也不做规范化：目录由 `interface` 层从进程
/// 工作目录取得并交进来，领域层不该去碰文件系统。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceRoot(PathBuf);

impl WorkspaceRoot {
    /// 用给定目录作为根。
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self(path.into())
    }

    /// 只读访问根目录路径。
    pub fn as_path(&self) -> &Path {
        &self.0
    }
}

/// 路径越界错误：模型给出的相对路径试图逃出 workspace。
///
/// 按 `tools.rs` 原有实现，越界路径统一归为"拒绝访问"，不再细分原因——
/// 错误说明会作为观察结果回传给模型，细分别的型号对模型没有意义。
#[derive(Debug, thiserror::Error)]
#[error("拒绝访问 `{raw}`：只允许 workspace 内的相对路径")]
pub struct PathEscape {
    /// 模型给出的原始路径文本，原样回显便于模型自行纠正。
    pub raw: String,
}

/// workspace 内的一条已验证路径。
///
/// 字段私有：只能通过 [`WorkspacePath::resolve`] 构造，因此不可能存在
/// 绕过校验的实例。这是"构造即合法"的含义。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspacePath(PathBuf);

impl WorkspacePath {
    /// 把模型给出的路径解析到 workspace 内。
    ///
    /// 这是 M1 的词法校验，不解析符号链接；完整的沙箱隔离是后续里程碑的工作。
    ///
    /// # Errors
    ///
    /// 路径为绝对路径、或含 `..` / 根 / 前缀等越界成分时返回 [`PathEscape`]。
    pub fn resolve(root: &WorkspaceRoot, raw: &str) -> Result<Self, PathEscape> {
        let relative = Path::new(raw);
        let escapes = relative.is_absolute()
            || relative.components().any(|component| {
                matches!(
                    component,
                    Component::ParentDir | Component::RootDir | Component::Prefix(_)
                )
            });
        if escapes {
            return Err(PathEscape { raw: raw.to_owned() });
        }
        Ok(Self(root.0.join(relative)))
    }

    /// 只读访问已解析的绝对路径。
    pub fn as_path(&self) -> &Path {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 词法校验不要求 workspace 真实存在。
    fn workspace() -> WorkspaceRoot {
        WorkspaceRoot::new("D:/tmp/demo-workspace")
    }

    #[test]
    fn relative_path_joins_workspace() {
        let resolved = WorkspacePath::resolve(&workspace(), "src/main.rs").unwrap();
        assert_eq!(resolved.as_path(), workspace().as_path().join("src/main.rs"));
    }

    #[test]
    fn rejects_absolute_paths() {
        assert!(WorkspacePath::resolve(&workspace(), "C:/Windows/system32").is_err());
        // `/etc/passwd` 在 Windows 上不是 is_absolute，但含 RootDir，同样要拒绝。
        assert!(WorkspacePath::resolve(&workspace(), "/etc/passwd").is_err());
    }

    #[test]
    fn rejects_parent_dir_escape() {
        assert!(WorkspacePath::resolve(&workspace(), "../secrets.txt").is_err());
        assert!(WorkspacePath::resolve(&workspace(), "src/../../outside.txt").is_err());
    }

    #[test]
    fn allows_current_dir_component() {
        let resolved = WorkspacePath::resolve(&workspace(), "./README.md").unwrap();
        assert_eq!(resolved.as_path(), workspace().as_path().join("./README.md"));
    }
}

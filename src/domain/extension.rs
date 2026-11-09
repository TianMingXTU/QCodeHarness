//! 扩展声明：身份、工具权限与可展示的启用状态。
//!
//! 权限以实际工具名精确匹配；风险注解不参与授权。

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

/// Skill 工具的保留命名空间。
pub const SKILL_TOOL_PREFIX: &str = "skill__";

/// 扩展作者声明的身份与权限上限，不代表用户已经授权。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionDescriptor {
    /// 安装键，同时用于生成 Skill 工具名。限小写 ASCII、数字、下划线与连字符。
    pub id: String,
    /// 安装与审批界面展示的用途说明。
    pub description: String,
    /// 允许申请的实际工具名；不支持通配符或工具风险类别。
    pub permissions: Vec<String>,
}

impl ExtensionDescriptor {
    /// 校验外部声明；已知工具集合由应用层另行检查。
    pub fn validate(&self) -> Result<(), String> {
        validate_extension_id(&self.id)?;
        if self.description.trim().is_empty() {
            return Err(format!("扩展 `{}` 缺少 description", self.id));
        }
        let mut seen = BTreeSet::new();
        for permission in &self.permissions {
            if permission.is_empty()
                || permission.len() > 128
                || !permission
                    .bytes()
                    .all(|ch| ch.is_ascii_alphanumeric() || ch == b'_' || ch == b'-')
                || permission.starts_with(SKILL_TOOL_PREFIX)
            {
                return Err(format!(
                    "扩展 `{}` 的权限 `{permission}` 必须是实际 backing 工具名",
                    self.id
                ));
            }
            if !seen.insert(permission) {
                return Err(format!("扩展 `{}` 重复声明权限 `{permission}`", self.id));
            }
        }
        Ok(())
    }

    /// 该 Skill 暴露给模型的工具名。
    pub fn skill_tool_name(&self) -> String {
        format!("{SKILL_TOOL_PREFIX}{}", self.id)
    }
}

/// 校验可安全用作文件名的扩展 ID，不清洗或改写用户输入。
pub fn validate_extension_id(id: &str) -> Result<(), String> {
    crate::domain::ids::validate_safe_identifier("扩展 ID", id, 56)
}

/// 扩展列表中的状态快照，不能用它决定未来调用的权限。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledExtension {
    /// 作者声明的身份与权限上限。
    pub descriptor: ExtensionDescriptor,
    /// 该条目是否注册了 Skill 侧（提示词与 `skill__<id>` 工具）。
    pub has_skill: bool,
    /// 该条目是否注册了 Hook 侧（事件规划动作）。
    pub has_hook: bool,
    /// 是否处于启用状态；禁用条目的动作一律拒绝。
    pub enabled: bool,
    /// 用户显式批准的声明子集；禁用时清空。
    pub approved_permissions: Vec<String>,
}

/// 单独持久化的授权状态，不混入作者提供的 manifest。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionState {
    /// 安装键，与 [`ExtensionDescriptor::id`] 对应。
    pub id: String,
    /// 是否启用；恢复时与批准子集一起还原。
    pub enabled: bool,
    /// 已批准的权限子集，必须是声明权限的子集。
    pub permissions: Vec<String>,
}

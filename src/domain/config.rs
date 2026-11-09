//! 已合并并校验的应用配置；文件格式与加载顺序由基础设施层处理。

use std::path::PathBuf;

/// 应用运行参数与扩展选择。启用扩展不授予工具权限。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AppConfig {
    pub llm: LlmSettings,
    pub run: RunConfig,
    pub policy_path: Option<PathBuf>,
    pub mcp_config_path: Option<PathBuf>,
    pub skills: SkillsConfig,
    pub hooks: HooksConfig,
    pub parallel: ParallelConfig,
}

/// 模型连接参数；未配置的值由启动入口继续解析。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LlmSettings {
    pub base_url: Option<String>,
    pub model: Option<String>,
}

/// 单次运行的预算与摘要触发条件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunConfig {
    pub max_turns: u32,
    /// 未配置时不设置墙钟预算。
    pub max_wall_clock_seconds: Option<u64>,
    /// 未配置时使用运行器的默认摘要阈值。
    pub summarize_min_dropped: Option<usize>,
}

impl Default for RunConfig {
    fn default() -> Self {
        Self {
            max_turns: 20,
            max_wall_clock_seconds: None,
            summarize_min_dropped: None,
        }
    }
}

/// 启用的 Skill 标识符，不包含实现或权限声明。
///
/// `None` 表示配置里没写这一项，装配层不额外限制选择范围；`Some(空)` 是
/// 显式清空，表示一个都不选。两者语义不同，不能用空 `Vec` 同时表达。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SkillsConfig {
    pub enabled: Option<Vec<String>>,
}

/// 启用的 Hook 标识符，不包含实现或权限声明；`None` 与 `Some(空)` 的区别
/// 同 [`SkillsConfig`]。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HooksConfig {
    pub enabled: Option<Vec<String>>,
}

/// 并行子任务预算与合并前验证。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParallelConfig {
    /// 允许 0..=16；0 禁用并行，1 仍经调度器串行执行。
    pub max_subtasks: usize,
    /// 在合并候选工作区执行的验证命令；未配置时只做结构校验。
    pub verify_command: Option<String>,
}

impl Default for ParallelConfig {
    fn default() -> Self {
        Self {
            max_subtasks: 4,
            verify_command: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_leave_optional_inputs_unset_and_extensions_disabled() {
        let config = AppConfig::default();

        assert_eq!(config.llm, LlmSettings::default());
        assert_eq!(config.run.max_turns, 20);
        assert_eq!(config.run.max_wall_clock_seconds, None);
        assert_eq!(config.run.summarize_min_dropped, None);
        assert_eq!(config.policy_path, None);
        assert_eq!(config.mcp_config_path, None);
        assert_eq!(config.skills.enabled, None);
        assert_eq!(config.hooks.enabled, None);
        assert_eq!(config.parallel.max_subtasks, 4);
        assert_eq!(config.parallel.verify_command, None);
    }
}

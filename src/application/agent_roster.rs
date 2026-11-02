//! 角色花名册：按角色查档案、按档案裁剪工具集。
//!
//! 编排器按步骤的角色来这里取档案（提示词 + 工具白名单），恢复入口
//! 用同一条裁剪路径——工具子集是硬边界，任何把注册表交到模型面前的
//! 路径都必须过这道滤网，不能只在编排一条路上生效。

use crate::application::tool_registry::{ToolEntry, ToolRegistry};
use crate::domain::agent::AgentRole;
use crate::domain::agent_profile::AgentProfile;

/// 角色档案的注册表：按角色查档案。
#[derive(Debug, Clone)]
pub struct AgentRoster {
    profiles: Vec<AgentProfile>,
}

impl AgentRoster {
    /// 内置花名册：四个角色各一份档案。
    pub fn builtin() -> Self {
        Self {
            profiles: AgentProfile::builtin(),
        }
    }

    /// 用给定档案集构造（测试与将来的自定义配置用）。
    pub fn new(profiles: Vec<AgentProfile>) -> Self {
        Self { profiles }
    }

    /// 按角色查档案。查不到是装配错误：编排器必须显式报错，
    /// 不能静默退回某个默认角色——静默退回等于让模型拿着
    /// 错误的提示词与工具集上场。
    pub fn find(&self, role: AgentRole) -> Option<&AgentProfile> {
        self.profiles.iter().find(|profile| profile.role == role)
    }

    /// 档案允许的工具名。`None` 表示不限（用全部已注册工具）。
    ///
    /// 角色没有档案时也返回 `None`，但调用方应先经 [`Self::find`]
    /// 排除这种情况——`None` 的语义是"不限"，不是"没有"。
    pub fn allowed_tools(&self, role: AgentRole) -> Option<&[String]> {
        self.find(role)
            .and_then(|profile| profile.tools.as_deref())
    }

    /// 把整张注册表裁剪成该角色可见的工具集（白名单的硬边界）。
    ///
    /// 顺序跟随档案配置，重复项只装一次。档案配了但注册表没有的
    /// 工具名打警告后忽略——档案是配置，注册表是实现，配置超前于实现
    /// （比如某台机器没装某个 MCP Server）不该让整个任务崩，但也不该
    /// 静默到没人发现装配缩水了。
    ///
    /// 角色没有档案时返回全量注册表（`None` = 不限的语义）；
    /// 常规编排路径在调用前已用 [`Self::find`]-把缺档案变成显式错误。
    pub fn filter_registry(&self, role: AgentRole, registry: &ToolRegistry) -> ToolRegistry {
        let Some(allowed) = self.allowed_tools(role) else {
            return registry.clone();
        };
        let mut entries: Vec<ToolEntry> = Vec::with_capacity(allowed.len());
        for name in allowed {
            if entries.iter().any(|entry| entry.spec.name == *name) {
                continue;
            }
            match registry.find(name) {
                Some(entry) => entries.push(entry.clone()),
                None => eprintln!(
                    "警告：角色 {} 的档案配置了未注册的工具 `{name}`，已忽略",
                    role.name()
                ),
            }
        }
        ToolRegistry::new(entries)
    }

    /// 给每个角色的提示词追加项目指令。
    ///
    /// 注入点在构造花名册时，不在 runner 里：快照存的是 `AgentSpec`，
    /// 恢复时直接用快照里的提示词。若等到 runner 才拼，中断后恢复的
    /// 会话就丢了项目约定——而恢复往往正好发生在任务跑到一半、
    /// 最需要项目约定的时候。
    ///
    /// 只动提示词，不动工具白名单：项目指令改变模型"想做什么"，
    /// 不改变它"能做什么"。
    pub fn with_project_instructions(mut self, instructions: &str) -> Self {
        for profile in &mut self.profiles {
            profile.system_prompt = format!(
                "{}\n\n{}\n{instructions}",
                profile.system_prompt, PROJECT_INSTRUCTIONS_HEADER
            );
        }
        self
    }
}

/// 项目指令段的标题。
///
/// 必须写清冲突时听谁的：项目约定（"提交信息用中文"、"别动生成代码"）
/// 和内置规则冲突时，模型需要明确的优先级，否则它会两边各猜一半。
const PROJECT_INSTRUCTIONS_HEADER: &str =
    "──── 项目指令（项目自己的约定，与上述通用规则冲突时以项目为准）────";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_roster_covers_all_four_roles() {
        let roster = AgentRoster::builtin();
        for role in [
            AgentRole::Planner,
            AgentRole::Coder,
            AgentRole::Tester,
            AgentRole::Reviewer,
        ] {
            let profile = roster
                .find(role)
                .unwrap_or_else(|| panic!("{role:?} 应有档案"));
            assert_eq!(profile.role, role);
        }
    }

    #[test]
    fn allowed_tools_reflects_the_profile_whitelist() {
        let roster = AgentRoster::builtin();
        // 评审者是白名单角色：名单里没有任何写工具。
        let reviewer_tools = roster.allowed_tools(AgentRole::Reviewer).unwrap();
        assert!(!reviewer_tools.iter().any(|name| name == "write_file"));
        assert!(!reviewer_tools.iter().any(|name| name == "edit_file"));
        assert!(!reviewer_tools.iter().any(|name| name == "run_command"));
        // 编码者不限工具。
        assert_eq!(roster.allowed_tools(AgentRole::Coder), None);
    }

    #[test]
    fn missing_profile_is_none_not_a_default() {
        // 空花名册：查任何角色都是 None，编排器据此显式报错。
        let roster = AgentRoster::new(vec![]);
        assert!(roster.find(AgentRole::Coder).is_none());
    }

    // ── P8-4 项目指令注入 ─────────────────────────────────────────────

    /// 四个角色的提示词都要带上项目指令。
    ///
    /// 漏一个角色就意味着那个角色按通用习惯干活——而编排器按步骤
    /// 分派角色，漏掉的那个往往正是碰代码最多的那一步。
    #[test]
    fn project_instructions_reach_every_role() {
        let roster = AgentRoster::builtin().with_project_instructions("提交信息用中文");

        for role in [
            AgentRole::Planner,
            AgentRole::Coder,
            AgentRole::Tester,
            AgentRole::Reviewer,
        ] {
            let prompt = &roster.find(role).expect("应有档案").system_prompt;
            assert!(
                prompt.contains("提交信息用中文"),
                "{role:?} 的提示词应含项目指令"
            );
            assert!(
                prompt.contains(PROJECT_INSTRUCTIONS_HEADER),
                "{role:?} 的提示词应有项目指令分隔标题"
            );
        }
    }

    /// 项目指令接在内置提示词之后，不是替换它。
    ///
    /// ReAct 工作方式与"不许越界"这些通用约束是底线，项目约定只该
    /// 叠加不该覆盖——覆盖了就等于让一份 Markdown 关掉全部安全提示。
    #[test]
    fn project_instructions_are_appended_after_the_builtin_prompt() {
        let base = AgentRoster::builtin()
            .find(AgentRole::Coder)
            .expect("应有档案")
            .system_prompt
            .clone();
        let with = AgentRoster::builtin()
            .with_project_instructions("别动生成代码")
            .find(AgentRole::Coder)
            .expect("应有档案")
            .system_prompt
            .clone();

        assert!(with.starts_with(&base), "内置提示词必须原样保留在前");
        assert!(with.ends_with("别动生成代码"), "项目指令应在末尾");
        assert!(with.contains("ReAct"), "内置约束不能被顶掉");
    }

    /// 注入只改提示词，不动工具白名单。
    ///
    /// 项目指令改变模型"想做什么"，不改变它"能做什么"——后者是
    /// 硬边界，不能因为一份 Markdown 就松。
    #[test]
    fn project_instructions_do_not_change_tool_whitelists() {
        let before = AgentRoster::builtin();
        let after = before.clone().with_project_instructions("随便写点什么");
        for role in [AgentRole::Planner, AgentRole::Coder, AgentRole::Tester, AgentRole::Reviewer] {
            assert_eq!(after.allowed_tools(role), before.allowed_tools(role));
        }
    }

    #[test]
    fn filter_registry_cuts_the_full_registry_down_to_the_whitelist() {
        // 恢复路径复用这条裁剪：白名单角色拿到的注册表必须真的没有
        // 写工具，而不是只在编排路径上过滤。
        let registry = crate::application::tool_registry::ToolRegistry::new(
            crate::infrastructure::tool::all_tools(crate::domain::workspace::WorkspaceRoot::new(
                std::env::temp_dir(),
            )),
        );
        let roster = AgentRoster::builtin();

        let reviewer_view = roster.filter_registry(AgentRole::Reviewer, &registry);
        let specs = reviewer_view.specs();
        let names: Vec<&str> = specs.iter().map(|spec| spec.name.as_str()).collect();
        assert_eq!(names.len(), 5);
        assert!(names.iter().all(|name| {
            !matches!(*name, "write_file" | "edit_file" | "run_command")
        }));

        // 不限工具的角色拿全量。
        assert_eq!(
            roster.filter_registry(AgentRole::Coder, &registry).specs().len(),
            registry.specs().len()
        );
    }
}

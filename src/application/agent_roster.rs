//! 角色花名册：按角色查档案、按档案给出工具白名单。
//!
//! 编排器按步骤的角色来这里取档案（提示词 + 工具白名单），恢复入口
//! 用同一份档案——角色边界是硬边界，任何把工具交到模型面前或手里的
//! 路径都必须认这道名单，不能只在编排一条路上生效。
//!
//! 白名单的执行点在"调用时校验"，而不是"装配时过滤注册表"：模型看到
//! 的工具定义对所有步骤统一（否则每一步的工具数组都不同，前缀缓存一步
//! 都命不中），而 runner 在每条执行入口上按这份名单裁决。边界没有消失，
//! 只是从"看不见"变成"调不动"。

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
    ///
    /// 这是角色边界的唯一真相来源：runner 拿着它在调用层
    /// 校验每一次工具调用（见 `application::agent_run`）。
    pub fn allowed_tools(&self, role: AgentRole) -> Option<&[String]> {
        self.find(role)
            .and_then(|profile| profile.tools.as_deref())
    }

    /// 给每个角色配上自己的模型名与档位。
    ///
    /// **模型与档位只影响"用哪个模型"，不影响"能干什么"。** 工具白名单
    /// 是硬边界，不在这里动——给 Tester 配一个更强的模型不等于让它能改
    /// 文件。
    ///
    /// 没配到的角色保持 `None`（用运行时的默认模型与 provider 默认档位），
    /// 行为与未引入角色模型配置时完全一致。
    ///
    /// 装配期就把配置写进档案：编排器按角色取档案时一并取到模型，
    /// 不用在运行路径上再读一次环境变量（那会让 domain 与 IO 搅在一起）。
    pub fn with_role_models(
        mut self,
        models: &std::collections::HashMap<AgentRole, String>,
        efforts: &std::collections::HashMap<AgentRole, crate::domain::agent::Effort>,
        verbosities: &std::collections::HashMap<AgentRole, crate::domain::agent::Verbosity>,
    ) -> Self {
        for profile in &mut self.profiles {
            profile.model = models.get(&profile.role).cloned();
            profile.effort = efforts.get(&profile.role).copied();
            profile.verbosity = verbosities.get(&profile.role).copied();
        }
        self
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
    use crate::domain::agent::{Effort, Verbosity};
    use std::collections::HashMap;

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

    /// 白名单里的每个名字都必须是真实注册的工具。
    ///
    /// 白名单由 runner 在调用层按名字校验，拼错的名字不会在装配时打一条
    /// 警告——它会静默地让该角色永远调不到那个工具。所以这条断言从
    /// "白名单数据"一侧守住边界：档案写错了要让测试红，而不是让用户
    /// 以为评审者"本来就不能用 search_text"。
    #[test]
    fn every_whitelisted_tool_name_is_a_real_tool() {
        let registry = crate::application::tool_registry::ToolRegistry::new(
            crate::infrastructure::tool::all_tools(crate::domain::workspace::WorkspaceRoot::new(
                std::env::temp_dir(),
            )),
        );
        let registered: Vec<String> = registry
            .specs()
            .into_iter()
            .map(|spec| spec.name)
            .collect();
        let roster = AgentRoster::builtin();

        for role in [
            AgentRole::Planner,
            AgentRole::Coder,
            AgentRole::Tester,
            AgentRole::Reviewer,
        ] {
            let Some(allowed) = roster.allowed_tools(role) else {
                continue;
            };
            for name in allowed {
                assert!(
                    registered.iter().any(|real| real == name),
                    "{role:?} 的白名单配了未注册的工具 `{name}`；已注册: {registered:?}"
                );
            }
        }
    }

    #[test]
    fn missing_profile_is_none_not_a_default() {
        // 空花名册：查任何角色都是 None，编排器据此显式报错。
        let roster = AgentRoster::new(vec![]);
        assert!(roster.find(AgentRole::Coder).is_none());
    }

    // ── 项目指令注入 ───────────────────────────────────────────────────

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

    // ── 按角色分配模型与档位 ───────────────────────────────────────────

    /// 四个角色各自拿到自己的模型与档位。
    ///
    /// 配置的用途就是"这一步该用哪个模型"，所以每个角色都要能独立配。
    /// 漏配一个角色会让它在四步任务里静默地用着默认模型——而默认模型
    /// 往往是那个最贵的。
    #[test]
    fn each_role_gets_its_own_model_and_tiers() {
        let models = HashMap::from([
            (AgentRole::Planner, "planner-model".to_owned()),
            (AgentRole::Coder, "coder-model".to_owned()),
            (AgentRole::Tester, "tester-model".to_owned()),
            (AgentRole::Reviewer, "reviewer-model".to_owned()),
        ]);
        let efforts = HashMap::from([
            (AgentRole::Planner, Effort::High),
            (AgentRole::Coder, Effort::High),
            (AgentRole::Tester, Effort::Low),
            (AgentRole::Reviewer, Effort::Medium),
        ]);
        let verbosities = HashMap::from([
            (AgentRole::Planner, Verbosity::Medium),
            (AgentRole::Coder, Verbosity::Low),
            (AgentRole::Tester, Verbosity::Low),
            (AgentRole::Reviewer, Verbosity::High),
        ]);

        let roster =
            AgentRoster::builtin().with_role_models(&models, &efforts, &verbosities);

        assert_eq!(
            roster.find(AgentRole::Planner).unwrap().model.as_deref(),
            Some("planner-model")
        );
        assert_eq!(
            roster.find(AgentRole::Coder).unwrap().model.as_deref(),
            Some("coder-model")
        );
        assert_eq!(
            roster.find(AgentRole::Tester).unwrap().model.as_deref(),
            Some("tester-model")
        );
        assert_eq!(
            roster.find(AgentRole::Reviewer).unwrap().model.as_deref(),
            Some("reviewer-model")
        );

        assert_eq!(
            roster.find(AgentRole::Tester).unwrap().effort,
            Some(Effort::Low),
            "测试者用低推理档：它跑命令读输出，不需要强推理"
        );
        assert_eq!(
            roster.find(AgentRole::Coder).unwrap().verbosity,
            Some(Verbosity::Low)
        );
    }

    /// 没配的角色保持 `None`：用运行时的默认模型与 provider 默认档位。
    ///
    /// 这是兼容底线——没配角色模型时，请求与未引入这套配置时完全一致
    /// （`request.model` 为 `None`，请求里连 `model` 字段都不出现）。
    #[test]
    fn roles_without_configuration_keep_the_defaults() {
        let models = HashMap::from([(AgentRole::Coder, "coder-model".to_owned())]);

        let roster = AgentRoster::builtin().with_role_models(&models, &HashMap::new(), &HashMap::new());

        assert_eq!(
            roster.find(AgentRole::Coder).unwrap().model.as_deref(),
            Some("coder-model")
        );
        for role in [AgentRole::Planner, AgentRole::Tester, AgentRole::Reviewer] {
            let profile = roster.find(role).unwrap();
            assert_eq!(profile.model, None, "{role:?} 未配置应保持 None");
            assert_eq!(profile.effort, None);
            assert_eq!(profile.verbosity, None);
        }
    }

    /// 配模型与档位不改变任何角色的工具白名单。
    ///
    /// **模型与档位只影响"用哪个模型"，不影响"能干什么"。** 给测试者
    /// 配一个更强的模型，不等于让它能改文件——白名单是硬边界，与模型
    /// 能力无关。
    #[test]
    fn role_models_do_not_change_tool_whitelists() {
        let before = AgentRoster::builtin();
        let after = before.clone().with_role_models(
            &HashMap::from([(AgentRole::Tester, "stronger-model".to_owned())]),
            &HashMap::from([(AgentRole::Tester, Effort::High)]),
            &HashMap::from([(AgentRole::Tester, Verbosity::High)]),
        );

        for role in [AgentRole::Planner, AgentRole::Coder, AgentRole::Tester, AgentRole::Reviewer] {
            assert_eq!(
                after.allowed_tools(role),
                before.allowed_tools(role),
                "{role:?} 的白名单不应因换模型而变"
            );
        }
    }
}

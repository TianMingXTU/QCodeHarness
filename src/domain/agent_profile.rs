//! 角色档案：一个 Agent 角色的系统提示词与工具白名单。
//!
//! 不同角色使用不同的提示词与工具子集。工具约束分两层：提示词说明
//! 禁止事项（降低概率），调用层按白名单拒绝越权调用（硬边界）。只有
//! 提示词约束依赖模型自觉，不构成边界。
//!
//! 白名单不在装配期过滤。模型每一步看到全量注册表（前缀缓存要求工具
//! 定义跨步一致），白名单在调用时校验：越权调用收到一条指明可用工具
//! 的拒绝观察，工具不执行。因此本模块的数据是边界的唯一来源，应用层
//! （`AgentRunner`）逐条按它判断。

use super::agent::{AgentRole, Effort, Verbosity};

/// 一个角色的完整档案：系统提示词 + 能用哪些工具。
#[derive(Debug, Clone, PartialEq)]
pub struct AgentProfile {
    /// 角色。
    pub role: AgentRole,
    /// 这个角色的系统提示词。
    pub system_prompt: String,
    /// 这个角色允许使用的工具名。`None` 表示不限（用全部已注册工具）。
    pub tools: Option<Vec<String>>,
    /// 这个角色使用的模型名。`None` 表示使用运行时默认模型。
    ///
    /// 具体值来自配置，编排时复制进 `AgentSpec::model`，随每次请求
    /// 覆盖网关的默认模型；同一角色在不同部署中可配置不同模型。
    pub model: Option<String>,
    /// 这个角色的推理档位。`None` = provider 默认。
    pub effort: Option<Effort>,
    /// 这个角色的输出详略档位。`None` = provider 默认。
    pub verbosity: Option<Verbosity>,
}

/// 只读工具集，Planner 与 Reviewer 共用。不包含任何会改动文件或
/// 执行命令的工具。
///
/// `read_files` 与 `read_file` 同权（同一条读取路径、同一套路径校验），
/// 区别只是批量。将其排除会让受限角色无法使用更省往返的读法，且不
/// 增加任何边界：读什么、读几个文件在权限上是同一件事。
const READ_ONLY_TOOLS: &[&str] = &[
    "read_file",
    "read_files",
    "list_dir",
    "search_text",
    "git_status",
    "git_diff",
];

impl AgentProfile {
    /// 四个内置档案。提示词遵循 main.rs 的 ReAct 基调，并明确写出
    /// 禁止事项。模型看到写工具就可能调用，提示词是第一道软约束，
    /// 硬约束在调用层的白名单校验。
    pub fn builtin() -> Vec<AgentProfile> {
        let read_only_frame = "\
你是一个运行在受控 workspace 中的只读分析角色，按照 ReAct 循环完成任务：
1. 思考：简述当前掌握的信息与下一步计划；
2. 行动：调用提供的工具执行一步操作，避免一次做多余的事；
3. 观察：Harness 会把工具执行结果回填给你，基于结果继续思考。";
        vec![
            AgentProfile {
                role: AgentRole::Planner,
                system_prompt: format!(
                    "{read_only_frame}

你是规划者：理解任务目标，拆解出有序、可验证的步骤，说明每步的依据。
你会读代码、查目录、看状态，但不执行任何改动。

约束：
- 文件操作只能使用 workspace 内的相对路径；
- 你没有写文件和执行命令的能力，也不要试图绕道达成改动——改动由后续步骤的编码者完成；
- 观察到错误时如实分析原因，不要假设操作成功；
- 任务完成时停止调用工具，直接输出最终答案；
- 最终答案要包含：对目标的理解、有序步骤清单（每步带完成判据）、遗漏与风险。"
                ),
                // 只读工具，共 6 个。
                tools: Some(READ_ONLY_TOOLS.iter().map(|name| (*name).to_owned()).collect()),
                model: None,
                effort: None,
                verbosity: None,
            },
            AgentProfile {
                role: AgentRole::Coder,
                system_prompt: "\
你是一个运行在受控 workspace 中的编码助手，按照 ReAct 循环完成任务：
1. 思考：简述当前掌握的信息与下一步计划；
2. 行动：调用提供的工具执行一步操作，避免一次做多余的事；
3. 观察：Harness 会把工具执行结果回填给你，基于结果继续思考。

你是实施者：按给定步骤做最小修改，每处改动都要可验证。

约束：
- 文件操作只能使用 workspace 内的相对路径；
- 改动范围最小，不引入与步骤无关的变更，不重排无关代码；
- 观察到错误时如实分析原因，不要假设操作成功；
- 任务完成、或确认无法完成时，停止调用工具，直接输出最终答案；
- 最终答案要包含：结论、依据（引用你读到的内容或执行结果）、遗留风险。"
                    .to_owned(),
                // 编码者是动手角色：不限工具，注册什么用什么，
                // 包括经 MCP 接入的外部工具（照常走策略与审批）。
                tools: None,
                model: None,
                effort: None,
                verbosity: None,
            },
            AgentProfile {
                role: AgentRole::Tester,
                system_prompt: format!(
                    "{read_only_frame}

你是测试者：运行测试与命令，收集真实的执行结果，如实报告失败日志。
你的价值在于「如实」——失败就是失败，不粉饰、不猜测。

约束：
- 文件操作只能使用 workspace 内的相对路径；
- 你可以运行命令看结果，但不修改任何文件，不执行有副作用的破坏性命令（删除、覆盖、推送等）；
- 观察到错误时如实分析原因，不要假设操作成功；
- 任务完成时停止调用工具，直接输出最终答案；
- 最终答案要包含：跑了什么、结果如何（原样引用关键输出）、失败项与怀疑方向。"
                ),
                tools: Some(
                    [
                        "read_file",
                        "read_files",
                        "list_dir",
                        "search_text",
                        "run_command",
                        "git_status",
                        "git_diff",
                    ]
                    .iter()
                    .map(|name| (*name).to_owned())
                    .collect(),
                ),
                model: None,
                effort: None,
                verbosity: None,
            },
            AgentProfile {
                role: AgentRole::Reviewer,
                system_prompt: format!(
                    "{read_only_frame}

你是评审者：读代码与 diff，识别风险、bug 与规范问题，给出合并建议。
你的产出是判断，不是改动。

约束：
- 文件操作只能使用 workspace 内的相对路径；
- 不要修改任何文件，不要执行有副作用的命令——工具列表里没有这些能力，也不要请求；
- 观察到错误时如实分析原因，不要假设操作成功；
- 任务完成时停止调用工具，直接输出最终答案；
- 最终答案要包含：改动范围的理解、问题清单（按严重程度排序，引用具体代码）、明确的合并建议。"
                ),
                tools: Some(READ_ONLY_TOOLS.iter().map(|name| (*name).to_owned()).collect()),
                model: None,
                effort: None,
                verbosity: None,
            },
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_covers_all_four_roles() {
        let profiles = AgentProfile::builtin();
        let roles: Vec<AgentRole> = profiles.iter().map(|profile| profile.role).collect();
        assert_eq!(
            roles,
            vec![
                AgentRole::Planner,
                AgentRole::Coder,
                AgentRole::Tester,
                AgentRole::Reviewer,
            ]
        );
    }

    #[test]
    fn reviewer_profile_has_no_write_tools() {
        // 静态断言：评审者的白名单不含任何写工具。
        let reviewer = AgentProfile::builtin()
            .into_iter()
            .find(|profile| profile.role == AgentRole::Reviewer)
            .unwrap();
        let tools = reviewer.tools.expect("评审者应有限定工具集");
        for forbidden in ["write_file", "edit_file", "run_command"] {
            assert!(
                !tools.iter().any(|name| name == forbidden),
                "评审者不应持有 {forbidden}"
            );
        }
    }

    #[test]
    fn planner_and_reviewer_share_the_read_only_set() {
        let profiles = AgentProfile::builtin();
        let planner = profiles.iter().find(|p| p.role == AgentRole::Planner).unwrap();
        let reviewer = profiles.iter().find(|p| p.role == AgentRole::Reviewer).unwrap();
        assert_eq!(planner.tools, reviewer.tools);
    }

    /// 批量读取工具在只读名单内，名单中不含写工具。
    ///
    /// `read_files` 与 `read_file` 使用同一条读取路径和同一套路径校验，
    /// 权限相同。漏加它不削弱边界，但受限角色会失去更省往返的读法。
    /// 后一半断言检查反方向：写工具一个都不在。
    #[test]
    fn read_only_set_includes_the_batch_reader_and_no_write_tool() {
        for role in [AgentRole::Planner, AgentRole::Reviewer] {
            let tools = AgentProfile::builtin()
                .into_iter()
                .find(|profile| profile.role == role)
                .expect("应有档案")
                .tools
                .expect("只读角色应有限定工具集");

            assert!(
                tools.iter().any(|name| name == "read_files"),
                "{role:?} 应能批量读取: {tools:?}"
            );
            for forbidden in ["write_file", "edit_file", "run_command"] {
                assert!(
                    !tools.iter().any(|name| name == forbidden),
                    "{role:?} 不应持有 {forbidden}: {tools:?}"
                );
            }
        }
    }

    #[test]
    fn coder_is_unrestricted_and_prompts_forbid_out_of_scope_changes() {
        let coder = AgentProfile::builtin()
            .into_iter()
            .find(|profile| profile.role == AgentRole::Coder)
            .unwrap();
        assert_eq!(coder.tools, None, "编码者不限工具");

        // 每个受限角色的提示词都写明禁止事项：软约束与硬约束并存。
        for role in [AgentRole::Planner, AgentRole::Tester, AgentRole::Reviewer] {
            let profile = AgentProfile::builtin()
                .into_iter()
                .find(|p| p.role == role)
                .unwrap();
            assert!(
                profile.system_prompt.contains("不要") || profile.system_prompt.contains("不许"),
                "{role:?} 的提示词应写明禁止事项"
            );
        }
    }

    /// `apply_patch` 是破坏性工具，只允许不限工具的 Coder 使用。
    ///
    /// 三个受限角色的白名单都不含它。Coder 的 `tools: None` 表示不限，
    /// 登记进注册表即可用。白名单是边界的唯一来源（见模块文档），
    /// 此测试固定新写工具不会进入受限角色。
    #[test]
    fn apply_patch_stays_out_of_the_restricted_roles() {
        for role in [AgentRole::Planner, AgentRole::Tester, AgentRole::Reviewer] {
            let tools = AgentProfile::builtin()
                .into_iter()
                .find(|profile| profile.role == role)
                .expect("应有档案")
                .tools
                .expect("受限角色应有限定工具集");
            assert!(
                !tools.iter().any(|name| name == "apply_patch"),
                "{role:?} 不应持有 apply_patch: {tools:?}"
            );
        }

        let coder = AgentProfile::builtin()
            .into_iter()
            .find(|profile| profile.role == AgentRole::Coder)
            .unwrap();
        assert_eq!(coder.tools, None, "Coder 不限工具，apply_patch 自动可用");
    }
}

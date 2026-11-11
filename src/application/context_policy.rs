//! 上下文策略：决定发给模型的历史视图。
//!
//! 循环只负责发送视图，视图的生成归这个扩展点。默认策略是现有的预算
//! 裁剪；自定义策略（按相关性挑选、保留固定前缀等）实现
//! [`ContextPolicy`] 即可，循环不用改。
//!
//! 裁剪报告不属于 trait：策略只交视图，"裁了几轮、省了多少 token"由
//! [`CompactionReport::measure`](crate::domain::context::CompactionReport::measure)
//! 按实际视图与完整历史现测。自定义策略下，报告仍描述真正发出的内容，
//! 不依赖具体算法。是否落审计事件由循环按"有没有整轮被裁"决定，沿用
//! 既有口径。

use crate::domain::context::{ContextBudget, Summary, compact_slices};
use crate::domain::session::{ContextSlice, Session};

/// 模型可见历史的生成策略。
///
/// 实现只读会话，不改会话：返回的片段借用 `session`，新增文本（摘要、
/// 占位说明、台账）时自己持有 `Cow::Owned`。
///
/// `view` 在一轮内可能被调用两次（先探测哪些轮次会被裁掉，补摘要后再
/// 生成正式视图）。实现必须是纯函数，不依赖调用次数，不产生副作用。
pub trait ContextPolicy: Send + Sync {
    /// 生成本次要发给模型的历史视图。
    ///
    /// `summary` 是调用方刚生成的摘要，可能为 `None`。显式传入是为了让
    /// 尚未写回会话的摘要也能立刻生效。
    fn view<'a>(
        &self,
        session: &'a Session,
        budget: &ContextBudget,
        summary: Option<&Summary>,
    ) -> Vec<ContextSlice<'a>>;
}

/// 默认策略：先去除框架步骤交代里的重复目标，再按 token 预算裁剪。
/// 可容纳的既有摘要优先于占位说明，原始会话不变。
pub struct DefaultContextPolicy;

impl ContextPolicy for DefaultContextPolicy {
    fn view<'a>(
        &self,
        session: &'a Session,
        budget: &ContextBudget,
        summary: Option<&Summary>,
    ) -> Vec<ContextSlice<'a>> {
        compact_slices(
            session,
            crate::application::step_prompt::model_view(session),
            budget,
            summary,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::agent::AgentTurn;
    use crate::domain::context::{CompactionReport, compact};
    use crate::domain::ids::{CallId, SessionId};
    use crate::domain::tool::{ToolCall, ToolOutcome};
    use serde_json::json;

    /// 建一个带 `n` 轮历史的会话。
    fn session_with_rounds(n: u32) -> Session {
        let mut session = Session::new(SessionId::new("s1"), "任务");
        for index in 1..=n {
            session
                .commit_round(
                    AgentTurn {
                        text: format!("思考{index}"),
                        calls: vec![ToolCall {
                            id: CallId::new(format!("call_{index}")),
                            name: "read_file".to_owned(),
                            arguments: json!({"path": format!("r{index}.txt")}),
                        }],
                    },
                    vec![ToolOutcome::success(format!("观察{index}"))],
                )
                .unwrap();
        }
        session
    }

    fn tight_budget() -> ContextBudget {
        ContextBudget {
            max_tokens: Some(1),
            min_recent_rounds: 0,
            keep_task: true,
            summarize: None,
        }
    }

    /// 没有框架步骤交代时，默认策略与直接裁剪逐片相同，报告也一致。
    #[test]
    fn the_default_policy_matches_direct_compaction() {
        let session = session_with_rounds(6);
        let budget = tight_budget();

        let view = DefaultContextPolicy.view(&session, &budget, session.summary());
        let (expected, report) = compact(&session, &budget);

        assert_eq!(view, expected, "普通会话沿用相同的裁剪规则");
        assert_eq!(CompactionReport::measure(&session, &view), report);
    }

    #[test]
    fn long_workflow_steps_share_one_goal_in_the_model_view() {
        use crate::application::step_prompt::step_prompt;
        use crate::domain::task::TaskKind;
        use crate::domain::workflow::WorkflowCatalog;

        let goal = "保留公共接口并修复输入校验。".repeat(400);
        let catalog = WorkflowCatalog::builtin();
        for kind in [
            TaskKind::BugFix,
            TaskKind::Feature,
            TaskKind::Review,
            TaskKind::Refactor,
        ] {
            let steps = catalog.find(kind).unwrap().expand(&goal);
            let mut session = Session::new(
                SessionId::new("long-task"),
                step_prompt(&goal, &steps[0], 0, steps.len()),
            );
            for index in 0..steps.len() {
                if index > 0 {
                    session.push_user(step_prompt(&goal, &steps[index], index, steps.len()));
                }
                for _ in 0..3 {
                    session
                        .commit_round(
                            AgentTurn {
                                text: "分析记录".repeat(300),
                                calls: Vec::new(),
                            },
                            Vec::new(),
                        )
                        .unwrap();
                }
            }
            let original = session.snapshot();
            let view = DefaultContextPolicy.view(&session, &ContextBudget::unlimited(), None);
            let instructions = view
                .iter()
                .filter_map(|slice| match slice {
                    ContextSlice::User(text) => Some(text.as_ref()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            assert_eq!(
                instructions.matches(&goal).count(),
                1,
                "{kind:?} 应只交代一次目标"
            );
            for (index, step) in steps.iter().enumerate() {
                assert!(instructions.contains(&format!("第 {}/{} 步", index + 1, steps.len())));
                assert!(instructions.contains(&step.done_when));
            }

            let budget = ContextBudget::default();
            let view = DefaultContextPolicy.view(&session, &budget, None);
            let report = CompactionReport::measure(&session, &view);
            assert!(
                report.tokens_after <= budget.max_tokens.unwrap(),
                "{kind:?}: {report:?}"
            );
            assert!(
                report.rounds_after > budget.min_recent_rounds,
                "长目标不能把历史挤到轮次下限"
            );
            assert_eq!(session.snapshot(), original, "视图不得改写完整历史");
        }
    }

    #[test]
    fn goal_projection_preserves_handoffs_and_independent_user_constraints() {
        use crate::application::step_prompt::step_prompt;
        use crate::domain::task::{StepOutput, TaskKind};
        use crate::domain::workflow::WorkflowCatalog;

        let goal = "修复多行输入\n保留中文和空行。".repeat(100);
        let steps = WorkflowCatalog::builtin()
            .find(TaskKind::BugFix)
            .unwrap()
            .expand(&goal);
        let mut session = Session::new(
            SessionId::new("constraints"),
            step_prompt(&goal, &steps[0], 0, steps.len()),
        );
        let handoff = StepOutput {
            step: 0,
            role: steps[0].role,
            answer: "根因在解析器，新增约束见用户输入。".to_owned(),
        }
        .handoff_message();
        session.push_user(&handoff);
        session.push_user(step_prompt(&goal, &steps[1], 1, steps.len()));
        let constraint = format!("新增约束：只能改解析器。\n任务：{goal}");
        session.push_input("input-1".to_owned(), constraint.clone());
        let legacy_constraint = "旧调用方追加的用户约束：禁止修改公共接口。";
        session.push_user(legacy_constraint);
        let original = session.snapshot();
        let view = DefaultContextPolicy.view(&session, &ContextBudget::default(), None);
        let users: Vec<&str> = view
            .iter()
            .filter_map(|slice| match slice {
                ContextSlice::User(text) => Some(text.as_ref()),
                _ => None,
            })
            .collect();

        assert!(users.contains(&handoff.as_str()));
        assert!(
            users.contains(&constraint.as_str()),
            "独立输入即使重复目标也必须原样保留"
        );
        assert!(users.contains(&legacy_constraint));
        assert_eq!(
            users
                .iter()
                .map(|text| text.matches(&goal).count())
                .sum::<usize>(),
            2
        );
        assert_eq!(session.snapshot(), original);
    }

    #[test]
    fn short_goals_do_not_rewrite_step_constraints_or_identifiers() {
        use crate::application::step_prompt::step_prompt;
        use crate::domain::agent::AgentRole;
        use crate::domain::task::{TaskKind, TaskStep};
        use crate::domain::workflow::WorkflowCatalog;

        let builtin = WorkflowCatalog::builtin()
            .find(TaskKind::BugFix)
            .unwrap()
            .expand("根因");
        let custom = TaskStep {
            description: "修改 data_parser；字段 a 原样保留。".to_owned(),
            role: AgentRole::Coder,
            done_when: "data_parser 测试通过，字段 a 仍存在。".to_owned(),
        };
        for (goal, step, total) in [("根因", &builtin[0], builtin.len()), ("a", &custom, 1)] {
            let session = Session::new(
                SessionId::new("short-goal"),
                step_prompt(goal, step, 0, total),
            );
            let original = session.snapshot();
            let view = DefaultContextPolicy.view(&session, &ContextBudget::unlimited(), None);
            let text = view
                .iter()
                .filter_map(|slice| match slice {
                    ContextSlice::User(text) => Some(text.as_ref()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            assert!(
                text.contains(&step.done_when),
                "完成判据不得被目标替换改写: {text}"
            );
            if goal == "a" {
                assert!(
                    text.contains(&step.description),
                    "自定义步骤必须原样保留: {text}"
                );
            } else {
                assert!(
                    text.contains("找出根因"),
                    "模板中的独立要求不得被改写: {text}"
                );
            }
            assert_eq!(session.snapshot(), original);
        }
    }

    /// 自定义策略可以不裁剪：视图是完整历史，报告测出没有丢失。
    #[test]
    fn a_custom_policy_can_keep_the_whole_history() {
        struct KeepEverything;

        impl ContextPolicy for KeepEverything {
            fn view<'a>(
                &self,
                session: &'a Session,
                _budget: &ContextBudget,
                _summary: Option<&Summary>,
            ) -> Vec<ContextSlice<'a>> {
                session.slices()
            }
        }

        let session = session_with_rounds(3);
        let view = KeepEverything.view(&session, &tight_budget(), None);
        let report = CompactionReport::measure(&session, &view);

        assert_eq!(view.len(), session.slices().len());
        assert!(report.dropped.is_empty(), "全量视图不应报告任何裁剪");
        assert_eq!(report.dropped_tokens, 0);
    }
}

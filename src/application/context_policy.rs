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

use crate::domain::context::{compact_with_summary, ContextBudget, Summary};
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

/// 默认策略：按 token 预算裁剪，摘要优先于占位说明。
///
/// 行为与 [`compact_with_summary`] 完全一致，只是把既有逻辑移到了接口
/// 后面。
pub struct DefaultContextPolicy;

impl ContextPolicy for DefaultContextPolicy {
    fn view<'a>(
        &self,
        session: &'a Session,
        budget: &ContextBudget,
        summary: Option<&Summary>,
    ) -> Vec<ContextSlice<'a>> {
        compact_with_summary(session, budget, summary).0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::agent::AgentTurn;
    use crate::domain::context::{compact, CompactionReport};
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

    /// 默认策略交出的视图与直接裁剪逐片相同，报告也一致。
    #[test]
    fn the_default_policy_matches_direct_compaction() {
        let session = session_with_rounds(6);
        let budget = tight_budget();

        let view = DefaultContextPolicy.view(&session, &budget, session.summary());
        let (expected, report) = compact(&session, &budget);

        assert_eq!(view, expected, "默认策略只是既有裁剪的转发");
        assert_eq!(CompactionReport::measure(&session, &view), report);
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

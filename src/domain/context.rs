//! 上下文预算与历史裁剪。
//!
//! 长任务的历史会无限增长：每轮都把完整历史发给模型，token 成本线性上升，
//! 最终超出模型输入上限直接报错。裁剪是"发给模型时怎么看历史"的纯函数
//! 变换——[`Session`] 本身不被修改，证据、审计与将来的恢复都依赖完整历史。
//!
//! P3 只做确定性策略（保留近期 + 占位说明），不做 LLM 摘要：摘要是又一次
//! 模型调用，不可预测还烧 token，属于 P4/P5 的记忆能力。

use std::borrow::Cow;

use super::session::{ContextSlice, Session};

/// 发给模型前，历史最多保留多少个完整轮次。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextBudget {
    /// 最多保留多少轮完整历史，更早的轮次整体裁掉。
    pub max_rounds: usize,
    /// 是否始终保留最初的用户任务。强烈建议 true——模型忘了自己在干什么，
    /// 后面每一轮都会跑偏。
    pub keep_task: bool,
}

impl ContextBudget {
    /// 不裁剪的预算：轮数上限放到 `usize` 最大值。
    pub fn unlimited() -> Self {
        Self {
            max_rounds: usize::MAX,
            keep_task: true,
        }
    }
}

impl Default for ContextBudget {
    fn default() -> Self {
        Self {
            max_rounds: DEFAULT_MAX_ROUNDS,
            keep_task: true,
        }
    }
}

/// 默认保留的轮数。
///
/// `LLM_MAX_TURNS` 默认 20：16 意味着接近满轮数的任务也会在末段开始
/// 丢历史，而日常任务（<16 轮）完全不受影响。宁可保守地少丢。
pub const DEFAULT_MAX_ROUNDS: usize = 16;

/// 一次裁剪的结果，报告丢掉了什么，供证据与调试使用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactionReport {
    /// 裁剪前的完整轮数。
    pub rounds_before: usize,
    /// 裁剪后保留的轮数。
    pub rounds_after: usize,
    /// 被丢弃的轮次号。
    pub dropped: Vec<u32>,
}

/// 把会话历史裁剪到预算内，返回保留的片段与裁剪报告。
///
/// 纯函数：不读时钟、不做 IO、不改 `session`。裁剪以"轮"为最小单位——
/// 半轮历史（有调用没观察）会让 OpenAI 兼容端点直接 400，所以
/// `max_rounds` 之外的轮次连同它们的观察一起整体丢弃。
pub fn compact<'a>(
    session: &'a Session,
    budget: &ContextBudget,
) -> (Vec<ContextSlice<'a>>, CompactionReport) {
    let slices = session.slices();
    let round_numbers: Vec<u32> = slices
        .iter()
        .filter_map(|slice| match slice {
            ContextSlice::Round { number, .. } => Some(*number),
            ContextSlice::User(_) => None,
        })
        .collect();
    let rounds_before = round_numbers.len();
    let drop_count = rounds_before.saturating_sub(budget.max_rounds);
    if drop_count == 0 {
        return (
            slices,
            CompactionReport {
                rounds_before,
                rounds_after: rounds_before,
                dropped: Vec::new(),
            },
        );
    }

    let dropped = round_numbers[..drop_count].to_vec();
    let mut kept: Vec<ContextSlice<'a>> = Vec::with_capacity(budget.max_rounds + 2);
    // 任务消息是历史的第一条用户消息。先它、后占位说明、再保留的轮次，
    // 顺序与"人先交代任务、再补一句摘要、然后是近期工作"一致。
    if budget.keep_task {
        if let Some(ContextSlice::User(task)) = slices.first() {
            kept.push(ContextSlice::User(task.clone()));
        }
    }
    kept.push(ContextSlice::User(Cow::Owned(omitted_notice(&dropped))));
    kept.extend(slices.iter().filter_map(|slice| match slice {
        ContextSlice::Round { number, .. } if !dropped.contains(number) => Some(slice.clone()),
        _ => None,
    }));

    let rounds_after = rounds_before - drop_count;
    (
        kept,
        CompactionReport {
            rounds_before,
            rounds_after,
            dropped,
        },
    )
}

/// 被省略历史的占位说明。
///
/// 要写清省略了哪些轮、去哪找回来：模型看到历史缺口时最常见的错是
/// 凭想象补全被删掉的内容，指路能压住这个倾向。
fn omitted_notice(dropped: &[u32]) -> String {
    let first = dropped.first().copied().unwrap_or_default();
    let last = dropped.last().copied().unwrap_or_default();
    format!(
        "（为控制上下文长度，已省略第 {first}-{last} 轮的历史，共 {} 轮；\
         如需细节请用 search_text 定位，或用 read_file 重新查看相关文件。）",
        dropped.len()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::agent::AgentTurn;
    use crate::domain::ids::{CallId, SessionId};
    use crate::domain::tool::{ToolCall, ToolOutcome};
    use serde_json::json;

    /// 建一个带 `n` 轮历史的会话：第 `i` 轮读 `r{i}.txt`，观察文本带轮次号，
    /// 供断言"某轮的观察是否出现在裁剪结果里"。
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

    fn budget(max_rounds: usize) -> ContextBudget {
        ContextBudget {
            max_rounds,
            keep_task: true,
        }
    }

    fn round_number(slice: &ContextSlice<'_>) -> Option<u32> {
        match slice {
            ContextSlice::Round { number, .. } => Some(*number),
            ContextSlice::User(_) => None,
        }
    }

    #[test]
    fn under_budget_keeps_everything() {
        let session = session_with_rounds(3);
        let (slices, report) = compact(&session, &budget(5));

        assert_eq!(slices.len(), 1 + 3, "任务消息 + 三轮");
        assert!(report.dropped.is_empty());
        assert_eq!(report.rounds_before, 3);
        assert_eq!(report.rounds_after, 3);
    }

    #[test]
    fn over_budget_keeps_the_last_rounds_and_lists_dropped() {
        let session = session_with_rounds(6);
        let (slices, report) = compact(&session, &budget(2));

        let numbers: Vec<u32> = slices.iter().filter_map(round_number).collect();
        assert_eq!(numbers, vec![5, 6], "只保留最后两轮");
        assert_eq!(report.dropped, vec![1, 2, 3, 4]);
        assert_eq!(report.rounds_before, 6);
        assert_eq!(report.rounds_after, 2);
    }

    #[test]
    fn keep_task_pins_the_task_message_first() {
        let session = session_with_rounds(4);
        let (slices, _) = compact(&session, &budget(1));

        assert!(
            matches!(&slices[0], ContextSlice::User(text) if text.as_ref() == "任务"),
            "任务消息必须排在第一位，实际 {:?}",
            slices[0]
        );
        assert!(matches!(&slices[1], ContextSlice::User(_)), "第二位是占位说明");

        // 关掉 keep_task：任务也被裁掉，占位说明顶到第一位。
        let no_task = ContextBudget {
            max_rounds: 1,
            keep_task: false,
        };
        let (slices, _) = compact(&session, &no_task);
        let Some(ContextSlice::User(first)) = slices.first() else {
            panic!("第一位应当是用户消息");
        };
        assert_ne!(first.as_ref(), "任务", "keep_task=false 时任务消息不应保留");
    }

    #[test]
    fn placeholder_names_the_omitted_range() {
        let session = session_with_rounds(6);
        let (slices, _) = compact(&session, &budget(2));

        let Some(ContextSlice::User(placeholder)) = slices.get(1) else {
            panic!("第二位应当是占位说明，实际 {:?}", slices.get(1));
        };
        assert!(placeholder.contains("第 1-4 轮"), "{}", placeholder);
        assert!(placeholder.contains("共 4 轮"), "{}", placeholder);
        assert!(placeholder.contains("search_text"), "应指路 search_text: {placeholder}");
        assert!(placeholder.contains("read_file"), "应指路 read_file: {placeholder}");
    }

    #[test]
    fn report_reconciles_before_after_and_dropped() {
        let session = session_with_rounds(7);
        let (_, report) = compact(&session, &budget(3));

        assert_eq!(
            report.rounds_before - report.rounds_after,
            report.dropped.len()
        );
    }

    #[test]
    fn zero_round_budget_keeps_only_task_and_placeholder() {
        let session = session_with_rounds(2);
        let (slices, report) = compact(&session, &budget(0));

        assert_eq!(slices.len(), 2, "只剩任务 + 占位说明");
        assert!(slices.iter().all(|slice| matches!(slice, ContextSlice::User(_))));
        assert_eq!(report.dropped, vec![1, 2]);
    }

    #[test]
    fn session_is_left_untouched_by_compaction() {
        let session = session_with_rounds(5);
        let rounds_before = session.rounds();
        let turns_before = session.turns().len();
        let _ = compact(&session, &budget(1));

        assert_eq!(session.rounds(), rounds_before);
        assert_eq!(session.turns().len(), turns_before);
    }
}

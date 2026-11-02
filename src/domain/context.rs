//! 上下文预算与历史裁剪。
//!
//! 长任务的历史会无限增长：每轮都把完整历史发给模型，token 成本线性上升，
//! 最终超出模型输入上限直接报错。裁剪是"发给模型时怎么看历史"的纯函数
//! 变换——[`Session`] 本身不被修改，证据、审计与将来的恢复都依赖完整历史。
//!
//! 被裁掉的历史有两种呈现：占位说明（确定性、零成本），或一段已生成的
//! 摘要（信息量大，但摘要本身是一次模型调用，挂在 `Session` 上只做一次）。
//! `compact` 不调模型——摘要文本由调用方喂进来，这里只负责"有就用"。

use std::borrow::Cow;

use serde::{Deserialize, Serialize};

use super::session::{ContextSlice, Session};

/// 一段由模型生成的历史摘要。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Summary {
    /// 摘要覆盖的轮次范围（含）。
    pub covers: (u32, u32),
    /// 摘要文本，由模型生成。
    pub text: String,
}

impl Summary {
    /// 是否覆盖（含端点）给定的全部轮次。
    pub fn covers_all(&self, rounds: &[u32]) -> bool {
        rounds
            .iter()
            .all(|round| *round >= self.covers.0 && *round <= self.covers.1)
    }
}

/// 摘要策略：控制"被裁掉多少轮才值得再花一次模型调用"。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SummarizePolicy {
    /// 被裁轮次达到这个数才生成/更新摘要。调太高会一直用占位说明，
    /// 调太低则摘要调用太频繁；1-2 适合短上下文预算，默认给 2。
    pub min_dropped_to_summarize: usize,
}

/// 发给模型前，历史最多保留多少个完整轮次。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextBudget {
    /// 最多保留多少轮完整历史，更早的轮次整体裁掉。
    pub max_rounds: usize,
    /// 是否始终保留最初的用户任务。强烈建议 true——模型忘了自己在干什么，
    /// 后面每一轮都会跑偏。
    pub keep_task: bool,
    /// 被裁历史是否用摘要替代占位说明。`None` 表示只用占位说明（P3 行为）。
    pub summarize: Option<SummarizePolicy>,
}

impl ContextBudget {
    /// 不裁剪的预算：轮数上限放到 `usize` 最大值。
    pub fn unlimited() -> Self {
        Self {
            max_rounds: usize::MAX,
            keep_task: true,
            summarize: None,
        }
    }

    /// 开启摘要，被裁轮次达到 `min_dropped_to_summarize` 才生成。
    pub fn with_summarize(mut self, min_dropped_to_summarize: usize) -> Self {
        self.summarize = Some(SummarizePolicy {
            min_dropped_to_summarize,
        });
        self
    }
}

impl Default for ContextBudget {
    fn default() -> Self {
        Self {
            max_rounds: DEFAULT_MAX_ROUNDS,
            keep_task: true,
            // 默认不开摘要：多一次模型调用是不可忽略的成本，显式开启。
            summarize: None,
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
    let mut kept: Vec<ContextSlice<'a>> = Vec::with_capacity(budget.max_rounds + 4);
    // 任务消息是历史的第一条用户消息。先它、后占位说明/摘要、再保留的轮次，
    // 顺序与"人先交代任务、再补一句摘要、然后是近期工作"一致。
    if budget.keep_task {
        if let Some(ContextSlice::User(task)) = slices.first() {
            kept.push(ContextSlice::User(task.clone()));
        }
    }
    kept.push(ContextSlice::User(Cow::Owned(omitted_text(session, &dropped))));
    for (index, slice) in slices.iter().enumerate() {
        match slice {
            // 任务消息已在最前面钉住（或按 keep_task 主动裁掉），原样再放
            // 一遍只会让模型把同一件事看两遍。
            ContextSlice::User(_) if index == 0 => {}
            // 其余用户消息是步骤边界（P9-3 单会话编排）：它们是"现在在
            // 第几步"的唯一来源，不是历史轮次。裁掉它模型就不知道自己
            // 进行到哪了——而裁剪恰恰发生在步数最多的长任务上，正是最
            // 需要这一步信息的时候。按原位置保留，不跳到前面：把第 3 步
            // 的交代挪到第 1 步的观察之前，等于给模型一份假时间线。
            ContextSlice::User(_) => kept.push(slice.clone()),
            ContextSlice::Round { number, .. } if !dropped.contains(number) => {
                kept.push(slice.clone())
            }
            ContextSlice::Round { .. } => {}
        }
    }

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

/// 被省略历史处显示的文本：有覆盖全部被裁轮次的摘要就用摘要，否则用
/// 占位说明。摘要没覆盖全时宁可保守——把没被摘要的那几轮说成"已概括"
/// 会诱导模型把没看过的内容当成看过的。
fn omitted_text(session: &Session, dropped: &[u32]) -> String {
    match session.summary() {
        Some(summary) if summary.covers_all(dropped) => summary.text.clone(),
        _ => omitted_notice(dropped),
    }
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
            summarize: None,
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
            summarize: None,
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

    // ── P9-3：步骤边界的用户消息不占轮次预算 ────────────────────────

    /// 造一个跨三步的单会话历史：每步两轮，步骤之间夹一条用户消息。
    fn session_with_steps(rounds_per_step: u32) -> Session {
        let mut session = Session::new(SessionId::new("s1"), "任务");
        for step in 1..=3u32 {
            if step > 1 {
                session.push_user(format!("[第 {step}/3 步] 第 {step} 步的交代"));
            }
            for _ in 0..rounds_per_step {
                session
                    .commit_round(
                        AgentTurn {
                            text: format!("第 {step} 步的思考"),
                            calls: vec![ToolCall {
                                id: CallId::new(format!("call_{step}")),
                                name: "read_file".to_owned(),
                                arguments: json!({"path": "a.txt"}),
                            }],
                        },
                        vec![ToolOutcome::success(format!("第 {step} 步的观察"))],
                    )
                    .unwrap();
            }
        }
        session
    }

    #[test]
    fn step_boundaries_survive_compaction_in_their_original_order() {
        let session = session_with_steps(2);
        // 只保留两轮：第 1、2、3 步各两轮里只有最后两轮活下来，
        // 但三条步骤交代一条都不能丢。
        let (slices, report) = compact(&session, &budget(2));

        assert_eq!(report.dropped, vec![1, 2, 3, 4], "裁掉前四轮");
        let texts: Vec<String> = slices
            .iter()
            .filter_map(|slice| match slice {
                ContextSlice::User(text) => Some(text.as_ref().to_owned()),
                ContextSlice::Round { .. } => None,
            })
            .collect();
        assert_eq!(texts[0], "任务", "任务交代钉在最前");
        assert!(texts[1].contains("已省略第 1-4 轮"), "第二位是占位说明: {texts:?}");
        // 第 1 步没有独立交代——任务消息就是第 1 步的交代。
        for step in 2..=3 {
            assert!(
                texts.contains(&format!("[第 {step}/3 步] 第 {step} 步的交代")),
                "第 {step} 步的交代必须保留: {texts:?}"
            );
        }
        // 顺序不能乱：第 3 步的交代必须排在第 3 步的观察之前。
        let boundary = slices
            .iter()
            .position(|slice| matches!(slice, ContextSlice::User(text) if text.as_ref() == "[第 3/3 步] 第 3 步的交代"))
            .expect("应有第 3 步的交代");
        let last_round = slices
            .iter()
            .rposition(|slice| matches!(slice, ContextSlice::Round { number: 6, .. }))
            .expect("第 6 轮应保留");
        assert!(boundary < last_round, "步骤交代必须排在本步工作之前");
    }

    #[test]
    fn step_boundaries_do_not_count_against_the_round_budget() {
        let session = session_with_steps(3);
        // 预算正好等于总轮数：一条都不该裁，步骤交代也原样在位。
        let (slices, report) = compact(&session, &budget(9));

        assert!(report.dropped.is_empty(), "九轮全在预算内: {report:?}");
        assert_eq!(slices.len(), 9 + 3, "九轮 + 任务 + 第 2/3 步的两条交代");
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

    // ── 摘要：有则用、不全覆盖则退回占位、纯函数不调模型 ─────────────

    /// 给会话挂一段覆盖 `from..=to` 轮的摘要。
    fn with_summary(session: &mut Session, from: u32, to: u32) {
        session.set_summary(Summary {
            covers: (from, to),
            text: format!("摘要：第 {from}-{to} 轮做了什么事"),
        });
    }

    fn second_slice_text(slices: &[ContextSlice<'_>]) -> String {
        match slices.get(1) {
            Some(ContextSlice::User(text)) => text.to_string(),
            other => panic!("第二位应是用户消息（占位/摘要），实际 {other:?}"),
        }
    }

    #[test]
    fn without_summary_the_placeholder_behaviour_is_unchanged() {
        let session = session_with_rounds(6);
        let (slices, _) = compact(&session, &budget(2));

        let text = second_slice_text(&slices);
        assert!(text.contains("已省略第 1-4 轮"), "{text}");
        assert!(!text.contains("摘要"), "没有摘要时不应出现摘要文本: {text}");
    }

    #[test]
    fn summary_covering_all_dropped_rounds_replaces_the_placeholder() {
        let mut session = session_with_rounds(6);
        with_summary(&mut session, 1, 4);
        let (slices, _) = compact(&session, &budget(2));

        let text = second_slice_text(&slices);
        assert!(text.contains("摘要：第 1-4 轮做了什么事"), "{text}");
        assert!(!text.contains("已省略"), "摘要覆盖全部被裁轮次时不应再出现占位说明: {text}");
    }

    #[test]
    fn partial_summary_falls_back_to_the_placeholder() {
        let mut session = session_with_rounds(6);
        // 摘要只覆盖第 1-2 轮，被裁的第 3、4 轮在覆盖范围外。
        with_summary(&mut session, 1, 2);
        let (slices, _) = compact(&session, &budget(2));

        let text = second_slice_text(&slices);
        assert!(text.contains("已省略第 1-4 轮"), "{text}");
        assert!(!text.contains("摘要"), "部分覆盖时不得把没摘要的轮次说成已概括: {text}");
    }

    #[test]
    fn compact_stays_pure_when_a_summary_is_set() {
        let mut session = session_with_rounds(5);
        with_summary(&mut session, 1, 3);
        let summary_before = session.summary().cloned();
        let turns_before = session.turns().len();

        let (first, _) = compact(&session, &budget(2));
        let (second, _) = compact(&session, &budget(2));

        assert_eq!(first, second, "同一输入两次裁剪结果一致（无隐藏状态变更）");
        assert_eq!(session.summary(), summary_before.as_ref(), "摘要不被裁剪改动");
        assert_eq!(session.turns().len(), turns_before);
        assert!(
            first.iter().any(|slice| match slice {
                ContextSlice::User(text) => text.contains("摘要"),
                _ => false,
            }),
            "裁剪结果里应出现摘要文本"
        );
    }
}

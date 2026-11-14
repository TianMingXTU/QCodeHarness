//! 手动压缩 `/compact` 的应用层入口。
//!
//! 自动压缩由 `ContextBudget` 在每轮模型请求前执行；手动入口复用同一套
//! 证据事件，让事后审计能把两种触发方式分开看，而不必新增事件类型。
//! 摘要正文由调用方给出：本入口不偷偷再调一次模型，也不把未知用量记 0。

use crate::application::port::{EvidenceSink, Timestamp};
use crate::domain::context::{CompactionReport, ContextBudget, Summary, compact_with_summary};
use crate::domain::evidence::{AuditEvent, TokenUsage};
use crate::domain::session::{ContextSlice, Session};

/// 手动压缩失败。失败发生在任何写操作前，会话与证据都保持原状。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManualCompactError {
    NoCompletedRounds,
    EmptySummary,
}

/// 手动压缩结果。裁剪账目直接来自会话视图测量，不手工估算。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManualCompactReport {
    pub summary: Summary,
    pub dropped: Vec<u32>,
    pub rounds_before: usize,
    pub rounds_after: usize,
    pub dropped_tokens: usize,
}

/// 手动压缩入口。
///
/// `sink` 与时钟由装配层注入：领域层保持纯，测试可用独立临时目录的
/// JSONL 证据汇，生产代码接现有会话证据文件。
pub struct ManualCompact<S, C> {
    sink: S,
    clock: C,
}

impl<S, C> ManualCompact<S, C>
where
    S: EvidenceSink,
    C: Fn() -> Timestamp,
{
    pub fn new(sink: S, clock: C) -> Self {
        Self { sink, clock }
    }

    /// 手动生成摘要、写回会话，并留下三条可审计证据。
    ///
    /// `/compact` 的摘要可以来自用户在命令里给出的重点，或上层显式调用的
    /// 模型摘要；这里只接收已经确定的文本。空文本会被拒绝，避免用空白
    /// 摘要替换历史后仍显示"已压缩"。
    pub fn compact(
        &self,
        session: &mut Session,
        summary_text: impl Into<String>,
        turn: u32,
    ) -> Result<ManualCompactReport, ManualCompactError> {
        let rounds = session.rounds();
        if rounds == 0 {
            return Err(ManualCompactError::NoCompletedRounds);
        }
        let text = summary_text.into();
        if text.trim().is_empty() {
            return Err(ManualCompactError::EmptySummary);
        }
        let summary = Summary {
            covers: (1, rounds),
            text,
            steps: distinct_steps(session.round_steps()),
        };
        // 测量时把摘要当作已有值传入，视图确认后才写回会话。失败路径
        // 不留下半成品状态。tokens 报告来自会话历史与实际视图测量。
        let budget = ContextBudget {
            max_tokens: Some(1),
            min_recent_rounds: 0,
            keep_task: false,
            summarize: None,
        };
        let (view, report) = compact_with_summary(session, &budget, Some(&summary));
        if view
            .iter()
            .any(|slice| matches!(slice, ContextSlice::Round { .. }))
        {
            return Err(ManualCompactError::EmptySummary);
        }
        // 视图确认无误后才写回摘要：失败路径同样不留下半成品状态。
        session.set_summary(summary.clone());
        let at = (self.clock)();
        self.sink.record(
            at,
            AuditEvent::SummaryGenerated {
                turn,
                covers: summary.covers,
                usage: TokenUsage::default(),
            },
        );
        self.sink.record(
            at,
            AuditEvent::ContextCompacted {
                turn,
                rounds_before: report.rounds_before,
                rounds_after: report.rounds_after,
                dropped: report.dropped.clone(),
            },
        );
        self.sink.record(
            at,
            AuditEvent::ContextBudgeted {
                turn,
                tokens_before: report.tokens_before as u64,
                tokens_after: report.tokens_after as u64,
                dropped_tokens: report.dropped_tokens as u64,
            },
        );
        Ok(ManualCompactReport {
            summary,
            dropped: report.dropped,
            rounds_before: report.rounds_before,
            rounds_after: report.rounds_after,
            dropped_tokens: report.dropped_tokens,
        })
    }
}

/// 摘要覆盖的步骤号去重后按出现顺序保留，与 SummarizeInput::steps 同口径。
fn distinct_steps(steps: Vec<u32>) -> Vec<u32> {
    let mut result = Vec::new();
    for step in steps {
        if !result.contains(&step) {
            result.push(step);
        }
    }
    result
}

/// 复用既有报告类型，避免把同一组数字复制到另一套结构里。
pub type CompactReport = CompactionReport;

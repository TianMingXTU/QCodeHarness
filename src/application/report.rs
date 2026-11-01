//! 运行报表：从证据日志统计出成功率、工具结局、审批分布与成本。
//!
//! [`summarize`] 与 [`BenchmarkReport::from_sessions`] 是纯函数——
//! 无 IO、无时钟，输入相同的 `SessionLog` 永远得到相同的摘要；
//! 证据怎么读进来（[`crate::infrastructure::evidence::reader`]）与
//! 报表怎么渲染出去都在这两个函数之外，统计逻辑因此可以直接用
//! 合成数据测试。
//!
//! [`SessionLog`] 放在本模块而不是 reader：统计是它的消费方，
//! 而 infrastructure 依赖 application 是本仓库既定的方向
//! （证据汇依赖端口同此理）。

use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::domain::evidence::{AuditEvent, Evidence};
use crate::domain::ids::SessionId;

/// 一次运行（一个会话）的证据汇总。
#[derive(Debug, Clone)]
pub struct SessionLog {
    /// 会话 ID（来自证据文件名，去掉 `.jsonl` 后缀）。
    pub session_id: SessionId,
    /// 全部事件，按文件顺序（写盘顺序）排列。
    pub events: Vec<Evidence>,
    /// 因解析失败被跳过的非空行数。
    ///
    /// 统计要区分"这个会话本来就没干什么"和"日志坏了一部分"，
    /// 只看 `events.len()` 会把后者误读成前者。
    pub skipped_lines: usize,
}

impl SessionLog {
    /// 事件序号是否严格连续（每条比前一条大 1）。
    ///
    /// 同一个 sink 生命周期内序号严格递增；文件里出现乱序或跳号，
    /// 说明日志被并发写过、或中途有写入失败被丢弃。这只影响解读
    /// （比如"序号 5 之前发生过写失败"），不足以让整个文件作废，
    /// 所以只暴露查询，不在读取时报错。
    pub fn is_sequential(&self) -> bool {
        self.events
            .windows(2)
            .all(|pair| pair[1].sequence == pair[0].sequence + 1)
    }

    /// 首条事件的发生时刻。
    pub fn started_at(&self) -> Option<SystemTime> {
        self.events.first().map(|evidence| evidence.recorded_at)
    }

    /// 末条事件的发生时刻。
    pub fn ended_at(&self) -> Option<SystemTime> {
        self.events.last().map(|evidence| evidence.recorded_at)
    }
}

/// 一个会话的统计摘要。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionSummary {
    /// 会话 ID。
    pub session_id: SessionId,
    /// 读到的事件总数。
    pub events: usize,
    /// 跳过的坏行数。
    pub skipped_lines: usize,
    /// 模型调用次数（`Thought` 事件数）。
    pub model_calls: usize,
    /// 工具调用总数（`ToolRequested` 数）。
    pub tool_calls: usize,
    /// 工具调用按结局分类。
    pub succeeded: usize,
    /// 工具失败次数。
    pub failed: usize,
    /// 工具被拒绝次数。
    pub refused: usize,
    /// 审批次数。
    pub approvals: usize,
    /// 审批放行次数。
    pub approved: usize,
    /// 审批否决次数。
    pub denied: usize,
    /// 策略裁决为 Allow 的次数。
    pub policy_allow: usize,
    /// 策略裁决为 RequireApproval 的次数。
    pub policy_require_approval: usize,
    /// 策略裁决为 Deny 的次数。
    pub policy_deny: usize,
    /// 输入 token 合计。
    pub input_tokens: u64,
    /// 输出 token 合计。
    pub output_tokens: u64,
    /// 成本合计（美元）。任一次调用成本未知时为 `None`——
    /// 部分已知的合计是被低估的数字，比"未知"更危险。
    pub cost_usd: Option<f64>,
    /// 摘要调用次数（`SummaryGenerated` + `SummaryFailed`）。
    pub summary_calls: usize,
    /// 上下文裁剪次数（`ContextCompacted` 数）。
    pub compactions: usize,
    /// 首事件时刻。
    pub started_at: Option<SystemTime>,
    /// 末事件时刻。
    pub ended_at: Option<SystemTime>,
}

/// 多个会话的汇总。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BenchmarkReport {
    /// 各会话的摘要。
    pub sessions: Vec<SessionSummary>,
    /// 全部会话的输入 token 合计。
    pub total_input_tokens: u64,
    /// 全部会话的输出 token 合计。
    pub total_output_tokens: u64,
    /// 全部会话的成本合计。任一会话成本未知时为 `None`。
    pub total_cost_usd: Option<f64>,
    /// 成功会话数：会话以 `Thought` 事件收尾（ReAct 循环只有交付
    /// 最终答案时才以思考收尾；被预算/超时打断的会话停在工具观察上）。
    pub succeeded: usize,
    /// 失败会话数（未以 `Thought` 收尾）。
    pub failed: usize,
}

impl BenchmarkReport {
    /// 从多个会话汇总。空清单返回全零报表，不 panic。
    pub fn from_sessions(logs: &[SessionLog]) -> Self {
        let sessions: Vec<SessionSummary> = logs.iter().map(|log| summarize(log)).collect();
        let mut total_input = 0u64;
        let mut total_output = 0u64;
        // 没有任何成本数据（空清单或全部会话零调用）时是"未知"；
        // 每有一个已知成本就累加，遇到未知即整体转未知。
        let mut total_cost = None;
        let mut succeeded = 0usize;
        for (log, summary) in logs.iter().zip(&sessions) {
            total_input += summary.input_tokens;
            total_output += summary.output_tokens;
            match summary.cost_usd {
                Some(cost) => {
                    total_cost = Some(total_cost.unwrap_or(0.0) + cost);
                }
                None => total_cost = None,
            }
            if ends_with_answer(log) {
                succeeded += 1;
            }
        }
        Self {
            succeeded,
            failed: sessions.len() - succeeded,
            sessions,
            total_input_tokens: total_input,
            total_output_tokens: total_output,
            total_cost_usd: total_cost,
        }
    }
}

/// 会话是否以最终答案收尾：最后一条事件是 `Thought`。
///
/// ReAct 循环只有交付最终答案（模型回复不带工具调用）时才以思考
/// 收尾；被预算、超时或取消打断的会话停在工具观察或审批上。
/// 这是"成功"的机器可查代理判据——它回答的是"有没有交付答案"，
/// 不是"答案对不对"，后者只有基准任务的判据（P6-3）能回答。
fn ends_with_answer(log: &SessionLog) -> bool {
    matches!(
        log.events.last().map(|evidence| &evidence.event),
        Some(AuditEvent::Thought { .. })
    )
}

/// 从证据日志统计一个会话。
///
/// 纯函数：无 IO、无时钟。成本只在**每次**调用都已知时才有合计；
/// token 对 `Thought` 与 `SummaryGenerated` 两类调用求和（摘要调用
/// 是真实花出去的配额，不计的话 token 账对不上）。
pub fn summarize(log: &SessionLog) -> SessionSummary {
    let mut summary = SessionSummary {
        session_id: log.session_id.clone(),
        events: log.events.len(),
        skipped_lines: log.skipped_lines,
        model_calls: 0,
        tool_calls: 0,
        succeeded: 0,
        failed: 0,
        refused: 0,
        approvals: 0,
        approved: 0,
        denied: 0,
        policy_allow: 0,
        policy_require_approval: 0,
        policy_deny: 0,
        input_tokens: 0,
        output_tokens: 0,
        cost_usd: Some(0.0),
        summary_calls: 0,
        compactions: 0,
        started_at: log.started_at(),
        ended_at: log.ended_at(),
    };

    for evidence in &log.events {
        match &evidence.event {
            AuditEvent::Thought { usage, .. } => {
                summary.model_calls += 1;
                add_usage(&mut summary, usage);
            }
            AuditEvent::ToolRequested { .. } => summary.tool_calls += 1,
            AuditEvent::ToolObserved { outcome, .. } => match outcome.status {
                crate::domain::tool::OutcomeStatus::Success => summary.succeeded += 1,
                crate::domain::tool::OutcomeStatus::Failed => summary.failed += 1,
                crate::domain::tool::OutcomeStatus::Refused => summary.refused += 1,
            },
            AuditEvent::ApprovalDecided { allowed, .. } => {
                summary.approvals += 1;
                if *allowed {
                    summary.approved += 1;
                } else {
                    summary.denied += 1;
                }
            }
            AuditEvent::PolicyEvaluated { verdict, .. } => match verdict {
                crate::domain::policy::PolicyVerdict::Allow => summary.policy_allow += 1,
                crate::domain::policy::PolicyVerdict::RequireApproval => {
                    summary.policy_require_approval += 1;
                }
                crate::domain::policy::PolicyVerdict::Deny => summary.policy_deny += 1,
            },
            AuditEvent::ContextCompacted { .. } => summary.compactions += 1,
            AuditEvent::SummaryGenerated { usage, .. } => {
                summary.summary_calls += 1;
                add_usage(&mut summary, usage);
            }
            AuditEvent::SummaryFailed { .. } => summary.summary_calls += 1,
            // StepStarted 不计任何业务计数器，但它算事件总数的一员。
            AuditEvent::StepStarted { .. } => {}
        }
    }

    // 空会话没有任何成本数据："未知"而不是 0。
    if summary.model_calls == 0 && summary.summary_calls == 0 {
        summary.cost_usd = None;
    }
    summary
}

/// 累加一次调用的用量。成本未知时把整个会话的成本合计置为未知。
fn add_usage(summary: &mut SessionSummary, usage: &crate::domain::evidence::TokenUsage) {
    summary.input_tokens += usage.input_tokens;
    summary.output_tokens += usage.output_tokens;
    match usage.cost_usd {
        Some(cost) => {
            if let Some(total) = summary.cost_usd.as_mut() {
                *total += cost;
            }
        }
        None => summary.cost_usd = None,
    }
}

/// 成本列的显示值：未知就写"未知"，绝不显示 `$0.0000`。
/// 报表渲染与基准运行器的进度行共用同一口径。
pub(crate) fn cost_label(cost: Option<f64>) -> String {
    match cost {
        Some(value) => format!("${value:.4}"),
        None => "未知".to_owned(),
    }
}

/// 终端对齐时一个字符的显示宽度：CJK 全角算 2，其余算 1。
/// 手写这十行而不是引入 unicode-width 依赖——本报表只有
/// 表头和"未知"两处非 ASCII。
fn display_width(text: &str) -> usize {
    text.chars()
        .map(|c| {
            if matches!(
                c as u32,
                0x1100..=0x115F
                    | 0x2E80..=0xA4CF
                    | 0xAC00..=0xD7A3
                    | 0xF900..=0xFAFF
                    | 0xFE30..=0xFE4F
                    | 0xFF00..=0xFF60
                    | 0xFFE0..=0xFFE6
            ) {
                2
            } else {
                1
            }
        })
        .sum()
}

/// 按显示宽度右补空格。
fn pad(text: &str, width: usize) -> String {
    let mut padded = text.to_owned();
    for _ in display_width(text)..width {
        padded.push(' ');
    }
    padded
}

/// 渲染成人读的对齐表格。表头与"未知"两处 CJK 按显示宽度 2 参与对齐。
pub fn render_text(report: &BenchmarkReport) -> String {
    // 列定义：表头固定，数据从摘要取。
    let headers = ["会话", "事件", "模型", "工具成", "工具败", "工具拒", "审批放", "审批否", "输入tok", "输出tok", "成本"];
    let rows_of = |summary: &SessionSummary| -> Vec<String> {
        vec![
            summary.session_id.as_str().to_owned(),
            summary.events.to_string(),
            summary.model_calls.to_string(),
            summary.succeeded.to_string(),
            summary.failed.to_string(),
            summary.refused.to_string(),
            summary.approved.to_string(),
            summary.denied.to_string(),
            summary.input_tokens.to_string(),
            summary.output_tokens.to_string(),
            cost_label(summary.cost_usd),
        ]
    };

    let data: Vec<Vec<String>> = report.sessions.iter().map(rows_of).collect();
    let mut widths: Vec<usize> = headers
        .iter()
        .map(|header| display_width(header))
        .collect();
    for row in &data {
        for (index, cell) in row.iter().enumerate() {
            widths[index] = widths[index].max(display_width(cell));
        }
    }
    // 数值列右对齐：宽度至少留出表头本身的宽度。
    let render_row = |cells: &[String]| -> String {
        cells
            .iter()
            .enumerate()
            .map(|(index, cell)| {
                let width = widths[index];
                if index == 0 {
                    pad(cell, width)
                } else {
                    // 右对齐数字与成本。
                    let lead = width.saturating_sub(display_width(cell));
                    format!("{}{}", " ".repeat(lead), cell)
                }
            })
            .collect::<Vec<_>>()
            .join("  ")
    };

    let mut out = String::new();
    out.push_str(&render_row(
        &headers.iter().map(|h| h.to_string()).collect::<Vec<_>>(),
    ));
    out.push('\n');
    for row in &data {
        out.push_str(&render_row(row));
        out.push('\n');
    }
    let total = vec![
        "合计".to_owned(),
        report.sessions.iter().map(|s| s.events).sum::<usize>().to_string(),
        report.sessions.iter().map(|s| s.model_calls).sum::<usize>().to_string(),
        report.sessions.iter().map(|s| s.succeeded).sum::<usize>().to_string(),
        report.sessions.iter().map(|s| s.failed).sum::<usize>().to_string(),
        report.sessions.iter().map(|s| s.refused).sum::<usize>().to_string(),
        report.sessions.iter().map(|s| s.approved).sum::<usize>().to_string(),
        report.sessions.iter().map(|s| s.denied).sum::<usize>().to_string(),
        report.total_input_tokens.to_string(),
        report.total_output_tokens.to_string(),
        cost_label(report.total_cost_usd),
    ];
    out.push_str(&render_row(&total));
    out
}

/// 渲染成机器可读的 JSON。
///
/// # Errors
///
/// 序列化失败时返回错误（正常数据不会走到这里）。
pub fn render_json(report: &BenchmarkReport) -> Result<String, serde_json::Error> {
    serde_json::to_string_pretty(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::agent::AgentRole;
    use crate::domain::evidence::TokenUsage;
    use crate::domain::ids::CallId;
    use crate::domain::tool::{ToolCall, ToolOutcome};
    use serde_json::json;
    use std::time::{Duration, UNIX_EPOCH};

    fn call(name: &str) -> ToolCall {
        ToolCall {
            id: CallId::new("call_1"),
            name: name.to_owned(),
            arguments: json!({}),
        }
    }

    fn evidence(sequence: u64, event: AuditEvent) -> Evidence {
        Evidence {
            sequence,
            recorded_at: UNIX_EPOCH + Duration::from_secs(sequence),
            event,
        }
    }

    /// 合成一个覆盖全部事件种类的会话，计数器手工可算。
    fn synthetic_log(cost_of_second_thought: Option<f64>) -> SessionLog {
        SessionLog {
            session_id: SessionId::new("session-test-1"),
            events: vec![
                evidence(1, AuditEvent::StepStarted { turn: 1, role: AgentRole::Coder }),
                evidence(
                    2,
                    AuditEvent::Thought {
                        turn: 1,
                        text: "先写".to_owned(),
                        usage: TokenUsage { input_tokens: 100, output_tokens: 20, cost_usd: Some(0.25) },
                    },
                ),
                evidence(
                    3,
                    AuditEvent::PolicyEvaluated {
                        turn: 1,
                        tool: "write_file".to_owned(),
                        verdict: crate::domain::policy::PolicyVerdict::RequireApproval,
                        rule_index: None,
                        reason: "默认".to_owned(),
                    },
                ),
                evidence(4, AuditEvent::ToolRequested { turn: 1, call: call("write_file") }),
                evidence(
                    5,
                    AuditEvent::ApprovalDecided {
                        turn: 1,
                        tool: "write_file".to_owned(),
                        allowed: true,
                        reason: None,
                    },
                ),
                evidence(
                    6,
                    AuditEvent::ToolObserved {
                        turn: 1,
                        call: call("write_file"),
                        outcome: ToolOutcome::success("已写入"),
                    },
                ),
                evidence(
                    7,
                    AuditEvent::Thought {
                        turn: 2,
                        text: "再读".to_owned(),
                        usage: TokenUsage {
                            input_tokens: 50,
                            output_tokens: 10,
                            cost_usd: cost_of_second_thought,
                        },
                    },
                ),
                evidence(
                    8,
                    AuditEvent::PolicyEvaluated {
                        turn: 2,
                        tool: "read_file".to_owned(),
                        verdict: crate::domain::policy::PolicyVerdict::Allow,
                        rule_index: Some(0),
                        reason: "只读".to_owned(),
                    },
                ),
                evidence(9, AuditEvent::ToolRequested { turn: 2, call: call("read_file") }),
                evidence(
                    10,
                    AuditEvent::ToolObserved {
                        turn: 2,
                        call: call("read_file"),
                        outcome: ToolOutcome::failed("文件不存在"),
                    },
                ),
                evidence(
                    11,
                    AuditEvent::ContextCompacted {
                        turn: 3,
                        rounds_before: 20,
                        rounds_after: 16,
                        dropped: vec![1, 2],
                    },
                ),
                evidence(
                    12,
                    AuditEvent::SummaryGenerated {
                        turn: 3,
                        covers: (1, 2),
                        usage: TokenUsage {
                            input_tokens: 200,
                            output_tokens: 40,
                            cost_usd: Some(0.125),
                        },
                    },
                ),
            ],
            skipped_lines: 0,
        }
    }

    #[test]
    fn summarize_counts_every_event_kind() {
        let summary = summarize(&synthetic_log(Some(0.5)));

        assert_eq!(summary.events, 12);
        assert_eq!(summary.model_calls, 2);
        assert_eq!(summary.tool_calls, 2);
        assert_eq!((summary.succeeded, summary.failed, summary.refused), (1, 1, 0));
        assert_eq!((summary.approvals, summary.approved, summary.denied), (1, 1, 0));
        assert_eq!(
            (
                summary.policy_allow,
                summary.policy_require_approval,
                summary.policy_deny
            ),
            (1, 1, 0)
        );
        assert_eq!(summary.summary_calls, 1);
        assert_eq!(summary.compactions, 1);
        // Thought(100/20) + Thought(50/10) + Summary(200/40)。
        assert_eq!(summary.input_tokens, 350);
        assert_eq!(summary.output_tokens, 70);
        // 0.25 + 0.5 + 0.125，全部是二进制可精确表示的小数。
        assert_eq!(summary.cost_usd, Some(0.875));
        assert!(summary.started_at.is_some() && summary.ended_at.is_some());
    }

    #[test]
    fn one_unknown_cost_makes_the_whole_session_unknown() {
        let summary = summarize(&synthetic_log(None));
        assert_eq!(
            summary.cost_usd, None,
            "任一次调用成本未知，会话合计就是未知，不能当 0"
        );
        // token 不受影响：成本未知不等于用量未知。
        assert_eq!(summary.input_tokens, 350);
    }

    #[test]
    fn empty_session_yields_all_zeroes_without_panicking() {
        let log = SessionLog {
            session_id: SessionId::new("empty"),
            events: vec![],
            skipped_lines: 0,
        };
        let summary = summarize(&log);
        assert_eq!(summary.events, 0);
        assert_eq!(summary.model_calls, 0);
        assert_eq!(summary.input_tokens, 0);
        assert_eq!(summary.cost_usd, None, "空会话没有成本数据：未知而非 0");
        assert_eq!(summary.started_at, None);
    }

    /// 给会话补一条收尾思考（最终答案），使其以成功态结束。
    fn with_final_answer(mut log: SessionLog, cost: Option<f64>) -> SessionLog {
        let turn = log.events.last().map(|e| e.event.turn()).unwrap_or(1) + 1;
        log.events.push(evidence(
            log.events.len() as u64 + 1,
            AuditEvent::Thought {
                turn,
                text: "最终答案".to_owned(),
                usage: TokenUsage { input_tokens: 10, output_tokens: 5, cost_usd: cost },
            },
        ));
        log
    }

    #[test]
    fn report_classifies_by_final_answer_and_sums_costs() {
        // 会话 A 以 Thought 收尾（交付了答案）→ 成功；会话 B 停在工具
        // 观察（被打断）→ 失败。
        let mut log_a = with_final_answer(synthetic_log(Some(0.25)), Some(0.5));
        log_a.session_id = SessionId::new("a");
        let mut log_b = synthetic_log(None);
        log_b.session_id = SessionId::new("b");
        // 把 B 的收尾换成工具观察（截到最后一次观察为止）。
        log_b.events.truncate(10);

        let report = BenchmarkReport::from_sessions(&[log_a, log_b]);
        assert_eq!((report.succeeded, report.failed), (1, 1));
        // A: 350+10（补的收尾思考）；B 截到第 10 条（摘要的 200 被截掉）：150。
        assert_eq!(report.total_input_tokens, 510);
        // A 已知（0.25+0.25+0.125+0.5）、B 未知 → 合计未知。
        assert_eq!(report.total_cost_usd, None);
    }

    #[test]
    fn report_total_cost_is_some_only_when_every_session_is_known() {
        let mut log_a = synthetic_log(Some(0.25));
        log_a.session_id = SessionId::new("a");
        let mut log_b = synthetic_log(Some(0.5));
        log_b.session_id = SessionId::new("b");

        let report = BenchmarkReport::from_sessions(&[log_a.clone(), log_b.clone()]);
        assert_eq!(report.total_cost_usd, Some(1.5), "全部已知时合计正确");

        let mut log_c = synthetic_log(None);
        log_c.session_id = SessionId::new("c");
        let report = BenchmarkReport::from_sessions(&[log_a, log_c]);
        assert_eq!(report.total_cost_usd, None);
        assert_eq!(report.sessions.len(), 2);
    }

    #[test]
    fn empty_report_has_no_cost_data() {
        let report = BenchmarkReport::from_sessions(&[]);
        assert_eq!(report.sessions.len(), 0);
        assert_eq!(report.total_input_tokens, 0);
        assert_eq!(report.total_cost_usd, None, "没有任何数据时是未知而非 0");
        assert_eq!((report.succeeded, report.failed), (0, 0));
    }

    #[test]
    fn json_rendering_round_trips() {
        let mut log = synthetic_log(Some(0.25));
        log.session_id = SessionId::new("session-roundtrip");
        let report = BenchmarkReport::from_sessions(&[log]);

        let text = render_json(&report).expect("序列化");
        let back: BenchmarkReport = serde_json::from_str(&text).expect("反序列化");
        assert_eq!(back, report);
    }

    #[test]
    fn text_rendering_marks_unknown_cost_honestly() {
        let log = synthetic_log(None);
        let report = BenchmarkReport::from_sessions(&[log]);
        let text = render_text(&report);

        assert!(text.contains("未知"), "成本未知必须显示'未知'：{text}");
        assert!(!text.contains("$0.0000"), "不能把未知渲染成 0：{text}");
        assert!(text.contains("合计"));
    }

    #[test]
    fn text_rendering_aligns_known_costs() {
        // 0.875（第二次思考）+ 0.25（第一次）+ 0.125（摘要）= 1.25。
        let log = synthetic_log(Some(0.875));
        let report = BenchmarkReport::from_sessions(&[log]);
        let text = render_text(&report);
        assert!(text.contains("$1.2500"), "{text}");
    }
}

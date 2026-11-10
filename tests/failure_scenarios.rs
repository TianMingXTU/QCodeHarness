//! 失败场景的报表统计验收。
//!
//! 用合成审计事件核对恢复、MCP 生命周期、沙箱、扩展拒绝与上下文压缩
//! 的计数和渲染，避免报表把拒绝当失败或漏记结果未知的调用。各子系统
//! 实际产生证据和处理故障的行为由对应的集成测试验证。

use std::time::{Duration, UNIX_EPOCH};

use qcode_harness::application::report::{BenchmarkReport, SessionLog, summarize};
use qcode_harness::domain::evidence::{AuditEvent, Evidence, TokenUsage};
use qcode_harness::domain::ids::CallId;
use qcode_harness::domain::mcp::{McpCallFailure, McpFaultKind, McpLifecycleFact, McpLifecycleKind};
use qcode_harness::domain::tool::{ToolCall, ToolOutcome};
use serde_json::json;

/// 一条带毫秒时序的证据：序号即毫秒偏移，便于核对墙钟与等待。
fn ev(sequence: u64, event: AuditEvent) -> Evidence {
    Evidence::new(sequence, UNIX_EPOCH + Duration::from_millis(sequence * 100), event)
}

fn thought(sequence: u64, turn: u32, text: &str) -> Evidence {
    ev(sequence, AuditEvent::Thought {
        turn,
        text: text.to_owned(),
        usage: TokenUsage {
            input_tokens: 0,
            output_tokens: 0,
            cached_input_tokens: 0,
            cache_creation_input_tokens: 0,
            cost_usd: None,
        },
    })
}

fn call(name: &str) -> ToolCall {
    ToolCall {
        id: CallId::new("call_1"),
        name: name.to_owned(),
        arguments: json!({}),
    }
}

/// 一条 MCP 生命周期证据。
fn mcp_lifecycle(sequence: u64, kind: McpLifecycleKind) -> Evidence {
    ev(
        sequence,
        AuditEvent::McpLifecycle {
            turn: 0,
            fact: McpLifecycleFact {
                server: "demo".to_owned(),
                generation: 1,
                kind,
                fault: None,
                call_id: None,
                process_isolation: true,
                tools_changed: false,
            },
        },
    )
}

/// 一条 MCP 调用失败证据。
fn mcp_call_failed(sequence: u64, kind: McpFaultKind, result_unknown: bool) -> Evidence {
    ev(
        sequence,
        AuditEvent::McpCallFailed {
            turn: 1,
            call_id: CallId::new("call_1"),
            tool: "mcp__demo__send_message".to_owned(),
            failure: McpCallFailure {
                server: "demo".to_owned(),
                generation: 1,
                kind,
                result_unknown,
                process_isolation: true,
            },
        },
    )
}

/// 一个只含某类失败证据、以思考收尾的会话。
fn log_with(events: Vec<Evidence>) -> SessionLog {
    let mut session = SessionLog {
        session_id: qcode_harness::domain::ids::SessionId::new("session-failure"),
        events,
        skipped_lines: 0,
    };
    // 报表按"是否交付答案"划分成败：统一以思考收尾，使失败体现在
    // 各子系统计数里而非被算成"会话没跑完"。
    session
        .events
        .push(thought(session.events.len() as u64 + 1, 1, "最终答案"));
    session
}

// ── 恢复：中断后未确认的调用与审批 ────────────────────────────────────────

/// 恢复路径把"结果未知的重放"与"重新询问"都留给模型决定，报表要能数出
/// 这两类恢复痕迹，否则"是否重复触发了副作用"无从核对。
#[test]
fn recovery_failures_are_visible_in_the_report() {
    let session = log_with(vec![
        ev(
            1,
            AuditEvent::ToolCallReplayed {
                turn: 1,
                tool: "run_command".to_owned(),
                reason: "中断前可能已执行".to_owned(),
                previous_outcome: None,
            },
        ),
        ev(
            2,
            AuditEvent::ApprovalReasked {
                turn: 1,
                tool: "write_file".to_owned(),
                reason: "上一次未答复".to_owned(),
            },
        ),
    ]);

    let summary = summarize(&session);
    assert_eq!(summary.replays, 1, "一次结果未知的重放被数出");
    assert_eq!(summary.approval_reasks, 1, "一次重新询问被数出");

    let report = BenchmarkReport::from_sessions(&[session]);
    // 重放与重新询问不计工具结局：会话仍以思考收尾，判为成功。
    assert_eq!(report.succeeded, 1, "恢复痕迹不该把会话算成失败");
    assert_eq!(report.sessions[0].replays, 1);
    assert_eq!(report.sessions[0].approval_reasks, 1);
}

// ── MCP 生命周期：连接故障与结果未知 ──────────────────────────────────────

/// MCP 断连、恢复失败、调用超时且结果未知：这些故障在报表里独立计数，
/// 且"结果未知"要与已取消区分开。
#[test]
fn mcp_lifecycle_failures_are_counted_separately() {
    let session = log_with(vec![
        mcp_lifecycle(1, McpLifecycleKind::Disconnected),
        mcp_lifecycle(2, McpLifecycleKind::RecoveryFailed),
        mcp_call_failed(3, McpFaultKind::CallTimeout, true),
    ]);

    let summary = summarize(&session);
    assert_eq!(summary.mcp_disconnects, 1, "断连一次");
    assert_eq!(summary.mcp_recovery_failures, 1, "恢复失败一次");
    assert_eq!(summary.mcp_call_timeouts, 1, "调用超时一次");
    assert_eq!(summary.mcp_unknown_results, 1, "结果未知一次");
    // 故障只进 MCP 计数，不污染工具与沙箱统计。
    assert_eq!(summary.tool_calls, 0);
    assert_eq!(summary.sandbox_protected_runs, 0);

    // 报表的 MCP 汇总段把"结果未知不等于已取消"讲清楚，避免误读。
    let text = qcode_harness::application::report::render_text(&BenchmarkReport::from_sessions(&[
        session,
    ]));
    assert!(text.contains("MCP 生命周期"), "{text}");
    assert!(text.contains("结果未知 1 次"), "{text}");
    assert!(text.contains("不会自动重放"), "{text}");
}

// ── 沙箱：受保护与未受保护执行的分账 ─────────────────────────────────────

/// 同一会话混用本地子进程与 MCP 调用：进程沙箱生效与否按每次执行记录，
/// 报表要把受保护与未受保护分账，未受保护是沙箱未生效的信号。
#[test]
fn sandbox_protection_is_tracked_per_run() {
    let sandboxed = |seq: u64, tool: &str, active: bool| {
        ev(
            seq,
            AuditEvent::SandboxApplied {
                turn: 1,
                call: call(tool),
                platform: "Windows JobObject".to_owned(),
                active,
                boundary_checked: active,
            },
        )
    };
    let session = log_with(vec![
        sandboxed(1, "run_command", true),
        sandboxed(2, "git_status", true),
        // 边界检查失败的那次：进程仍在，但未纳入限额，算未受保护。
        sandboxed(3, "git_diff", false),
        sandboxed(4, "mcp__demo__send_message", false),
    ]);

    let summary = summarize(&session);
    assert_eq!(summary.sandbox_protected_runs, 2, "两次实际受进程沙箱保护");
    assert_eq!(summary.sandbox_unprotected_runs, 2, "边界失败与 MCP 各记一次");

    // 未受保护的执行要能落到 JSON，供自动化流水线判定沙箱是否全程生效。
    let json = qcode_harness::application::report::render_json(&BenchmarkReport::from_sessions(&[
        session,
    ]))
    .expect("序列化");
    let value: serde_json::Value = serde_json::from_str(&json).expect("JSON 报表");
    assert_eq!(value["sessions"][0]["sandbox_unprotected_runs"], 2);
}

// ── 扩展守卫：越界调用被拒 ───────────────────────────────────────────────

/// 扩展的 ToolGuard 拒绝一次越界调用：拒绝走 `ToolObserved::Refused`，
/// 报表要把它记为"拒绝"而非"失败"，且拒绝不构成重复副作用。
#[test]
fn extension_guard_refusals_are_not_failures() {
    let refused = |seq: u64| {
        ev(
            seq,
            AuditEvent::ToolObserved {
                turn: 1,
                call: call("run_command"),
                outcome: ToolOutcome::refused("路径越出工作区"),
            },
        )
    };
    let session = log_with(vec![refused(1), refused(2)]);

    let summary = summarize(&session);
    assert_eq!(summary.refused, 2, "两次越界调用都被记为拒绝");
    assert_eq!(summary.failed, 0, "拒绝不是失败");
    assert_eq!(summary.succeeded, 0, "也没有成功");

    // 报表的合计行要能核对"没有成功、两次被拒"。
    let report = BenchmarkReport::from_sessions(&[session]);
    assert_eq!(report.sessions[0].refused, 2);
    assert_eq!(report.sessions[0].failed, 0);
}

// ── 上下文压缩：关键信息是否保住 ─────────────────────────────────────────

/// 长任务依赖压缩后关键上下文仍在：压缩次数、省下的 token、摘要调用都
/// 要进报表，压缩判据才有的放矢。
#[test]
fn context_compaction_is_measured_in_the_report() {
    let session = log_with(vec![
        ev(
            1,
            AuditEvent::ContextCompacted {
                turn: 3,
                rounds_before: 20,
                rounds_after: 16,
                dropped: vec![1, 2, 3, 4],
            },
        ),
        ev(
            2,
            AuditEvent::ContextBudgeted {
                turn: 3,
                tokens_before: 20_000,
                tokens_after: 18_000,
                dropped_tokens: 2_000,
            },
        ),
        ev(
            3,
            AuditEvent::SummaryGenerated {
                turn: 3,
                covers: (1, 16),
                usage: TokenUsage {
                    input_tokens: 1_000,
                    output_tokens: 200,
                    cached_input_tokens: 0,
                    cache_creation_input_tokens: 0,
                    cost_usd: Some(0.01),
                },
            },
        ),
    ]);

    let summary = summarize(&session);
    assert_eq!(summary.compactions, 1, "一次裁剪");
    assert_eq!(summary.dropped_context_tokens, 2_000, "净省 token 进账");
    assert_eq!(summary.summary_calls, 1, "摘要调用计一次");

    let text = qcode_harness::application::report::render_text(&BenchmarkReport::from_sessions(
        std::slice::from_ref(&session),
    ));
    // 压缩本身不进对齐表格，但 JSON 报表保留这些字段供机器读取。
    let json = qcode_harness::application::report::render_json(&BenchmarkReport::from_sessions(
        std::slice::from_ref(&session),
    ))
    .expect("序列化");
    let value: serde_json::Value = serde_json::from_str(&json).expect("JSON 报表");
    assert_eq!(value["sessions"][0]["compactions"], 1);
    assert_eq!(value["sessions"][0]["dropped_context_tokens"], 2_000);
    assert!(!text.is_empty(), "文本报表不为空");
}

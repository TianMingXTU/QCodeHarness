//! MCP 生命周期证据的落盘与读回验收。
//!
//! 单元测试用内存事件流就够了，这里要验的是**磁盘契约**：新事件写进
//! JSONL 之后旧 reader 读得回来、报表算得对、`--report` 不会因为多一份
//! 连接日志就把任务判成败。这几件事只有真实文件能证明。

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use qcode_harness::application::mcp_lifecycle::McpLifecycleRecorder;
use qcode_harness::application::port::{Clock, EvidenceSink};
use qcode_harness::application::report::{
    BenchmarkReport, is_lifecycle_only, render_text, summarize,
};
use qcode_harness::domain::evidence::{AuditEvent, EVIDENCE_FORMAT_MILLIS, Evidence, TokenUsage};
use qcode_harness::domain::ids::{CallId, SessionId};
use qcode_harness::domain::mcp::{
    McpCallFailure, McpFaultKind, McpLifecycleFact, McpLifecycleKind,
};
use qcode_harness::domain::tool::{ToolCall, ToolOutcome};
use qcode_harness::infrastructure::evidence::jsonl::JsonlEvidenceSink;
use qcode_harness::infrastructure::evidence::reader;
use serde_json::json;

static DIR_COUNTER: AtomicUsize = AtomicUsize::new(0);

/// 每个用例一个独立临时目录，避免并行测试互相踩文件。
fn temp_dir(label: &str) -> PathBuf {
    let unique = DIR_COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!(
        "qch-mcp-lifecycle-{label}-{}-{unique}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("建临时目录");
    dir
}

/// 固定时钟：连接事件的时刻要可断言，而不是"看测试跑多快"。
struct FixedClock(SystemTime);

impl Clock for FixedClock {
    fn now(&self) -> SystemTime {
        self.0
    }
}

/// 按文件名打开一份追加写的 JSONL 证据汇。
fn open(dir: &Path, name: &str) -> Arc<dyn EvidenceSink> {
    Arc::new(JsonlEvidenceSink::open(&dir.join(name)).expect("打开证据文件"))
}

fn fact(kind: McpLifecycleKind) -> McpLifecycleFact {
    McpLifecycleFact {
        server: "demo".to_owned(),
        generation: 2,
        kind,
        fault: None,
        call_id: None,
        process_isolation: true,
        tools_changed: false,
    }
}

fn call(name: &str) -> ToolCall {
    ToolCall {
        id: CallId::new("call_1"),
        name: name.to_owned(),
        arguments: json!({"url": "example.com"}),
    }
}

/// 经 `McpLifecycleRecorder` 写的连接事件，落盘后被旧 reader 原样读回。
///
/// 这是"审计链闭环"的最小证据：执行层给的事实、写到磁盘的内容、报表读回
/// 的统计必须是同一件事，中间没有靠输出文本反推的环节。
#[test]
fn lifecycle_recorder_events_survive_the_jsonl_round_trip() {
    let dir = temp_dir("recorder");
    let path = dir.join("mcp-demo.jsonl");
    {
        let recorder = McpLifecycleRecorder::new(
            open(&dir, "mcp-demo.jsonl"),
            Arc::new(FixedClock(UNIX_EPOCH + Duration::from_secs(1_700))),
        );
        for kind in [
            McpLifecycleKind::Connected,
            McpLifecycleKind::Disconnected,
            McpLifecycleKind::Recovered,
            McpLifecycleKind::RecoveryFailed,
            McpLifecycleKind::ConnectFailed,
        ] {
            recorder.record(fact(kind));
        }
    }

    let log = reader::read_session(&path)
        .expect("读回")
        .expect("文件里有事件");
    assert_eq!(log.skipped_lines, 0, "新事件不能让旧 reader 读不懂");
    assert!(log.is_sequential(), "同一份日志的序号必须严格递增");
    assert!(
        is_lifecycle_only(&log),
        "只有连接事件的日志要按事件识别出来"
    );

    let kinds: Vec<McpLifecycleKind> = log
        .events
        .iter()
        .map(|entry: &Evidence| match &entry.event {
            AuditEvent::McpLifecycle { turn, fact } => {
                assert_eq!(*turn, 0, "连接事件不带模型轮次");
                assert_eq!(
                    entry.recorded_at,
                    UNIX_EPOCH + Duration::from_secs(1_700),
                    "时刻取事件发生的那一刻，不是落盘的那一刻"
                );
                assert_eq!(fact.generation, 2, "generation 要能区分同名 Server 的实例");
                assert!(fact.process_isolation, "进程保护状态要按事实读回");
                fact.kind
            }
            other => panic!("生命周期日志里混进了 {other:?}"),
        })
        .collect();
    assert_eq!(
        kinds,
        vec![
            McpLifecycleKind::Connected,
            McpLifecycleKind::Disconnected,
            McpLifecycleKind::Recovered,
            McpLifecycleKind::RecoveryFailed,
            McpLifecycleKind::ConnectFailed,
        ]
    );

    let summary = summarize(&log);
    assert_eq!(summary.mcp_connect_failures, 1);
    assert_eq!(summary.mcp_disconnects, 1);
    assert_eq!(summary.mcp_recoveries, 1);
    assert_eq!(summary.mcp_recovery_failures, 1);
    // 启动失败没有工具调用：既不伪造工具结局，也不伪造执行边界记录。
    assert_eq!(summary.tool_calls, 0);
    assert_eq!(
        (summary.succeeded, summary.failed, summary.refused),
        (0, 0, 0)
    );
    assert_eq!(
        (
            summary.sandbox_protected_runs,
            summary.sandbox_unprotected_runs
        ),
        (0, 0)
    );

    let _ = fs::remove_dir_all(&dir);
}

/// 一份连接日志 + 一份真会话：报表说"成功 1 份"，不说"失败 1 份"。
///
/// 这是用户看得见的差别：MCP 没起来时 `--report` 不能同时告诉他"有个
/// 任务挂了"，也不能因为连接日志没有成本数据就把整轮成本拖成未知。
#[test]
fn connection_only_log_does_not_distort_the_session_counts() {
    let dir = temp_dir("mixed");
    {
        let session = open(&dir, "session-1.jsonl");
        session.record(
            UNIX_EPOCH + Duration::from_millis(1_000),
            AuditEvent::Thought {
                turn: 1,
                text: "先取页面".to_owned(),
                usage: TokenUsage {
                    input_tokens: 10,
                    output_tokens: 5,
                    cached_input_tokens: 0,
                    cache_creation_input_tokens: 0,
                    cost_usd: Some(0.125),
                },
            },
        );
        session.record(
            UNIX_EPOCH + Duration::from_millis(1_500),
            AuditEvent::ToolObserved {
                turn: 1,
                call: call("mcp__demo__fetch_page"),
                outcome: ToolOutcome::failed("MCP Server `demo`：工具调用超时"),
            },
        );
        session.record(
            UNIX_EPOCH + Duration::from_millis(1_600),
            AuditEvent::McpCallFailed {
                turn: 1,
                call_id: CallId::new("call_1"),
                tool: "mcp__demo__fetch_page".to_owned(),
                failure: McpCallFailure {
                    server: "demo".to_owned(),
                    generation: 1,
                    kind: McpFaultKind::CallTimeout,
                    result_unknown: true,
                    process_isolation: false,
                },
            },
        );
        session.record(
            UNIX_EPOCH + Duration::from_millis(2_000),
            AuditEvent::Thought {
                turn: 2,
                text: "交付答案".to_owned(),
                // 必须是"已知为 0"：`TokenUsage::default()` 的 `cost_usd` 是
                // `None`（未知），一次未知调用就把整份合计拖成未知，那样这条
                // 断言测的就不是"连接日志被排除"，而是成本口径本身。
                usage: TokenUsage {
                    cost_usd: Some(0.0),
                    ..TokenUsage::default()
                },
            },
        );

        let mcp = open(&dir, "mcp-demo.jsonl");
        mcp.record(
            UNIX_EPOCH + Duration::from_millis(500),
            AuditEvent::McpLifecycle {
                turn: 0,
                fact: fact(McpLifecycleKind::ConnectFailed),
            },
        );
        mcp.record(
            UNIX_EPOCH + Duration::from_millis(51_000),
            AuditEvent::McpLifecycle {
                turn: 0,
                fact: fact(McpLifecycleKind::Disconnected),
            },
        );
    }

    let logs = reader::read_all(&dir).expect("读全部日志");
    assert_eq!(logs.len(), 2);
    let report = BenchmarkReport::from_sessions(&logs);
    assert_eq!(report.mcp_lifecycle_logs, 1, "按事件识别出那一份连接日志");
    assert_eq!((report.succeeded, report.failed), (1, 0));
    assert_eq!(
        report.total_cost_usd,
        Some(0.125),
        "连接日志没有成本数据，不该把已知成本拖成未知"
    );
    let session_log = logs
        .iter()
        .find(|log| log.session_id == SessionId::new("session-1"))
        .expect("会话日志");
    assert_eq!(
        report.total_wall_clock_ms,
        summarize(session_log).wall_clock_ms,
        "墙钟合计只含真正的任务会话"
    );

    let session_summary = report
        .sessions
        .iter()
        .find(|s| s.session_id == SessionId::new("session-1"))
        .expect("会话摘要在报表里");
    assert_eq!(session_summary.mcp_call_timeouts, 1);
    assert_eq!(session_summary.mcp_unknown_results, 1);
    assert_eq!(session_summary.failed, 1, "工具结局仍按 ToolObserved 计");
    assert_eq!(
        session_summary.tool_calls, 0,
        "调用故障事实不冒充 ToolRequested 计数"
    );
    let connection_summary = report
        .sessions
        .iter()
        .find(|s| s.session_id == SessionId::new("mcp-demo"))
        .expect("连接日志的摘要也在报表里");
    assert_eq!(connection_summary.mcp_connect_failures, 1);
    assert_eq!(connection_summary.mcp_disconnects, 1);
    assert_eq!(connection_summary.tool_calls, 0, "连接日志不伪造工具调用");

    let text = render_text(&report);
    assert!(text.contains("结果未知 1 次"), "{text}");

    let _ = fs::remove_dir_all(&dir);
}

/// 混写进同一份日志时，末尾那条关闭事件也不能把已交付的会话判成失败。
#[test]
fn trailing_close_in_a_mixed_log_keeps_the_session_successful() {
    let dir = temp_dir("trailing");
    {
        let session = open(&dir, "session-mixed.jsonl");
        session.record(
            UNIX_EPOCH + Duration::from_millis(1_000),
            AuditEvent::Thought {
                turn: 1,
                text: "交付答案".to_owned(),
                usage: TokenUsage {
                    input_tokens: 1,
                    output_tokens: 1,
                    cached_input_tokens: 0,
                    cache_creation_input_tokens: 0,
                    cost_usd: Some(0.01),
                },
            },
        );
        session.record(
            UNIX_EPOCH + Duration::from_millis(1_200),
            AuditEvent::McpLifecycle {
                turn: 0,
                fact: fact(McpLifecycleKind::Closed),
            },
        );
    }

    let logs = reader::read_all(&dir).expect("读全部日志");
    let report = BenchmarkReport::from_sessions(&logs);
    assert_eq!(report.mcp_lifecycle_logs, 0, "混写的日志仍是任务会话");
    assert_eq!((report.succeeded, report.failed), (1, 0));

    let _ = fs::remove_dir_all(&dir);
}

/// 旧格式的证据文件（没有这些新字段）照常读，MCP 统计落成 0。
///
/// `#[serde(default)]` 只在 JSON 层面兜底；真正要保证的是历史文件不会
/// 因为新增事件类型而读不动——用户机器上还躺着 P9/P10 的证据。
#[test]
fn legacy_evidence_file_still_reads_with_zero_mcp_counts() {
    let dir = temp_dir("legacy");
    // 手写成 P9 之前的秒级格式：没有 format 键，也没有任何 MCP 事件。
    fs::write(
        dir.join("session-old.jsonl"),
        json!({
            "sequence": 1,
            "recorded_at": 1_000u64,
            "event": {"Thought": {"turn": 1, "text": "旧答案", "usage": {
                "input_tokens": 3, "output_tokens": 1, "cost_usd": 0.01
            }}},
        })
        .to_string(),
    )
    .expect("写旧文件");
    fs::write(
        dir.join("session-old-2.jsonl"),
        json!({
            "sequence": 2,
            "format": EVIDENCE_FORMAT_MILLIS,
            "recorded_at": 2_000u64,
            "event": {"ToolObserved": {"turn": 1, "call": {
                "id": "call_1", "name": "read_file", "arguments": {"path": "Cargo.toml"}
            }, "outcome": {"content": "内容", "status": "Success"}}},
        })
        .to_string(),
    )
    .expect("写旧文件 2");

    let logs = reader::read_all(&dir).expect("读旧文件");
    assert_eq!(logs.len(), 2);
    let report = BenchmarkReport::from_sessions(&logs);
    assert_eq!(report.mcp_lifecycle_logs, 0);
    for summary in &report.sessions {
        assert_eq!(summary.mcp_connect_failures, 0);
        assert_eq!(summary.mcp_disconnects, 0);
        assert_eq!(summary.mcp_recoveries, 0);
        assert_eq!(summary.mcp_recovery_failures, 0);
        assert_eq!(summary.mcp_call_timeouts, 0);
        assert_eq!(summary.mcp_unknown_results, 0);
    }

    let _ = fs::remove_dir_all(&dir);
}

/// 落盘的观察保持旧的两字段形状。
///
/// `mcp_failure` 是进程内交接的事实，一旦跟着 `ToolObserved` 进文件，
/// 恢复路径与历史 reader 就要多认一种它们不认识的形状。
#[test]
fn observed_outcome_keeps_the_legacy_two_field_shape() {
    let dir = temp_dir("shape");
    let path = dir.join("session-shape.jsonl");
    {
        let session = open(&dir, "session-shape.jsonl");
        session.record(
            UNIX_EPOCH + Duration::from_millis(10),
            AuditEvent::ToolObserved {
                turn: 1,
                call: call("mcp__demo__fetch_page"),
                outcome: ToolOutcome::failed("MCP Server `demo`：连接已断开"),
            },
        );
    }

    let raw = fs::read_to_string(&path).expect("读原始行");
    assert!(
        !raw.contains("mcp_failure") && !raw.contains("process_isolation"),
        "落盘观察不该带上进程内交接的事实字段: {raw}"
    );
    let log = reader::read_session(&path).expect("读回").expect("有事件");
    assert_eq!(log.events[0].format, EVIDENCE_FORMAT_MILLIS);
    match &log.events[0].event {
        AuditEvent::ToolObserved { outcome, .. } => {
            assert_eq!(
                outcome,
                &ToolOutcome::failed("MCP Server `demo`：连接已断开")
            );
            assert!(outcome.mcp_failure.is_none());
        }
        other => panic!("期望 ToolObserved，实际 {other:?}"),
    }

    let _ = fs::remove_dir_all(&dir);
}

/// 共享同一个写入器的多个 Server 并发写日志，序号不能撞车。
///
/// 报表靠"序号连续"发现日志被截断或改写过；两条事件写成同一个序号，
/// 这条自检就废了，用户拿到的是一份说不清是否完整的证据。
#[test]
fn concurrent_servers_write_contiguous_sequences() {
    let dir = temp_dir("concurrent");
    let path = dir.join("mcp-many.jsonl");
    {
        let recorder = Arc::new(McpLifecycleRecorder::new(
            open(&dir, "mcp-many.jsonl"),
            Arc::new(FixedClock(SystemTime::now())),
        ));
        let writers: Vec<_> = (0..12)
            .map(|index| {
                let recorder = recorder.clone();
                std::thread::spawn(move || {
                    recorder.record(McpLifecycleFact {
                        server: format!("server-{index}"),
                        generation: 1,
                        kind: McpLifecycleKind::Connected,
                        fault: None,
                        call_id: None,
                        process_isolation: false,
                        tools_changed: false,
                    })
                })
            })
            .collect();
        for writer in writers {
            writer.join().expect("写入线程不该 panic");
        }
    }

    let log = reader::read_session(&path).expect("读回").expect("有事件");
    assert_eq!(log.events.len(), 12);
    assert!(
        log.is_sequential(),
        "12 个 Server 并发写也要 1..=12 各一次，实际 {:?}",
        log.events.iter().map(|e| e.sequence).collect::<Vec<_>>()
    );

    let _ = fs::remove_dir_all(&dir);
}

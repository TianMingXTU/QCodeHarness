//! 服务事件纯统计函数的测试：只喂字节，不碰文件系统。

use std::path::Path;

use qcode_harness::application::report::{
    BenchmarkReport, ServiceSummary, ServiceSummaryStatus, render_text, summarize_service_events,
};
use qcode_harness::domain::service::PROTOCOL_VERSION;
use serde_json::{Value, json};

fn event(sequence: u64, kind: &str) -> Value {
    json!({
        "version": PROTOCOL_VERSION,
        "sequence": sequence,
        "time_unix_ms": sequence,
        "task_id": null,
        "session_id": null,
        "execution_id": null,
        "origin": {"parent_session_id": null, "extension_id": null},
        "payload": {"type": kind},
    })
}

fn journal(events: &[Value]) -> Vec<u8> {
    let mut contents = Vec::new();
    for event in events {
        serde_json::to_writer(&mut contents, event).expect("序列化事件");
        contents.push(b'\n');
    }
    contents
}

#[test]
fn invalid_event_lines_do_not_hide_later_readable_events() {
    let path = Path::new("events.jsonl");
    let mut contents = journal(&[event(1, "execution_started")]);
    contents.extend_from_slice(b"not JSON\n");
    let mut future = event(2, "cancel_requested");
    future["version"] = json!(PROTOCOL_VERSION + 1);
    contents.extend(journal(&[future]));
    let mut missing_sequence = event(2, "cancel_requested");
    missing_sequence.as_object_mut().unwrap().remove("sequence");
    contents.extend(journal(&[missing_sequence]));
    contents.extend(journal(&[event(2, "cancel_requested")]));

    let summary = summarize_service_events(&contents, path);

    assert_eq!(summary.status, ServiceSummaryStatus::Incomplete);
    assert_eq!(summary.events_total, Some(2));
    assert_eq!(summary.sequence_gaps, Some(0));
    assert_eq!(summary.last_sequence, Some(2));
    assert_eq!(summary.corrupt_files.len(), 3);
    assert_eq!(summary.corrupt_files.iter().map(|issue| issue.line).collect::<Vec<_>>(), vec![Some(2), Some(3), Some(4)]);
    assert!(summary.corrupt_files[1].reason.contains("version"));
}

#[test]
fn duplicate_and_reordered_sequences_do_not_inflate_missing_numbers() {
    let events = [1, 3, 1, 2, 3, u64::MAX].map(|sequence| event(sequence, "execution_started"));
    let summary = summarize_service_events(&journal(&events), Path::new("events.jsonl"));

    assert_eq!(summary.status, ServiceSummaryStatus::Incomplete);
    assert_eq!(summary.events_total, Some(6));
    assert_eq!(summary.sequence_gaps, Some(u64::MAX - 4));
    assert_eq!(summary.corrupt_files.len(), 3);
}

#[test]
fn a_complete_json_value_without_a_newline_is_an_uncommitted_event() {
    let contents = serde_json::to_vec(&event(1, "execution_started")).unwrap();
    let summary = summarize_service_events(&contents, Path::new("events.jsonl"));

    assert_eq!(summary.status, ServiceSummaryStatus::Incomplete);
    assert_eq!(summary.events_total, Some(0));
    assert_eq!(summary.last_sequence, None);
    assert_eq!(summary.corrupt_files[0].line, Some(1));
    assert!(summary.corrupt_files[0].reason.contains("换行"));
}

#[test]
fn zero_sequence_and_empty_payload_type_are_rejected() {
    let summary = summarize_service_events(
        &journal(&[event(0, "execution_started"), event(1, " ")]),
        Path::new("events.jsonl"),
    );

    assert_eq!(summary.status, ServiceSummaryStatus::Incomplete);
    assert_eq!(summary.events_total, Some(0));
    assert_eq!(summary.corrupt_files.len(), 2);
}

#[test]
fn legacy_json_report_without_service_still_deserializes() {
    let legacy = json!({
        "sessions": [],
        "total_input_tokens": 0,
        "total_output_tokens": 0,
        "total_cached_input_tokens": 0,
        "total_wall_clock_ms": 0,
        "total_approvals": 0,
        "total_approval_wait_ms": 0,
        "total_cost_usd": null,
        "succeeded": 0,
        "failed": 0,
    });
    let report: BenchmarkReport = serde_json::from_value(legacy).expect("旧报表应继续可读");

    assert_eq!(report.service, ServiceSummary::default());
    assert_eq!(report.service.status, ServiceSummaryStatus::NotRead);
    assert_eq!(report.service.events_total, None);
    assert_eq!(report, BenchmarkReport::from_sessions(&[]));
    assert!(!render_text(&report).contains("常驻服务"));
}

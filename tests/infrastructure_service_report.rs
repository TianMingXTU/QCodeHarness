//! 服务元数据的目录收集测试：覆盖真实路径下的读取、快照枚举与损坏保留。

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use qcode_harness::application::report::{
    BenchmarkReport, ServiceSummaryStatus, render_json, render_text, service_summary_lines,
    summarize_service_events,
};
use qcode_harness::domain::service::PROTOCOL_VERSION;
use qcode_harness::infrastructure::service_report::{summarize_service};
use serde_json::{Value, json};

struct Workspace(PathBuf);

impl Workspace {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "qch-service-report-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).expect("创建测试目录");
        Self(root)
    }

    fn service(&self, events: &[u8], state: &Value) {
        let service = self.0.join(".qcodeharness/service");
        fs::create_dir_all(&service).expect("创建服务目录");
        fs::write(service.join("events.jsonl"), events).expect("写事件日志");
        fs::write(service.join("state.json"), state.to_string()).expect("写服务状态");
    }

    fn snapshot(&self, directory: &str, name: &str, contents: &[u8]) -> PathBuf {
        let directory = self.0.join(".qcodeharness").join(directory);
        fs::create_dir_all(&directory).expect("创建快照目录");
        let path = directory.join(name);
        fs::write(&path, contents).expect("写快照");
        path
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_dir_all(&self.0)
            && !std::thread::panicking()
        {
            panic!("清理测试目录失败: {error}");
        }
    }
}

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
fn summarize_service_is_exported_from_infrastructure() {
    // 依赖方向守卫：目录收集属于 infrastructure，经该模块公开。
    let workspace = Workspace::new();
    let summary = summarize_service(&workspace.0);
    assert_eq!(summary.status, ServiceSummaryStatus::NotPresent);
}

#[test]
fn absent_service_returns_an_explicit_empty_summary() {
    let workspace = Workspace::new();
    let summary = summarize_service(&workspace.0);

    assert_eq!(summary.status, ServiceSummaryStatus::NotPresent);
    assert_eq!(summary.events_total, None);
    assert_eq!(summary.first_sequence, None);
    assert_eq!(summary.last_sequence, None);
    assert_eq!(summary.sequence_gaps, None);
    assert_eq!(summary.status_counts, None);
    assert_eq!(summary.task_count, None);
    assert_eq!(summary.state_schema_version, None);
    assert_eq!(summary.task_version_counts, None);
    assert_eq!(summary.snapshot_version_counts, None);
    assert!(summary.corrupt_files.is_empty());
    assert_eq!(summary.state_path, Some(workspace.0.join(".qcodeharness/service/state.json")));
    assert_eq!(summary.events_path, Some(workspace.0.join(".qcodeharness/service/events.jsonl")));
    let text = service_summary_lines(&summary).join("\n");
    assert!(text.contains("not_present"), "{text}");
    assert!(text.contains("未知"), "{text}");
    assert!(!workspace.0.join(".qcodeharness").exists(), "报表读取不创建状态目录");
}

#[test]
fn continuous_events_and_independent_snapshot_versions_are_reported() {
    let workspace = Workspace::new();
    let events = journal(&[
        event(1, "execution_started"),
        event(2, "cancel_requested"),
        event(3, "execution_started"),
    ]);
    workspace.service(&events, &json!({"schema_version": 2}));
    workspace.snapshot("tasks", "task-a.json", br#"{"format_version":1}"#);
    workspace.snapshot("tasks", "task-b.json", br#"{"format_version":1}"#);
    workspace.snapshot("snapshots", "run-a.json", br#"{"format_version":4}"#);
    workspace.snapshot("snapshots", "run-b.json", br#"{"format_version":99}"#);
    workspace.snapshot("snapshots", "run-a.json.bak-v3", b"backup is not a snapshot");

    let summary = summarize_service(&workspace.0);

    assert_eq!(summary.status, ServiceSummaryStatus::Read);
    assert_eq!(summary.events_total, Some(3));
    assert_eq!((summary.first_sequence, summary.last_sequence), (Some(1), Some(3)));
    assert_eq!(summary.sequence_gaps, Some(0));
    let counts = summary.status_counts.as_ref().expect("事件类型计数");
    assert_eq!(counts["execution_started"], 2);
    assert_eq!(counts["cancel_requested"], 1);
    assert_eq!(summary.task_count, Some(2));
    assert_eq!(summary.state_schema_version, Some(2));
    assert_eq!(summary.task_version_counts.as_ref().expect("任务版本")[&1], 2);
    let versions = summary.snapshot_version_counts.as_ref().expect("运行版本");
    assert_eq!(versions[&4], 1);
    assert_eq!(versions[&99], 1, "报表盘点版本，不代替恢复逻辑拒绝未知快照版本");
    assert!(summary.corrupt_files.is_empty());

    let pure = summarize_service_events(&events, summary.events_path.as_deref().unwrap());
    assert_eq!(pure.events_total, summary.events_total);
    assert_eq!(pure.status_counts, summary.status_counts);
    let original = BenchmarkReport::from_sessions(&[]);
    let report = original.clone().with_service(summary.clone());
    assert_eq!(report.service, summary);
    assert_eq!(report.sessions, original.sessions);
    assert_eq!(report.total_cost_usd, original.total_cost_usd);
    let back: BenchmarkReport = serde_json::from_str(&render_json(&report).unwrap()).unwrap();
    assert_eq!(back, report);
    let text = render_text(&report);
    assert!(text.contains("常驻服务"), "{text}");
    assert!(text.contains("execution_started=2"), "{text}");
}

#[test]
fn sequence_gap_is_counted_and_has_a_file_and_line_diagnostic() {
    let workspace = Workspace::new();
    workspace.service(
        &journal(&[event(1, "execution_started"), event(3, "cancel_requested")]),
        &json!({}),
    );

    let summary = summarize_service(&workspace.0);

    assert_eq!(summary.status, ServiceSummaryStatus::Incomplete);
    assert_eq!(summary.events_total, Some(2));
    assert_eq!(summary.sequence_gaps, Some(1));
    let issue = summary.corrupt_files.first().expect("断号诊断");
    assert_eq!(issue.path, workspace.0.join(".qcodeharness/service/events.jsonl"));
    assert_eq!(issue.line, Some(2));
    assert!(issue.reason.contains("期望 2"), "{}", issue.reason);
}

#[test]
fn corrupt_task_and_run_snapshots_remain_visible() {
    let workspace = Workspace::new();
    workspace.service(b"", &json!({}));
    workspace.snapshot("tasks", "valid.json", br#"{"format_version":1}"#);
    let bad_task = workspace.snapshot("tasks", "bad.json", br#"{"format_version":"one"}"#);
    let bad_run = workspace.snapshot("snapshots", "broken.json", b"{truncated");
    let missing_version = workspace.snapshot("snapshots", "missing.json", b"{}");

    let summary = summarize_service(&workspace.0);

    assert_eq!(summary.status, ServiceSummaryStatus::Incomplete);
    assert_eq!(summary.task_count, Some(2), "文件数包括损坏文件，版本计数只包括可读文件");
    assert_eq!(summary.task_version_counts.as_ref().unwrap()[&1], 1);
    assert!(summary.snapshot_version_counts.as_ref().unwrap().is_empty());
    for path in [&bad_task, &bad_run, &missing_version] {
        let issue = summary.corrupt_files.iter().find(|issue| &issue.path == path).expect("损坏快照诊断");
        assert!(!issue.reason.is_empty());
        assert_eq!(issue.line, None);
    }
    assert_eq!(summary.corrupt_files.len(), 3);
    let text = service_summary_lines(&summary).join("\n");
    assert!(text.contains("bad.json"), "{text}");
    assert!(text.contains("format_version"), "{text}");
}

#[test]
fn legacy_state_defaults_its_missing_schema_version_to_zero() {
    let workspace = Workspace::new();
    workspace.service(b"", &json!({"tasks": {}}));
    workspace.snapshot("tasks", "ignored.txt", b"not JSON");
    workspace.snapshot("snapshots", "ignored.txt", b"not JSON");

    let summary = summarize_service(&workspace.0);

    assert_eq!(summary.status, ServiceSummaryStatus::Read);
    assert_eq!(summary.state_schema_version, Some(0));
    assert_eq!(summary.events_total, Some(0), "实际读到空日志才是零事件");
    assert_eq!(summary.sequence_gaps, Some(0));
    assert_eq!(summary.first_sequence, None);
    assert_eq!(summary.last_sequence, None);
    assert_eq!(summary.task_count, Some(0));
    assert!(summary.task_version_counts.as_ref().unwrap().is_empty());
    assert!(summary.snapshot_version_counts.as_ref().unwrap().is_empty());
    assert!(summary.corrupt_files.is_empty());
}

#[test]
fn missing_service_files_and_unreadable_snapshot_directory_are_reported() {
    let workspace = Workspace::new();
    fs::create_dir_all(workspace.0.join(".qcodeharness/service")).unwrap();
    fs::write(workspace.0.join(".qcodeharness/tasks"), b"not a directory").unwrap();

    let summary = summarize_service(&workspace.0);

    assert_eq!(summary.status, ServiceSummaryStatus::Incomplete);
    assert_eq!(summary.events_total, None);
    assert_eq!(summary.sequence_gaps, None);
    assert_eq!(summary.status_counts, None);
    assert_eq!(summary.task_count, None);
    assert_eq!(summary.state_schema_version, None);
    assert_eq!(summary.task_version_counts, None);
    assert_eq!(summary.snapshot_version_counts, None);
    assert_eq!(summary.corrupt_files.len(), 3);
}

#[test]
fn invalid_state_metadata_is_not_reported_as_a_legacy_version() {
    for state in [json!({"schema_version": "old"}), json!(null), json!([])] {
        let workspace = Workspace::new();
        workspace.service(b"", &state);
        let summary = summarize_service(&workspace.0);
        assert_eq!(summary.status, ServiceSummaryStatus::Incomplete);
        assert_eq!(summary.state_schema_version, None);
        assert_eq!(summary.corrupt_files.len(), 1);
        assert_eq!(summary.corrupt_files[0].path, workspace.0.join(".qcodeharness/service/state.json"));
    }
}

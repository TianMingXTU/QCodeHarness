//! 嵌套委派的协议级行为测试。
//!
//! 测试只看跨进程边界能观察到的帧与证据账本：深度边界、状态伪装、版本
//! 拒绝、以及每一级审批从请求到结论是否都留下显式记录。不假设父进程读过
//! 子进程的私有状态，也不引入真实子进程，避免把协议错误掩盖成调度问题。

use std::sync::Arc;

use qcode_harness::application::nested_delegation::{
    ApprovalFramePayload, ApprovalState, DelegationFrame, FRAME_VERSION, MAX_NESTING, frame_fact,
    frame_line, parse_frame, validate_nesting, validate_state,
};
use qcode_harness::application::service::EventJournal;
use qcode_harness::domain::ids::CallId;
use qcode_harness::domain::service::{EventContext, EventOrigin, ServiceEventPayload};
use qcode_harness::domain::tool::ToolCall;
use qcode_harness::infrastructure::nested_delegation::ApprovalFrameTransport;
use qcode_harness::infrastructure::service_events::JsonEventJournal;
use serde_json::{Value, json};

/// 协议帧所需的最小调用，避免把测试的重心转移到工具参数。
fn call() -> ToolCall {
    ToolCall {
        id: CallId::new("call-nested-1"),
        name: "write_file".into(),
        arguments: json!({"path": "nested.txt", "content": "approval"}),
    }
}

fn context() -> EventContext {
    EventContext {
        task_id: "task-nested-1".into(),
        session_id: "session-nested-1".into(),
        execution_id: "execution-nested-1".into(),
        origin: EventOrigin::default(),
    }
}

fn request(parent_id: &str, depth: usize) -> DelegationFrame {
    DelegationFrame::new(
        parent_id,
        depth,
        ApprovalState::Pending,
        ApprovalFramePayload::ApprovalRequest {
            approval_id: format!("approval-{depth}"),
            call: call(),
            reason: Some(format!("第 {depth} 级审批")),
        },
    )
    .unwrap()
}

/// 每个测试独立日志目录；事件账本只由 JsonEventJournal 写入。
fn journal() -> (Arc<JsonEventJournal>, TestDirectory) {
    let dir = TestDirectory::new();
    let journal = Arc::new(JsonEventJournal::open(dir.log()).unwrap());
    (journal, dir)
}

struct TestDirectory(std::path::PathBuf);

impl TestDirectory {
    fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let serial = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "qcode-r5-nested-{}-{stamp}-{serial}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn log(&self) -> std::path::PathBuf {
        self.0.join("events.jsonl")
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn depth_zero_one_and_two_are_allowed_and_depth_three_stops_derivation() {
    for depth in 0..MAX_NESTING {
        assert!(
            validate_nesting(depth).is_ok(),
            "深度 {depth} 应在有限上限内"
        );
        let frame = request("root", depth);
        assert!(frame_line(&frame).is_ok(), "合法深度必须可编码成协议帧");
        assert_eq!(parse_frame(&frame_line(&frame).unwrap()).unwrap(), frame);
    }
    assert_eq!(
        validate_nesting(MAX_NESTING).unwrap_err().code,
        "nesting_limit"
    );
    assert_eq!(
        validate_nesting(MAX_NESTING + 1).unwrap_err().code,
        "nesting_limit"
    );
    assert_eq!(
        DelegationFrame::new(
            "root",
            MAX_NESTING,
            ApprovalState::Pending,
            ApprovalFramePayload::ApprovalRequest {
                approval_id: "too-deep".into(),
                call: call(),
                reason: None,
            },
        )
        .unwrap_err()
        .code,
        "nesting_limit"
    );
}

#[test]
fn explicit_frame_cannot_silently_change_approval_state() {
    let raw = json!({
        "version": FRAME_VERSION,
        "parent_id": "root",
        "depth": 1,
        "approval_state": "granted",
        "payload": {
            "kind": "approval_request",
            "approval_id": "approval-1",
            "call": {
                "id": "call-nested-1",
                "name": "write_file",
                "arguments": {"path": "nested.txt"}
            },
            "reason": null
        }
    });
    let error = parse_frame(&raw.to_string()).unwrap_err();
    assert_eq!(error.code, "invalid_frame_state");
    let outdated = json!({
        "version": FRAME_VERSION + 1,
        "parent_id": "root",
        "depth": 1,
        "approval_state": "pending",
        "payload": {
            "kind": "approval_request",
            "approval_id": "approval-1",
            "call": {
                "id": "call-nested-1",
                "name": "write_file",
                "arguments": {}
            },
            "reason": null
        }
    });
    assert_eq!(
        parse_frame(&outdated.to_string()).unwrap_err().code,
        "unsupported_frame_version"
    );
    assert_eq!(
        parse_frame(&json!({"version": FRAME_VERSION, "depth": MAX_NESTING}).to_string())
            .unwrap_err()
            .code,
        "invalid_frame"
    );
    let frame = request("root", 1);
    let fact = frame_fact(&frame).unwrap();
    assert_eq!(fact["approval_state"], "pending");
    assert_eq!(fact["payload"]["kind"], "approval_request");
    assert_eq!(validate_state(&frame).unwrap(), ());
}

#[tokio::test]
async fn approval_chain_exchanges_request_and_resolution_frames_per_level() {
    let (journal, _dir) = journal();
    let transport = ApprovalFrameTransport::new(journal.clone());
    let frames = (0..MAX_NESTING)
        .map(|depth| request("root", depth))
        .collect::<Vec<_>>();
    let outcomes = transport
        .exchange(Some(&context()), frames.clone())
        .await
        .unwrap();
    assert_eq!(outcomes.len(), MAX_NESTING * 2, "每级应有一对显式协议帧");
    for (index, pair) in outcomes.chunks_exact(2).enumerate() {
        let request_line = frame_line(&pair[0]).unwrap();
        let granted_line = frame_line(&pair[1]).unwrap();
        assert_eq!(parse_frame(&request_line).unwrap(), pair[0]);
        assert_eq!(parse_frame(&granted_line).unwrap(), pair[1]);
        assert_eq!(pair[0].depth, index);
        assert_eq!(pair[0].approval_state, ApprovalState::Pending);
        assert_eq!(pair[1].depth, index);
        assert_eq!(pair[1].approval_state, ApprovalState::Granted);
        assert!(matches!(
            &pair[1].payload,
            ApprovalFramePayload::ApprovalGranted { approval_id } if approval_id == &format!("approval-{index}")
        ));
    }
    let events = journal.read(0, 100).unwrap();
    assert_eq!(events.len(), MAX_NESTING * 2);
    for index in 0..MAX_NESTING {
        let approval_id = format!("approval-{index}");
        assert!(events.iter().any(|event| matches!(
            &event.payload,
            ServiceEventPayload::ApprovalResolved { approval_id: id, allowed: true, .. } if id.as_str() == approval_id.as_str()
        )), "第 {index} 级放行结论必须写进服务事件日志");
    }
    let raw = std::fs::read_to_string(_dir.log()).unwrap();
    let lines = raw
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(lines.len(), MAX_NESTING * 2);
    assert!(lines.iter().any(|line| line["payload"]["type"] == "audit"));
    assert!(
        lines
            .iter()
            .any(|line| line["payload"]["type"] == "approval_resolved")
    );
}

#[tokio::test]
async fn over_limit_frame_is_rejected_with_evidence() {
    let (journal, _dir) = journal();
    let transport = ApprovalFrameTransport::new(journal.clone());
    let mut frame = request("root", MAX_NESTING - 1);
    frame.depth = MAX_NESTING;
    let error = transport
        .exchange(Some(&context()), vec![frame])
        .await
        .unwrap_err();
    assert_eq!(error.code, "nesting_limit");
    let events = journal.read(0, 100).unwrap();
    assert!(events.iter().any(|event| matches!(
        &event.payload,
        ServiceEventPayload::Audit { event } if matches!(event, qcode_harness::domain::evidence::AuditEvent::PolicyEvaluated { .. })
    )), "拒绝继续派生也必须走证据账本");
}

//! 自动记忆：跨会话文件写入的行为验收。
//!
//! 写入是副作用，必须先过审批闸门；通过后 MEMORY.md 只放一行索引，事实
//! 正文进入独立文件，同一条证据账本能还原请求、审批、观察这三步。
//! 每个用例使用独立临时目录，不碰用户真实 MEMORY.md。

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use qcode_harness::application::memory::{AutoMemory, MEMORY_WRITE_TOOL, MemoryWriteError};
use qcode_harness::application::port::{
    ApprovalGate, ApprovalRequest, BoxFuture, Decision, Timestamp,
};
use qcode_harness::domain::evidence::AuditEvent;
use qcode_harness::domain::memory::{MemoryEntry, MemoryFileError};
use qcode_harness::infrastructure::evidence::jsonl::JsonlEvidenceSink;
use qcode_harness::infrastructure::memory::FileMemoryWriter;
use serde_json::Value;

/// 每个用例独占临时目录，避免并行时互相覆盖索引。
fn temp_dir(name: &str) -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "qch-auto-memory-{name}-{}-{stamp}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("建临时目录");
    dir
}

fn entry() -> MemoryEntry {
    MemoryEntry {
        slug: "qch-r4-memory".to_owned(),
        summary: "R4 自动记忆写入需要审批".to_owned(),
        fact: "写入前记录 ToolRequested，闸门给出结论后写 ApprovalDecided，落盘成功才写 ToolObserved。".to_owned(),
    }
}

fn evidence_events(path: &Path) -> Vec<AuditEvent> {
    fs::read_to_string(path)
        .expect("证据文件存在")
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let record: Value = serde_json::from_str(line).expect("每行是证据 JSON");
            serde_json::from_value(record["event"].clone()).expect("事件可反序列化")
        })
        .collect()
}

#[derive(Clone, Copy)]
enum Verdict {
    Allow,
    Deny,
}

/// 固定结论的审批闸门，记录请求次数以确认拒绝路径确实问过人。
struct FixedGate {
    verdict: Verdict,
    requests: Arc<std::sync::atomic::AtomicUsize>,
}

impl ApprovalGate for FixedGate {
    fn request<'a>(&'a self, _request: &'a ApprovalRequest) -> BoxFuture<'a, Decision> {
        self.requests
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let verdict = self.verdict;
        Box::pin(async move {
            match verdict {
                Verdict::Allow => Ok(()),
                Verdict::Deny => Err("用户拒绝写入记忆".to_owned()),
            }
        })
    }
}

#[tokio::test]
async fn approved_memory_writes_one_fact_file_and_an_auditable_index_line() {
    let dir = temp_dir("approved");
    let evidence_path = dir.join("evidence.jsonl");
    let requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let gate = Arc::new(FixedGate {
        verdict: Verdict::Allow,
        requests: Arc::clone(&requests),
    });
    let sink = JsonlEvidenceSink::open(&evidence_path).expect("证据 sink 可打开");
    let memory = AutoMemory::new(Arc::new(FileMemoryWriter), gate, Arc::new(sink), || {
        Timestamp::UNIX_EPOCH
    });

    let file = memory
        .remember(&dir, entry(), 7)
        .await
        .expect("审批通过应写盘");

    assert_eq!(requests.load(std::sync::atomic::Ordering::SeqCst), 1);
    let index = fs::read_to_string(dir.join("MEMORY.md")).expect("索引文件存在");
    assert!(index.contains("# Memory Index"));
    assert!(index.contains(&file.index_line().render()));
    assert_eq!(
        index.lines().filter(|line| line.starts_with("- [")).count(),
        1
    );
    let fact = fs::read_to_string(dir.join("qch-r4-memory.md")).expect("事实文件存在");
    assert_eq!(fact, entry().fact);

    let events = evidence_events(&evidence_path);
    assert!(events.iter().any(|event| matches!(
        event,
        AuditEvent::ToolRequested { turn: 7, call } if call.name == MEMORY_WRITE_TOOL
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        AuditEvent::ApprovalDecided {
            turn: 7,
            allowed: true,
            ..
        }
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        AuditEvent::ToolObserved { turn: 7, outcome, .. } if outcome.status == qcode_harness::domain::tool::OutcomeStatus::Success
    )));
    let _ = fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn denied_memory_never_writes_a_file_and_records_the_denial() {
    let dir = temp_dir("denied");
    let evidence_path = dir.join("evidence.jsonl");
    let requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let gate = Arc::new(FixedGate {
        verdict: Verdict::Deny,
        requests: Arc::clone(&requests),
    });
    let sink = JsonlEvidenceSink::open(&evidence_path).expect("证据 sink 可打开");
    let memory = AutoMemory::new(Arc::new(FileMemoryWriter), gate, Arc::new(sink), || {
        Timestamp::UNIX_EPOCH
    });

    let error = memory
        .remember(&dir, entry(), 8)
        .await
        .expect_err("拒绝后不能写盘");

    assert_eq!(
        error,
        MemoryWriteError::Rejected("用户拒绝写入记忆".to_owned())
    );
    assert_eq!(requests.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert!(!dir.join("MEMORY.md").exists(), "拒绝后不得创建索引");
    assert!(
        !dir.join("qch-r4-memory.md").exists(),
        "拒绝后不得创建事实文件"
    );
    let events = evidence_events(&evidence_path);
    assert!(events.iter().any(|event| matches!(
        event,
        AuditEvent::ApprovalDecided { turn: 8, allowed: false, reason: Some(reason), .. }
            if reason == "用户拒绝写入记忆"
    )));
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, AuditEvent::ToolObserved { .. }))
    );
    let _ = fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn malformed_fact_is_rejected_before_approval_or_disk_write() {
    let dir = temp_dir("malformed");
    let evidence_path = dir.join("evidence.jsonl");
    let requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let gate = Arc::new(FixedGate {
        verdict: Verdict::Allow,
        requests: Arc::clone(&requests),
    });
    let sink = JsonlEvidenceSink::open(&evidence_path).expect("证据 sink 可打开");
    let memory = AutoMemory::new(Arc::new(FileMemoryWriter), gate, Arc::new(sink), || {
        Timestamp::UNIX_EPOCH
    });
    let mut invalid = entry();
    invalid.fact = " ".to_owned();

    let error = memory
        .remember(&dir, invalid, 9)
        .await
        .expect_err("空事实须拒绝");

    assert_eq!(error, MemoryWriteError::Invalid(MemoryFileError::EmptyFact));
    assert_eq!(requests.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert!(!dir.join("MEMORY.md").exists());
    assert!(
        evidence_path
            .metadata()
            .map(|meta| meta.len() == 0)
            .unwrap_or(true)
    );
    let _ = fs::remove_dir_all(&dir);
}

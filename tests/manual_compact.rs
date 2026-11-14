//! 手动压缩 `/compact` 的行为验收。
//!
//! 用户可见行为：自动 token 预算压缩之外，手动入口按同一份会话历史生成
//! 摘要，并把摘要正文、覆盖轮次与裁剪账目写入证据账本；拒绝手工编造数字。
//! 测试不依赖真实模型，摘要文本由调用方明确提供，证据落到独立临时目录。

use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use qcode_harness::application::compact::{ManualCompact, ManualCompactError};
use qcode_harness::application::port::Timestamp;
use qcode_harness::domain::agent::AgentTurn;
use qcode_harness::domain::evidence::AuditEvent;
use qcode_harness::domain::ids::{CallId, SessionId};
use qcode_harness::domain::session::Session;
use qcode_harness::domain::tool::{ToolCall, ToolOutcome};
use qcode_harness::infrastructure::evidence::jsonl::JsonlEvidenceSink;
use serde_json::Value;

/// 每个用例独占临时目录，避免并行时共用证据文件。
fn temp_evidence(name: &str) -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "qch-manual-compact-{name}-{}-{stamp}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("建临时目录");
    dir.join("evidence.jsonl")
}

/// 两个完整轮次，足够让摘要有真实覆盖范围。
fn session_with_two_rounds() -> Session {
    let mut session = Session::new(SessionId::new("session-compact"), "完成 R4 手动压缩");
    for index in 1..=2 {
        session.push_user(format!("第 {index} 步"));
        let call = ToolCall {
            id: CallId::new(format!("call-{index}")),
            name: "read_file".to_owned(),
            arguments: serde_json::json!({"path": format!("src/{index}.rs")}),
        };
        session
            .commit_round(
                AgentTurn {
                    text: format!("第 {index} 轮读过源码"),
                    calls: vec![call.clone()],
                },
                vec![ToolOutcome::success(format!("读过 src/{index}.rs"))],
            )
            .expect("完整轮次可提交");
    }
    session
}

fn evidence_lines(path: &PathBuf) -> Vec<Value> {
    fs::read_to_string(path)
        .expect("证据文件存在")
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("每行都是证据 JSON"))
        .collect()
}

/// 手动压缩后摘要可找回关键上下文，裁剪范围与 token 账目都来自实际会话视图。
#[test]
fn manual_compact_writes_an_auditable_summary_and_compaction_ledger() {
    let path = temp_evidence("writes");
    let sink = JsonlEvidenceSink::open(&path).expect("证据 sink 可打开");
    let manual = ManualCompact::new(sink, || Timestamp::UNIX_EPOCH);
    let mut session = session_with_two_rounds();

    let report = manual
        .compact(
            &mut session,
            "前面两轮读了 src/1.rs 与 src/2.rs，关键约束是保留审批链",
            3,
        )
        .expect("手动压缩成功");

    assert_eq!(report.summary.covers, (1, 2));
    assert!(report.summary.text.contains("src/1.rs"));
    assert_eq!(session.summary(), Some(&report.summary));
    assert_eq!(report.dropped, vec![1, 2]);
    assert_eq!(report.rounds_before, 2);
    assert_eq!(report.rounds_after, 0);
    assert!(report.dropped_tokens > 0, "手动压缩应记下真实省下的 token");

    let lines = evidence_lines(&path);
    let events: Vec<AuditEvent> = lines
        .iter()
        .map(|line| serde_json::from_value(line["event"].clone()).expect("事件可反序列化"))
        .collect();
    assert!(events.iter().any(|event| matches!(
        event,
        AuditEvent::SummaryGenerated {
            turn: 3,
            covers: (1, 2),
            ..
        }
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        AuditEvent::ContextCompacted { turn: 3, rounds_before: 2, rounds_after: 0, dropped }
            if dropped == &[1, 2]
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        AuditEvent::ContextBudgeted { turn: 3, dropped_tokens, .. } if *dropped_tokens > 0
    )));
    assert_eq!(
        events.len(),
        3,
        "手动入口不额外调用模型，不记第二份摘要用量"
    );
    let _ = fs::remove_dir_all(path.parent().unwrap());
}

/// 摘要正文是审计对象，空摘要不能把历史裁掉后留下不可解释的空白。
#[test]
fn an_empty_manual_summary_is_rejected_without_mutating_history_or_evidence() {
    let path = temp_evidence("empty");
    let sink = JsonlEvidenceSink::open(&path).expect("证据 sink 可打开");
    let manual = ManualCompact::new(sink, || Timestamp::UNIX_EPOCH);
    let mut session = session_with_two_rounds();
    let before = session.snapshot().turns.clone();

    let error = manual
        .compact(&mut session, "  ", 4)
        .expect_err("空摘要须拒绝");

    assert!(matches!(error, ManualCompactError::EmptySummary));
    assert_eq!(session.snapshot().turns, before);
    assert_eq!(session.summary(), None);
    assert!(
        path.metadata()
            .map(|metadata| metadata.len() == 0)
            .unwrap_or(true),
        "失败路径不产生伪造的审计事件"
    );
    let _ = if path.parent().unwrap().exists() {
        fs::remove_dir_all(path.parent().unwrap())
    } else {
        Ok(())
    };
}

/// 手动入口只记录已发生的压缩，不从空历史里凑一个摘要范围。
#[test]
fn manual_compact_refuses_a_session_without_completed_rounds() {
    let path = temp_evidence("no-rounds");
    let sink = JsonlEvidenceSink::open(&path).expect("证据 sink 可打开");
    let manual = ManualCompact::new(sink, || Timestamp::UNIX_EPOCH);
    let mut session = Session::new(SessionId::new("session-empty"), "空会话");

    let error = manual
        .compact(&mut session, "无历史可压缩", 1)
        .expect_err("空会话须拒绝");

    assert!(matches!(error, ManualCompactError::NoCompletedRounds));
    assert_eq!(session.summary(), None);
    assert!(
        path.metadata()
            .map(|metadata| metadata.len() == 0)
            .unwrap_or(true)
    );
    let _ = if path.parent().unwrap().exists() {
        fs::remove_dir_all(path.parent().unwrap())
    } else {
        Ok(())
    };
}

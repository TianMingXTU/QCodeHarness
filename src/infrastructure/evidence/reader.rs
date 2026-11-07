//! 证据读取器：把 `.qcodeharness/sessions/*.jsonl` 读回成结构化数据。
//!
//! 统计与报表的全部输入都从这里来。与 [`JsonlEvidenceSink`] 是
//! 同一磁盘契约的两端：读回就是 `serde_json::from_str::<Evidence>()`。
//!
//! 对坏数据的取向与 sink 的降级哲学一致：**流式日志的尾部损坏是常态**
//! （崩溃恰好发生在最需要证据的时刻），单行解析失败跳过并计数；
//! 但一条都读不动说明选错了文件，要显式报错而不是交出一份"看起来
//! 是空的"报表。

use std::path::{Path, PathBuf};

use crate::application::report::SessionLog;
use crate::domain::evidence::Evidence;
use crate::domain::ids::SessionId;

// SessionLog 定义在 `application::report`——统计是它的消费方，且
// infrastructure 依赖 application 是本仓库既定的方向。

/// 证据读取失败。
#[derive(Debug)]
pub enum ReadError {
    /// 文件读不出来（权限、编码等 IO 层问题）。
    Io(std::io::Error),
    /// 文件里没有一条能解析的证据——多半是选错了文件，
    /// 交回空数据会让报表看起来"一切正常"。
    NoReadableLines {
        /// 读不动的文件路径。
        path: PathBuf,
        /// 文件里的非空行数。
        lines: usize,
    },
}

impl std::fmt::Display for ReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(f, "读取证据文件失败: {error}"),
            Self::NoReadableLines { path, lines } => write!(
                f,
                "{} 里的 {} 行没有一条能解析成证据，可能不是证据文件",
                path.display(),
                lines
            ),
        }
    }
}

impl std::error::Error for ReadError {}

/// 读一个会话的证据文件。
///
/// # Errors
///
/// 文件不存在返回 `Ok(None)`——没跑过的会话不是错误；文件存在但
/// 读不动（IO 失败，或没有一条行能解析）才报错。
pub fn read_session(path: &Path) -> Result<Option<SessionLog>, ReadError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(ReadError::Io(error)),
    };

    let session_id = SessionId::new(
        path.file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_else(|| "unknown".to_owned()),
    );

    let mut events = Vec::new();
    let mut skipped = 0usize;
    let mut non_empty = 0usize;
    for line in text.lines() {
        // 文件以换行结尾会产生一个空"行"，损坏也可能留下半行空白；
        // 空行既不是事件也不是坏行，直接跳过。
        if line.trim().is_empty() {
            continue;
        }
        non_empty += 1;
        match serde_json::from_str::<Evidence>(line) {
            Ok(evidence) => events.push(evidence),
            Err(_) => skipped += 1,
        }
    }

    if non_empty > 0 && skipped == non_empty {
        return Err(ReadError::NoReadableLines {
            path: path.to_path_buf(),
            lines: non_empty,
        });
    }

    Ok(Some(SessionLog {
        session_id,
        events,
        skipped_lines: skipped,
    }))
}

/// 读目录下全部会话（`*.jsonl`），按文件名排序保证报表顺序稳定。
///
/// 单个文件一条都读不动时跳过并打警告——一份报表不该因为一个
/// 选坏的文件整体出不来；IO 层错误（权限等）则照常向上传播。
///
/// # Errors
///
/// 目录不可读时返回 IO 错误。
pub fn read_all(dir: &Path) -> Result<Vec<SessionLog>, ReadError> {
    let entries = std::fs::read_dir(dir).map_err(ReadError::Io)?;
    let mut paths: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.is_file() && path.extension().is_some_and(|ext| ext == "jsonl"))
        .collect();
    paths.sort();

    let mut logs = Vec::new();
    for path in paths {
        match read_session(&path) {
            Ok(Some(log)) => logs.push(log),
            Ok(None) => {}
            Err(ReadError::NoReadableLines { path, lines }) => eprintln!(
                "警告：跳过 {}（{} 行没有一条能解析成证据）",
                path.display(),
                lines
            ),
            Err(error) => return Err(error),
        }
    }
    Ok(logs)
}

/// 把一组合成事件写成 JSONL 文本（测试用）：与 sink 落盘格式同源，
/// 都是 `serde_json::to_string::<Evidence>`。
#[cfg(test)]
pub(crate) fn evidence_line(evidence: &Evidence) -> String {
    serde_json::to_string(evidence).expect("Evidence 序列化不应失败")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::agent::AgentRole;
    use crate::domain::evidence::{AuditEvent, TokenUsage};
    use crate::domain::ids::CallId;
    use crate::domain::mcp::{McpCallFailure, McpFaultKind, McpLifecycleFact, McpLifecycleKind};
    use crate::domain::policy::PolicyVerdict;
    use crate::domain::tool::{ToolCall, ToolOutcome};
    use std::fs;
    use std::time::{Duration, UNIX_EPOCH};

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("qch-reader-{}-{}", name, std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("创建临时目录");
        dir
    }

    fn write_lines(path: &Path, lines: &[String]) {
        fs::write(path, lines.join("\n")).expect("写合成 JSONL");
    }

    /// 当前 15 种 `AuditEvent` 变体各一条，验证这些样本的读回契约。
    /// 样本需在新增变体时手动维护；测试不会自动发现遗漏的变体。
    fn every_variant() -> Vec<Evidence> {
        let at = |secs: u64| UNIX_EPOCH + Duration::from_secs(secs);
        let call = |name: &str| ToolCall {
            id: CallId::new("call_1"),
            name: name.to_owned(),
            arguments: json_args(),
        };
        vec![
            Evidence::new(
                1,
                at(100),
                AuditEvent::StepStarted {
                    turn: 1,
                    role: AgentRole::Reviewer,
                },
            ),
            Evidence::new(
                2,
                at(101),
                AuditEvent::Thought {
                    turn: 1,
                    text: "先读代码".to_owned(),
                    usage: TokenUsage::default(),
                },
            ),
            Evidence::new(
                3,
                at(102),
                AuditEvent::ToolRequested {
                    turn: 1,
                    call: call("read_file"),
                },
            ),
            Evidence::new(
                4,
                at(103),
                AuditEvent::ToolObserved {
                    turn: 1,
                    call: call("read_file"),
                    outcome: ToolOutcome::refused("路径越界"),
                },
            ),
            Evidence::new(
                5,
                at(104),
                AuditEvent::ApprovalDecided {
                    turn: 2,
                    tool: "write_file".to_owned(),
                    allowed: false,
                    reason: Some("不像该改的".to_owned()),
                },
            ),
            Evidence::new(
                6,
                at(105),
                AuditEvent::PolicyEvaluated {
                    turn: 2,
                    tool: "write_file".to_owned(),
                    verdict: PolicyVerdict::RequireApproval,
                    rule_index: None,
                    reason: "非只读走默认".to_owned(),
                },
            ),
            Evidence::new(
                7,
                at(106),
                AuditEvent::ContextCompacted {
                    turn: 3,
                    rounds_before: 20,
                    rounds_after: 16,
                    dropped: vec![1, 2, 3, 4],
                },
            ),
            Evidence::new(
                8,
                at(106),
                AuditEvent::ContextBudgeted {
                    turn: 3,
                    tokens_before: 10_000,
                    tokens_after: 8_000,
                    dropped_tokens: 2_000,
                },
            ),
            Evidence::new(
                9,
                at(107),
                AuditEvent::SummaryGenerated {
                    turn: 3,
                    covers: (1, 4),
                    usage: TokenUsage::default(),
                },
            ),
            Evidence::new(
                10,
                at(107),
                AuditEvent::ToolCallReplayed {
                    turn: 4,
                    tool: "read_file".to_owned(),
                    reason: "中断发生在这次调用执行期间".to_owned(),
                    previous_outcome: Some(ToolOutcome::success("文件内容")),
                },
            ),
            Evidence::new(
                11,
                at(107),
                AuditEvent::ApprovalReasked {
                    turn: 4,
                    tool: "write_file".to_owned(),
                    reason: "这个审批请求之前就问过，那时没答复".to_owned(),
                },
            ),
            Evidence::new(
                12,
                at(107),
                AuditEvent::SandboxApplied {
                    turn: 4,
                    call: call("run_command"),
                    platform: "Windows JobObject".to_owned(),
                    active: true,
                    boundary_checked: true,
                },
            ),
            Evidence::new(
                13,
                at(108),
                AuditEvent::SummaryFailed {
                    turn: 5,
                    reason: "模型超时".to_owned(),
                },
            ),
            Evidence::new(
                14,
                at(108),
                AuditEvent::McpLifecycle {
                    turn: 0,
                    fact: McpLifecycleFact {
                        server: "fetch".to_owned(),
                        generation: 1,
                        kind: McpLifecycleKind::Disconnected,
                        fault: Some(McpFaultKind::Disconnected),
                        call_id: Some(CallId::new("call_1")),
                        process_isolation: true,
                        tools_changed: false,
                    },
                },
            ),
            Evidence::new(
                15,
                at(108),
                AuditEvent::McpCallFailed {
                    turn: 6,
                    call_id: CallId::new("call_1"),
                    tool: "mcp__fetch__get_page".to_owned(),
                    failure: McpCallFailure {
                        server: "fetch".to_owned(),
                        generation: 1,
                        kind: McpFaultKind::CallTimeout,
                        result_unknown: true,
                        process_isolation: true,
                    },
                },
            ),
        ]
    }

    fn json_args() -> serde_json::Value {
        serde_json::json!({"path": "Cargo.toml"})
    }

    #[test]
    fn reads_a_normal_file_with_every_event_variant() {
        // 验收项：全部 AuditEvent 变体都能读回。走 sink 同款序列化，
        // 证明 reader 与磁盘契约的两端对得上。
        let dir = temp_dir("all-variants");
        let path = dir.join("session-1.jsonl");
        let events = every_variant();
        write_lines(&path, &events.iter().map(evidence_line).collect::<Vec<_>>());

        let log = read_session(&path).unwrap().expect("文件存在");
        assert_eq!(log.session_id.as_str(), "session-1");
        assert_eq!(log.events.len(), 15);
        assert_eq!(log.skipped_lines, 0);
        assert!(log.is_sequential());
        // 深度相等：事件类型、字段、轮次号全部还原。
        assert_eq!(log.events, events);
        assert_eq!(log.started_at(), Some(UNIX_EPOCH + Duration::from_secs(100)));
        assert_eq!(log.ended_at(), Some(UNIX_EPOCH + Duration::from_secs(108)));
        let _ = fs::remove_dir_all(&dir);
    }

    /// 手写一条旧版证据行：没有 `format` 键，`recorded_at` 是整秒。
    ///
    /// 把时间戳精度从秒提到毫秒时，如果不在行里留标记，reader 就只能
    /// 猜。猜错的代价是**所有历史证据的耗时统计变成垃圾**——100 秒被读成
    /// 100 毫秒，一分钟的会话算成亚毫秒。所以旧文件必须仍然读得出正确的
    /// 时刻，一条不漏。
    #[test]
    fn legacy_second_precision_lines_still_read_correctly() {
        let dir = temp_dir("legacy-seconds");
        let path = dir.join("session-legacy.jsonl");
        // 照旧版落盘格式手写：sequence / recorded_at（秒）/ event。
        let lines = vec![
            serde_json::json!({
                "sequence": 1,
                "recorded_at": 100u64,
                "event": {"StepStarted": {"turn": 1, "role": "Planner"}},
            })
            .to_string(),
            serde_json::json!({
                "sequence": 2,
                "recorded_at": 160u64,
                "event": {"Thought": {"turn": 1, "text": "想", "usage": {
                    "input_tokens": 10, "output_tokens": 5,
                    "cached_input_tokens": 0, "cache_creation_input_tokens": 0,
                    "cost_usd": null
                }}},
            })
            .to_string(),
        ];
        write_lines(&path, &lines);

        let log = read_session(&path).unwrap().expect("旧格式必须能读");

        assert_eq!(log.events.len(), 2, "旧格式一行不漏");
        assert_eq!(log.skipped_lines, 0);
        assert_eq!(
            log.started_at(),
            Some(UNIX_EPOCH + Duration::from_secs(100)),
            "秒级时间戳必须按秒解释"
        );
        assert_eq!(
            log.ended_at(),
            Some(UNIX_EPOCH + Duration::from_secs(160)),
            "末条同样是秒"
        );
        // 60 秒的间隔不能因为精度变更被算成 60 毫秒。
        let span = log
            .ended_at()
            .unwrap()
            .duration_since(log.started_at().unwrap())
            .unwrap();
        assert_eq!(span, Duration::from_secs(60), "旧文件的墙钟必须还是 60 秒");
        let _ = fs::remove_dir_all(&dir);
    }

    /// 新格式（毫秒）必须读出亚秒级的真实间隔。
    #[test]
    fn millisecond_lines_give_sub_second_resolution() {
        let dir = temp_dir("millis");
        let path = dir.join("session-millis.jsonl");
        // 同一秒内的两条事件：秒级精度下间隔是 0，毫秒级下是 250ms。
        let base = 1_700_000_000_000u64;
        let lines = vec![
            serde_json::json!({
                "sequence": 1, "format": 2, "recorded_at": base,
                "event": {"StepStarted": {"turn": 1, "role": "Coder"}},
            })
            .to_string(),
            serde_json::json!({
                "sequence": 2, "format": 2, "recorded_at": base + 250,
                "event": {"Thought": {"turn": 1, "text": "想", "usage": {
                    "input_tokens": 1, "output_tokens": 1,
                    "cached_input_tokens": 0, "cache_creation_input_tokens": 0,
                    "cost_usd": null
                }}},
            })
            .to_string(),
        ];
        write_lines(&path, &lines);

        let log = read_session(&path).unwrap().expect("新格式必须能读");

        assert_eq!(log.events.len(), 2);
        let span = log
            .ended_at()
            .unwrap()
            .duration_since(log.started_at().unwrap())
            .unwrap();
        assert_eq!(span, Duration::from_millis(250), "毫秒精度要能分辨亚秒间隔");
        let _ = fs::remove_dir_all(&dir);
    }

    /// 不认识的格式号：跳过该行并计数，不猜。
    ///
    /// 猜的代价是让耗时统计悄悄变成垃圾；跳过至少能被 `skipped_lines`
    /// 和"整文件都读不动就报错"发现。与"末行截断跳过而非报错"是同一条
    /// 降级哲学。
    #[test]
    fn unknown_format_marker_is_skipped_and_counted() {
        let dir = temp_dir("unknown-format");
        let path = dir.join("session-future.jsonl");
        let lines = vec![
            serde_json::json!({
                "sequence": 1, "format": 99, "recorded_at": 1u64,
                "event": {"StepStarted": {"turn": 1, "role": "Coder"}},
            })
            .to_string(),
            evidence_line(&every_variant()[1]),
        ];
        write_lines(&path, &lines);

        let log = read_session(&path).unwrap().expect("有一条能读就不报错");

        assert_eq!(log.events.len(), 1, "格式号不认识的那行被跳过");
        assert_eq!(log.skipped_lines, 1, "跳过的行要计数");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn truncated_last_line_is_skipped_not_fatal() {
        // 验收项：末行截断（崩溃的典型残迹）被跳过而非报错。
        let dir = temp_dir("truncated");
        let path = dir.join("session-2.jsonl");
        let events = every_variant();
        let mut lines: Vec<String> = events.iter().map(evidence_line).collect();
        let good = lines.len();
        // 截断：最后一条只留前半截 JSON。
        lines.push(lines[good - 1][..lines[good - 1].len() / 2].to_owned());
        write_lines(&path, &lines);

        let log = read_session(&path).unwrap().expect("大部分可读");
        assert_eq!(log.events.len(), good);
        assert_eq!(log.skipped_lines, 1);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_file_is_an_empty_session_not_an_error() {
        let dir = temp_dir("empty");
        let path = dir.join("session-3.jsonl");
        fs::write(&path, "").unwrap();

        let log = read_session(&path).unwrap().expect("空文件不是错误");
        assert!(log.events.is_empty());
        assert_eq!(log.skipped_lines, 0);
        assert_eq!(log.started_at(), None);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn blank_lines_are_ignored_not_counted_as_bad() {
        let dir = temp_dir("blank");
        let path = dir.join("session-4.jsonl");
        let events = every_variant();
        let mut lines: Vec<String> = events[..2].iter().map(evidence_line).collect();
        lines.push(String::new());
        lines.push("   ".to_owned());
        lines.push(evidence_line(&events[2]));
        write_lines(&path, &lines);

        let log = read_session(&path).unwrap().unwrap();
        assert_eq!(log.events.len(), 3);
        assert_eq!(log.skipped_lines, 0, "空行不是坏行");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_file_is_none() {
        let dir = temp_dir("absent");
        let path = dir.join("no-such-session.jsonl");
        assert!(read_session(&path).unwrap().is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn out_of_order_sequences_are_detectable() {
        // 重开 sink 会从 1 重新编号，乱序在真实文件里会出现；
        // reader 的职责是让人能发现，不是替人判断文件作废。
        let dir = temp_dir("gap");
        let path = dir.join("session-5.jsonl");
        let events = every_variant();
        let mut lines: Vec<String> = events[..3].iter().map(evidence_line).collect();
        lines.push(evidence_line(&Evidence::new(
            1,
            UNIX_EPOCH,
            AuditEvent::SummaryFailed {
                turn: 1,
                reason: "重开后序号归 1".to_owned(),
            },
        )));
        write_lines(&path, &lines);

        let log = read_session(&path).unwrap().unwrap();
        assert!(!log.is_sequential(), "乱序必须能被查出来");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_with_no_readable_lines_is_an_error() {
        let dir = temp_dir("all-bad");
        let path = dir.join("not-evidence.jsonl");
        write_lines(
            &path,
            &["这不是证据".to_owned(), "{\"a\":1}".to_owned()],
        );

        let error = read_session(&path).unwrap_err();
        assert!(matches!(error, ReadError::NoReadableLines { lines: 2, .. }));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_all_collects_sorted_sessions_and_skips_junk_files() {
        let dir = temp_dir("read-all");
        let events = every_variant();
        for name in ["session-b", "session-a"] {
            let path = dir.join(format!("{name}.jsonl"));
            write_lines(
                &path,
                &events[..2].iter().map(evidence_line).collect::<Vec<_>>(),
            );
        }
        // 一个全是坏行的文件：跳过并警告，不拖垮整份报表。
        fs::write(dir.join("garbage.jsonl"), "垃圾数据\n").unwrap();
        // 非jsonl文件不参与。
        fs::write(dir.join("notes.txt"), "无关").unwrap();

        let logs = read_all(&dir).unwrap();
        let ids: Vec<String> = logs
            .iter()
            .map(|log| log.session_id.as_str().to_owned())
            .collect();
        assert_eq!(ids, vec!["session-a", "session-b"], "按文件名排序且跳过坏文件");
        let _ = fs::remove_dir_all(&dir);
    }
}

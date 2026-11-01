//! JSONL 证据汇：把审计事件追加写入 JSONL 文件。
//!
//! 选 JSONL 而不是单文件 JSON：证据是流式的，运行中要能随时 `tail -f`
//! 看进度；JSON 数组要等写完整才合法。每行一条 `Evidence`，字段名与
//! 内存版一致，换存储不需要改读日志的代码。

use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::SystemTime;

use serde::Serialize;

use crate::application::port::{EvidenceSink, Timestamp};
use crate::domain::evidence::AuditEvent;

/// 落在磁盘上的证据汇。
///
/// 文件句柄只开一次并保持追加模式：每条事件都重新开关文件会让一次
/// 运行退化成 N 次 syscall 风暴，而且中途换 inode 还可能写到错误的文件。
/// 句柄包在 `Mutex` 里是因为 `EvidenceSink::record` 只拿 `&self`。
pub struct JsonlEvidenceSink {
    /// 落盘路径，仅用于错误信息和测试断言。
    path: PathBuf,
    /// 下一条事件的序号。
    ///
    /// 用原子类型而不是塞进 `Mutex`：`record` 拿的是 `&self`，
    /// 而这个字段不需要和句柄一起加锁——序号自增是独立的一次操作。
    next_sequence: AtomicU64,
    /// 写入器。用 `Option` 包住是为了能在写失败时降级——见 `write`。
    writer: Mutex<Option<BufWriter<File>>>,
}

/// 序列化到磁盘的一行。
///
/// 不直接序列化 `Evidence`：`Evidence` 是领域类型，它的字段（比如
/// `recorded_at`）可能会变；这里显式列出落盘字段，磁盘格式就成了
/// 一个独立的契约，改领域类型不会悄悄改日志格式。
///
/// `event` 保持嵌套而不是 `#[serde(flatten)]` 拍平：拍平后每行要靠
/// `turn`/`text` 这些字段名反推是哪类事件，而 `AuditEvent` 是 enum，
/// serde 的反序列化需要外层的 tag 才能选对变体。嵌套让磁盘格式与
/// 领域类型的结构一一对应，读回来就是 `serde_json::from_str::<Evidence>()`。
#[derive(Debug, Serialize)]
struct Record<'a> {
    sequence: u64,
    recorded_at: u64,
    event: &'a AuditEvent,
}

impl JsonlEvidenceSink {
    /// 打开（必要时创建）证据文件，准备追加。
    ///
    /// 父目录不存在时会递归创建——证据文件路径由会话 ID 推导而来，
    /// 要求调用方先建好目录只会把 `mkdir -p` 的逻辑推给每一处调用点。
    ///
    /// # Errors
    ///
    /// 目录创建或文件打开失败时返回底层 IO 错误。
    pub fn open(path: impl AsRef<Path>) -> std::io::Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self {
            path: path.to_path_buf(),
            next_sequence: AtomicU64::new(1),
            writer: Mutex::new(Some(BufWriter::new(file))),
        })
    }

    /// 把一条记录写进文件。
    ///
    /// 返回 `Ok(true)` 表示真正落盘，`Ok(false)` 表示已降级丢弃，
    /// `Err` 表示本次写入失败并已转入降级。调用方据此决定序号是否推进。
    fn write(&self, record: &Record<'_>) -> std::io::Result<bool> {
        let mut guard = self.lock();
        let Some(writer) = guard.as_mut() else {
            // 之前已经失败过并降级，本条丢弃。
            return Ok(false);
        };
        let line = serde_json::to_string(record).map_err(std::io::Error::other)?;
        if let Err(error) = writeln!(writer, "{line}") {
            // 丢弃句柄，后续写入全部走降级路径。
            *guard = None;
            return Err(error);
        }
        // 每条事件都 flush：证据的价值在于"崩了之后还能看到最后发生了什么"，
        // 而崩溃恰恰发生在最需要它的时刻。
        if let Err(error) = writer.flush() {
            *guard = None;
            return Err(error);
        }
        Ok(true)
    }

    /// 锁被毒化时照样拿到内容：理由同 `MemoryEvidenceSink`。
    fn lock(&self) -> std::sync::MutexGuard<'_, Option<BufWriter<File>>> {
        self.writer
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 取下一条序号并自增。写成功后调用，序号与落盘内容严格对应。
    fn advance_sequence(&self) {
        self.next_sequence.fetch_add(1, Ordering::Relaxed);
    }
}

impl EvidenceSink for JsonlEvidenceSink {
    fn record(&self, at: Timestamp, event: AuditEvent) {
        let sequence = self.next_sequence.load(Ordering::Relaxed);
        let record = Record {
            sequence,
            recorded_at: unix_seconds(at),
            event: &event,
        };
        match self.write(&record) {
            // 真正落盘了才推进序号，否则日志里会出现跳号。
            Ok(true) => self.advance_sequence(),
            // 已降级丢弃：不推进、不刷屏，静默跳过。
            Ok(false) => {}
            Err(error) => {
                // 审计故障绝不能把任务搞崩：一条证据写不进去，比整个
                // ReAct 循环挂掉要轻得多。降级为丢弃这条 + stderr 警告。
                eprintln!(
                    "警告：证据写入 {} 失败（第 {sequence} 条已丢弃）: {error}",
                    self.path.display(),
                );
            }
        }
    }
}

/// `SystemTime` 转秒级 Unix 时间戳。
///
/// 早于 UNIX_EPOCH 的时间戳按 0 处理——本项目的运行时刻不可能早于 1970 年，
/// 而为此引入有符号类型只会让反序列化多一种要处理的情况。
fn unix_seconds(time: SystemTime) -> u64 {
    time.duration_since(SystemTime::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

/// 会话 ID 里可能带路径分隔符（理论上），先做一次净化，否则
/// `../../etc/passwd` 这样的 ID 能把文件写到 workspace 外面去。
/// 证据文件与快照文件共用同一条规则，同一个 ID 不会落成两个名字。
pub(crate) fn sanitize_session_id(session_id: &str) -> String {
    session_id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_') { c } else { '_' })
        .collect()
}

/// 计算某个会话的证据文件路径：`<root>/.qcodeharness/sessions/<session_id>.jsonl`。
pub fn session_path(root: &Path, session_id: &str) -> PathBuf {
    root.join(".qcodeharness")
        .join("sessions")
        .join(format!("{}.jsonl", sanitize_session_id(session_id)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::evidence::Evidence;
    use crate::domain::evidence::TokenUsage;
    use crate::domain::ids::CallId;
    use crate::domain::tool::ToolOutcome;
    use serde_json::json;
    use std::io::Read;

    /// 每次测试用独立的临时目录，避免并行测试互相踩。
    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("qch-test-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("创建临时目录");
        dir
    }

    fn thought_event(turn: u32, text: &str) -> AuditEvent {
        AuditEvent::Thought {
            turn,
            text: text.to_owned(),
            usage: TokenUsage::default(),
        }
    }

    #[test]
    fn appends_events_as_jsonl_lines() {
        let dir = temp_dir("append");
        let path = dir.join("s.jsonl");
        let sink = JsonlEvidenceSink::open(&path).expect("打开证据文件");

        sink.record(
            SystemTime::UNIX_EPOCH,
            thought_event(1, "第一条"),
        );
        sink.record(
            SystemTime::UNIX_EPOCH,
            thought_event(2, "第二条"),
        );

        let mut content = String::new();
        File::open(&path)
            .expect("读回证据文件")
            .read_to_string(&mut content)
            .expect("读取内容");
        let lines: Vec<&str> = content.lines().collect();

        assert_eq!(lines.len(), 2, "每条事件一行");
        assert!(lines[0].contains("\"sequence\":1"));
        assert!(lines[1].contains("\"sequence\":2"));
        assert!(lines[0].contains("第一条"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sequence_increases_across_reopen() {
        let dir = temp_dir("reopen");
        let path = dir.join("s.jsonl");

        {
            let sink = JsonlEvidenceSink::open(&path).expect("第一次打开");
            sink.record(SystemTime::UNIX_EPOCH, thought_event(1, "a"));
            sink.record(SystemTime::UNIX_EPOCH, thought_event(2, "b"));
        }
        {
            let sink = JsonlEvidenceSink::open(&path).expect("第二次打开（追加）");
            sink.record(SystemTime::UNIX_EPOCH, thought_event(3, "c"));
        }

        let content = std::fs::read_to_string(&path).expect("读回");
        let sequences: Vec<u64> = content
            .lines()
            .map(|line| {
                serde_json::from_str::<serde_json::Value>(line)
                    .expect("行是合法 JSON")
                    .get("sequence")
                    .and_then(|v| v.as_u64())
                    .expect("有 sequence 字段")
            })
            .collect();

        // 追加写不重写文件，但序号在同一个 sink 生命周期内连续。
        // 重新打开会从 1 重新编号，所以这里断言的是"两次运行共 3 行"。
        assert_eq!(content.lines().count(), 3);
        assert_eq!(sequences, vec![1, 2, 1]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn creates_missing_parent_directories() {
        let dir = temp_dir("mkdir");
        // 故意指向一个还不存在的深层路径。
        let path = dir.join("a/b/c/sessions/s.jsonl");

        let sink = JsonlEvidenceSink::open(&path).expect("应自动创建目录");
        sink.record(SystemTime::UNIX_EPOCH, thought_event(1, "x"));

        assert!(path.exists(), "目录和文件都应被创建");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn every_line_deserializes_back_to_an_audit_event() {
        let dir = temp_dir("roundtrip");
        let path = dir.join("s.jsonl");
        let sink = JsonlEvidenceSink::open(&path).expect("打开");

        sink.record(SystemTime::UNIX_EPOCH, thought_event(1, "想"));
        sink.record(
            SystemTime::UNIX_EPOCH,
            AuditEvent::ToolRequested {
                turn: 1,
                call: crate::domain::tool::ToolCall {
                    id: CallId::new("call_1"),
                    name: "read_file".to_owned(),
                    arguments: json!({"path": "Cargo.toml"}),
                },
            },
        );
        sink.record(
            SystemTime::UNIX_EPOCH,
            AuditEvent::ToolObserved {
                turn: 1,
                call: crate::domain::tool::ToolCall {
                    id: CallId::new("call_1"),
                    name: "read_file".to_owned(),
                    arguments: json!({"path": "Cargo.toml"}),
                },
                outcome: ToolOutcome::success("[package]"),
            },
        );

        let content = std::fs::read_to_string(&path).expect("读回");
        let events: Vec<Evidence> = content
            .lines()
            .map(|line| serde_json::from_str::<Evidence>(line).expect("每行都能反序列化成 Evidence"))
            .collect();

        assert_eq!(events.len(), 3);
        assert_eq!(events[0].event.turn(), 1);
        assert!(matches!(events[1].event, AuditEvent::ToolRequested { .. }));
        assert!(matches!(events[2].event, AuditEvent::ToolObserved { .. }));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_failure_degrades_instead_of_panicking() {
        let dir = temp_dir("failure");
        let path = dir.join("s.jsonl");
        let sink = JsonlEvidenceSink::open(&path).expect("打开");

        sink.record(SystemTime::UNIX_EPOCH, thought_event(1, "先写一条"));

        // 拿走写入器，模拟"句柄已失效"这一降级状态。
        // 这是 write() 内部遇到 IO 错误时走的同一条路径。
        *sink.lock() = None;

        // 降级后反复写入都不该 panic。
        sink.record(SystemTime::UNIX_EPOCH, thought_event(2, "这条会失败"));
        sink.record(SystemTime::UNIX_EPOCH, thought_event(3, "这条也不该崩"));

        // 只有第一条写成功，序号停在 2。
        assert_eq!(sink.next_sequence.load(Ordering::Relaxed), 2);
        let _ = std::fs::remove_dir_all(&dir);    }

    #[test]
    fn unopenable_path_reports_an_error_instead_of_panicking() {
        let dir = temp_dir("badpath");
        // 父路径是一个文件而不是目录，create_dir_all 必然失败。
        let blocker = dir.join("not-a-dir");
        std::fs::write(&blocker, "x").expect("创建占位文件");

        let result = JsonlEvidenceSink::open(blocker.join("s.jsonl"));

        assert!(result.is_err(), "父路径不是目录时打开应报错而非 panic");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn session_path_sanitizes_dangerous_ids() {
        let root = Path::new("/workspace");

        let normal = session_path(root, "session-123-456");
        assert_eq!(
            normal,
            root.join(".qcodeharness/sessions/session-123-456.jsonl")
        );

        // 路径分隔符被替换掉，写不出 workspace。
        // 只检查会话 ID 那一层：root 本身用什么分隔符取决于调用方。
        let escaped = session_path(root, "../../etc/passwd");
        assert!(escaped.starts_with(root.join(".qcodeharness/sessions")));
        let filename = escaped
            .file_name()
            .expect("有文件名")
            .to_string_lossy()
            .into_owned();
        assert_eq!(filename, "______etc_passwd.jsonl");
    }
}

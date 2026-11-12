//! JSONL 证据汇：把审计事件追加写入 JSONL 文件。
//!
//! 证据是流式的，运行中需要能 `tail -f` 查看进度；JSON 数组必须写完整
//! 才合法，因此选 JSONL。每行一条 `Evidence`，字段名与内存实现一致，
//! 更换存储不需要修改日志读取代码。

use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::SystemTime;

use serde::Serialize;

use crate::application::port::{EvidenceSink, Timestamp};
use crate::domain::evidence::{AuditEvent, EVIDENCE_FORMAT_MILLIS};
use crate::domain::ids::sanitize_file_id;

/// 写入磁盘的证据汇。
///
/// 文件句柄只打开一次并保持追加模式。每次事件重新开关文件会产生大量
/// syscall，中途换 inode 还可能写入错误的文件。句柄放在 `Mutex` 中，
/// 因为 `EvidenceSink::record` 只接收 `&self`。
pub struct JsonlEvidenceSink {
    /// 落盘路径，仅用于错误信息和测试断言。
    path: PathBuf,
    /// 下一条事件的序号。
    ///
    /// 只在写锁内读写。用原子类型是为了让锁外的读者（测试、诊断）能安全
    /// 地看到当前值而不必拿锁，并发控制仍由写锁负责。
    next_sequence: AtomicU64,
    /// 写入器。用 `Option` 包住，写失败后可降级，见 `write`。
    writer: Mutex<Option<BufWriter<File>>>,
}

/// 序列化到磁盘的一行。
///
/// 不直接序列化 `Evidence`：领域类型的字段（如 `recorded_at`）可能变化。
/// 这里显式列出落盘字段，使磁盘格式成为独立契约，修改领域类型不会
/// 改变日志格式。
///
/// `event` 保持嵌套，不使用 `#[serde(flatten)]`。拍平后需要靠 `turn` /
/// `text` 等字段名反推事件类型，而 `AuditEvent` 是 enum，serde 反序列化
/// 需要外层 tag 选择变体。嵌套使磁盘格式与领域结构一一对应，可直接
/// `serde_json::from_str::<Evidence>()`。
#[derive(Debug, Serialize)]
struct Record<'a> {
    sequence: u64,
    /// 磁盘格式号。reader 按它解释 `recorded_at` 的单位，缺省为秒，
    /// 以兼容旧证据文件。使用固定常量：这是磁盘契约，字面量不应被
    /// 间接修改。
    format: u32,
    recorded_at: u64,
    event: &'a AuditEvent,
}

impl JsonlEvidenceSink {
    /// 打开证据文件（必要时创建），准备追加。
    ///
    /// 父目录不存在时递归创建。证据文件路径由会话 ID 推导，要求调用方
    /// 先建目录会把 `mkdir -p` 逻辑推给每个调用点。
    ///
    /// # Errors
    ///
    /// 目录创建或文件打开失败时返回底层 IO 错误。
    pub fn open(path: impl AsRef<Path>) -> std::io::Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // 追加到已有文件时，序号从最后一条继续。
        //
        // 否则重开 sink（会话级恢复、两段式基准）会让序号回到 1，
        // 文件里出现两段从 1 开始的编号。证据仍可读取，但"序号连续"
        // 这一性质失效，而它用于发现证据被截断或改写。
        let next_sequence = last_sequence(path).map_or(1, |sequence| sequence + 1);
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self {
            path: path.to_path_buf(),
            next_sequence: AtomicU64::new(next_sequence),
            writer: Mutex::new(Some(BufWriter::new(file))),
        })
    }

    /// 把一条记录写入文件。
    ///
    /// 序号在持锁期间分配，写盘也在同一把锁内。这样"取号、写盘、推进"
    /// 是一个整体：并发写入者不会拿到同一个序号。
    ///
    /// `Ok(true)` 表示已落盘；`Ok(false)` 表示 sink 已降级，本条丢弃；
    /// `Err` 表示本次写入失败并转入降级。调用方据此决定是否推进序号。
    fn write(&self, event: &AuditEvent, at: Timestamp) -> std::io::Result<bool> {
        let mut guard = self.lock();
        let Some(writer) = guard.as_mut() else {
            // 之前已经失败过并降级，本条丢弃。
            return Ok(false);
        };
        // 取号与写盘在同一临界区内，序号与落盘顺序严格对应。
        let sequence = self.next_sequence.load(Ordering::Relaxed);
        let record = Record {
            sequence,
            format: EVIDENCE_FORMAT_MILLIS,
            recorded_at: unix_millis(at),
            event,
        };
        let line = serde_json::to_string(&record).map_err(std::io::Error::other)?;
        if let Err(error) = writeln!(writer, "{line}") {
            // 丢弃句柄，后续写入全部走降级路径。
            *guard = None;
            return Err(error);
        }
        // 每条事件都 flush：崩溃后仍需看到最后发生的事件。
        if let Err(error) = writer.flush() {
            *guard = None;
            return Err(error);
        }
        // 真正落盘了才推进序号，否则日志里会出现跳号。
        self.next_sequence.store(sequence + 1, Ordering::Relaxed);
        Ok(true)
    }

    /// 锁毒化时仍取出内容，理由同 `MemoryEvidenceSink`。
    fn lock(&self) -> std::sync::MutexGuard<'_, Option<BufWriter<File>>> {
        self.writer
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl EvidenceSink for JsonlEvidenceSink {
    fn record(&self, at: Timestamp, event: AuditEvent) {
        match self.write(&event, at) {
            // 真正落盘了才推进序号，否则日志里会出现跳号。推进在写锁内
            // 完成，见 `write`。
            Ok(true) => {}
            // 已降级丢弃：不推进序号，不重复告警。
            Ok(false) => {}
            Err(error) => {
                // 审计写入失败不能中断任务。丢弃本条并向 stderr 输出警告，
                // 避免整个 ReAct 循环失败。
                eprintln!(
                    "警告：证据写入 {} 失败（本条已丢弃）: {error}",
                    self.path.display(),
                );
            }
        }
    }
}

/// 把 `SystemTime` 转为毫秒 Unix 时间戳。
///
/// 早于 UNIX_EPOCH 的时间按 0 处理。运行时刻不会早于 1970 年，引入
/// 有符号类型只会增加反序列化分支。
///
/// 使用毫秒精度：统计每轮模型调用耗时时，几百毫秒的调用在秒级精度下
/// 会记成 0，无法还原延迟分布。精度必须与 `format` 一起记录（见
/// [`EVIDENCE_FORMAT_MILLIS`]）；不标记就改精度会使历史证据的耗时统计失效。
fn unix_millis(time: SystemTime) -> u64 {
    time.duration_since(SystemTime::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

/// 读取文件中最后一条证据的序号。
///
/// 只读文件尾部：证据文件可能有几万行，为最后一个编号全量读取不划算。
/// 尾部内容不完整（末行是崩溃留下的半截 JSON）时跳过并继续向前查找。
/// 一条都读不出时返回 `None`，调用方从 1 开始编号；序号连续性变差，
/// 但不会丢失已有证据。
fn last_sequence(path: &Path) -> Option<u64> {
    /// 尾部读取字节数。足以容纳最后一条证据（可能带较大的工具输出），
    /// 又不需要读入整个文件。
    const TAIL_BYTES: u64 = 64 * 1024;

    let mut file = File::open(path).ok()?;
    let size = file.metadata().ok()?.len();
    if size == 0 {
        return None;
    }
    let tail_len = size.min(TAIL_BYTES);
    let mut buffer = vec![0u8; tail_len as usize];
    file.seek(SeekFrom::Start(size - tail_len)).ok()?;
    file.read_exact(&mut buffer).ok()?;
    let text = String::from_utf8_lossy(&buffer);
    text.lines()
        .rev()
        .filter(|line| !line.trim().is_empty())
        .find_map(|line| {
            let value = serde_json::from_str::<serde_json::Value>(line).ok()?;
            value.get("sequence")?.as_u64()
        })
}

/// 计算某个会话的证据文件路径：`<root>/.qcodeharness/sessions/<session_id>.jsonl`。
pub fn session_path(root: &Path, session_id: &str) -> PathBuf {
    root.join(".qcodeharness")
        .join("sessions")
        .join(format!("{}.jsonl", sanitize_file_id(session_id)))
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
    use std::sync::Arc;

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

        // 重开 sink 是会话级恢复与两段式基准的必经路径。序号必须从文件
        // 最后一条继续，否则会出现两段从 1 开始的编号，失去"序号连续"
        // 这一检测证据截断的手段。
        assert_eq!(content.lines().count(), 3);
        assert_eq!(sequences, vec![1, 2, 3]);
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

        // 移除写入器，模拟句柄失效的降级状态。
        // 这与 write() 内部遇到 IO 错误时走的是同一条路径。
        *sink.lock() = None;

        // 降级后反复写入都不该 panic。
        sink.record(SystemTime::UNIX_EPOCH, thought_event(2, "这条会失败"));
        sink.record(SystemTime::UNIX_EPOCH, thought_event(3, "这条也不该崩"));

        // 只有第一条写成功，序号停在 2。
        assert_eq!(sink.next_sequence.load(Ordering::Relaxed), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 并发写入时序号不得重复。
    ///
    /// 取号与写盘在写锁内完成，序号与落盘顺序严格对应。取号在锁外时，
    /// 两个线程会读到同一个序号，文件里出现两条相同编号的记录，而
    /// "序号连续"是发现证据被截断或改写的手段。
    #[test]
    fn concurrent_writers_get_distinct_sequences() {
        let dir = temp_dir("concurrent");
        let path = dir.join("s.jsonl");
        let sink = Arc::new(JsonlEvidenceSink::open(&path).expect("打开"));

        let writers = 4;
        let per_writer = 25;
        let handles: Vec<_> = (0..writers)
            .map(|writer| {
                let sink = Arc::clone(&sink);
                std::thread::spawn(move || {
                    for index in 0..per_writer {
                        sink.record(
                            SystemTime::UNIX_EPOCH,
                            thought_event(writer as u32 + 1, &format!("{writer}-{index}")),
                        );
                    }
                })
            })
            .collect();
        for handle in handles {
            handle.join().expect("写入线程不应 panic");
        }

        let content = std::fs::read_to_string(&path).expect("读回");
        let sequences: Vec<u64> = content
            .lines()
            .map(|line| {
                serde_json::from_str::<serde_json::Value>(line)
                    .expect("行是合法 JSON")
                    .get("sequence")
                    .and_then(|value| value.as_u64())
                    .expect("有 sequence 字段")
            })
            .collect();

        assert_eq!(sequences.len(), writers * per_writer, "每条事件都要落盘");
        let mut sorted = sequences.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), sequences.len(), "序号不得重复");
        assert_eq!(sorted.first(), Some(&1));
        assert_eq!(sorted.last(), Some(&((writers * per_writer) as u64)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unopenable_path_reports_an_error_instead_of_panicking() {
        let dir = temp_dir("badpath");
        // 父路径是文件，create_dir_all 必然失败。
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

        // 路径分隔符被替换，无法写出 workspace。
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

//! 内存证据汇：`EvidenceSink` 的内存实现。
//!
//! 不访问文件、进程或网络，因此留在应用层，与端口 trait 同层。
//! 生产路径用 JSONL 落盘；这里保存的是测试与 CLI 记录所需的副本。

use std::sync::Mutex;

use crate::application::port::{EvidenceSink, Timestamp};
use crate::domain::evidence::{AuditEvent, Evidence};

/// 把事件存在内存里的证据汇。
#[derive(Debug, Default)]
pub struct MemoryEvidenceSink {
    events: Mutex<Vec<Evidence>>,
}

impl MemoryEvidenceSink {
    /// 空的证据汇。
    pub fn new() -> Self {
        Self::default()
    }

    /// 已记录的事件，按发生顺序。
    ///
    /// 仅测试使用。生产证据汇是 JSONL，读取证据应查文件；内存副本
    /// 随进程退出丢失。
    #[cfg(test)]
    pub fn events(&self) -> Vec<Evidence> {
        self.lock().clone()
    }

    /// 锁毒化时仍取出内容。毒化只说明某个持有者 panic 过，不影响已写入的记录。
    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Evidence>> {
        self.events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl EvidenceSink for MemoryEvidenceSink {
    fn record(&self, at: Timestamp, event: AuditEvent) {
        let mut events = self.lock();
        let sequence = events.len() as u64 + 1;
        events.push(Evidence::new(sequence, at, event));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::evidence::TokenUsage;

    #[test]
    fn records_events_with_increasing_sequence() {
        let sink = MemoryEvidenceSink::new();
        let at = Timestamp::UNIX_EPOCH;

        sink.record(
            at,
            AuditEvent::Thought {
                turn: 1,
                text: "想".to_owned(),
                usage: TokenUsage::default(),
            },
        );
        sink.record(
            at,
            AuditEvent::Thought {
                turn: 2,
                text: "再想".to_owned(),
                usage: TokenUsage::default(),
            },
        );

        let events = sink.events();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].sequence, 1);
        assert_eq!(events[1].sequence, 2);
        assert_eq!(events[0].recorded_at, at);
    }
}

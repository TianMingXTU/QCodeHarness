//! 证据基础设施：内存存储。
//!
//! 事件类型在领域层定好，所以换存储实现不用改产生事件的代码——
//! 这正是先把 `AuditEvent` 放进 domain 的原因。

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
    /// 只给测试用：生产的证据汇是 JSONL，查询证据应该去读文件，
    /// 而不是从内存里捞——进程退出就没了。
    #[cfg(test)]
    pub fn events(&self) -> Vec<Evidence> {
        self.lock().clone()
    }

    /// 锁被毒化时照样取出内容：毒化只说明某个持有者 panic 了，
    /// 不影响里面已经写好的记录。
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

//! MCP 连接级事件的写入口。
//!
//! 连接事实由基础设施层的 Server 产生，落点却是证据链。中间这一层存在的
//! 理由有两条：
//!
//! 1. **不把 `EvidenceSink`/`Clock` 端口交给基础设施去拼装。** Server 只管
//!    交出 [`McpLifecycleFact`]，"什么时候算发生、写成哪条事件"由应用层定；
//!    连接事件因此和工具事件走同一套审计口径，而不是各写各的。
//! 2. **连接事件不属于任何模型轮次。** 一次启动失败没有 `ToolObserved`，
//!    也没有 `SandboxApplied`——伪造这两类事件会让报表把"没起来的 Server"
//!    算成一次工具调用。这里统一落成 `turn: 0`。
//!
//! 与 `AgentRunner` 的证据写入分开是刻意的：runner 一次只有一个会话在跑，
//! 而多个 MCP Server 会在同一时刻上报（握手超时、断连监视器、显式恢复）。

use std::sync::{Arc, Mutex};

use crate::application::port::{Clock, EvidenceSink};
use crate::domain::evidence::AuditEvent;
use crate::domain::mcp::McpLifecycleFact;

/// 连接事件写入器。
///
/// 一个进程应当只构造一个实例并在所有 Server 之间共享：`JsonlEvidenceSink`
/// 的"读序号 → 写盘 → 推进序号"不是一次原子操作，两个 Server 同时写入同一份
/// 生命周期日志会拿到同一个序号。这里把整个写入串起来，序号就重新严格递增。
pub struct McpLifecycleRecorder {
    evidence: Arc<dyn EvidenceSink>,
    clock: Arc<dyn Clock>,
    /// 写入串行锁。用 `std::sync::Mutex` 而不是异步锁：`EvidenceSink::record`
    /// 本身是同步的，临界区只有一次时间戳读取和一行落盘，不需要让执行器
    /// 在持锁期间 `await`。
    write_order: Mutex<()>,
}

impl McpLifecycleRecorder {
    /// 用给定的证据汇与时钟构造。
    pub fn new(evidence: Arc<dyn EvidenceSink>, clock: Arc<dyn Clock>) -> Self {
        Self {
            evidence,
            clock,
            write_order: Mutex::new(()),
        }
    }

    /// 记一条连接级事实。
    ///
    /// 时间戳在进锁之前取：`now()` 要的是"这件事发生的时刻"，等锁的时间
    /// 不属于事件本身。写失败由 sink 自己降级（与全部审计写入同一条线），
    /// 这里不向上报错——一次审计失败不能把 Server 的连接流程带崩。
    pub fn record(&self, fact: McpLifecycleFact) {
        let at = self.clock.now();
        let _order = self
            .write_order
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        self.evidence
            .record(at, AuditEvent::McpLifecycle { turn: 0, fact });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::mcp::{McpFaultKind, McpLifecycleKind};
    use crate::infrastructure::evidence::memory::MemoryEvidenceSink;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    /// 固定时钟：连接事件的时刻必须可断言，而不是"看测试跑得多快"。
    struct FixedClock(SystemTime);

    impl Clock for FixedClock {
        fn now(&self) -> SystemTime {
            self.0
        }
    }

    fn fact(server: &str, kind: McpLifecycleKind) -> McpLifecycleFact {
        McpLifecycleFact {
            server: server.to_owned(),
            generation: 1,
            kind,
            fault: (kind == McpLifecycleKind::ConnectFailed).then_some(McpFaultKind::Handshake),
            call_id: None,
            process_isolation: true,
            tools_changed: false,
        }
    }

    #[test]
    fn connection_facts_land_as_turn_zero_lifecycle_events() {
        let evidence = Arc::new(MemoryEvidenceSink::new());
        let recorder = McpLifecycleRecorder::new(
            evidence.clone(),
            Arc::new(FixedClock(UNIX_EPOCH + Duration::from_secs(7))),
        );

        recorder.record(fact("demo", McpLifecycleKind::Connected));
        recorder.record(fact("demo", McpLifecycleKind::ConnectFailed));

        let events = evidence.events();
        assert_eq!(events.len(), 2, "两条事实两条事件，不掺工具事件");
        for entry in &events {
            assert_eq!(entry.event.turn(), 0, "连接事件不属于任何模型轮次");
            assert_eq!(entry.recorded_at, UNIX_EPOCH + Duration::from_secs(7));
            match &entry.event {
                AuditEvent::McpLifecycle { fact, .. } => {
                    assert_eq!(fact.server, "demo");
                    assert!(fact.process_isolation, "保护状态照事实写");
                }
                other => panic!("只该产生连接事件，实际是 {other:?}"),
            }
        }
        // 序号由 sink 分配，严格递增——报表靠它发现日志被截断。
        assert_eq!(
            events.iter().map(|e| e.sequence).collect::<Vec<_>>(),
            vec![1, 2]
        );
    }

    /// 多个 Server 同时上报也不能让序号撞车。
    ///
    /// 这个替身复刻 `JsonlEvidenceSink` 的真实形状：**先**读序号、**后**在
    /// 写锁里落盘、最后推进序号——三段不是一次原子操作，所以两个线程
    /// 同时进来会拿到同一个序号。`tests/mcp_lifecycle_evidence.rs` 用真
    /// JSONL 文件验同一件事；这里用替身是为了让"撞号"在单元测试里就可见。
    #[derive(Default)]
    struct SplitSequenceSink {
        next: std::sync::atomic::AtomicU64,
        written: Mutex<Vec<(u64, AuditEvent)>>,
    }

    impl EvidenceSink for SplitSequenceSink {
        fn record(&self, _at: crate::application::port::Timestamp, event: AuditEvent) {
            let sequence = self.next.load(std::sync::atomic::Ordering::Relaxed);
            self.written
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push((sequence, event));
            // 落盘与推进之间留一个换线程的口子：没有写入串行锁时，
            // 另一个线程一定会在这里读到同一个序号。
            std::thread::yield_now();
            self.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }

    #[test]
    fn concurrent_servers_get_distinct_increasing_sequences() {
        let evidence = Arc::new(SplitSequenceSink::default());
        let recorder = Arc::new(McpLifecycleRecorder::new(
            evidence.clone(),
            Arc::new(FixedClock(SystemTime::now())),
        ));

        let writers: Vec<_> = (0..8)
            .map(|index| {
                let recorder = recorder.clone();
                std::thread::spawn(move || {
                    recorder.record(fact(
                        &format!("server-{index}"),
                        McpLifecycleKind::Connected,
                    ))
                })
            })
            .collect();
        for writer in writers {
            writer.join().expect("写入线程不该 panic");
        }

        let written = evidence.written.lock().unwrap();
        assert_eq!(written.len(), 8);
        let mut sequences: Vec<u64> = written.iter().map(|(sequence, _)| *sequence).collect();
        sequences.sort_unstable();
        let expected: Vec<u64> = (0..8).collect();
        assert_eq!(sequences, expected, "并发写入必须各得一个序号");
    }
}

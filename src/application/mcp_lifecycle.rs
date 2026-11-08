//! MCP 连接级事件的写入口。
//!
//! 连接事实由基础设施层的 Server 产生，最终写入证据链。中间层负责两件事：
//!
//! 1. 基础设施不接触 `EvidenceSink`/`Clock` 端口。Server 只交出
//!    [`McpLifecycleFact`]，事件内容与发生时机由应用层决定，连接事件
//!    因此与工具事件使用同一套审计规则。
//! 2. 连接事件不属于任何模型轮次。启动失败没有 `ToolObserved`，也没有
//!    `SandboxApplied`；伪造这两类事件会让报表把未启动的 Server 算成一次
//!    工具调用。这里统一记为 `turn: 0`。
//!
//! 与 `AgentRunner` 的证据写入分开：runner 一次只跑一个会话，多个 MCP
//! Server 会同时上报（握手超时、断连监视器、显式恢复）。

use std::sync::{Arc, Mutex};

use crate::application::port::{Clock, EvidenceSink};
use crate::domain::evidence::AuditEvent;
use crate::domain::mcp::McpLifecycleFact;

/// 连接事件写入器。
///
/// 一个进程只应构造一个实例并在所有 Server 间共享。这里串行化整个写入
/// 过程，让并发上报的事件在文件里按时间戳先后排列：多个 Server 会同时
/// 上报（握手超时、断连监视器、显式恢复），不串行化时事件顺序会与
/// 发生顺序错位。
///
/// 序号由 sink 在写锁内分配，本身已经保证唯一；这里的锁管的是**顺序**，
/// 不是序号。
pub struct McpLifecycleRecorder {
    evidence: Arc<dyn EvidenceSink>,
    clock: Arc<dyn Clock>,
    /// 写入串行锁。`EvidenceSink::record` 是同步的，临界区只有一次落盘，
    /// 无需异步锁，持锁期间也不 `await`。
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

    /// 记录一条连接级事实。
    ///
    /// 时间戳在持锁后取得，使文件中的事件时刻与写入顺序一致。并发上报时
    /// 若在锁外取时间戳，后写入的事件可能带更早的时刻，按首末事件计算的
    /// 墙钟会失真。
    ///
    /// 写入失败由 sink 自行降级，与其余审计写入一致。这里不返回错误：
    /// 审计失败不应中断 Server 的连接流程。
    pub fn record(&self, fact: McpLifecycleFact) {
        let _order = self
            .write_order
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let at = self.clock.now();
        self.evidence
            .record(at, AuditEvent::McpLifecycle { turn: 0, fact });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::mcp::{McpFaultKind, McpLifecycleKind};
    use crate::application::memory_evidence::MemoryEvidenceSink;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    /// 固定时钟：让连接事件的时刻可断言，不受测试执行速度影响。
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
        // 序号由 sink 分配且严格递增；`SessionLog::is_sequential` 及多个
        // 测试靠这条性质发现日志被截断或改写。
        assert_eq!(
            events.iter().map(|e| e.sequence).collect::<Vec<_>>(),
            vec![1, 2]
        );
    }

    /// 多个 Server 同时上报时，序号必须各不相同。
    ///
    /// 替身复刻 `JsonlEvidenceSink` 的行为：在写锁内分配序号并落盘。
    /// 这里用替身，让序号冲突在单元测试中即可复现；
    /// `tests/mcp_lifecycle_evidence.rs` 用真实 JSONL 文件验证同一行为。
    #[derive(Default)]
    struct SplitSequenceSink {
        next: std::sync::atomic::AtomicU64,
        written: Mutex<Vec<(u64, AuditEvent)>>,
    }

    impl EvidenceSink for SplitSequenceSink {
        fn record(&self, _at: crate::application::port::Timestamp, event: AuditEvent) {
            // 与真实 sink 同构：取号与落盘都在同一把锁内。
            let mut written = self
                .written
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let sequence = self.next.load(std::sync::atomic::Ordering::Relaxed);
            written.push((sequence, event));
            self.next
                .store(sequence + 1, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// 多个 Server 同时上报时，序号必须各不相同。
    ///
    /// recorder 的锁把取号与落盘串成一个整体，序号由 sink 在自己的写锁内
    /// 分配。两把锁叠加后并发写入者不会拿到同一个序号。
    #[test]
    fn concurrent_servers_get_distinct_sequences() {
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

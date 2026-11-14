//! 跨会话记忆写入的纯应用编排。
//!
//! 记忆文件遵循 MEMORY.md 的"一条事实一文件，索引只放一行摘要"语义。
//! 领域层负责纯文本格式，本模块把"先审批、再写盘、后记证据"固定成同一个
//! 顺序；基础设施实现只负责真实文件落盘，装配层提供闸门与证据汇。

use std::future::Future;
use std::sync::Arc;

use serde_json::json;

use crate::application::port::{ApprovalGate, ApprovalRequest, Decision, EvidenceSink, Timestamp};
use crate::domain::ids::CallId;
use crate::domain::memory::{MemoryEntry, MemoryFile, MemoryFileError};
use crate::domain::tool::{ToolAnnotations, ToolCall};

/// 记忆写入工具名。与任何只读工具不同，该调用会请求人工审批。
pub const MEMORY_WRITE_TOOL: &str = "memory_write";

/// 记忆写入端口。实现必须原子替换索引与事实文件，或明确返回 IO 错误。
pub trait MemoryWriter: Send + Sync {
    /// 把一条事实及更新后的索引写入指定目录。
    fn write(&self, root: &std::path::Path, file: &MemoryFile) -> Result<(), String>;
}

/// 记忆写入错误。审批拒绝与验证失败都不会产生文件或证据副作用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemoryWriteError {
    Invalid(MemoryFileError),
    Rejected(String),
    Io(String),
}

/// 自动记忆编排入口。
pub struct AutoMemory<W, G, S, C> {
    writer: Arc<W>,
    gate: Arc<G>,
    evidence: Arc<S>,
    clock: C,
}

impl<W, G, S, C> AutoMemory<W, G, S, C>
where
    W: MemoryWriter + 'static,
    G: ApprovalGate + 'static,
    S: EvidenceSink + 'static,
    C: Fn() -> Timestamp,
{
    pub fn new(writer: Arc<W>, gate: Arc<G>, evidence: Arc<S>, clock: C) -> Self {
        Self {
            writer,
            gate,
            evidence,
            clock,
        }
    }

    /// 请求写入一条事实。审批通过后才调用 [`MemoryWriter::write`]，并在
    /// 成功落盘后把请求、审批、观察三条事件写入既有证据账本。
    pub fn remember(
        &self,
        root: &std::path::Path,
        entry: MemoryEntry,
        turn: u32,
    ) -> impl Future<Output = Result<MemoryFile, MemoryWriteError>> + '_ {
        let writer = Arc::clone(&self.writer);
        let gate = Arc::clone(&self.gate);
        let evidence = Arc::clone(&self.evidence);
        let root = root.to_path_buf();
        async move {
            let file = MemoryFile::with_entry(entry).map_err(MemoryWriteError::Invalid)?;
            let call = ToolCall {
                id: CallId::new(format!("memory-{turn}")),
                name: MEMORY_WRITE_TOOL.to_owned(),
                arguments: json!({
                    "slug": file.index_line().slug,
                    "summary": file.index_line().summary,
                }),
            };
            let request = ApprovalRequest {
                turn,
                call: call.clone(),
                annotations: ToolAnnotations::new(false, true, false, false),
                reason: Some("跨会话记忆会修改用户目录，须人工确认".to_owned()),
                resumed: false,
            };
            let at = (self.clock)();
            evidence.record(
                at,
                crate::domain::evidence::AuditEvent::ToolRequested {
                    turn,
                    call: call.clone(),
                },
            );
            let decision: Decision = gate.request(&request).await;
            evidence.record(
                at,
                crate::domain::evidence::AuditEvent::ApprovalDecided {
                    turn,
                    tool: MEMORY_WRITE_TOOL.to_owned(),
                    allowed: decision.is_ok(),
                    reason: decision.as_ref().err().cloned(),
                },
            );
            match decision {
                Ok(()) => {}
                Err(reason) => return Err(MemoryWriteError::Rejected(reason)),
            }
            writer.write(&root, &file).map_err(MemoryWriteError::Io)?;
            evidence.record(
                at,
                crate::domain::evidence::AuditEvent::ToolObserved {
                    turn,
                    call,
                    outcome: crate::domain::tool::ToolOutcome::success(format!(
                        "记忆 {} 已写入",
                        file.index_line().slug
                    )),
                },
            );
            Ok(file)
        }
    }
}

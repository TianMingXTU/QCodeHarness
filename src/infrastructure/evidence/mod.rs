//! 证据基础设施：内存存储、JSONL 持久化与读回（P6 统计的入口）。
//!
//! memory 与 jsonl 实现同一个 `EvidenceSink`，事件类型在领域层定好，
//! 所以换存储不用改产生事件的代码；reader 走同一条磁盘契约把
//! `.qcodeharness/sessions/*.jsonl` 读回成结构化数据。

pub mod memory;
pub mod jsonl;
pub mod reader;

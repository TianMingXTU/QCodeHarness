//! 证据基础设施：内存存储与 JSONL 持久化。
//!
//! 两者实现同一个 `EvidenceSink`，事件类型在领域层定好，
//! 所以换存储不用改产生事件的代码。

pub mod memory;
pub mod jsonl;

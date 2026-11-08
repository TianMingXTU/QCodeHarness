//! 证据基础设施：JSONL 持久化与读回（统计与报表的入口）。
//!
//! jsonl 实现 `EvidenceSink`，事件类型在领域层定义，更换存储不需要修改
//! 产生事件的代码。reader 按同一份磁盘契约把
//! `.qcodeharness/sessions/*.jsonl` 读回为结构化数据。

pub mod jsonl;
pub mod reader;

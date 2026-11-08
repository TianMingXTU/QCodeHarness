//! 任务快照存储。
//!
//! 目前只有一种实现：单文件 JSON（[`json::JsonTaskStore`]）。任务快照
//! 整体读写，不像证据那样逐条追加，因此不使用 JSONL。

pub mod json;

pub use json::JsonTaskStore;

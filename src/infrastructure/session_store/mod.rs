//! 会话快照存储。
//!
//! 目录下目前只有一种实现：单文件 JSON（[`json::JsonSessionStore`]）。
//! 快照是"整体读写"的数据，不像证据那样逐条追加，所以不用 JSONL。

pub mod json;

pub use json::JsonSessionStore;

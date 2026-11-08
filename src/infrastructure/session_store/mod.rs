//! 会话快照存储。
//!
//! 目前只有单文件 JSON 实现（[`json::JsonSessionStore`]）。快照整体读写，
//! 不像证据逐条追加，因此不用 JSONL。

pub mod json;

pub use json::JsonSessionStore;

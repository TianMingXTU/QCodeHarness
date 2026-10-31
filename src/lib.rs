//! QCodeHarness 库入口。
//!
//! 把四个层暴露成一个库目标，供集成测试（`tests/`）与未来的其他前端
//! 复用。`main.rs` 是薄壳，只做装配；所有逻辑都在库这边。

pub mod application;
pub mod domain;
pub mod infrastructure;
pub mod interface;

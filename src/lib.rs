//! QCodeHarness 库入口。
//!
//! 四个层作为库目标导出，供集成测试（`tests/`）与其他前端复用。
//! `main.rs` 只做装配，逻辑均在库中。

pub mod application;
pub mod domain;
pub mod infrastructure;
pub mod interface;

//! 模型基础设施：rig 客户端封装。
//!
//! 把 rig 的 `DynModel<Completion>` 包装为 `ModelGateway` trait 的实现，
//! 并提供 rig ↔ domain 类型的双向翻译。

pub mod rig_chat;

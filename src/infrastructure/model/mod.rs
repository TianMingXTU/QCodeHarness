//! 模型基础设施：rig 客户端封装。
//!
//! 把 rig 的 `DynModel<Completion>` 包装成 `ModelGateway` 的实现，并提供
//! rig 与 domain 类型的双向转换。

pub mod openai_compat;
pub mod rig_chat;

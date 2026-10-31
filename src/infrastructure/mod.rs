//! 基础设施层：与外部世界交互的具体实现。
//!
//! 这一层的职责是把领域层的抽象"落地"：
//! - rig 客户端封装为 `ModelGateway`
//! - 文件操作封装为 `ToolInvoker`
//! - 审批、证据收集、时钟的具体实现

pub mod model;
pub mod tool;
pub mod approval;
pub mod evidence;
pub mod policy;
pub mod clock;

//! 审批基础设施：自动审批。
//!
//! P0 恒放行——这是为了先把接缝打通：循环里"副作用工具执行前必须过闸"
//! 这条路径已经存在且有测试覆盖，P1 换成 CLI 交互式审批时只换这个实现。

use crate::application::port::{ApprovalGate, ApprovalRequest, BoxFuture, Decision};

/// 自动批准一切请求的闸门。
#[derive(Debug, Clone, Copy, Default)]
pub struct AutoApproveGate;

impl ApprovalGate for AutoApproveGate {
    fn request(&self, _request: &ApprovalRequest) -> BoxFuture<'_, Decision> {
        Box::pin(async { Ok(()) })
    }
}

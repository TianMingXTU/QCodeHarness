//! 审批基础设施：自动审批。
//!
//! 恒放行，但闸门这个接缝本身是真实的：循环里"副作用工具执行前必须过闸"
//! 这条路径存在且有测试覆盖；CLI 交互式审批只是同一接缝的另一个实现。

use crate::application::port::{ApprovalGate, ApprovalRequest, BoxFuture, Decision};

/// 自动批准一切请求的闸门。
#[derive(Debug, Clone, Copy, Default)]
pub struct AutoApproveGate;

impl ApprovalGate for AutoApproveGate {
    fn request(&self, _request: &ApprovalRequest) -> BoxFuture<'_, Decision> {
        Box::pin(async { Ok(()) })
    }
}

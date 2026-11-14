//! 按权限档位裁决审批请求的闸门。
//!
//! 包装既有 `ApprovalGate`：档位允许的动作自动放行，档位禁止的动作直接
//! 否决，只有档位要求人工确认时才交给内层闸门。无头模式注入非交互闸门
//! 后，拒绝与自动放行都会写入 `ApprovalDecided` 证据，审计链不因提前
//! 返回而断档。

use std::sync::Arc;
use std::time::SystemTime;

use crate::application::port::{ApprovalGate, ApprovalRequest, BoxFuture, Decision, EvidenceSink};
use crate::domain::evidence::AuditEvent;
use crate::domain::permission_profile::PermissionProfile;

/// 带档位的审批闸门。
pub struct ProfileApprovalGate {
    inner: Arc<dyn ApprovalGate>,
    evidence: Arc<dyn EvidenceSink>,
    profile: PermissionProfile,
}

impl ProfileApprovalGate {
    /// 包装一个闸门，并指定用于记录提前裁决的证据汇。
    pub fn new(
        inner: Arc<dyn ApprovalGate>,
        evidence: Arc<dyn EvidenceSink>,
        profile: PermissionProfile,
    ) -> Self {
        Self {
            inner,
            evidence,
            profile,
        }
    }

    /// 转成 `Shared` 所需的动态闸门。
    pub fn boxed(self) -> Arc<dyn ApprovalGate> {
        Arc::new(self)
    }

    /// 记录一次审批裁决；`allowed=false` 时理由必须保留。
    fn record(&self, request: &ApprovalRequest, allowed: bool, reason: Option<String>) {
        self.evidence.record(
            SystemTime::now(),
            AuditEvent::ApprovalDecided {
                turn: request.turn,
                tool: request.call.name.clone(),
                allowed,
                reason,
            },
        );
    }
}

impl ApprovalGate for ProfileApprovalGate {
    fn request<'a>(&'a self, request: &'a ApprovalRequest) -> BoxFuture<'a, Decision> {
        let decision = self.profile.decide(&request.call);
        let reason = decision.reason;
        let verdict = decision.verdict;

        Box::pin(async move {
            match verdict {
                crate::domain::policy::PolicyVerdict::Allow => {
                    self.record(request, true, Some(reason));
                    Ok(())
                }
                crate::domain::policy::PolicyVerdict::Deny => {
                    self.record(request, false, Some(reason.clone()));
                    Err(reason)
                }
                crate::domain::policy::PolicyVerdict::RequireApproval => {
                    let result = self.inner.request(request).await;
                    self.record(request, result.is_ok(), result.as_ref().err().cloned());
                    result
                }
            }
        })
    }

    fn bind_cancel(&self, cancel: crate::application::port::CancelHandle) {
        self.inner.bind_cancel(cancel);
    }
}

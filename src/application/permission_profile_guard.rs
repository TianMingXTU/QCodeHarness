//! 审批分级的工具守卫：把 profile 与现有策略基线合并成可见三态。
//!
//! 守卫只能给策略结论加一票，不能把已有 `Deny` 改回放行；真正放宽审批
//! 的动作由 `infrastructure::approval::profile_gate` 在闸门接缝处完成。
//! 本守卫只在 `ReadOnly` 档用 `ToolSpec` 保留更严格的基线条：工具自述不是
//! 只读时，profile 不会凭命令形状替它免审，避免测试/文档与真实执行链
//! 出现两套结论。其余档位直接透传领域层 `decide` 的结论。

use std::sync::Arc;

use crate::application::tool_guard::{GuardContext, GuardVerdict, ToolGuard};
use crate::domain::permission_profile::PermissionProfile;
use crate::domain::policy::PolicyVerdict;
use crate::domain::tool::{ToolCall, ToolSpec};

/// profile 守卫，注入 `AgentDependencies::additional_guards`。
pub struct ProfileGuard {
    profile: PermissionProfile,
    baseline: Option<PolicyVerdict>,
}

impl ProfileGuard {
    /// 构造守卫并装盒。
    ///
    /// `baseline` 是配置层对同名命令已有的策略结论。传入 `Deny` 时任何
    /// profile 都不能放宽，避免配置里明确封禁的动作被 `full` 档绕过。
    pub fn new(profile: PermissionProfile, baseline: Option<PolicyVerdict>) -> Arc<dyn ToolGuard> {
        Arc::new(Self { profile, baseline })
    }

    /// 合并 profile 与已知策略结论，返回守卫链的单一裁决。
    fn inspect_with_baseline(&self, call: &ToolCall, spec: &ToolSpec) -> GuardVerdict {
        if self.baseline == Some(PolicyVerdict::Deny) {
            return GuardVerdict::Deny("现有策略明确拒绝，profile 不能放宽".to_owned());
        }
        let decision = self.profile.decide(call);
        let verdict = match (self.profile, decision.verdict) {
            // 只读档：工具自述不是只读时，命令形状再像 `cargo test` 也要拒绝。
            (PermissionProfile::ReadOnly, PolicyVerdict::Allow) if !spec.annotations.read_only => {
                PolicyVerdict::Deny
            }
            // 策略基线要求审批、profile 又放行时，保留一次审批，不让 profile
            // 静默取消人工确认。
            (_, PolicyVerdict::Allow) if self.baseline == Some(PolicyVerdict::RequireApproval) => {
                PolicyVerdict::RequireApproval
            }
            (_, verdict) => verdict,
        };
        match verdict {
            PolicyVerdict::Allow => GuardVerdict::Pass,
            PolicyVerdict::RequireApproval => GuardVerdict::RequireApproval(decision.reason),
            PolicyVerdict::Deny => GuardVerdict::Deny(decision.reason),
        }
    }
}

impl ToolGuard for ProfileGuard {
    fn name(&self) -> &str {
        "permission-profile"
    }

    fn inspect(&self, call: &ToolCall, ctx: &GuardContext<'_>) -> GuardVerdict {
        self.inspect_with_baseline(call, ctx.spec)
    }
}

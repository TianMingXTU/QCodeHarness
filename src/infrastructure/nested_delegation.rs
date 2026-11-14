//! 显式审批协议帧的服务端交换与证据落账。
//!
//! 本模块只负责把 `application::nested_delegation` 的纯帧写入已存在的服务
//! 事件账本，再从同一账本构造结论帧。父进程不读取子进程私有状态，也不把
//! 事件日志当作隐式授权；请求、结论和越界拒绝各有可核对的一行事件。

use std::sync::Arc;

use crate::application::nested_delegation::{
    ApprovalFramePayload, ApprovalState, DelegationFrame, FRAME_VERSION, MAX_NESTING, frame_fact,
    frame_line, parse_frame, validate_nesting, validate_state,
};
use crate::application::service::EventJournal;
use crate::domain::evidence::AuditEvent;
use crate::domain::policy::PolicyVerdict;
use crate::domain::service::{EventContext, ServiceError, ServiceEvent, ServiceEventPayload};

/// 把显式审批帧交给服务事件账本的传输实现。
pub struct ApprovalFrameTransport {
    journal: Arc<dyn EventJournal>,
}

impl ApprovalFrameTransport {
    /// 用共享事件账本构造；账本的写入、广播与序号仍由服务统一负责。
    pub fn new(journal: Arc<dyn EventJournal>) -> Self {
        Self { journal }
    }

    /// 把一批请求帧逐行编码、校验，再写成请求与结论两类事件。
    ///
    /// 返回的是跨进程边界上真正可发送/可接收的一对帧：请求帧仍保持
    /// `Pending`，结论帧带同一 `parent_id + depth + approval_id`。任一层
    /// 越界时先写拒绝证据再返回 `nesting_limit`，绝不静默丢弃请求。
    pub async fn exchange(
        &self,
        context: Option<&EventContext>,
        frames: Vec<DelegationFrame>,
    ) -> Result<Vec<DelegationFrame>, ServiceError> {
        let mut outcomes = Vec::with_capacity(frames.len() * 2);
        let mut payloads = Vec::with_capacity(frames.len() * 2);
        for frame in frames {
            if let Err(error) = validate_nesting(frame.depth) {
                self.reject_depth(context, frame.depth)?;
                return Err(error);
            }
            let line = frame_line(&frame)?;
            let parsed = parse_frame(&line)?;
            let fact = frame_fact(&parsed)?;
            let Some(approval_id) = request_approval_id(&parsed) else {
                return Err(ServiceError::new(
                    "invalid_frame_state",
                    "审批链交换只接受显式的 approval_request 帧",
                ));
            };
            if parsed.approval_state != ApprovalState::Pending {
                return Err(ServiceError::new(
                    "invalid_frame_state",
                    "审批链交换只接受尚未答复的请求帧",
                ));
            }
            let tool = match &parsed.payload {
                ApprovalFramePayload::ApprovalRequest { call, .. } => call.name.clone(),
                _ => unreachable!("request_approval_id 已校验载荷类型"),
            };
            let reason = format!(
                "显式审批请求帧 depth={} parent={} fact={fact}",
                parsed.depth, parsed.parent_id
            );
            payloads.push(ServiceEventPayload::Audit {
                event: AuditEvent::ApprovalReasked {
                    turn: u32::try_from(parsed.depth + 1)
                        .map_err(|_| ServiceError::new("nesting_limit", "审批深度超出轮次范围"))?,
                    tool: tool.clone(),
                    reason,
                },
            });
            let granted = DelegationFrame::new(
                parsed.parent_id.clone(),
                parsed.depth,
                ApprovalState::Granted,
                ApprovalFramePayload::ApprovalGranted {
                    approval_id: approval_id.clone(),
                },
            )?;
            let granted_fact = frame_fact(&granted)?;
            payloads.push(ServiceEventPayload::ApprovalResolved {
                approval_id: approval_id.clone(),
                allowed: true,
                reason: Some(format!(
                    "显式放行帧 depth={} fact={granted_fact}",
                    parsed.depth
                )),
            });
            outcomes.push(parsed);
            outcomes.push(granted);
        }
        self.journal.append_batch(context, payloads)?;
        Ok(outcomes)
    }

    /// 从持久事件账本读取显式结论帧；旧版没有帧事实的事件不会被推断成放行。
    pub fn read_granted_frames(
        &self,
        after: u64,
        limit: usize,
    ) -> Result<Vec<DelegationFrame>, ServiceError> {
        let events = self.journal.read(after, limit)?;
        events.iter().filter_map(granted_frame_from_event).collect()
    }

    /// 记录一次越界派生被拒绝；拒绝本身也是策略证据，不静默吞掉。
    pub fn reject_depth(
        &self,
        context: Option<&EventContext>,
        attempted_depth: usize,
    ) -> Result<ServiceEvent, ServiceError> {
        let error = match validate_nesting(attempted_depth) {
            Err(error) => error,
            Ok(()) => {
                return Err(ServiceError::new(
                    "nesting_limit",
                    format!("深度 {attempted_depth} 未越界，不能记为拒绝"),
                ));
            }
        };
        let frame = DelegationFrame {
            version: FRAME_VERSION,
            parent_id: "nesting-limit".into(),
            depth: MAX_NESTING - 1,
            approval_state: ApprovalState::Denied,
            payload: ApprovalFramePayload::NestingRejected {
                attempted_depth,
                limit: MAX_NESTING,
            },
        };
        let fact = frame_fact(&frame)?;
        self.journal.append(
            context,
            ServiceEventPayload::Audit {
                event: AuditEvent::PolicyEvaluated {
                    turn: 1,
                    tool: "delegate_subtasks".into(),
                    verdict: PolicyVerdict::Deny,
                    rule_index: None,
                    reason: format!("{}；拒绝帧={fact}", error.message),
                },
            },
        )
    }
}

fn request_approval_id(frame: &DelegationFrame) -> Option<String> {
    match &frame.payload {
        ApprovalFramePayload::ApprovalRequest { approval_id, .. } => Some(approval_id.clone()),
        _ => None,
    }
}

fn granted_frame_from_event(event: &ServiceEvent) -> Option<Result<DelegationFrame, ServiceError>> {
    let ServiceEventPayload::ApprovalResolved {
        approval_id,
        allowed: true,
        reason,
    } = &event.payload
    else {
        return None;
    };
    let Some((_, fact)) = reason
        .as_ref()
        .and_then(|reason| reason.split_once("fact="))
    else {
        return None;
    };
    let mut frame: DelegationFrame = match serde_json::from_str(fact) {
        Ok(frame) => frame,
        Err(error) => return Some(Err(ServiceError::new("invalid_frame", error.to_string()))),
    };
    frame.approval_state = ApprovalState::Granted;
    frame.payload = ApprovalFramePayload::ApprovalGranted {
        approval_id: approval_id.clone(),
    };
    Some(
        match validate_state(&frame).and_then(|_| validate_nesting(frame.depth)) {
            Ok(()) => Ok(frame),
            Err(error) => Err(error),
        },
    )
}

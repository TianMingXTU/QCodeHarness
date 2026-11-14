//! 有限嵌套委派：纯类型、深度边界和跨进程审批协议帧。
//!
//! 这里不碰进程、文件、工作树或运行时，也不从证据文件推断审批结论。
//! 父子之间的状态只由显式帧表达，帧经 NDJSON 序列化后由基础设施层交换；
//! 每一级审批链都能凭 `parent_id + depth + approval_id` 对账，读回合并既不
//! 能表达"谁尚未答复"，也无法阻止子任务无限派生。

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::domain::service::ServiceError;
use crate::domain::tool::ToolCall;

/// 允许的最大嵌套深度。
///
/// 深度 0 是根任务，1 是子任务，2 是孙任务；深度 3 及以上不再装配子运行。
/// 上限取 3 而非复用 16 个子任务并发上限，是为了让每级审批链仍能逐条复核。
pub const MAX_NESTING: usize = 3;

/// 协议帧版本；帧形状不兼容变化时必须提升并由接收方明确拒绝。
pub const FRAME_VERSION: u32 = 1;

/// 一次委派在审批链上的状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalState {
    /// 已发出请求，尚无人答复。不能据此放行。
    Pending,
    /// 显式放行；必须与 approval_id、深度和父标识一起核对。
    Granted,
    /// 显式否决；只影响本次调用，不等于终止整棵委派树。
    Denied,
}

/// 显式协议帧的载荷，覆盖审批请求、放行、否决和深度拒绝。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ApprovalFramePayload {
    /// 某一级请求人工审批。调用参数原样携带，供前端与证据账本核对。
    ApprovalRequest {
        approval_id: String,
        call: ToolCall,
        reason: Option<String>,
    },
    /// 某一级明确批准。
    ApprovalGranted { approval_id: String },
    /// 某一级明确否决，并保留原因。
    ApprovalDenied { approval_id: String, reason: String },
    /// 超过嵌套上限，接收方拒绝继续派生。
    NestingRejected {
        attempted_depth: usize,
        limit: usize,
    },
}

/// 跨进程协议帧。NDJSON 一行一帧，帧自带版本、父标识和深度。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DelegationFrame {
    pub version: u32,
    pub parent_id: String,
    pub depth: usize,
    pub approval_state: ApprovalState,
    pub payload: ApprovalFramePayload,
}

impl DelegationFrame {
    /// 构造当前协议版本的帧，并校验深度。
    pub fn new(
        parent_id: impl Into<String>,
        depth: usize,
        approval_state: ApprovalState,
        payload: ApprovalFramePayload,
    ) -> Result<Self, ServiceError> {
        validate_nesting(depth)?;
        Ok(Self {
            version: FRAME_VERSION,
            parent_id: parent_id.into(),
            depth,
            approval_state,
            payload,
        })
    }
}

/// 构造一次审批请求帧；测试与装配方共用，避免各自拼字段后语义漂移。
///
/// `approval_id` 是调用方对账用的业务标识；帧载荷已经携带同名标识，这里
/// 只核对它与载荷一致，避免帧头与审批请求指向两次不同的审批。
pub fn approval_frame(
    parent_id: impl Into<String>,
    depth: usize,
    approval_id: impl Into<String>,
    payload: ApprovalFramePayload,
) -> Result<DelegationFrame, ServiceError> {
    let approval_id = approval_id.into();
    match &payload {
        ApprovalFramePayload::ApprovalRequest {
            approval_id: id, ..
        } if id == &approval_id => {}
        _ => {
            return Err(ServiceError::new(
                "invalid_frame_state",
                "审批请求帧的 approval_id 与载荷不一致",
            ));
        }
    }
    DelegationFrame::new(parent_id, depth, ApprovalState::Pending, payload)
}

/// 校验嵌套深度；达到或超过上限都拒绝。
///
/// 返回服务层稳定错误码而不是 bool，让跨进程接收方能明确回复拒绝帧，
/// 不必从空结果猜原因。
pub fn validate_nesting(depth: usize) -> Result<(), ServiceError> {
    if depth >= MAX_NESTING {
        return Err(ServiceError::new(
            "nesting_limit",
            format!("委派深度 {depth} 已达上限 {MAX_NESTING}，不再派生子任务"),
        ));
    }
    Ok(())
}

/// 把帧编码成一行 NDJSON；拒绝嵌入换行，防止一条物理行被读成两条帧。
pub fn frame_line(frame: &DelegationFrame) -> Result<String, ServiceError> {
    let line = serde_json::to_string(frame)
        .map_err(|error| ServiceError::new("frame_encoding", error.to_string()))?;
    if line.contains(['\n', '\r']) {
        return Err(ServiceError::new("frame_encoding", "协议帧不能包含换行"));
    }
    Ok(line)
}

/// 从一行 NDJSON 解析帧；版本、字段与深度都在边界处一次校验。
pub fn parse_frame(line: &str) -> Result<DelegationFrame, ServiceError> {
    let frame: DelegationFrame = serde_json::from_str(line)
        .map_err(|error| ServiceError::new("invalid_frame", error.to_string()))?;
    if frame.version != FRAME_VERSION {
        return Err(ServiceError::new(
            "unsupported_frame_version",
            format!(
                "不支持委派帧版本 {}，当前版本为 {FRAME_VERSION}",
                frame.version
            ),
        ));
    }
    validate_nesting(frame.depth)?;
    validate_state(&frame)?;
    Ok(frame)
}

/// 校验帧状态与载荷是否自洽，防止把"待答复"文本误当成已批准。
pub fn validate_state(frame: &DelegationFrame) -> Result<(), ServiceError> {
    let consistent = match (&frame.approval_state, &frame.payload) {
        (ApprovalState::Pending, ApprovalFramePayload::ApprovalRequest { .. }) => true,
        (ApprovalState::Granted, ApprovalFramePayload::ApprovalGranted { .. }) => true,
        (ApprovalState::Denied, ApprovalFramePayload::ApprovalDenied { .. })
        | (ApprovalState::Denied, ApprovalFramePayload::NestingRejected { .. }) => true,
        _ => false,
    };
    if !consistent {
        return Err(ServiceError::new(
            "invalid_frame_state",
            format!("审批状态 {:?} 与协议帧载荷不一致", frame.approval_state),
        ));
    }
    Ok(())
}

/// 把帧载荷编码成服务事件可审计的 JSON 值。
///
/// 只做数据形态转换，不写日志：落 `events.jsonl` 与广播由基础设施层完成。
pub fn frame_fact(frame: &DelegationFrame) -> Result<Value, ServiceError> {
    validate_state(frame)?;
    serde_json::to_value(frame)
        .map_err(|error| ServiceError::new("frame_encoding", error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::ids::CallId;
    use serde_json::json;

    #[test]
    fn pending_request_round_trips_as_one_ndjson_line() {
        let frame = DelegationFrame::new(
            "root",
            1,
            ApprovalState::Pending,
            ApprovalFramePayload::ApprovalRequest {
                approval_id: "a1".into(),
                call: ToolCall {
                    id: CallId::new("c1"),
                    name: "write_file".into(),
                    arguments: json!({"path": "a.txt"}),
                },
                reason: None,
            },
        )
        .unwrap();
        let line = frame_line(&frame).unwrap();
        assert!(!line.contains('\n'));
        assert_eq!(parse_frame(&line).unwrap(), frame);
    }

    #[test]
    fn granted_state_cannot_be_disguised_as_a_pending_request() {
        let frame = DelegationFrame {
            version: FRAME_VERSION,
            parent_id: "root".into(),
            depth: 1,
            approval_state: ApprovalState::Granted,
            payload: ApprovalFramePayload::ApprovalRequest {
                approval_id: "a1".into(),
                call: ToolCall {
                    id: CallId::new("c1"),
                    name: "write_file".into(),
                    arguments: json!({}),
                },
                reason: None,
            },
        };
        assert_eq!(
            validate_state(&frame).unwrap_err().code,
            "invalid_frame_state"
        );
    }

    #[test]
    fn nesting_boundary_is_explicit() {
        assert!(validate_nesting(MAX_NESTING - 1).is_ok());
        assert_eq!(
            validate_nesting(MAX_NESTING).unwrap_err().code,
            "nesting_limit"
        );
        assert_eq!(
            validate_nesting(MAX_NESTING + 10).unwrap_err().code,
            "nesting_limit"
        );
    }
}

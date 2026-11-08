//! 工具抽象：模型可调用的能力。
//!
//! 领域层的工具定义不依赖 rig，由基础设施层翻译成 rig 的 `ToolDefinition`。
//! 模型看到的能力与 Harness 的执行框架因此可以独立变化。

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::ids::CallId;
use super::mcp::McpCallFailure;
use super::sandbox::ExecutionBoundary;

/// MCP 工具注册名的前缀：`mcp__<server>__<tool>`。
pub const MCP_TOOL_PREFIX: &str = "mcp__";

/// 工具的风险注解：向模型和审批环节声明副作用特征。
///
/// rig 已有 `ToolAnnotations`，但原有代码从未填写，哪些工具能改文件只
/// 存在于 prompt 中。这里在领域侧用自有类型表达同一组语义，由基础设施层
/// 翻译。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ToolAnnotations {
    /// 是否只读。只读是"确定无副作用"的唯一可靠信号。
    pub read_only: bool,
    /// 是否有破坏性（覆盖、删除、不可逆修改）。
    pub destructive: bool,
    /// 是否幂等：重复执行同样的参数是否得到同样结果。
    pub idempotent: bool,
    /// 是否与开放世界交互（网络、外部服务）。
    pub open_world: bool,
}

impl ToolAnnotations {
    /// 构造注解。四个 bool 连续传入，调用点需注意参数顺序。
    #[allow(clippy::too_many_arguments)]
    pub fn new(read_only: bool, destructive: bool, idempotent: bool, open_world: bool) -> Self {
        Self {
            read_only,
            destructive,
            idempotent,
            open_world,
        }
    }

    /// 是否需要通过审批闸门。
    ///
    /// 判据是 `!read_only` 而非 `destructive`：破坏性由工具作者主观声明，
    /// 漏标即留下后门；「非只读」是客观属性，只要可能改变状态就先询问，
    /// 与任何恒放行的实现保持同一接缝。
    pub fn requires_approval(&self) -> bool {
        !self.read_only
    }
}

/// 工具规格：向模型描述一个可调用能力，并携带它的风险注解。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    /// 工具名，模型用它来指名调用。
    pub name: String,
    /// 给模型看的功能说明。
    pub description: String,
    /// 参数的 JSON Schema，基础设施层原样序列化给模型。
    pub parameters: Value,
    /// 风险注解，驱动审批闸门。
    pub annotations: ToolAnnotations,
}

/// 模型提出的一次工具调用意图。领域层只记录，是否执行与执行时机由
/// 应用层决定。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    /// 模型给出的调用 ID，观察结果凭此送回。
    pub id: CallId,
    /// 被指名的工具名。可能是未注册的名字，由应用层生成拒绝观察。
    pub name: String,
    /// 模型给出的参数。
    pub arguments: Value,
}

/// 工具执行结果的三种结局。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OutcomeStatus {
    /// 正常完成。
    Success,
    /// 执行失败（IO 错误、参数错误等），原因在 `ToolOutcome::content` 里。
    Failed,
    /// Harness 主动拒绝（路径越界、审批否决、工具未注册）。
    Refused,
}

/// 一次工具执行的观察结果，会回填给模型。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolOutcome {
    /// 给模型看的文本内容。失败时这里是原因说明。
    pub content: String,
    /// 结局分类，决定回填时是否标记 `is_error`。
    pub status: OutcomeStatus,
    /// 执行器交给应用层的边界事实；未启动进程或未调用外部工具时为 None。
    ///
    /// 只作进程内交接，应用层取走后单独记入 `SandboxApplied`。
    /// 不写入 `ToolObserved` 或快照，保持既有 JSON 字段形状兼容。
    #[serde(skip)]
    pub execution: Option<ExecutionBoundary>,
    /// MCP 执行器给出的调用故障事实；应用层取走后单独记入 `McpCallFailed`。
    ///
    /// 与 `execution` 相同，只作进程内交接：不写入 `ToolObserved`、快照或
    /// 历史，避免用执行器错误文本推断故障类别。
    #[serde(skip)]
    pub mcp_failure: Option<McpCallFailure>,
}

impl ToolOutcome {
    /// 构造一份成功观察。
    pub fn success(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            status: OutcomeStatus::Success,
            execution: None,
            mcp_failure: None,
        }
    }

    /// 构造一份失败观察。
    pub fn failed(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            status: OutcomeStatus::Failed,
            execution: None,
            mcp_failure: None,
        }
    }

    /// 构造一份拒绝观察。
    pub fn refused(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            status: OutcomeStatus::Refused,
            execution: None,
            mcp_failure: None,
        }
    }

    /// 附加已执行操作的边界事实，由执行器在启动进程或调用外部工具后填写。
    pub fn with_execution(mut self, boundary: ExecutionBoundary) -> Self {
        self.execution = Some(boundary);
        self
    }

    /// 附加 MCP 调用故障的分类事实，由执行器在调用失败时填写。
    pub fn with_mcp_failure(mut self, failure: McpCallFailure) -> Self {
        self.mcp_failure = Some(failure);
        self
    }

    /// 回填给模型时是否标记为错误。拒绝也算错误，模型需要知道这一步未成功，
    /// 才能选择其他路径，避免按成功继续推理。
    pub fn is_error(&self) -> bool {
        !matches!(self.status, OutcomeStatus::Success)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::mcp::{McpCallFailure, McpFaultKind};
    use serde_json::json;

    fn failure() -> McpCallFailure {
        McpCallFailure {
            server: "demo".to_owned(),
            generation: 2,
            kind: McpFaultKind::CallTimeout,
            result_unknown: true,
            process_isolation: true,
        }
    }

    #[test]
    fn mcp_failure_metadata_never_reaches_persisted_outcomes() {
        let outcome = ToolOutcome::failed("调用超时").with_mcp_failure(failure());
        let old_shape = json!({"content": "调用超时", "status": "Failed"});
        assert_eq!(serde_json::to_value(&outcome).unwrap(), old_shape);
        let restored: ToolOutcome = serde_json::from_value(old_shape).unwrap();
        assert_eq!(restored, ToolOutcome::failed("调用超时"));
        assert!(restored.mcp_failure.is_none());
    }

    #[test]
    fn constructors_start_with_no_mcp_failure() {
        for outcome in [
            ToolOutcome::success("好"),
            ToolOutcome::failed("坏"),
            ToolOutcome::refused("不"),
        ] {
            assert!(outcome.mcp_failure.is_none());
        }
    }

    #[test]
    fn execution_metadata_does_not_change_persisted_outcomes() {
        let outcome = ToolOutcome::failed("命令超时")
            .with_execution(ExecutionBoundary::process(false, true));
        let old_shape = json!({"content": "命令超时", "status": "Failed"});
        assert_eq!(serde_json::to_value(&outcome).unwrap(), old_shape);
        let restored: ToolOutcome = serde_json::from_value(old_shape).unwrap();
        assert_eq!(restored, ToolOutcome::failed("命令超时"));
        assert!(restored.execution.is_none());
    }

    #[test]
    fn outcome_text_does_not_assert_execution() {
        for outcome in [
            ToolOutcome::success("退出码: 0"),
            ToolOutcome::failed("命令超时"),
            ToolOutcome::refused("拒绝执行"),
        ] {
            assert!(outcome.execution.is_none());
        }
    }
}

//! 工具抽象：模型可调用的能力。
//!
//! 领域层的工具定义不依赖 rig，只在基础设施层翻译成 rig 的 `ToolDefinition`。
//! 这样"模型看到什么"与"Harness 用什么框架执行"可以独立变化。

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::ids::CallId;

/// 工具的风险注解：向模型和审批环节声明这个工具的副作用特征。
///
/// rig 的 `ToolAnnotations` 早已存在，但原有代码完全没有填写，
/// 等于把"哪些工具能改文件"这件事只留在了 prompt 里。这里在领域侧
/// 用自己的类型表达同一组语义，由基础设施层翻译过去。
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
    /// 组装一份注解。字段用命名参数形式传入，避免四个 bool 连写看错行。
    #[allow(clippy::too_many_arguments)]
    pub fn new(read_only: bool, destructive: bool, idempotent: bool, open_world: bool) -> Self {
        Self {
            read_only,
            destructive,
            idempotent,
            open_world,
        }
    }

    /// 是否需要过审批闸门。
    ///
    /// 判据取 `!read_only` 而不是 `destructive`：破坏性是工具作者的主观声明，
    /// 漏标一个工具就等于开了一个后门；而"不是只读"是客观的——
    /// 只要可能改动状态就先问一声。P0 的 `AutoApproveGate` 恒放行，
    /// 行为与现状一致，但接缝已经打通。
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

/// 模型提出的一次工具调用意图。
///
/// 注意这只是"意图"：领域层记录它，但执行与否、何时执行由应用层决定。
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

/// 一次工具执行的观察结果，会被回填给模型。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolOutcome {
    /// 给模型看的文本内容。失败时这里是原因说明。
    pub content: String,
    /// 结局分类，决定回填时是否标记 `is_error`。
    pub status: OutcomeStatus,
}

impl ToolOutcome {
    /// 构造一份成功观察。
    pub fn success(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            status: OutcomeStatus::Success,
        }
    }

    /// 构造一份失败观察。
    pub fn failed(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            status: OutcomeStatus::Failed,
        }
    }

    /// 构造一份拒绝观察。
    pub fn refused(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            status: OutcomeStatus::Refused,
        }
    }

    /// 回填给模型时是否标记为错误。拒绝同样算错误——模型需要知道
    /// 这一步没成，才能换路子，而不是假设成功继续往下编。
    pub fn is_error(&self) -> bool {
        !matches!(self.status, OutcomeStatus::Success)
    }
}

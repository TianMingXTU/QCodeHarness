//! Agent 抽象：ReAct 循环中的参与者角色与状态。
//!
//! 领域层的 Agent 类型表达"一轮交互"的完整信息，不依赖 rig 的消息类型——
//! rig 的 `AssistantContent` 是协议层的表示，会随框架版本变，不能渗进领域。

use super::tool::ToolCall;

/// Agent 角色。P0 只有单一编码助手；先留出枚举位置，
/// 多 Agent 编排（P4/P5）时按角色分化行为，不必改调用方。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentRole {
    /// 编码助手：读写文件、回答问题。
    Coder,
}

/// Agent 规格：一次运行所需的全部静态配置。
///
/// 把配置收敛成一个不可变值对象，`AgentRunner` 就不必各自持有
/// prompt——少了一处可能不同步的状态。预算不在其中：它是每次运行
/// 可调的外部输入，由 `AgentRunner::run` 传入。
///
/// 工具规格也不在这里：P2 起模型可见的工具由 `ToolRegistry` 提供，
/// 与执行入口绑在一起（`ModelRequest.tools`），避免"spec 声明了工具
/// 却没人执行它"的双源头漂移。
#[derive(Debug, Clone, PartialEq)]
pub struct AgentSpec {
    /// 角色。
    pub role: AgentRole,
    /// 系统提示词，说明 ReAct 工作方式与约束。
    pub system_prompt: String,
}

/// 模型一轮回复的领域表示。
///
/// 刻意不是 rig 的 `AssistantContent`：领域只关心"想了什么、想调什么"，
/// 不关心协议里还有哪些内容块类型。
#[derive(Debug, Clone, PartialEq)]
pub struct AgentTurn {
    /// 模型本轮输出的文本，即"思考"；部分模型直接给调用而文本为空。
    pub text: String,
    /// 模型提出的工具调用意图；为空表示给出最终答案。
    pub calls: Vec<ToolCall>,
}

impl AgentTurn {
    /// 是否为最终答案：没有调用意图时，文本就是结论。
    pub fn is_final_answer(&self) -> bool {
        self.calls.is_empty()
    }
}

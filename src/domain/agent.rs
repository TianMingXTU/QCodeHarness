//! Agent 抽象：ReAct 循环中的参与者角色与状态。
//!
//! 领域层的 Agent 类型表达"一轮交互"的完整信息，不依赖 rig 的消息类型——
//! rig 的 `AssistantContent` 是协议层的表示，会随框架版本变，不能渗进领域。

use serde::{Deserialize, Serialize};

use super::tool::ToolCall;

/// Agent 角色。P0 只有单一编码助手；P5 起按角色分化行为——
/// 不同角色看到不同的系统提示词、拿到不同的工具子集（见
/// `domain::agent_profile`），编排器按步骤的角色分派。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentRole {
    /// 规划者：理解目标、拆解任务。只读工具。
    Planner,
    /// 编码者：读写文件、跑命令。全量工具。
    Coder,
    /// 测试者：跑测试、收集结果。只读 + 命令。
    Tester,
    /// 评审者：读 diff、识别风险。只读工具。
    Reviewer,
}

impl AgentRole {
    /// 角色标识，用于日志、`--list-sessions` 输出与将来的策略规则。
    pub fn name(&self) -> &'static str {
        match self {
            Self::Planner => "planner",
            Self::Coder => "coder",
            Self::Tester => "tester",
            Self::Reviewer => "reviewer",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_round_trips_through_serde() {
        // 角色要进快照（AgentSpec 可序列化），扩展变体后每个变体都必须
        // 能无损往返——漏掉任何一个，恢复时就报反序列化错误。
        for role in [
            AgentRole::Planner,
            AgentRole::Coder,
            AgentRole::Tester,
            AgentRole::Reviewer,
        ] {
            let text = serde_json::to_string(&role).unwrap();
            let restored: AgentRole = serde_json::from_str(&text).unwrap();
            assert_eq!(restored, role);
        }
    }

    #[test]
    fn role_names_are_stable_identifiers() {
        // 这些名字会出现在日志与 --list-sessions 里，改了就是对外变更。
        assert_eq!(AgentRole::Planner.name(), "planner");
        assert_eq!(AgentRole::Coder.name(), "coder");
        assert_eq!(AgentRole::Tester.name(), "tester");
        assert_eq!(AgentRole::Reviewer.name(), "reviewer");
    }
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
///
/// 可序列化：快照要把它一起存，恢复时才能用回同一份系统提示词。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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

//! Agent 抽象：ReAct 循环中的参与者角色与状态。
//!
//! 领域层的 Agent 类型表达"一轮交互"的完整信息，不依赖 rig 的消息类型——
//! rig 的 `AssistantContent` 是协议层的表示，会随框架版本变，不能渗进领域。

use serde::{Deserialize, Serialize};

use super::tool::ToolCall;

/// Agent 角色。P0 只有单一编码助手；P5 起按角色分化行为——
/// 不同角色看到不同的系统提示词、拿到不同的工具子集（见
/// `domain::agent_profile`），编排器按步骤的角色分派。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
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

    /// 这个角色用的模型环境变量名。
    ///
    /// 四个角色的模型名各自独立：规划要强推理、测试跑命令读输出就行，
    /// 用同一个模型是浪费。变量名按 `LLM_MODEL_<角色>` 的规律拼，
    /// 用户看一眼就知道怎么配。
    pub fn model_env_name(&self) -> &'static str {
        match self {
            Self::Planner => "LLM_MODEL_PLANNER",
            Self::Coder => "LLM_MODEL_CODER",
            Self::Tester => "LLM_MODEL_TESTER",
            Self::Reviewer => "LLM_MODEL_REVIEWER",
        }
    }

    /// 这个角色的推理档位环境变量名。
    pub fn effort_env_name(&self) -> &'static str {
        match self {
            Self::Planner => "LLM_EFFORT_PLANNER",
            Self::Coder => "LLM_EFFORT_CODER",
            Self::Tester => "LLM_EFFORT_TESTER",
            Self::Reviewer => "LLM_EFFORT_REVIEWER",
        }
    }

    /// 这个角色的输出档位环境变量名。
    pub fn verbosity_env_name(&self) -> &'static str {
        match self {
            Self::Planner => "LLM_VERBOSITY_PLANNER",
            Self::Coder => "LLM_VERBOSITY_CODER",
            Self::Tester => "LLM_VERBOSITY_TESTER",
            Self::Reviewer => "LLM_VERBOSITY_REVIEWER",
        }
    }

    /// 全部四个角色。
    ///
    /// 配置装配要遍历所有角色——少一个，那个角色的专属配置就静默失效。
    pub fn all() -> [AgentRole; 4] {
        [
            Self::Planner,
            Self::Coder,
            Self::Tester,
            Self::Reviewer,
        ]
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

    // ── P11-3 模型与档位 ───────────────────────────────────────────────

    /// 档位短名能往返解析。
    ///
    /// 档位从环境变量来，是人手写的：`High` 与 `high` 不该是两回事，
    /// 拼错则必须当场报错而不是静默退回默认值。
    #[test]
    fn effort_names_round_trip() {
        for effort in [Effort::Minimal, Effort::Low, Effort::Medium, Effort::High] {
            assert_eq!(Effort::parse(effort.name()).unwrap(), effort);
        }
        // 大小写与首尾空白都不该改变含义。
        assert_eq!(Effort::parse(" HIGH ").unwrap(), Effort::High);
        assert!(Effort::parse("最大").is_err(), "认不出的档位要报错");
    }

    #[test]
    fn verbosity_names_round_trip() {
        for verbosity in [Verbosity::Low, Verbosity::Medium, Verbosity::High] {
            assert_eq!(Verbosity::parse(verbosity.name()).unwrap(), verbosity);
        }
        assert_eq!(Verbosity::parse("Medium").unwrap(), Verbosity::Medium);
        assert!(Verbosity::parse("verbose").is_err(), "认不出的档位要报错");
    }

    /// 规格里的模型与档位要能无损往返（快照要存它）。
    ///
    /// `AgentSpec` 进快照：恢复会话时要用回同一份系统提示词与模型。
    /// 模型与档位一起存，才不会在恢复时悄悄换成一个不一样的模型——
    /// 恢复出来的会话跑出了另一种结果，还以为是模型不稳定。
    #[test]
    fn the_spec_round_trips_through_serde() {
        let spec = AgentSpec {
            role: AgentRole::Coder,
            system_prompt: "系统提示".to_owned(),
            model: Some("qwen3-coder".to_owned()),
            effort: Some(Effort::High),
            verbosity: Some(Verbosity::Low),
        };

        let text = serde_json::to_string(&spec).unwrap();
        let restored: AgentSpec = serde_json::from_str(&text).unwrap();

        assert_eq!(restored, spec);
        assert_eq!(restored.model.as_deref(), Some("qwen3-coder"));
        assert_eq!(restored.effort, Some(Effort::High));
        assert_eq!(restored.verbosity, Some(Verbosity::Low));
    }

    /// 空的模型与档位也要能往返。
    ///
    /// 默认形态（`None`）是绝大多数部署的样子，它也得能存能取——
    /// 往返失败会让"没配"的会话存不进快照。
    #[test]
    fn a_spec_without_tiers_round_trips_through_serde() {
        let spec = AgentSpec::new(AgentRole::Tester, "测试者");
        let text = serde_json::to_string(&spec).unwrap();
        let restored: AgentSpec = serde_json::from_str(&text).unwrap();
        assert_eq!(restored, spec);
        assert_eq!(restored.model, None);
        assert_eq!(restored.effort, None);
        assert_eq!(restored.verbosity, None);
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
    /// 这个角色用的模型名。`None` = 用运行时的默认模型。
    ///
    /// 模型是**运行时**的属性而不是 agent 规格的一部分——同一个角色在
    /// 不同部署里该用不同的模型。但规格要能带它，因为编排器按角色装配
    /// runner 时才决定用哪个模型。
    pub model: Option<String>,
    /// 推理档位。`None` = provider 默认。
    pub effort: Option<Effort>,
    /// 输出详略档位。`None` = provider 默认。
    pub verbosity: Option<Verbosity>,
}

impl AgentSpec {
    /// 造一份不带模型与档位的规格：都用运行时的默认。
    ///
    /// 三个可选字段各有各的默认值，逐处写 `None` 容易漏、也读不出
    /// "这里就是默认"的意思。
    pub fn new(role: AgentRole, system_prompt: impl Into<String>) -> Self {
        Self {
            role,
            system_prompt: system_prompt.into(),
            model: None,
            effort: None,
            verbosity: None,
        }
    }
}

/// 推理档位：模型在回答前多想多少。
///
/// **领域层自有这个类型，而不是用 rig 的 `rig_core::completion::options::Effort`。**
/// 理由与 `ToolAnnotations` 同款：domain 不依赖 rig，框架换版本时领域
/// 类型不变；基础设施层负责翻译（见 `infrastructure::model::rig_chat`）。
///
/// 档位从低到高。命名对齐 OpenAI 的 `reasoning.effort`（minimal / low /
/// medium / high），provider 不认某个档位时由 rig 的降级策略处理。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Effort {
    /// 最少的推理。
    Minimal,
    /// 少推理。
    Low,
    /// 中等推理。
    Medium,
    /// 多推理。
    High,
}

impl Effort {
    /// 档位标识：配置与日志里用的短名。
    pub fn name(&self) -> &'static str {
        match self {
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }

    /// 从短名解析档位。配置写错时给出可操作的说明。
    ///
    /// 大小写不敏感：配置是人手写的，`High` 与 `high` 不该是两回事。
    pub fn parse(raw: &str) -> Result<Self, String> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "minimal" => Ok(Self::Minimal),
            "low" => Ok(Self::Low),
            "medium" => Ok(Self::Medium),
            "high" => Ok(Self::High),
            other => Err(format!(
                "未知的推理档位 `{other}`：可选 minimal / low / medium / high"
            )),
        }
    }
}

/// 输出详略档位：回答该多长。
///
/// 同样自有类型，理由见 [`Effort`]。命名对齐 OpenAI 的
/// `verbosity`（low / medium / high）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Verbosity {
    /// 尽量简短。
    Low,
    /// 适中。
    Medium,
    /// 详细。
    High,
}

impl Verbosity {
    /// 档位标识：配置与日志里用的短名。
    pub fn name(&self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }

    /// 从短名解析档位。配置写错时给出可操作的说明。
    pub fn parse(raw: &str) -> Result<Self, String> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "low" => Ok(Self::Low),
            "medium" => Ok(Self::Medium),
            "high" => Ok(Self::High),
            other => Err(format!(
                "未知的输出档位 `{other}`：可选 low / medium / high"
            )),
        }
    }
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

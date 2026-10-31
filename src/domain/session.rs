//! Session 抽象：ReAct 循环的完整状态。
//!
//! Session 保证消息配对：每轮交互由 AgentTurn + Observation 组成，
//! 从类型上消除"只 push 助手消息"的可能。
//!
//! 为什么值得单独一个模块：OpenAI 兼容协议要求"带 tool_calls 的助手消息"
//! 之后必须紧跟对应的 tool 结果消息，否则直接 400。原有实现是
//! `main.rs:157/188` 两行独立的 `history.push`，先后顺序写反不会报错，
//! 只会让模型收到一份自相矛盾的历史。把两次 push 收敛成一次
//! [`Session::commit_round`]，顺序错误在编译期就不可能表达。

use std::borrow::Cow;

use super::agent::AgentTurn;
use super::ids::SessionId;
use super::tool::{ToolCall, ToolOutcome};

/// 会话历史中的一个条目。
///
/// `Agent` 与 `Observation` 必须成对出现，且 `Observation` 只能紧跟 `Agent`。
/// 这个约束由 [`Session::commit_round`] 保证——枚举虽是 `pub` 的，但
/// [`Session::turns`] 不提供插入入口，外部无法构造出半轮历史。
#[derive(Debug, Clone, PartialEq)]
pub enum Turn {
    /// 用户消息。第一条是任务描述。
    User(String),
    /// 模型一轮回复：文本（思考）+ 工具调用意图。
    Agent(AgentTurn),
    /// 与紧前一个 `Agent` 的调用一一对应的观察结果。
    Observation(Vec<ToolOutcome>),
}

/// 会话状态被破坏的错误。
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error(
        "第 {turn} 轮的观察数（{observations}）与工具调用数（{calls}）不一致：每条调用必须恰好有一条观察"
    )]
    Unpaired {
        /// 出错的轮次号。
        turn: u32,
        /// 模型提出的调用数。
        calls: usize,
        /// 实际收集到的观察数。
        observations: usize,
    },
}

/// 会话历史的视图片段。
///
/// 为什么不直接暴露 `&[Turn]`：`Turn` 是消息级的（Agent/Observation 交替），
/// 调用方要自己向下看一格才能凑出"一轮"，很容易写错；而上下文窗口管理
/// 又必须以"轮"为单位整体裁剪或摘要。`slices()` 一次把配对关系算好，
/// P3 的裁剪就变成对切片的纯函数变换（见 `domain::context`），循环本身不用改。
///
/// `User` 用 `Cow`：真实历史里的用户消息借用自 `Session`，而裁剪时插入的
/// "已省略历史"占位说明是合成的用户消息，没有可借用的原文——两种来源
/// 要能装进同一个切片序列里发给模型。
#[derive(Debug, Clone, PartialEq)]
pub enum ContextSlice<'a> {
    /// 一条用户消息。
    User(Cow<'a, str>),
    /// 模型一轮，`calls` 与 `observations` 同下标一一对应。
    Round {
        /// 轮次号，从 1 开始，与日志、证据对齐。
        number: u32,
        /// 模型本轮的文本。
        thought: &'a str,
        /// 模型本轮提出的调用。
        calls: &'a [ToolCall],
        /// 本轮的观察结果。
        observations: &'a [ToolOutcome],
    },
}

/// 一次 ReAct 会话。
#[derive(Debug, Clone)]
pub struct Session {
    id: SessionId,
    turns: Vec<Turn>,
}

impl Session {
    /// 新建会话，`task` 作为第一条用户消息进入历史。
    pub fn new(id: SessionId, task: impl Into<String>) -> Self {
        Self {
            id,
            turns: vec![Turn::User(task.into())],
        }
    }

    /// 会话 ID。
    pub fn id(&self) -> &SessionId {
        &self.id
    }

    /// 原子地提交一轮：模型回复与它的观察必须成对。
    ///
    /// 这是消息配对不变量的执行点。观察数与调用数不一致时拒绝提交，
    /// 历史里因此不可能留下没有观察的助手消息。
    ///
    /// # Errors
    ///
    /// 观察数与调用数不相等时返回 [`SessionError::Unpaired`]。
    pub fn commit_round(
        &mut self,
        agent: AgentTurn,
        observations: Vec<ToolOutcome>,
    ) -> Result<(), SessionError> {
        if agent.calls.len() != observations.len() {
            return Err(SessionError::Unpaired {
                turn: self.rounds() + 1,
                calls: agent.calls.len(),
                observations: observations.len(),
            });
        }
        self.turns.push(Turn::Agent(agent));
        // 最终答案轮没有调用，也就没有观察——只留助手消息，
        // 否则历史末尾会多出一条空的 tool 结果。
        if !observations.is_empty() {
            self.turns.push(Turn::Observation(observations));
        }
        Ok(())
    }

    /// 已提交的轮数。
    pub fn rounds(&self) -> u32 {
        self.turns
            .iter()
            .filter(|turn| matches!(turn, Turn::Agent(_)))
            .count() as u32
    }

    /// 原始历史，按发生顺序排列。
    pub fn turns(&self) -> &[Turn] {
        &self.turns
    }

    /// 按用户消息与轮次分组返回视图。
    pub fn slices(&self) -> Vec<ContextSlice<'_>> {
        let mut slices = Vec::new();
        let mut round = 0_u32;
        for (index, turn) in self.turns.iter().enumerate() {
            match turn {
                Turn::User(text) => slices.push(ContextSlice::User(Cow::Borrowed(text))),
                Turn::Agent(agent) => {
                    round += 1;
                    // commit_round 保证 Observation 紧跟 Agent，
                    // 所以向后看一格就能取到配对关系。
                    let observations = match self.turns.get(index + 1) {
                        Some(Turn::Observation(outcomes)) => outcomes.as_slice(),
                        _ => &[],
                    };
                    slices.push(ContextSlice::Round {
                        number: round,
                        thought: &agent.text,
                        calls: &agent.calls,
                        observations,
                    });
                }
                // 已被上面的 Agent 分支消费。
                Turn::Observation(_) => {}
            }
        }
        slices
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::ids::CallId;
    use serde_json::json;

    fn call(name: &str) -> ToolCall {
        ToolCall {
            id: CallId::new(format!("call_{name}")),
            name: name.to_owned(),
            arguments: json!({}),
        }
    }

    fn turn_with(calls: Vec<ToolCall>) -> AgentTurn {
        AgentTurn {
            text: "想一下".to_owned(),
            calls,
        }
    }

    #[test]
    fn commit_round_pairs_calls_with_observations() {
        let mut session = Session::new(SessionId::new("s1"), "读一下 Cargo.toml");
        let calls = vec![call("read_file"), call("list_dir")];
        let observations = vec![ToolOutcome::success("a"), ToolOutcome::success("b")];

        session
            .commit_round(turn_with(calls.clone()), observations.clone())
            .unwrap();

        assert_eq!(session.rounds(), 1);
        assert_eq!(
            session.turns(),
            &[
                Turn::User("读一下 Cargo.toml".to_owned()),
                Turn::Agent(turn_with(calls)),
                Turn::Observation(observations),
            ]
        );
    }

    #[test]
    fn commit_round_rejects_missing_observation() {
        let mut session = Session::new(SessionId::new("s1"), "任务");
        let result = session.commit_round(turn_with(vec![call("read_file")]), Vec::new());

        assert!(matches!(result, Err(SessionError::Unpaired { .. })));
        // 拒绝提交后历史保持原样，不会留下孤立的助手消息。
        assert_eq!(session.rounds(), 0);
        assert_eq!(session.turns().len(), 1);
    }

    #[test]
    fn final_answer_round_leaves_no_empty_observation() {
        let mut session = Session::new(SessionId::new("s1"), "任务");
        session
            .commit_round(
                AgentTurn {
                    text: "结论".to_owned(),
                    calls: Vec::new(),
                },
                Vec::new(),
            )
            .unwrap();

        assert_eq!(session.turns().len(), 2);
        assert_eq!(
            session.slices(),
            vec![
                ContextSlice::User(Cow::Borrowed("任务")),
                ContextSlice::Round {
                    number: 1,
                    thought: "结论",
                    calls: &[],
                    observations: &[],
                },
            ]
        );
    }

    #[test]
    fn slices_align_calls_with_observations() {
        let mut session = Session::new(SessionId::new("s1"), "任务");
        session
            .commit_round(
                turn_with(vec![call("read_file")]),
                vec![ToolOutcome::success("内容")],
            )
            .unwrap();

        let slices = session.slices();
        assert_eq!(slices.len(), 2);
        let Some(ContextSlice::Round {
            number,
            thought,
            calls,
            observations,
        }) = slices.last()
        else {
            panic!("最后一帧应当是一轮模型回复");
        };
        assert_eq!(*number, 1);
        assert_eq!(*thought, "想一下");
        assert_eq!(calls.len(), 1);
        assert_eq!(observations.len(), 1);
    }
}

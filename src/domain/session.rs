//! Session 抽象：ReAct 循环的完整状态。
//!
//! Session 保证消息配对：每轮交互由 AgentTurn + Observation 组成，
//! 从类型上消除"只 push 助手消息"的可能。
//!
//! OpenAI 兼容协议要求带 tool_calls 的助手消息之后紧跟对应的 tool 结果
//! 消息，否则直接 400。早期实现是两行独立的 `history.push`，顺序写反
//! 不会报错，只会让模型收到自相矛盾的历史。两次 push 收敛成一次
//! [`Session::commit_round`] 后，顺序错误无法表达。

use std::borrow::Cow;

use serde::{Deserialize, Serialize};

use super::agent::AgentTurn;
use super::context::Summary;
use super::ids::SessionId;
use super::tool::{ToolCall, ToolOutcome};

/// 会话历史中的一个条目。
///
/// 带调用的轮次里，`Agent` 与 `Observation` 必须成对出现，`Observation`
/// 只能紧跟 `Agent`；最终答案轮只有 `Agent`，没有 `Observation`。该约束
/// 由 [`Session::commit_round`] 保证：枚举是 `pub` 的，但
/// [`Session::turns`] 不提供插入入口，外部无法构造半轮历史。
///
/// 可序列化：快照保存完整历史，恢复时逐条校验后重建。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Turn {
    /// 用户消息。第一条是任务描述。
    User(String),
    /// 运行期间追加的用户输入，不表示模板步骤边界。
    Input { id: String, text: String },
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
    /// 快照历史的布局非法。
    ///
    /// `commit_round` 在构造路径上排除了这些形态。快照中出现它们说明
    /// 数据被改坏或来自不兼容的写入方，重建时报显式错误，不构造
    /// 自相矛盾的历史。
    #[error("快照历史损坏：{reason}")]
    Corrupt {
        /// 哪里坏了、为什么。
        reason: String,
    },
}

/// 会话历史的可序列化快照：`Vec<Turn>` 的裸结构。
///
/// `Session` 不直接 derive `Deserialize`：反序列化得到的是不可信的外部
/// 数据，直接装入 `Session` 会绕过 `commit_round` 的配对不变量。一条
/// "有调用没观察"的半轮历史会让下一次模型请求直接 400。快照先落在裸
/// 结构上，由 [`Session::from_snapshot`] 逐条校验后重建。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionSnapshot {
    /// 完整历史，按发生顺序。
    pub turns: Vec<Turn>,
    /// 已生成的历史摘要。老快照没有这个字段，`#[serde(default)]` 让它
    /// 反序列化为 `None`。新增可选字段不构成快照格式的不兼容变化。
    #[serde(default)]
    pub summary: Option<Summary>,
}

impl SessionSnapshot {
    /// 已经开始了多少步。
    ///
    /// 与 [`Session::started_steps`] 同一口径。快照读回的历史尚未校验成
    /// 会话，恢复前用这个数核对会话停在第几步。
    pub fn started_steps(&self) -> usize {
        count_step_prompts(&self.turns)
    }

    /// 每一轮属于第几步（1-based）。与 [`Session::round_steps`] 同口径。
    pub fn round_steps(&self) -> Vec<u32> {
        round_step_numbers(&self.turns)
    }
}

/// 会话历史的视图片段。
///
/// 不直接暴露 `&[Turn]`：`Turn` 是消息级的（Agent/Observation 交替），
/// 调用方要向下看一格才能凑出一轮，容易写错；上下文窗口管理又必须以轮
/// 为单位裁剪或摘要。`slices()` 一次算好配对关系，裁剪成为对切片的纯
/// 函数变换（见 `domain::context`），循环不用改。
///
/// `User` 使用 `Cow`：真实历史里的用户消息借用自 `Session`，裁剪时插入的
/// "已省略历史"占位说明是合成消息，没有可借用的原文。两种来源要放进
/// 同一个切片序列发给模型。
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
    /// 已生成的历史摘要。摘要不进入 `turns`，它是历史的一种压缩视图：
    /// 挂在会话上随快照保存，裁剪时按覆盖范围取用，覆盖范围不够时会
    /// 生成新的覆盖旧的（见 `domain::context`）。
    summary: Option<Summary>,
}

impl Session {
    /// 新建会话，`task` 作为第一条用户消息进入历史。
    pub fn new(id: SessionId, task: impl Into<String>) -> Self {
        Self {
            id,
            turns: vec![Turn::User(task.into())],
            summary: None,
        }
    }

    /// 记录一段摘要，覆盖旧摘要。
    ///
    /// 调用方（运行循环）负责保证覆盖范围比现有摘要更广。范围更窄的摘要
    /// 会让裁剪退回占位说明，结果不会错，但那次调用没有收益。
    pub fn set_summary(&mut self, summary: Summary) {
        self.summary = Some(summary);
    }

    /// 追加一条用户消息。
    ///
    /// 单会话编排用：步骤边界是一条用户消息（"现在进行第 N 步"），必须和
    /// 任务交代一样进历史、进快照。下一步的模型靠它知道进度，裁剪时也要
    /// 能认出它。
    ///
    /// 独立于 [`Self::commit_round`]：步骤交代不是模型回复，没有观察可配，
    /// 放进 commit_round 会造出一个空壳轮次。
    pub fn push_user(&mut self, message: impl Into<String>) {
        self.turns.push(Turn::User(message.into()));
    }

    /// 追加一条有稳定 ID 的补充输入。
    pub fn push_input(&mut self, id: String, text: String) {
        self.turns.push(Turn::Input { id, text });
    }

    /// 查找已经应用的输入，供重复投递时核对内容。
    pub fn input_text(&self, id: &str) -> Option<&str> {
        self.turns.iter().find_map(|turn| match turn {
            Turn::Input { id: existing, text } if existing == id => Some(text.as_str()),
            _ => None,
        })
    }

    /// 在第 `index` 条历史之前插入一条用户消息。
    ///
    /// 任务级恢复用：交回已完成步骤的结论时，结论要放在"当前这一步的
    /// 交代"**之前**。它是对前几步的总结，放在这一步的交代之后顺序就
    /// 颠倒了。
    ///
    /// `index` 越界时追加到末尾。插入位置算错不应让恢复路径崩溃，消息
    /// 追加后模型仍能看到。
    pub fn insert_user_at(&mut self, index: usize, message: impl Into<String>) {
        let index = index.min(self.turns.len());
        self.turns.insert(index, Turn::User(message.into()));
    }

    /// 当前步骤交代在历史里的位置。
    ///
    /// 当前步骤交代是最后一条**非交接**的用户消息。交接消息是结论总结，
    /// 不参与定位。
    ///
    /// 找不到时返回历史长度，即追加到末尾。
    pub fn step_prompt_position(&self) -> usize {
        self.turns
            .iter()
            .rposition(|turn| match turn {
                Turn::User(text) => !crate::domain::task::is_handoff_message(text),
                _ => false,
            })
            .unwrap_or(self.turns.len())
    }

    /// 已经开始了多少步。
    ///
    /// 每一步的交代是一条用户消息，任务交代就是第 1 步的交代。因此
    /// 非交接用户消息的条数就是已经开始的步数。
    ///
    /// 恢复前用这个数核对任务进度：会话开了几步，任务就该走到第几步
    /// 附近。对不上说明两部分不属于同一个任务。
    pub fn started_steps(&self) -> usize {
        count_step_prompts(&self.turns)
    }

    /// 每一轮属于第几步（1-based）。
    ///
    /// 摘要按步骤分段时使用，模型需要知道这段历史属于第几步。步号由
    /// 结构数出：每条非交接的用户消息是一步的交代，不解析文本里的编号。
    pub fn round_steps(&self) -> Vec<u32> {
        round_step_numbers(&self.turns)
    }

    /// 历史里的全部步骤交代，按发生顺序。
    ///
    /// 排除交接消息：那是步骤的**结论**，不是步骤的**交代**。两者都是
    /// 用户消息但用途不同，混在一起会让"会话开了几步"算错。
    pub fn step_prompts(&self) -> Vec<&str> {
        self.turns
            .iter()
            .filter_map(|turn| match turn {
                Turn::User(text) if !crate::domain::task::is_handoff_message(text) => {
                    Some(text.as_str())
                }
                _ => None,
            })
            .collect()
    }

    /// 历史里的最后一条消息是否是指定的用户消息。
    ///
    /// 恢复路径用：中断那一步的交代已经进了历史，恢复时再推一次会让
    /// 同一句交代出现两次。模型看到两条相同指令，"会话开了几步"也会
    /// 算错。
    pub fn ends_with_user_message(&self, text: &str) -> bool {
        matches!(self.turns.last(), Some(Turn::User(last)) if last == text)
    }

    /// 历史里是否已有这条用户消息。
    ///
    /// 交接用：同一份快照恢复两次时，第二次不再推同一句结论。
    pub fn has_user_message(&self, message: &str) -> bool {
        self.turns.iter().any(|turn| {
            matches!(turn, Turn::User(text) if text == message)
        })
    }

    /// 已挂的摘要。
    pub fn summary(&self) -> Option<&Summary> {
        self.summary.as_ref()
    }

    /// 会话 ID。
    pub fn id(&self) -> &SessionId {
        &self.id
    }

    /// 原子地提交一轮：模型回复与观察必须成对。
    ///
    /// 这是消息配对不变量的执行点。观察数与调用数不一致时拒绝提交，
    /// 历史里不会留下没有观察的助手消息。
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
        // 最终答案轮没有调用，也没有观察。只留助手消息，否则历史末尾
        // 会多出一条空的 tool 结果。
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

    /// 导出可序列化的历史快照。
    pub fn snapshot(&self) -> SessionSnapshot {
        SessionSnapshot {
            turns: self.turns.clone(),
            summary: self.summary.clone(),
        }
    }

    /// 从快照重建会话，重建时逐条校验配对不变量。
    ///
    /// 快照是不可信的外部数据，直接装入 `Session` 会绕过 `commit_round`
    /// 的校验。这里按"构造路径能产生什么"验收：用户消息开头、带调用的
    /// 轮次紧跟等量观察、最终答案轮不带观察、观察不能开头或连续出现。
    /// 校验失败返回 [`SessionError`]，不构造自相矛盾的历史。
    ///
    /// 用户消息允许出现在历史中部：步骤边界就是用户消息，
    /// [`Session::push_user`] 可以产生。但**第一条**必须是用户消息，
    /// 那是任务交代，没有它就无法回答"这个会话在干什么"。
    pub fn from_snapshot(
        id: SessionId,
        snapshot: SessionSnapshot,
    ) -> Result<Self, SessionError> {
        let turns = snapshot.turns;
        if !matches!(turns.first(), Some(Turn::User(_))) {
            return Err(SessionError::Corrupt {
                reason: "会话必须以用户消息开头".to_owned(),
            });
        }
        let mut rounds = 0_u32;
        let mut index = 0;
        while index < turns.len() {
            match &turns[index] {
                // 中部用户消息是步骤边界，构造路径可以产生。过去拒绝
                // 它们的理由（裁剪会静默丢掉）已不成立：见
                // `domain::context::compact`，步骤交代现在完整保留。
                Turn::User(_) | Turn::Input { .. } => index += 1,
                Turn::Agent(agent) => {
                    rounds += 1;
                    let calls = agent.calls.len();
                    match turns.get(index + 1) {
                        // 带调用的轮次：观察必须紧跟且数量一致。
                        Some(Turn::Observation(outcomes)) if calls > 0 => {
                            if outcomes.len() != calls {
                                return Err(SessionError::Unpaired {
                                    turn: rounds,
                                    calls,
                                    observations: outcomes.len(),
                                });
                            }
                            index += 2;
                        }
                        // 最终答案轮不带观察。
                        Some(Turn::Observation(_)) => {
                            return Err(SessionError::Corrupt {
                                reason: format!(
                                    "第 {rounds} 轮没有工具调用，后面却跟着观察"
                                ),
                            });
                        }
                        // 带调用的轮次后面不是观察：历史无法回放。
                        other if calls > 0 => {
                            let follow = if other.is_some() {
                                "但被下一条消息打断"
                            } else {
                                "但没有任何观察（历史在此截断）"
                            };
                            return Err(SessionError::Corrupt {
                                reason: format!("第 {rounds} 轮有 {calls} 个调用，{follow}"),
                            });
                        }
                        // 无调用的轮次后面没有观察：合法（最终答案轮）。
                        _ => index += 1,
                    }
                }
                // 能走到这个分支，说明这条观察前面不是带调用的 Agent
                // 轮次。
                Turn::Observation(_) => {
                    return Err(SessionError::Corrupt {
                        reason: "观察只能紧跟在带工具调用的模型轮次之后".to_owned(),
                    });
                }
            }
        }
        if let Some(summary) = &snapshot.summary {
            // 轮次从 1 开始、范围正向、不超出已有历史、文本非空。四条任一
            // 不满足都说明快照已损坏，与配对错误一样在重建时拦下。空摘要
            // 会让 compact 在占位说明的位置发出一条空用户消息。
            let (from, to) = summary.covers;
            if from == 0 || from > to || to > rounds || summary.text.trim().is_empty() {
                return Err(SessionError::Corrupt {
                    reason: format!(
                        "摘要覆盖范围 {from}-{to} 或文本无效（历史共 {rounds} 轮）"
                    ),
                });
            }
        }
        Ok(Self {
            id,
            turns,
            summary: snapshot.summary,
        })
    }

    /// 按用户消息与轮次分组返回视图。
    pub fn slices(&self) -> Vec<ContextSlice<'_>> {
        let mut slices = Vec::new();
        let mut round = 0_u32;
        for (index, turn) in self.turns.iter().enumerate() {
            match turn {
                Turn::User(text) | Turn::Input { text, .. } => {
                    slices.push(ContextSlice::User(Cow::Borrowed(text)));
                }
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

/// 历史里有多少条步骤交代，交接消息不计。
///
/// 单独成函数：会话与快照都要数这个数，两者历史形状相同
/// （`Vec<Turn>`）。写两处会逐渐分叉。
pub fn count_step_prompts(turns: &[Turn]) -> usize {
    turns
        .iter()
        .filter(|turn| match turn {
            Turn::User(text) => !crate::domain::task::is_handoff_message(text),
            _ => false,
        })
        .count()
}

/// 每一轮属于第几步（1-based）。
///
/// 步骤边界是**结构性**的：每条非交接的用户消息是一步的交代，数一遍
/// 用户消息就知道每轮属于第几步。解析文本里的步号时，格式一改就会
/// 对不上，而且没有报错。
///
/// 与 [`count_step_prompts`] 同源，会话与快照都从这里取。
pub fn round_step_numbers(turns: &[Turn]) -> Vec<u32> {
    let mut steps = Vec::new();
    let mut step = 0u32;
    for turn in turns {
        match turn {
            Turn::User(text) if !crate::domain::task::is_handoff_message(text) => {
                step += 1;
            }
            Turn::Agent(_) => steps.push(step),
            _ => {}
        }
    }
    steps
}

#[cfg(test)]
mod tests {    use super::*;
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

    // ── 步骤边界的用户消息 ───────────────────────────────────────────

    #[test]
    fn push_user_appends_a_step_boundary_message() {
        let mut session = Session::new(SessionId::new("s1"), "任务");
        session
            .commit_round(
                AgentTurn {
                    text: "根因在解析层".to_owned(),
                    calls: Vec::new(),
                },
                Vec::new(),
            )
            .unwrap();

        session.push_user("[第 2/3 步] 按上面的根因动手改");

        assert_eq!(session.rounds(), 1, "步骤交代不算一轮");
        assert_eq!(
            session.turns(),
            &[
                Turn::User("任务".to_owned()),
                Turn::Agent(AgentTurn {
                    text: "根因在解析层".to_owned(),
                    calls: Vec::new(),
                }),
                Turn::User("[第 2/3 步] 按上面的根因动手改".to_owned()),
            ]
        );
        // 视图里它是独立的一条用户消息，紧跟在那轮之后。
        let slices = session.slices();
        assert_eq!(slices.len(), 3);
        assert!(matches!(&slices[2], ContextSlice::User(text) if text.as_ref() == "[第 2/3 步] 按上面的根因动手改"));
    }

    #[test]
    fn from_snapshot_accepts_user_messages_at_step_boundaries() {
        // 单会话编排的历史：任务交代 → 几步工作 → 步骤交代 → 更多工作。
        // 重建必须原样接受，否则一个跨步的快照永远恢复不了。
        let snapshot = SessionSnapshot {
            summary: None,
            turns: vec![
                Turn::User("任务".to_owned()),
                Turn::Agent(turn_with(vec![call("read_file")])),
                Turn::Observation(vec![ToolOutcome::success("内容")]),
                Turn::User("[第 2/3 步] 接着改".to_owned()),
                Turn::Agent(turn_with(vec![call("write_file")])),
                Turn::Observation(vec![ToolOutcome::success("写好了")]),
            ],
        };

        let restored = Session::from_snapshot(SessionId::new("s1"), snapshot).expect("应能重建");

        assert_eq!(restored.rounds(), 2);
        assert_eq!(restored.turns().len(), 6);
    }

    #[test]
    fn from_snapshot_still_requires_a_leading_user_message() {
        // 放宽的只是"中部可以有用户消息"，不是"可以没有任务交代"。
        let snapshot = SessionSnapshot {
            summary: None,
            turns: vec![Turn::User("[第 2/3 步] 接着改".to_owned())],
        };
        // 这一条合法：第一条就是用户消息。非法的情况在下面。
        assert!(Session::from_snapshot(SessionId::new("s1"), snapshot).is_ok());

        let snapshot = SessionSnapshot {
            summary: None,
            turns: vec![Turn::Agent(turn_with(vec![call("read_file")]))],
        };
        let error = Session::from_snapshot(SessionId::new("s1"), snapshot).unwrap_err();
        assert!(error.to_string().contains("用户消息"), "{error}");
    }

    // ── 快照导出与校验重建 ──────────────────────────────────────────

    /// 造一个带 n 轮合法历史的会话。
    fn session_with_rounds(n: u32) -> Session {
        let mut session = Session::new(SessionId::new("s1"), "任务");
        for _ in 0..n {
            session
                .commit_round(
                    turn_with(vec![call("read_file")]),
                    vec![ToolOutcome::success("内容")],
                )
                .unwrap();
        }
        session
    }

    #[test]
    fn snapshot_round_trips_through_from_snapshot() {
        let session = session_with_rounds(2);
        let restored = Session::from_snapshot(SessionId::new("s1"), session.snapshot()).unwrap();

        assert_eq!(restored.id(), session.id());
        assert_eq!(restored.turns(), session.turns());
        assert_eq!(restored.rounds(), 2);
    }

    #[test]
    fn from_snapshot_rejects_history_not_starting_with_user() {
        let snapshot = SessionSnapshot {
            summary: None,
            turns: vec![Turn::Agent(turn_with(vec![call("read_file")]))],
        };
        let error = Session::from_snapshot(SessionId::new("s1"), snapshot).unwrap_err();
        assert!(error.to_string().contains("用户消息"), "{error}");
    }

    #[test]
    fn from_snapshot_rejects_observation_count_mismatch() {
        let snapshot = SessionSnapshot {
            summary: None,
            turns: vec![
                Turn::User("任务".to_owned()),
                Turn::Agent(turn_with(vec![call("a"), call("b")])),
                // 两个调用只跟了一条观察。
                Turn::Observation(vec![ToolOutcome::success("只有一条")]),
            ],
        };
        let error = Session::from_snapshot(SessionId::new("s1"), snapshot).unwrap_err();
        assert!(matches!(error, SessionError::Unpaired { calls: 2, observations: 1, .. }), "{error}");
    }

    #[test]
    fn from_snapshot_rejects_agent_without_following_observation() {
        let snapshot = SessionSnapshot {
            summary: None,
            turns: vec![
                Turn::User("任务".to_owned()),
                Turn::Agent(turn_with(vec![call("read_file")])),
                // 观察缺席，直接是下一轮模型回复。
                Turn::Agent(turn_with(vec![])),
            ],
        };
        let error = Session::from_snapshot(SessionId::new("s1"), snapshot).unwrap_err();
        assert!(error.to_string().contains("第 1 轮"), "{error}");
    }

    #[test]
    fn from_snapshot_rejects_observation_after_final_answer() {
        // 最终答案轮没有调用，观察挂在其后是构造路径产生不出的形态。
        let snapshot = SessionSnapshot {
            summary: None,
            turns: vec![
                Turn::User("任务".to_owned()),
                Turn::Agent(AgentTurn {
                    text: "结论".to_owned(),
                    calls: Vec::new(),
                }),
                Turn::Observation(vec![ToolOutcome::success("多余")]),
            ],
        };
        let error = Session::from_snapshot(SessionId::new("s1"), snapshot).unwrap_err();
        assert!(error.to_string().contains("没有工具调用"), "{error}");
    }

    #[test]
    fn from_snapshot_rejects_double_observation() {
        // 第二条观察前面是观察，不是带调用的 Agent，构造路径产生不出。
        let snapshot = SessionSnapshot {
            summary: None,
            turns: vec![
                Turn::User("任务".to_owned()),
                Turn::Agent(turn_with(vec![call("read_file")])),
                Turn::Observation(vec![ToolOutcome::success("第一条")]),
                Turn::Observation(vec![ToolOutcome::success("孤儿观察")]),
            ],
        };
        let error = Session::from_snapshot(SessionId::new("s1"), snapshot).unwrap_err();
        assert!(
            error.to_string().contains("紧跟在带工具调用"),
            "{error}"
        );
    }

    #[test]
    fn summary_round_trips_through_the_snapshot() {
        let mut session = session_with_rounds(3);
        session.set_summary(Summary {
            covers: (1, 2),
            text: "前两轮读了文件".to_owned(),
            steps: vec![1, 2],
        });

        let restored = Session::from_snapshot(SessionId::new("s1"), session.snapshot()).unwrap();

        let summary = restored.summary().unwrap();
        assert_eq!(summary.covers, (1, 2));
        assert_eq!(summary.text, "前两轮读了文件");
    }

    #[test]
    fn from_snapshot_rejects_an_invalid_summary_range() {
        // 覆盖范围倒着写：能解析但不能是重建出来的状态。
        let snapshot = SessionSnapshot {
            summary: Some(Summary {
                covers: (3, 1),
                text: "坏摘要".to_owned(),
                steps: Vec::new(),
            }),
            turns: vec![Turn::User("任务".to_owned())],
        };
        let error = Session::from_snapshot(SessionId::new("s1"), snapshot).unwrap_err();
        assert!(error.to_string().contains("覆盖范围"), "{error}");

        // 覆盖范围超出历史轮数同理。
        let snapshot = SessionSnapshot {
            summary: Some(Summary {
                covers: (1, 99),
                text: "坏摘要".to_owned(),
                steps: Vec::new(),
            }),
            turns: vec![Turn::User("任务".to_owned())],
        };
        let error = Session::from_snapshot(SessionId::new("s1"), snapshot).unwrap_err();
        assert!(error.to_string().contains("覆盖范围"), "{error}");
    }
}

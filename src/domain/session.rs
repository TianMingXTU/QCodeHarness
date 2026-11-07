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

use serde::{Deserialize, Serialize};

use super::agent::AgentTurn;
use super::context::Summary;
use super::ids::SessionId;
use super::tool::{ToolCall, ToolOutcome};

/// 会话历史中的一个条目。
///
/// `Agent` 与 `Observation` 必须成对出现，且 `Observation` 只能紧跟 `Agent`。
/// 这个约束由 [`Session::commit_round`] 保证——枚举虽是 `pub` 的，但
/// [`Session::turns`] 不提供插入入口，外部无法构造出半轮历史。
///
/// 可序列化：快照要把完整历史存下来，恢复时逐条校验后重建。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
    /// 快照历史的布局非法。
    ///
    /// `commit_round` 在构造路径上排除了这些形态，所以快照里出现它们
    /// 只能说明数据被改坏或来自不兼容的写入方——重建为显式错误，
    /// 而不是一个自相矛盾的历史。
    #[error("快照历史损坏：{reason}")]
    Corrupt {
        /// 哪里坏了、为什么。
        reason: String,
    },
}

/// 会话历史的可序列化快照：`Vec<Turn>` 的裸结构。
///
/// 为什么不让 `Session` 直接 derive `Deserialize`：反序列化得到的是
/// 不可信的外部数据，直接塞进 `Session` 会绕过 `commit_round` 的配对
/// 不变量——一条"有调用没观察"的半轮历史会让下一次模型请求直接 400。
/// 快照先落在这个裸结构上，由 [`Session::from_snapshot`] 逐条校验后重建。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionSnapshot {
    /// 完整历史，按发生顺序。
    pub turns: Vec<Turn>,
    /// 已生成的历史摘要。老快照没有这个字段，`#[serde(default)]` 让它们
    /// 照常反序列化为 `None`——新增可选字段不构成快照格式的不兼容变化。
    #[serde(default)]
    pub summary: Option<Summary>,
}

impl SessionSnapshot {
    /// 已经开始了多少步。
    ///
    /// 与 [`Session::started_steps`] 同一口径：从快照读回的历史还没被
    /// 校验成会话，而恢复前要拿这个数核对"会话停在第几步"。
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
/// 为什么不直接暴露 `&[Turn]`：`Turn` 是消息级的（Agent/Observation 交替），
/// 调用方要自己向下看一格才能凑出"一轮"，很容易写错；而上下文窗口管理
/// 又必须以"轮"为单位整体裁剪或摘要。`slices()` 一次把配对关系算好，
/// 裁剪就变成对切片的纯函数变换（见 `domain::context`），循环本身不用改。
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
    /// 已生成的历史摘要。摘要不是历史的一部分（不进 `turns`），
    /// 而是历史的一种"压缩视图"：挂在会话上只为生成一次、随快照走，
    /// 裁剪时按覆盖范围取用（见 `domain::context`）。
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

    /// 记录一段摘要，覆盖旧的。
    ///
    /// 调用方（运行循环）负责保证覆盖范围比现有摘要更广—— narrower 的
    /// 摘要会让裁剪退回占位说明，不会错，只是白花了一次调用。
    pub fn set_summary(&mut self, summary: Summary) {
        self.summary = Some(summary);
    }

    /// 追加一条用户消息。
    ///
    /// 单会话编排用：步骤边界就是一条用户消息（"现在进行第 N 步"），
    /// 它必须和任务交代一样进历史、进快照——下一步的模型要靠它知道
    /// 自己进行到哪了，裁剪时也要能把它认出来。
    ///
    /// 独立于 [`Self::commit_round`]：步骤交代不是一轮模型回复，
    /// 没有观察可配，硬塞进 commit_round 只会造出一个空壳轮次。
    pub fn push_user(&mut self, message: impl Into<String>) {
        self.turns.push(Turn::User(message.into()));
    }

    /// 在第 `index` 条历史之前插入一条用户消息。
    ///
    /// 任务级恢复用：把已完成步骤的结论交回给模型时，结论要落在
    /// "当前这一步的交代"**之前**——它是对前几步的总结，出现在这一步
    /// 的交代之后就成了倒着说。
    ///
    /// `index` 越界时按"追加到末尾"处理：插入位置算错了也不该让
    /// 恢复路径崩掉，消息本身追加着照样能被模型看见。
    pub fn insert_user_at(&mut self, index: usize, message: impl Into<String>) {
        let index = index.min(self.turns.len());
        self.turns.insert(index, Turn::User(message.into()));
    }

    /// 当前步骤交代在历史里的位置。
    ///
    /// "当前步骤交代"= 最后一条**非交接**的用户消息。交接消息（结论
    /// 交接）是总结，不是交代，不参与定位。
    ///
    /// 找不到时返回历史长度——即追加到末尾。
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
    /// 每一步的交代是一条用户消息，任务交代就是第 1 步的交代——
    /// 所以"非交接用户消息的条数"就是"已经开跑的步数"。
    ///
    /// 这个数与任务进度的关系是恢复前核对的关键：会话开了几步，
    /// 任务就该走到第几步附近。对不上说明手里的两半不是同一个任务。
    pub fn started_steps(&self) -> usize {
        count_step_prompts(&self.turns)
    }

    /// 每一轮属于第几步（1-based）。
    ///
    /// 摘要按步骤分段时要用：模型要知道"这段历史是第几步的事"。
    /// 步号由结构数出来（每条非交接的用户消息就是一步的交代），
    /// 不去解析交代文本里的编号。
    pub fn round_steps(&self) -> Vec<u32> {
        round_step_numbers(&self.turns)
    }

    /// 历史里的全部步骤交代，按发生顺序。
    ///
    /// 排除交接消息：那是步骤的**结论**，不是步骤的**交代**。两者都是
    /// 用户消息，长得却不一样——混在一起会让"会话开了几步"算错。
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

    /// 历史里的最后一条消息是不是给定的用户消息。
    ///
    /// 恢复路径用：中断的那一步，它的交代已经进了历史；恢复时再推
    /// 一遍就成了同一句交代出现两次——模型看到两条一模一样的指令，
    /// 而"会话开了几步"也会因此算错。
    pub fn ends_with_user_message(&self, text: &str) -> bool {
        matches!(self.turns.last(), Some(Turn::User(last)) if last == text)
    }

    /// 历史里是否已经有这条一模一样的用户消息。
    ///
    /// 交接用：同一份快照恢复两次，第二次不该把同一句结论再推一遍。
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

    /// 导出可序列化的历史快照。
    pub fn snapshot(&self) -> SessionSnapshot {
        SessionSnapshot {
            turns: self.turns.clone(),
            summary: self.summary.clone(),
        }
    }

    /// 从快照重建会话，重建时逐条校验配对不变量。
    ///
    /// 快照是不可信的外部数据：直接塞进 `Session` 会绕过 `commit_round`
    /// 的不变量校验。这里按"构造路径能产生什么"来验收：
    /// 用户消息开头、带调用的轮次必须紧跟等量的观察、最终答案轮不带观察、
    /// 观察不能开头或连续出现。校验不过返回 [`SessionError`]，
    /// 绝不构造出一个自相矛盾的历史。
    ///
    /// 用户消息允许出现在历史中部：步骤边界就是一条用户消息，
    /// [`Session::push_user`] 产生得出来。但**第一条**仍必须是用户消息——
    /// 那是任务交代，没有它的历史无法回答"这个会话在干什么"。
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
                // 中部用户消息是步骤边界，构造路径产生得出来。
                // 拒绝它们的理由（裁剪会静默丢掉）已不成立——
                // 见 `domain::context::compact`，步骤交代现在被完整保留。
                Turn::User(_) => index += 1,
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
                // 能走到这个分支，说明观察前面不是带调用的 Agent——
                // 即开头或紧跟另一个观察。
                Turn::Observation(_) => {
                    return Err(SessionError::Corrupt {
                        reason: "观察只能紧跟在带工具调用的模型轮次之后".to_owned(),
                    });
                }
            }
        }
        if let Some(summary) = &snapshot.summary {
            // 轮次从 1 开始、范围必须正着写、文本不能为空：这三条破格说明
            // 快照本身已损坏，与历史配对错误一样在重建时拦下——空摘要
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

/// 历史里的步骤交代有多少条（交接消息不算）。
///
/// 单独抽成函数：会话与它的快照都要数这个数，而两者的历史是同一份
/// 数据形状（`Vec<Turn>`）——写成两处，两处就会慢慢分叉。
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
/// 步骤边界是**结构性的**：历史里每条非交接的用户消息就是一步的交代，
/// 所以"这一轮属于第几步"数一遍用户消息就知道，不必去解析交代文本里
/// 的步号——解析文本的话，格式一改就会悄悄对不上，而且对不上时没有
/// 任何报错。
///
/// 与 [`count_step_prompts`] 同源：会话与它的快照都从这里取。
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
        // 这一条其实是合法的（第一条就是用户消息）——真正非法的是下面这种。
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
        // 第二条观察前面是观察而不是带调用的 Agent——构造路径产生不出。
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

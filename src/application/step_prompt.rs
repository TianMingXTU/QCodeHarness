//! 步骤交代的文本构造与识别。
//!
//! 一步的交代是发给模型的一句指令："现在是第几步、做什么、做到什么
//! 程度算完"。它同时是两样东西：
//!
//! - 模型看到的东西（`AgentRunner` 把它当用户消息推进历史）；
//! - 任务与会话之间的**对应关系**（恢复前靠它核对）。
//!
//! 第二条用途要求构造与识别共用一份文本格式，所以两者放在同一个模块：
//! 格式变了，两边一起变，不会出现"写的和认的不是一回事"。

use crate::domain::task::TaskStep;

/// 构造一步的交代文本。
///
/// `index` 是 0-based 的步号，`total` 是总步数。
///
/// 文本里必须带"第 N/M 步"：它是模型判断"我进行到哪了"的唯一依据，
/// 而恢复前的核对也靠它把会话里的交代与任务的步骤对上。
pub fn step_prompt(goal: &str, step: &TaskStep, index: usize, total: usize) -> String {
    format!(
        "任务：{goal}\n当前步骤（第 {now}/{total} 步）：{description}\n\
         完成判据：{done_when}\n达成判据后停止调用工具，直接输出最终答案。",
        now = index + 1,
        description = step.description,
        done_when = step.done_when,
    )
}

//! 步骤交代的文本构造与识别。
//!
//! 一步的交代是发给模型的一句指令：现在是第几步、做什么、做到什么程度
//! 算完。它有两项用途：
//!
//! - 模型可见的输入（`AgentRunner` 把它当用户消息推进历史）；
//! - 任务与会话的**对应关系**（恢复前靠它核对）。
//!
//! 第二项要求构造与识别共用一份文本格式，因此两者放在同一模块。格式
//! 变化时两边同步，不会出现写入与识别不一致。

use crate::domain::task::TaskStep;

/// 构造一步的交代文本。
///
/// `index` 是 0-based 步号，`total` 是总步数。
///
/// 文本必须带"第 N/M 步"：模型据此判断进行到哪了，恢复前的核对也靠它
/// 把会话里的交代与任务步骤对上。
pub fn step_prompt(goal: &str, step: &TaskStep, index: usize, total: usize) -> String {
    format!(
        "任务：{goal}\n当前步骤（第 {now}/{total} 步）：{description}\n\
         完成判据：{done_when}\n达成判据后停止调用工具，直接输出最终答案。",
        now = index + 1,
        description = step.description,
        done_when = step.done_when,
    )
}

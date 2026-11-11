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

use std::borrow::Cow;

use crate::domain::session::{ContextSlice, Session, Turn};
use crate::domain::task::{TaskKind, TaskStep, is_handoff_message};
use crate::domain::workflow::WorkflowCatalog;

const STOP_INSTRUCTION: &str = "\n达成判据后停止调用工具，直接输出最终答案。";

/// 构造一步的交代文本。
///
/// `index` 是 0-based 步号，`total` 是总步数。
///
/// 文本必须带"第 N/M 步"：模型据此判断进行到哪了，恢复前的核对也靠它
/// 把会话里的交代与任务步骤对上。
pub fn step_prompt(goal: &str, step: &TaskStep, index: usize, total: usize) -> String {
    format!(
        "任务：{goal}\n当前步骤（第 {now}/{total} 步）：{description}\n\
         完成判据：{done_when}{STOP_INSTRUCTION}",
        now = index + 1,
        description = step.description,
        done_when = step.done_when,
    )
}

/// 缩短模型视图中的重复目标，原始交代仍用于审计和恢复核对。
///
/// 只处理格式和步号都匹配的框架交代；补充输入、交接和无法识别的旧文本
/// 原样保留。目标包含格式分隔符时也保留原文，避免猜测用户文本的边界。
pub(crate) fn model_view(session: &Session) -> Vec<ContextSlice<'_>> {
    let mut view = session.slices();
    let Some(Turn::User(first)) = session.turns().first() else {
        return view;
    };
    let marker = "\n当前步骤（第 1/";
    if first.matches(marker).count() != 1 {
        return view;
    }
    let Some((goal, rest)) = first
        .strip_prefix("任务：")
        .and_then(|text| text.split_once(marker))
    else {
        return view;
    };
    let Some(total) = rest
        .split_once(" 步）：")
        .and_then(|(total, _)| total.parse::<usize>().ok())
    else {
        return view;
    };
    if goal.is_empty() || total == 0 || step_body(first, goal, 0, total).is_none() {
        return view;
    }

    let mut index = 0;
    let turns = session
        .turns()
        .iter()
        .filter(|turn| !matches!(turn, Turn::Observation(_)));
    for (slice, turn) in view.iter_mut().zip(turns) {
        let Turn::User(text) = turn else {
            continue;
        };
        if is_handoff_message(text) {
            continue;
        }
        if let Some(body) = step_body(text, goal, index, total) {
            let body =
                builtin_step_body(text, goal, index, total).unwrap_or_else(|| body.to_owned());
            let projected = if index == 0 {
                format!("任务：{goal}\n{body}{STOP_INSTRUCTION}")
            } else {
                format!("{body}{STOP_INSTRUCTION}")
            };
            *slice = ContextSlice::User(Cow::Owned(projected));
        }
        index += 1;
    }
    view
}

// 只重展开已匹配的模板占位符。直接替换目标原文会误改短目标命中的
// 标识符、固定说明和完成判据；自定义步骤无法确认来源时保留正文。
fn builtin_step_body(text: &str, goal: &str, index: usize, total: usize) -> Option<String> {
    let catalog = WorkflowCatalog::builtin();
    for kind in [
        TaskKind::BugFix,
        TaskKind::Feature,
        TaskKind::Review,
        TaskKind::Refactor,
    ] {
        let template = catalog.find(kind)?;
        if template.steps.len() != total {
            continue;
        }
        let expanded = template.expand(goal);
        if step_prompt(goal, expanded.get(index)?, index, total) != text {
            continue;
        }
        let shortened = template.expand("本任务目标");
        let prompt = step_prompt(goal, shortened.get(index)?, index, total);
        return step_body(&prompt, goal, index, total).map(str::to_owned);
    }
    None
}

fn step_body<'a>(text: &'a str, goal: &str, index: usize, total: usize) -> Option<&'a str> {
    let body = text
        .strip_prefix(&format!("任务：{goal}\n"))?
        .strip_suffix(STOP_INSTRUCTION)?;
    let header = format!("当前步骤（第 {}/{total} 步）：", index + 1);
    (index < total && body.starts_with(&header) && body.contains("\n完成判据：")).then_some(body)
}

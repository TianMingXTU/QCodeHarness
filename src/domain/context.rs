//! 上下文预算与历史裁剪。
//!
//! 长任务的历史会持续增长：每轮都发送完整历史，token 成本线性上升，
//! 最终超出模型输入上限。裁剪是发送前对历史做的一次纯函数变换，
//! [`Session`] 本身不被修改；证据、审计与恢复都依赖完整历史。
//!
//! 被裁掉的历史有两种呈现：占位说明（确定、零成本）或已生成的摘要
//! （信息量大，但摘要本身是一次模型调用；覆盖范围不够时还会再生成）。
//! `compact` 不调模型，摘要文本由调用方传入，这里只负责有则用之。
//!
//! **上下文与证据分开。** 历史被裁掉后，模型仍要知道这个任务动过什么：
//! 读过哪些文件、写过哪些文件、跑过哪些命令、哪些调用没成功。这些事实
//! 由 [`EvidenceIndex`] 从会话历史**算**出来，另记一份会与历史分叉。
//! 模型接收预算内的台账摘录；用户通过 `--report` 仍能取到完整台账。

use std::borrow::Cow;
use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use super::evidence::AuditEvent;
use super::session::{ContextSlice, Session, Turn};
use super::tool::{OutcomeStatus, ToolCall, ToolOutcome};

/// 一段由模型生成的历史摘要。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Summary {
    /// 摘要覆盖的轮次范围（含）。
    pub covers: (u32, u32),
    /// 摘要文本，由模型生成。
    pub text: String,
    /// 摘要覆盖的步骤号（1-based），由代码从会话算出。
    ///
    /// 步骤边界由**代码**确定：历史里每条非交接的用户消息是一步的交代，
    /// 数一遍就知道每轮属于第几步。模型可能漏写或写错编号，而摘要覆盖
    /// 哪几步必须准确，下一步的模型要靠它核对进度。
    ///
    /// `#[serde(default)]` 保持旧版快照可反序列化。
    #[serde(default)]
    pub steps: Vec<u32>,
}

impl Summary {
    /// 是否覆盖（含端点）给定的全部轮次。
    pub fn covers_all(&self, rounds: &[u32]) -> bool {
        rounds
            .iter()
            .all(|round| *round >= self.covers.0 && *round <= self.covers.1)
    }

    /// 覆盖的步骤范围（1-based，含端点）。未覆盖任何步骤时为 `None`。
    pub fn step_range(&self) -> Option<(u32, u32)> {
        let first = self.steps.first().copied()?;
        let last = self.steps.last().copied()?;
        Some((first, last))
    }
}

/// 摘要策略：控制被裁掉多少轮才值得再花一次模型调用。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SummarizePolicy {
    /// 被裁轮次达到这个数才生成或更新摘要。太高会一直用占位说明，
    /// 太低则摘要调用过频。1-2 适合短上下文预算。
    pub min_dropped_to_summarize: usize,
}

/// 发给模型前，历史最多占多少 token。
///
/// **token 优先，轮数只作兜底。** 轮数不是好的上限：同样一轮，读一个
/// 10 行文件与跑一条输出 5 万字的命令，token 量差三个数量级。轮数相同
/// 的两个会话，一个早该裁，一个远未到上限，按轮数裁要么多裁，要么
/// 仍然爆窗。
///
/// 代价是 token 数需要**估算**（见 [`estimate_tokens`]）。精确数需要
/// 分词器，这是新依赖，而且各家模型的分词器不同。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextBudget {
    /// 历史最多占多少 token。`None` 表示不按 token 裁剪。
    pub max_tokens: Option<usize>,
    /// 至少保留最近多少轮。
    ///
    /// 这是**下限**：token 预算再紧，最近几轮也保留。模型连续几轮看不到
    /// 自己刚做过什么，会重新问、重新读，它需要的是上一次调用的结果。
    pub min_recent_rounds: usize,
    /// 是否始终保留最初的用户任务。建议设为 true，模型忘了任务目标后，
    /// 后续每轮都会跑偏。
    pub keep_task: bool,
    /// 被裁历史是否用摘要替代占位说明。`None` 表示只用占位说明。
    pub summarize: Option<SummarizePolicy>,
}

impl ContextBudget {
    /// 不裁剪的预算：不设 token 上限。
    pub fn unlimited() -> Self {
        Self {
            max_tokens: None,
            min_recent_rounds: 0,
            keep_task: true,
            summarize: None,
        }
    }

    /// 开启摘要，被裁轮次达到 `min_dropped_to_summarize` 才生成。
    pub fn with_summarize(mut self, min_dropped_to_summarize: usize) -> Self {
        self.summarize = Some(SummarizePolicy {
            min_dropped_to_summarize,
        });
        self
    }

    /// 设一个 token 上限。
    pub fn with_max_tokens(mut self, max_tokens: usize) -> Self {
        self.max_tokens = Some(max_tokens);
        self
    }
}

impl Default for ContextBudget {
    fn default() -> Self {
        Self {
            max_tokens: Some(DEFAULT_MAX_TOKENS),
            min_recent_rounds: DEFAULT_MIN_RECENT_ROUNDS,
            keep_task: true,
            // 默认不开摘要：多一次模型调用是额外成本，需要显式开启。
            summarize: None,
        }
    }
}

/// 默认的历史 token 预算。
///
/// 取 16k：常见模型窗口至少 32k，扣掉系统提示词、工具定义与当前轮，
/// 留给历史的空间是这个量级。它比"16 轮"更贴近真实成本：一轮读文件的
/// 观察可能几百 token，一轮跑测试的观察可能上万。
pub const DEFAULT_MAX_TOKENS: usize = 16_000;

/// 默认至少保留的近期轮数。
///
/// 两轮是下限：一轮是刚做了什么，一轮是结果是什么。
pub const DEFAULT_MIN_RECENT_ROUNDS: usize = 2;

/// 估算一段文本占多少 token。
///
/// **这是预算用的估算，不是计费口径。** 真实用量以 provider 报的
/// `usage` 为准。两者在中文上能差三四倍，这里的数字只用于决定是否
/// 裁剪，不用于算钱。
///
/// 口径：CJK 字符按 1 个 token 记，其余字符按 4 个字符 1 个 token 记。
/// 纯"字符数 ÷ 4"是英文的通用近似，会把中文低估三到四倍。本项目的
/// 提示词、历史与工具输出以中文为主，用那个口径预算会形同虚设。
pub fn estimate_tokens(text: &str) -> usize {
    let mut cjk = 0usize;
    let mut other = 0usize;
    for ch in text.chars() {
        if is_cjk(ch) {
            cjk += 1;
        } else {
            other += 1;
        }
    }
    cjk + other.div_ceil(4)
}

/// 这个字符是否属于 CJK（含中日韩标点与全角形式）。
///
/// 全角与 CJK 标点一并计入：它们在中文文本里密度很高，漏掉会让估算
/// 再偏低。
fn is_cjk(ch: char) -> bool {
    matches!(
        ch as u32,
        0x3000..=0x303F // 中日韩符号与标点
        | 0x3400..=0x4DBF // 扩展 A
        | 0x4E00..=0x9FFF // 基本区
        | 0xF900..=0xFAFF // 兼容表意文字
        | 0xFF00..=0xFF60 // 全角 ASCII
        | 0xFFE0..=0xFFE6 // 全角货币符号
    )
}

/// 一个片段的 token 估算。
fn slice_tokens(slice: &ContextSlice<'_>) -> usize {
    match slice {
        ContextSlice::User(text) => estimate_tokens(text.as_ref()),
        ContextSlice::Round {
            thought,
            calls,
            observations,
            ..
        } => {
            let mut total = estimate_tokens(thought);
            for call in *calls {
                total += estimate_tokens(&call.name);
                total += estimate_tokens(
                    &serde_json::to_string(&call.arguments).unwrap_or_else(|_| "{}".to_owned()),
                );
            }
            for outcome in *observations {
                total += estimate_tokens(&outcome.content);
            }
            total
        }
    }
}

/// 会话的证据台账：这个任务动过什么。
///
/// **从会话历史算出，不另记一份账。** 另记一份会与历史分叉：模型改了
/// 主意、工具被拒绝、恢复时重跑了调用，账本不会跟着变，而模型看到的
/// "我做过什么"必须与历史一致。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceIndex {
    /// 读过的文件（去重，按首次出现排序）。
    pub files_read: Vec<String>,
    /// 写过的文件（去重，按首次出现排序）。
    pub files_written: Vec<String>,
    /// 跑过的命令（去重，按首次出现排序）。
    pub commands_run: Vec<String>,
    /// 没有成功的调用：`工具(参数) → 原因`。
    ///
    /// 这是"未完成事项"的**唯一**来源。哪些调用没成功，代码比模型清楚；
    /// 让模型自己列会漏，漏掉的往往是"上一步那条命令到底跑没跑成"。
    /// 这一份由代码从调用记录拼出。
    pub unfinished_calls: Vec<String>,
}

impl EvidenceIndex {
    /// 从会话历史算出台账。
    pub fn from_session(session: &Session) -> Self {
        let mut index = Self::default();
        let mut turns = session.turns().iter();
        while let Some(turn) = turns.next() {
            let Turn::Agent(agent) = turn else {
                continue;
            };
            // 观察紧跟在带调用的轮次之后，`commit_round` 保证配对。
            let Some(Turn::Observation(outcomes)) = turns.next() else {
                continue;
            };
            for (call, outcome) in agent.calls.iter().zip(outcomes) {
                index.add_call(call, Some(outcome));
            }
        }
        index
    }

    /// 从证据事件算出台账。
    ///
    /// 事件来源与会话不同，但**判定规则只有一份**（[`Self::add_call`]）。
    /// 两处各写一套，同一个调用在会话视图与报表视图里会归入不同类别，
    /// 用户看到的报表与模型看到的台账就对不上。
    pub fn from_events<'a>(events: impl IntoIterator<Item = &'a AuditEvent>) -> Self {
        let mut index = Self::default();
        for event in events {
            if let AuditEvent::ToolObserved { call, outcome, .. } = event {
                index.add_call(call, Some(outcome));
            }
        }
        index
    }

    /// 记一次调用。
    ///
    /// `outcome` 为 `None` 表示结果未知（调用已发起）。这种调用算未完成，
    /// 它可能已经产生副作用。
    pub fn add_call(&mut self, call: &ToolCall, outcome: Option<&ToolOutcome>) {
        let succeeded = outcome.is_some_and(|outcome| outcome.status == OutcomeStatus::Success);
        if succeeded {
            match classify(&call.name) {
                Some(ToolKind::Read) => push_all_unique(&mut self.files_read, subject(call)),
                Some(ToolKind::Write) => push_all_unique(&mut self.files_written, subject(call)),
                Some(ToolKind::Command) => push_all_unique(&mut self.commands_run, subject(call)),
                None => {}
            }
        }
        if !succeeded {
            let reason = outcome.map_or_else(
                || "结果未知（调用已发起，结果没记下来）".to_owned(),
                |outcome| outcome.content.clone(),
            );
            self.unfinished_calls.push(format!(
                "{}({}) → {}",
                call.name,
                brief_arguments(call),
                reason
            ));
        }
    }

    /// 台账里一条内容都没有。
    pub fn is_empty(&self) -> bool {
        self.files_read.is_empty()
            && self.files_written.is_empty()
            && self.commands_run.is_empty()
            && self.unfinished_calls.is_empty()
    }

    /// 渲染成台账正文，不含抬头。
    ///
    /// 抬头由调用方提供：给模型的抬头要说明"这是裁剪之后的账"，给用户的
    /// 抬头要说明"这个任务动过什么"，两者措辞不同。
    ///
    /// 空账本渲染成空串：没有内容就不占模型注意力，也不占 token。
    pub fn render(&self) -> String {
        if self.is_empty() {
            return String::new();
        }
        let mut lines = Vec::new();
        if !self.files_read.is_empty() {
            lines.push(format!("- 读过的文件：{}", self.files_read.join("、")));
        }
        if !self.files_written.is_empty() {
            lines.push(format!("- 写过的文件：{}", self.files_written.join("、")));
        }
        if !self.commands_run.is_empty() {
            lines.push(format!("- 跑过的命令：{}", self.commands_run.join("、")));
        }
        if !self.unfinished_calls.is_empty() {
            lines.push(format!(
                "- 没成功的调用：{}",
                self.unfinished_calls.join("；")
            ));
        }
        lines.join("\n")
    }
}

/// 模型台账最多占剩余预算的四分之一，且不超过这个上限。近期轮次和
/// 用户交代先占预算；完整台账仍由 `EvidenceIndex::render` 提供给报告。
const MODEL_LEDGER_MAX_TOKENS: usize = 1024;

fn model_ledger(session: &Session, max_tokens: usize) -> String {
    if max_tokens == 0 {
        return String::new();
    }
    let mut successful = EvidenceIndex::default();
    let mut failures = Vec::new();
    for slice in session.slices() {
        let ContextSlice::Round {
            number,
            calls,
            observations,
            ..
        } = slice
        else {
            continue;
        };
        for (call, outcome) in calls.iter().zip(observations) {
            if outcome.status == OutcomeStatus::Success {
                successful.add_call(call, Some(outcome));
            } else {
                failures.push(format!(
                    "- 第 {number} 轮 {} {}({}) → {:?}：{}",
                    call.id.as_str(),
                    call.name,
                    abbreviated(&brief_arguments(call)),
                    outcome.status,
                    abbreviated(&outcome.content),
                ));
            }
        }
    }
    if successful.is_empty() && failures.is_empty() {
        return String::new();
    }
    let mut text = format!(
        "【本会话的证据台账】限长视图；未成功调用 {} 条。完整参数与结果见原始会话/审计，按轮次与调用 ID 定位。",
        failures.len(),
    );
    if estimate_tokens(&text) > max_tokens {
        return String::new();
    }
    let facts = successful
        .files_read
        .iter()
        .map(|path| format!("- 读过的文件：{}", abbreviated(path)))
        .chain(
            successful
                .files_written
                .iter()
                .map(|path| format!("- 写过的文件：{}", abbreviated(path))),
        )
        .chain(
            successful
                .commands_run
                .iter()
                .map(|command| format!("- 跑过的命令：{}", abbreviated(command))),
        );
    // 失败优先，且从最近一次开始列，避免大量成功读操作挤掉当前阻塞。
    for line in failures.into_iter().rev().chain(facts) {
        let candidate = format!("{text}\n{line}");
        if estimate_tokens(&candidate) <= max_tokens {
            text = candidate;
        }
    }
    text
}

fn abbreviated(text: &str) -> String {
    let mut chars = text.chars();
    let mut brief: String = chars.by_ref().take(80).collect();
    if chars.next().is_some() {
        brief.push_str("…（已省略）");
    }
    brief
}

/// 内置工具的分类。
///
/// 分类依据工具名：领域层拿不到工具注解（注解在注册表里，注册表在应用
/// 层），而"读过的文件"与"写过的文件"必须分开，台账的价值在于模型能
/// 看出哪些读过、哪些改过。
///
/// 代价是内置工具名在这里出现两次：[`known_tool_names`] 供应用层与真实
/// 注册表对照，[`classify`] 的 match 做实际分类。两者独立维护，改名时
/// 必须同步（这是领域层拿不到工具注解的代价，见[`classify`]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolKind {
    /// 读文件。
    Read,
    /// 写文件。
    Write,
    /// 跑命令。
    Command,
}

/// 台账认识的工具名。
///
/// 唯一的用途是让应用层与真实注册表对照（见
/// `application::tool_registry` 的测试）。
pub fn known_tool_names() -> &'static [&'static str] {
    &[
        "read_file",
        "read_files",
        "write_file",
        "edit_file",
        "run_command",
    ]
}

/// 工具名属于哪一类。
fn classify(tool: &str) -> Option<ToolKind> {
    match tool {
        "read_file" | "read_files" => Some(ToolKind::Read),
        "write_file" | "edit_file" => Some(ToolKind::Write),
        "run_command" => Some(ToolKind::Command),
        _ => None,
    }
}

/// 这次调用的对象：文件路径或命令文本。
///
/// 批量读给出路径数组，这里逐个返回。台账按文件一项才认得出重复：
/// 若记成"a.rs、b.rs"一项，之后再单独读 a.rs 会变成两项。
fn subject(call: &ToolCall) -> Vec<String> {
    if let Some(path) = call.arguments.get("path").and_then(|value| value.as_str()) {
        return vec![path.to_owned()];
    }
    if let Some(paths) = call
        .arguments
        .get("paths")
        .and_then(|value| value.as_array())
    {
        let listed: Vec<String> = paths
            .iter()
            .filter_map(|path| path.as_str().map(str::to_owned))
            .collect();
        if !listed.is_empty() {
            return listed;
        }
    }
    if let Some(command) = call
        .arguments
        .get("command")
        .and_then(|value| value.as_str())
    {
        return vec![command.to_owned()];
    }
    vec![call.name.clone()]
}

/// 调用参数的逐项展开：`key=value` 逗号连接。台账只用于诊断展示，不做
/// 筛选或截断；`content` 这类长参数会整段进入台账。
fn brief_arguments(call: &ToolCall) -> String {
    let object = call.arguments.as_object().map(|map| {
        map.iter()
            .map(|(key, value)| {
                let text = match value {
                    serde_json::Value::String(text) => text.clone(),
                    other => other.to_string(),
                };
                format!("{key}={text}")
            })
            .collect::<Vec<_>>()
            .join(", ")
    });
    object.unwrap_or_default()
}

/// 往列表里追加一项，重复的不再追加。
fn push_unique(list: &mut Vec<String>, item: String) {
    if item.is_empty() || list.contains(&item) {
        return;
    }
    list.push(item);
}

/// 往列表里追加若干项，逐项去重。
fn push_all_unique(list: &mut Vec<String>, items: Vec<String>) {
    for item in items {
        push_unique(list, item);
    }
}

/// 一次裁剪的结果，记录丢弃内容，供证据与调试使用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactionReport {
    /// 裁剪前的完整轮数。
    pub rounds_before: usize,
    /// 裁剪后保留的轮数。
    pub rounds_after: usize,
    /// 被丢弃的轮次号。
    pub dropped: Vec<u32>,
    /// 裁剪前的历史 token 估算。
    pub tokens_before: usize,
    /// 裁剪后**实际发出**的 token 估算。
    ///
    /// 包含说明、摘要与模型台账的开销；必保内容本身过长时仍可超过预算。
    pub tokens_after: usize,
    /// 净减少的 token 数。
    ///
    /// 净值为 0 说明这次裁剪没有省下东西（台账比被裁内容还长），这个数
    /// 值得展示。
    pub dropped_tokens: usize,
}

impl CompactionReport {
    /// 对照完整历史与实际视图，测出这次裁剪丢掉了什么。
    ///
    /// 上下文策略只交视图，不交报告；证据里的"裁了几轮、省了多少 token"
    /// 由这里按视图重新测量。默认策略下结果与 [`compact`] 的报告逐字段
    /// 相同。换用其他视图生成方式时，测到的仍是实际发出内容与完整历史
    /// 之差，证据不依赖具体算法。
    pub fn measure(session: &Session, view: &[ContextSlice<'_>]) -> Self {
        let all = session.slices();
        let tokens_before: usize = all.iter().map(slice_tokens).sum();
        let tokens_after: usize = view.iter().map(slice_tokens).sum();
        let rounds_before = all.iter().filter_map(round_number).count();
        let kept_rounds: HashSet<u32> = view.iter().filter_map(round_number).collect();
        let dropped: Vec<u32> = all
            .iter()
            .filter_map(round_number)
            .filter(|number| !kept_rounds.contains(number))
            .collect();
        Self {
            rounds_before,
            rounds_after: rounds_before - dropped.len(),
            dropped,
            tokens_before,
            tokens_after,
            dropped_tokens: tokens_before.saturating_sub(tokens_after),
        }
    }
}

/// 把会话历史裁剪到预算内，返回保留的片段与裁剪报告。
///
/// 纯函数：不读时钟、不做 IO、不改 `session`。裁剪以"轮"为最小单位，
/// 半轮历史（有调用没观察）会让 OpenAI 兼容端点直接 400，因此预算之外
/// 的轮次连同观察整体丢弃。
///
/// **裁剪顺序**：先按 token 总量判断是否需要裁剪；需要时**从最老的轮次
/// 开始丢**，但最近 [`ContextBudget::min_recent_rounds`] 轮一定保留。
/// 步骤边界与结论交接这类用户消息永不丢弃，它们是"现在在第几步""前面
/// 查出了什么"的唯一来源，丢了模型会跑偏。最初的用户任务由
/// [`ContextBudget::keep_task`] 决定去留。
///
/// 占位说明、既有摘要与限长证据台账都在选择保留轮次前计入预算。
/// 必保内容本身已用尽预算时不再追加合成消息，原始历史仍完整保留。
pub fn compact<'a>(
    session: &'a Session,
    budget: &ContextBudget,
) -> (Vec<ContextSlice<'a>>, CompactionReport) {
    compact_with_summary(session, budget, session.summary())
}

/// 同 [`compact`]，但摘要由调用方显式给出。
///
/// 拆开是为了让上下文策略带着刚生成的摘要来裁剪，不必先写回会话。
/// `None` 表示没有可用摘要，被裁位置退回占位说明。
pub fn compact_with_summary<'a>(
    session: &'a Session,
    budget: &ContextBudget,
    summary: Option<&Summary>,
) -> (Vec<ContextSlice<'a>>, CompactionReport) {
    let view = compact_slices(session, session.slices(), budget, summary);
    let report = CompactionReport::measure(session, &view);
    (view, report)
}

/// 对已投影的模型视图分配预算，原始会话只用于构造证据索引。
pub(crate) fn compact_slices<'a>(
    session: &Session,
    slices: Vec<ContextSlice<'a>>,
    budget: &ContextBudget,
    summary: Option<&Summary>,
) -> Vec<ContextSlice<'a>> {
    let Some(max_tokens) = budget.max_tokens else {
        return slices;
    };
    let costs: Vec<usize> = slices.iter().map(slice_tokens).collect();
    if costs.iter().sum::<usize>() <= max_tokens {
        return slices;
    }
    let rounds: Vec<(u32, usize)> = slices
        .iter()
        .enumerate()
        .filter_map(|(index, slice)| round_number(slice).map(|number| (number, index)))
        .collect();
    let round_numbers: Vec<u32> = rounds.iter().map(|(number, _)| *number).collect();
    let mandatory_tokens: usize = slices
        .iter()
        .zip(&costs)
        .enumerate()
        .filter(|(position, (slice, _))| {
            matches!(slice, ContextSlice::User(_)) && (*position > 0 || budget.keep_task)
        })
        .map(|(_, (_, cost))| cost)
        .sum();
    let unpinned_total =
        mandatory_tokens + rounds.iter().map(|(_, index)| costs[*index]).sum::<usize>();
    if unpinned_total <= max_tokens {
        return if budget.keep_task {
            slices
        } else {
            slices
                .into_iter()
                .enumerate()
                .filter_map(|(position, slice)| (position != 0).then_some(slice))
                .collect()
        };
    }
    let floor_start = rounds.len() - budget.min_recent_rounds.min(rounds.len());
    let floor_tokens: usize = rounds[floor_start..]
        .iter()
        .map(|(_, index)| costs[*index])
        .sum();
    let room = max_tokens.saturating_sub(mandatory_tokens + floor_tokens);
    let ledger = if floor_start > 0 {
        model_ledger(session, (room / 4).min(MODEL_LEDGER_MAX_TOKENS))
    } else {
        String::new()
    };
    let ledger_tokens = estimate_tokens(&ledger);
    let notice_room = room.saturating_sub(ledger_tokens);

    let mut used = 0;
    let mut keep_from = floor_start;
    for index in (0..floor_start).rev() {
        let cost = costs[rounds[index].1];
        let overhead = if index == 0 {
            0
        } else {
            ledger_tokens
                + estimate_tokens(&budgeted_omission(
                    summary,
                    &round_numbers[..index],
                    notice_room,
                ))
        };
        if used + cost + overhead > room {
            break;
        }
        used += cost;
        keep_from = index;
    }

    let mut kept = Vec::with_capacity(slices.len() + 2);
    if budget.keep_task
        && let Some(ContextSlice::User(task)) = slices.first() {
            kept.push(ContextSlice::User(task.clone()));
        }
    if keep_from > 0 {
        let notice = budgeted_omission(summary, &round_numbers[..keep_from], notice_room);
        if !notice.is_empty() {
            kept.push(ContextSlice::User(Cow::Owned(notice)));
        }
        if !ledger.is_empty() {
            kept.push(ContextSlice::User(Cow::Owned(ledger)));
        }
    }
    let dropped: HashSet<u32> = round_numbers[..keep_from].iter().copied().collect();
    for (position, slice) in slices.into_iter().enumerate() {
        match &slice {
            ContextSlice::User(_) if position == 0 => {}
            // 保留用户约束、交接与步骤序列的原始位置。
            ContextSlice::User(_) => kept.push(slice),
            ContextSlice::Round { number, .. } if !dropped.contains(number) => kept.push(slice),
            ContextSlice::Round { .. } => {}
        }
    }
    kept
}

fn budgeted_omission(summary: Option<&Summary>, dropped: &[u32], max_tokens: usize) -> String {
    let text = omitted_text(summary, dropped);
    if estimate_tokens(&text) <= max_tokens {
        return text;
    }
    // 摘要不能挤掉必保轮次。没有余量时不追加合成消息，超预算仅由必保
    // 用户消息和近期轮次产生。
    let notice = omitted_notice(dropped);
    if estimate_tokens(&notice) <= max_tokens {
        notice
    } else {
        String::new()
    }
}

/// 切片的轮次号。用户消息没有轮次号。
fn round_number(slice: &ContextSlice<'_>) -> Option<u32> {
    match slice {
        ContextSlice::Round { number, .. } => Some(*number),
        ContextSlice::User(_) => None,
    }
}

/// 被省略历史处显示的文本：有覆盖全部被裁轮次的摘要就用摘要，否则用
/// 占位说明。摘要未完全覆盖时选择保守做法，把没摘要的轮次说成"已概括"
/// 会诱导模型把没看过的内容当成看过。
fn omitted_text(summary: Option<&Summary>, dropped: &[u32]) -> String {
    match summary {
        Some(summary) if summary.covers_all(dropped) => summary.text.clone(),
        _ => omitted_notice(dropped),
    }
}

/// 被省略历史的占位说明。
///
/// 写明省略了哪些轮、去哪里找回。模型看到历史缺口时容易凭想象补全，
/// 指路可以抑制这个倾向。
fn omitted_notice(dropped: &[u32]) -> String {
    let first = dropped.first().copied().unwrap_or_default();
    let last = dropped.last().copied().unwrap_or_default();
    format!(
        "（为控制上下文长度，已省略第 {first}-{last} 轮的历史，共 {} 轮；\
         如需细节请用 search_text 定位，或用 read_file 重新查看相关文件。）",
        dropped.len()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::agent::AgentTurn;
    use crate::domain::ids::{CallId, SessionId};
    use serde_json::json;

    /// 建一个带 `n` 轮历史的会话：第 `i` 轮读 `r{i}.txt`，观察文本带轮次号，
    /// 用于断言某轮的观察是否出现在裁剪结果里。
    fn session_with_rounds(n: u32) -> Session {
        session_with_output(n, "")
    }

    fn session_with_output(n: u32, padding: &str) -> Session {
        let mut session = Session::new(SessionId::new("s1"), "任务");
        for index in 1..=n {
            session
                .commit_round(
                    AgentTurn {
                        text: format!("思考{index}"),
                        calls: vec![ToolCall {
                            id: CallId::new(format!("call_{index}")),
                            name: "read_file".to_owned(),
                            arguments: json!({"path": format!("r{index}.txt")}),
                        }],
                    },
                    vec![ToolOutcome::success(format!("观察{index}{padding}"))],
                )
                .unwrap();
        }
        session
    }

    /// 只按 token 上限裁剪的预算。
    fn budget(max_tokens: usize) -> ContextBudget {
        ContextBudget {
            max_tokens: Some(max_tokens),
            min_recent_rounds: 0,
            keep_task: true,
            summarize: None,
        }
    }

    fn round_number(slice: &ContextSlice<'_>) -> Option<u32> {
        match slice {
            ContextSlice::Round { number, .. } => Some(*number),
            ContextSlice::User(_) => None,
        }
    }

    fn user_texts(slices: &[ContextSlice<'_>]) -> Vec<String> {
        slices
            .iter()
            .filter_map(|slice| match slice {
                ContextSlice::User(text) => Some(text.as_ref().to_owned()),
                ContextSlice::Round { .. } => None,
            })
            .collect()
    }

    /// 一定会留在视图里的用户消息的 token 代价。
    ///
    /// 计算上限时要把它们算进去，否则会挤掉一轮。`keep_task` 为 false 时
    /// 任务消息会被裁掉，不占预算。
    fn mandatory_tokens(session: &Session, keep_task: bool) -> usize {
        session
            .slices()
            .iter()
            .enumerate()
            .filter(|(position, slice)| {
                matches!(slice, ContextSlice::User(_)) && (*position > 0 || keep_task)
            })
            .map(|(_, slice)| slice_tokens(slice))
            .sum()
    }

    /// 每一轮的 token 代价，按轮次顺序。
    fn round_costs(session: &Session) -> Vec<usize> {
        session
            .slices()
            .iter()
            .filter(|slice| matches!(slice, ContextSlice::Round { .. }))
            .map(slice_tokens)
            .collect()
    }

    #[test]
    fn under_budget_keeps_everything() {
        let session = session_with_rounds(3);
        let (slices, report) = compact(&session, &budget(100_000));

        assert_eq!(slices.len(), 1 + 3, "任务消息 + 三轮");
        assert!(report.dropped.is_empty());
        assert_eq!(report.rounds_before, 3);
        assert_eq!(report.rounds_after, 3);
        assert_eq!(report.dropped_tokens, 0);
    }

    #[test]
    fn over_budget_drops_the_oldest_rounds() {
        let session = session_with_rounds(6);
        // 只够装两轮：把上限压到刚好容纳两轮的量。任务消息也占 token，
        // 上限要把它算进去，否则会挤掉一轮。
        let one_round = slice_tokens(&session.slices()[2]);
        let limit = one_round * 2 + mandatory_tokens(&session, true);
        let (slices, report) = compact(&session, &budget(limit));

        let numbers: Vec<u32> = slices.iter().filter_map(round_number).collect();
        assert_eq!(numbers, vec![5, 6], "只保留最后两轮");
        assert_eq!(report.dropped, vec![1, 2, 3, 4]);
        assert_eq!(report.rounds_before, 6);
        assert_eq!(report.rounds_after, 2);
    }

    /// 按实际视图测出的报告与裁剪自带的报告逐字段一致。
    ///
    /// 上层策略只交视图，证据靠 `measure` 重新测量。这条测试保证两条
    /// 路径结果一致：默认策略下必须完全等价。
    #[test]
    fn measuring_a_view_matches_the_compaction_report() {
        // 超预算：真的裁掉几轮。
        let session = session_with_rounds(6);
        let (view, report) = compact(&session, &budget(1));
        assert!(!report.dropped.is_empty(), "前提：这一档预算要真的裁人");
        assert_eq!(CompactionReport::measure(&session, &view), report);

        // 不超预算：视图就是完整历史，报告应是空裁剪。
        let (view, report) = compact(&session, &budget(100_000));
        assert_eq!(CompactionReport::measure(&session, &view), report);
    }

    #[test]
    fn keep_task_pins_the_task_message_first() {
        let session = session_with_output(4, &"x".repeat(2000));
        let (slices, _) = compact(&session, &budget(200));

        assert!(
            matches!(&slices[0], ContextSlice::User(text) if text.as_ref() == "任务"),
            "任务消息必须排在第一位，实际 {:?}",
            slices[0]
        );
        assert!(
            matches!(&slices[1], ContextSlice::User(_)),
            "第二位是占位说明"
        );

        // 关掉 keep_task：任务也被裁掉，占位说明排到第一位。
        let no_task = ContextBudget {
            max_tokens: Some(200),
            min_recent_rounds: 0,
            keep_task: false,
            summarize: None,
        };
        let (slices, _) = compact(&session, &no_task);
        let Some(ContextSlice::User(first)) = slices.first() else {
            panic!("第一位应当是用户消息");
        };
        assert_ne!(first.as_ref(), "任务", "keep_task=false 时任务消息不应保留");
    }

    /// `keep_task=false` 时任务消息不占预算。
    ///
    /// 任务消息会被裁掉，它的 token 就不该从预算里扣。仍计入时，一个
    /// 上限刚好够装三轮的会话会因为多扣了任务的代价而少留一轮，模型
    /// 看到的历史比它应得的少。
    #[test]
    fn a_dropped_task_message_does_not_consume_the_budget() {
        let session = session_with_rounds(6);
        let costs = round_costs(&session);
        // 上限只够装最后两轮。任务消息的代价不计入，否则装不下。
        let limit = costs[4] + costs[5];
        let no_task = ContextBudget {
            max_tokens: Some(limit),
            min_recent_rounds: 0,
            keep_task: false,
            summarize: None,
        };

        let (slices, report) = compact(&session, &no_task);

        let numbers: Vec<u32> = slices.iter().filter_map(round_number).collect();
        assert_eq!(numbers, vec![5, 6], "任务消息被裁掉时不占预算，应留下两轮");
        assert_eq!(report.dropped, vec![1, 2, 3, 4]);
        assert_eq!(report.rounds_after, 2);
    }

    #[test]
    fn dropping_only_an_unpinned_task_does_not_require_history_compaction() {
        let mut saved = session_with_rounds(6).snapshot();
        saved.turns[0] = Turn::User("可丢弃的任务描述".repeat(1000));
        let session =
            Session::from_snapshot(SessionId::new("unpinned-task"), saved.clone()).unwrap();
        let limit = round_costs(&session).iter().sum();
        let budget = ContextBudget {
            max_tokens: Some(limit),
            min_recent_rounds: 0,
            keep_task: false,
            summarize: None,
        };
        let (view, report) = compact(&session, &budget);

        assert!(
            report.dropped.is_empty(),
            "去掉任务后全部轮次都能装下: {report:?}"
        );
        assert_eq!(report.rounds_after, 6);
        assert!(report.tokens_after <= limit);
        assert!(user_texts(&view).is_empty());
        assert_eq!(session.snapshot(), saved);
    }

    /// 超预算但没有可裁轮次时，不写占位说明，也不写证据台账。
    ///
    /// 触发条件是任务消息本身超过预算，或所有轮次都在
    /// `min_recent_rounds` 的保护范围内。此时历史一个字都没少，
    /// "已省略第 0-0 轮"与"历史被裁剪了"都不成立。
    #[test]
    fn over_budget_without_droppable_rounds_adds_no_omitted_notice() {
        let session = session_with_rounds(0);
        let (slices, report) = compact(&session, &budget(1));

        assert!(report.dropped.is_empty());
        assert_eq!(user_texts(&slices), ["任务"], "只保留任务消息");

        // 轮次全被 min_recent_rounds 保住：同样不该出现占位说明或台账。
        let session = session_with_rounds(2);
        let floor_budget = ContextBudget {
            max_tokens: Some(1),
            min_recent_rounds: 5,
            keep_task: true,
            summarize: None,
        };
        let (slices, report) = compact(&session, &floor_budget);

        assert!(report.dropped.is_empty());
        assert_eq!(report.rounds_after, 2);
        assert_eq!(
            user_texts(&slices),
            ["任务"],
            "保住的两轮之外不追加用户消息"
        );
    }

    /// 没有裁剪时，摘要也不进入视图。
    ///
    /// `Summary::covers_all(&[])` 对空集合恒为真。旧实现据此把摘要当成
    /// "被裁历史的替代品"插入，即使没有任何历史被裁。
    #[test]
    fn a_summary_is_not_injected_when_nothing_was_dropped() {
        let session = session_with_rounds(0);
        let summary = Summary {
            covers: (1, 1),
            text: "摘要正文".to_owned(),
            steps: Vec::new(),
        };
        let (slices, _) = compact_with_summary(&session, &budget(1), Some(&summary));

        assert_eq!(user_texts(&slices), ["任务"]);
    }

    #[test]
    fn placeholder_names_the_omitted_range() {
        let session = session_with_output(6, &"x".repeat(2000));
        let (slices, _) = compact(&session, &budget(200));

        let Some(ContextSlice::User(placeholder)) = slices.get(1) else {
            panic!("第二位应当是占位说明，实际 {:?}", slices.get(1));
        };
        assert!(placeholder.contains("第 1-6 轮"), "{}", placeholder);
        assert!(placeholder.contains("共 6 轮"), "{}", placeholder);
        assert!(
            placeholder.contains("search_text"),
            "应指路 search_text: {placeholder}"
        );
        assert!(
            placeholder.contains("read_file"),
            "应指路 read_file: {placeholder}"
        );
    }

    #[test]
    fn report_reconciles_before_after_and_dropped() {
        let session = session_with_rounds(7);
        let (_, report) = compact(&session, &budget(1));

        assert_eq!(
            report.rounds_before - report.rounds_after,
            report.dropped.len()
        );
        assert_eq!(
            report.tokens_before.saturating_sub(report.tokens_after),
            report.dropped_tokens,
            "丢掉的 token 数要与前后差一致"
        );
    }

    #[test]
    fn zero_token_budget_keeps_the_task_without_synthetic_messages() {
        let session = session_with_rounds(2);
        let (slices, report) = compact(&session, &budget(0));

        assert_eq!(user_texts(&slices), ["任务"]);
        assert_eq!(report.tokens_after, estimate_tokens("任务"));
        assert_eq!(report.dropped, vec![1, 2]);
    }

    // ── 步骤边界的用户消息不占轮次预算 ──────────────────────────────

    /// 造一个跨三步的单会话历史：每步两轮，步骤之间夹一条用户消息。
    fn session_with_steps(rounds_per_step: u32) -> Session {
        let mut session = Session::new(SessionId::new("s1"), "任务");
        for step in 1..=3u32 {
            if step > 1 {
                session.push_user(format!("[第 {step}/3 步] 第 {step} 步的交代"));
            }
            for _ in 0..rounds_per_step {
                session
                    .commit_round(
                        AgentTurn {
                            text: format!("第 {step} 步的思考"),
                            calls: vec![ToolCall {
                                id: CallId::new(format!("call_{step}")),
                                name: "read_file".to_owned(),
                                arguments: json!({"path": "a.txt"}),
                            }],
                        },
                        vec![ToolOutcome::success(format!(
                            "第 {step} 步的观察{}",
                            "x".repeat(2000)
                        ))],
                    )
                    .unwrap();
            }
        }
        session
    }

    #[test]
    fn step_boundaries_survive_compaction_in_their_original_order() {
        let session = session_with_steps(2);
        // 留出说明与台账的空间，剩余预算只够最近两轮。
        let costs = round_costs(&session);
        let limit = costs[4] + costs[5] + mandatory_tokens(&session, true) + 256;
        let (slices, report) = compact(&session, &budget(limit));

        assert_eq!(report.dropped, vec![1, 2, 3, 4], "裁掉前四轮");
        let texts = user_texts(&slices);
        assert_eq!(texts[0], "任务", "任务交代钉在最前");
        assert!(
            texts[1].contains("已省略第 1-4 轮"),
            "第二位是占位说明: {texts:?}"
        );
        // 第 1 步没有独立交代，任务消息就是第 1 步的交代。
        for step in 2..=3 {
            assert!(
                texts.contains(&format!("[第 {step}/3 步] 第 {step} 步的交代")),
                "第 {step} 步的交代必须保留: {texts:?}"
            );
        }
        // 顺序不能乱：第 3 步的交代必须排在第 3 步的观察之前。
        let boundary = slices
            .iter()
            .position(|slice| matches!(slice, ContextSlice::User(text) if text.as_ref() == "[第 3/3 步] 第 3 步的交代"))
            .expect("应有第 3 步的交代");
        let last_round = slices
            .iter()
            .rposition(|slice| matches!(slice, ContextSlice::Round { number: 6, .. }))
            .expect("第 6 轮应保留");
        assert!(boundary < last_round, "步骤交代必须排在本步工作之前");
    }

    #[test]
    fn step_boundaries_do_not_count_against_the_token_budget() {
        let session = session_with_steps(3);
        // 预算足够装下全部轮次：一条都不该裁，步骤交代也原样在位。
        let (slices, report) = compact(&session, &budget(100_000));

        assert!(report.dropped.is_empty(), "全在预算内: {report:?}");
        assert_eq!(slices.len(), 9 + 3, "九轮 + 任务 + 第 2/3 步的两条交代");
    }

    #[test]
    fn session_is_left_untouched_by_compaction() {
        let session = session_with_rounds(5);
        let rounds_before = session.rounds();
        let turns_before = session.turns().len();
        let _ = compact(&session, &budget(1));

        assert_eq!(session.rounds(), rounds_before);
        assert_eq!(session.turns().len(), turns_before);
    }

    // ── 摘要：有则用、不全覆盖则退回占位、纯函数不调模型 ─────────────

    /// 给会话挂一段覆盖 `from..=to` 轮的摘要。
    fn with_summary(session: &mut Session, from: u32, to: u32) {
        session.set_summary(Summary {
            covers: (from, to),
            text: format!("摘要：第 {from}-{to} 轮做了什么事"),
            steps: Vec::new(),
        });
    }

    fn second_slice_text(slices: &[ContextSlice<'_>]) -> String {
        match slices.get(1) {
            Some(ContextSlice::User(text)) => text.to_string(),
            other => panic!("第二位应是用户消息（占位/摘要），实际 {other:?}"),
        }
    }

    #[test]
    fn without_summary_the_placeholder_behaviour_is_unchanged() {
        let session = session_with_output(6, &"x".repeat(2000));
        let (slices, _) = compact(&session, &budget(200));

        let text = second_slice_text(&slices);
        assert!(text.contains("已省略第 1-6 轮"), "{text}");
        assert!(!text.contains("摘要"), "没有摘要时不应出现摘要文本: {text}");
    }

    #[test]
    fn summary_covering_all_dropped_rounds_replaces_the_placeholder() {
        let mut session = session_with_output(6, &"x".repeat(2000));
        with_summary(&mut session, 1, 6);
        let (slices, _) = compact(&session, &budget(200));

        let text = second_slice_text(&slices);
        assert!(text.contains("摘要：第 1-6 轮做了什么事"), "{text}");
        assert!(
            !text.contains("已省略"),
            "摘要覆盖全部被裁轮次时不应再出现占位说明: {text}"
        );
    }

    #[test]
    fn partial_summary_falls_back_to_the_placeholder() {
        let mut session = session_with_output(6, &"x".repeat(2000));
        // 摘要只覆盖第 1-2 轮，被裁的第 3-6 轮在覆盖范围外。
        with_summary(&mut session, 1, 2);
        let (slices, _) = compact(&session, &budget(200));

        let text = second_slice_text(&slices);
        assert!(text.contains("已省略第 1-6 轮"), "{text}");
        assert!(
            !text.contains("摘要"),
            "部分覆盖时不得把没摘要的轮次说成已概括: {text}"
        );
    }

    /// 显式传入的摘要不必先写回会话，也能替代占位说明。
    ///
    /// 上下文策略就是这样用的：刚生成的摘要直接带进裁剪，会话保持只读。
    #[test]
    fn an_explicit_summary_is_used_without_writing_it_into_the_session() {
        let session = session_with_output(6, &"x".repeat(2000));
        let summary = Summary {
            covers: (1, 6),
            text: "外部摘要：全部六轮".to_owned(),
            steps: Vec::new(),
        };
        let (slices, _) = compact_with_summary(&session, &budget(200), Some(&summary));

        let text = second_slice_text(&slices);
        assert!(text.contains("外部摘要"), "{text}");
        assert_eq!(session.summary(), None, "显式摘要不得写回会话");
    }

    #[test]
    fn compact_stays_pure_when_a_summary_is_set() {
        let mut session = session_with_output(5, &"x".repeat(2000));
        // 预算能容纳摘要，但装不下一轮原始观察。
        with_summary(&mut session, 1, 5);
        let summary_before = session.summary().cloned();
        let turns_before = session.turns().len();

        let (first, _) = compact(&session, &budget(200));
        let (second, _) = compact(&session, &budget(200));

        assert_eq!(first, second, "同一输入两次裁剪结果一致（无隐藏状态变更）");
        assert_eq!(
            session.summary(),
            summary_before.as_ref(),
            "摘要不被裁剪改动"
        );
        assert_eq!(session.turns().len(), turns_before);
        assert!(
            first.iter().any(|slice| match slice {
                ContextSlice::User(text) => text.contains("摘要"),
                _ => false,
            }),
            "裁剪结果里应出现摘要文本"
        );
    }

    // ── token 预算 ───────────────────────────────────────────────────

    /// 中文与英文的估算口径不同：中文一个字一个 token。
    #[test]
    fn token_estimate_counts_cjk_characters_heavier_than_ascii() {
        assert_eq!(estimate_tokens("abcd"), 1, "四个 ASCII 字符算一个 token");
        assert_eq!(estimate_tokens("中文"), 2, "两个汉字算两个 token");
        // 纯 ASCII 的除法向上取整，不截断。
        assert_eq!(estimate_tokens("abcde"), 2);
    }

    /// 不设上限时不裁剪。
    #[test]
    fn an_unlimited_budget_never_compacts() {
        let session = session_with_rounds(20);
        let (slices, report) = compact(&session, &ContextBudget::unlimited());

        assert_eq!(slices.len(), 21);
        assert!(report.dropped.is_empty());
        assert_eq!(report.dropped_tokens, 0);
    }

    /// 最近几轮是下限：预算再紧也留着。
    #[test]
    fn the_most_recent_rounds_survive_any_budget() {
        let session = session_with_rounds(6);
        let budget = ContextBudget {
            max_tokens: Some(1),
            min_recent_rounds: 2,
            keep_task: true,
            summarize: None,
        };
        let (slices, report) = compact(&session, &budget);

        let numbers: Vec<u32> = slices.iter().filter_map(round_number).collect();
        assert_eq!(numbers, vec![5, 6], "最近两轮一定留着");
        assert_eq!(report.dropped, vec![1, 2, 3, 4]);
    }

    /// 造一个 4 轮的会话，第 2 轮带一大段观察。
    ///
    /// 用来对比"同样轮数、token 量差很多"的两个会话。
    fn session_with_a_fat_round() -> Session {
        let mut session = Session::new(SessionId::new("s1"), "任务");
        for index in 1..=4u32 {
            let observation = if index == 2 {
                "大".repeat(4000)
            } else {
                format!("观察{index}")
            };
            session
                .commit_round(
                    AgentTurn {
                        text: format!("思考{index}"),
                        calls: vec![ToolCall {
                            id: CallId::new(format!("call_{index}")),
                            name: "read_file".to_owned(),
                            arguments: json!({"path": format!("r{index}.txt")}),
                        }],
                    },
                    vec![ToolOutcome::success(observation)],
                )
                .unwrap();
        }
        session
    }

    /// 裁剪由 token 决定，不由轮数决定：轮数与上限相同，带大观察的会话
    /// 保留的轮次更少。
    ///
    /// 这是"token 优先于轮数"的理由：一轮读小文件与一轮读大文件的 token
    /// 量差三个数量级。按轮数裁时两个会话结果相同，一个被多裁，一个仍然
    /// 爆窗。
    #[test]
    fn the_budget_is_driven_by_tokens_not_by_round_count() {
        let plain = session_with_rounds(4);
        let fat = session_with_a_fat_round();

        // 同一个 token 上限：够装下 plain 的三轮，装不下 fat 的三轮。
        // 若按轮数计算，两个会话都会留下三轮。
        let limit = round_costs(&plain)[0] * 3 + mandatory_tokens(&plain, true);
        let (_, plain_report) = compact(&plain, &budget(limit));
        let (_, fat_report) = compact(&fat, &budget(limit));

        assert_eq!(plain_report.rounds_after, 3, "plain 留得下三轮");
        assert_eq!(fat_report.rounds_after, 2, "fat 只留得下两轮");
        assert_eq!(fat_report.dropped, vec![1, 2], "大观察那轮也被裁掉");
    }

    /// 裁剪报告要能说清丢了多少 token。
    #[test]
    fn the_report_accounts_for_the_dropped_tokens() {
        let session = session_with_rounds(10);
        let (_, report) = compact(&session, &budget(1));

        assert!(report.tokens_before > report.tokens_after);
        assert_eq!(
            report.dropped_tokens,
            report.tokens_before.saturating_sub(report.tokens_after)
        );
    }

    #[test]
    fn a_large_failed_payload_is_locatable_without_reinjection() {
        let mut session = Session::new(SessionId::new("large-failure"), "修复解析器");
        let patch = format!(
            "*** Begin Patch\n*** Update File: src/parser.rs\n{}\n*** End Patch",
            "+replacement\n".repeat(5000)
        );
        let error = format!("补丁格式错误：找不到上下文\n{}", "诊断细节".repeat(10000));
        let calls = vec![
            ToolCall {
                id: CallId::new("failed-patch"),
                name: "apply_patch".to_owned(),
                arguments: json!({"patch": patch}),
            },
            ToolCall {
                id: CallId::new("failed-command"),
                name: "run_command".to_owned(),
                arguments: json!({"command": "cargo test"}),
            },
        ];
        session
            .commit_round(
                AgentTurn {
                    text: "尝试修复".to_owned(),
                    calls: calls.clone(),
                },
                vec![
                    ToolOutcome::refused("补丁格式错误：拒绝执行"),
                    ToolOutcome::failed(error.clone()),
                ],
            )
            .unwrap();
        for _ in 0..6 {
            session
                .commit_round(
                    AgentTurn {
                        text: "近期工作".repeat(150),
                        calls: Vec::new(),
                    },
                    Vec::new(),
                )
                .unwrap();
        }
        let original = session.snapshot();
        let limit = ContextBudget {
            max_tokens: Some(2400),
            min_recent_rounds: 2,
            keep_task: true,
            summarize: None,
        };
        let (view, report) = compact(&session, &limit);
        let users = user_texts(&view).join("\n");

        assert!(report.dropped.contains(&1));
        assert!(
            report.tokens_after <= 2400,
            "失败参数不应重新撑爆预算: {report:?}"
        );
        assert!(
            users.contains("failed-patch")
                && users.contains("apply_patch")
                && users.contains("第 1 轮"),
            "被裁失败应能按轮次和调用 ID 定位"
        );
        assert!(users.contains("failed-command") && users.contains("补丁格式错误"));
        assert!(!users.contains(&patch));
        assert!(!users.contains(&error));
        assert_eq!(session.snapshot(), original);
        let full_report = EvidenceIndex::from_session(&session).render();
        assert!(full_report.contains(&patch), "人用报告仍保留完整失败参数");
        assert!(full_report.contains(&error), "人用报告仍保留完整错误");
        let events = vec![
            AuditEvent::ToolObserved {
                turn: 1,
                call: calls[0].clone(),
                outcome: ToolOutcome::refused("补丁格式错误：拒绝执行"),
            },
            AuditEvent::ToolObserved {
                turn: 1,
                call: calls[1].clone(),
                outcome: ToolOutcome::failed(error),
            },
        ];
        assert_eq!(
            EvidenceIndex::from_events(&events),
            EvidenceIndex::from_session(&session)
        );
    }

    #[test]
    fn many_ledger_entries_share_the_context_budget() {
        let mut session = Session::new(SessionId::new("many-entries"), "核对调用结果");
        for index in 1..=150 {
            session.commit_round(
                AgentTurn { text: "收集证据".to_owned(), calls: vec![
                    ToolCall { id: CallId::new(format!("read-{index}")), name: "read_file".to_owned(), arguments: json!({"path": format!("src/module_{index}/long_file_name.rs")}) },
                    ToolCall { id: CallId::new(format!("failure-{index}")), name: "edit_file".to_owned(), arguments: json!({"path": format!("src/module_{index}/target.rs"), "content": "新内容".repeat(200)}) },
                ] },
                vec![ToolOutcome::success("文件内容".repeat(80)), ToolOutcome::failed("匹配失败：原文不存在".repeat(50))],
            ).unwrap();
        }
        let original = session.snapshot();
        let limit = ContextBudget {
            max_tokens: Some(4000),
            min_recent_rounds: 2,
            keep_task: true,
            summarize: None,
        };
        let (view, report) = compact(&session, &limit);
        let users = user_texts(&view).join("\n");

        assert!(!report.dropped.is_empty());
        assert!(
            report.tokens_after <= 4000,
            "台账条数增长也必须受限: {report:?}"
        );
        assert!(users.contains("failure-150"), "优先保留近期失败的定位信息");
        assert!(users.contains("部分") || users.contains("限长") || users.contains("省略"));
        assert_eq!(session.snapshot(), original);
        assert_eq!(
            EvidenceIndex::from_session(&session).unfinished_calls.len(),
            150
        );
    }

    #[test]
    fn compaction_reserves_space_for_the_notice_and_ledger_before_rounds() {
        let mut session = session_with_rounds(6);
        for _ in 0..4 {
            session
                .commit_round(
                    AgentTurn {
                        text: "工作记录".repeat(250),
                        calls: Vec::new(),
                    },
                    Vec::new(),
                )
                .unwrap();
        }
        let costs = round_costs(&session);
        let limit = mandatory_tokens(&session, true) + costs[8] + costs[9];
        let budget = ContextBudget {
            max_tokens: Some(limit),
            min_recent_rounds: 1,
            keep_task: true,
            summarize: None,
        };
        let (view, report) = compact(&session, &budget);

        assert!(
            report.tokens_after <= limit,
            "新增台账和说明必须预先计入预算: {report:?}"
        );
        assert!(report.rounds_after >= 1);
        assert!(
            user_texts(&view).iter().any(|text| text.contains("r1.txt")),
            "被裁文件仍有证据索引"
        );
        assert_eq!(CompactionReport::measure(&session, &view), report);
    }

    #[test]
    fn compaction_accounts_for_overhead_at_each_budget_boundary() {
        let session = session_with_output(5, &"边界观察".repeat(20));
        let original = session.snapshot();
        let all_tokens = CompactionReport::measure(&session, &session.slices()).tokens_before;
        let costs = round_costs(&session);

        for keep_task in [false, true] {
            for min_recent_rounds in 0..=2 {
                let pinned_tokens = mandatory_tokens(&session, keep_task)
                    + costs[costs.len() - min_recent_rounds..]
                        .iter()
                        .sum::<usize>();
                for max_tokens in pinned_tokens..=all_tokens + 1 {
                    let budget = ContextBudget {
                        max_tokens: Some(max_tokens),
                        min_recent_rounds,
                        keep_task,
                        summarize: None,
                    };
                    let (view, report) = compact(&session, &budget);
                    assert!(
                        report.tokens_after <= max_tokens,
                        "边界预算应包含合成消息开销: {budget:?}, {report:?}"
                    );
                    if report.dropped.is_empty() {
                        assert!(
                            user_texts(&view).iter().all(|text| text == "任务"),
                            "未裁历史时不应追加说明或台账"
                        );
                    }
                }
            }
        }
        assert_eq!(session.snapshot(), original);
    }

    #[test]
    fn pinned_recent_rounds_may_exceed_the_budget_without_ledger_growth() {
        let mut session = session_with_rounds(4);
        for _ in 0..2 {
            session
                .commit_round(
                    AgentTurn {
                        text: "必须保留的近期结果".repeat(300),
                        calls: Vec::new(),
                    },
                    Vec::new(),
                )
                .unwrap();
        }
        let original = session.snapshot();
        let costs = round_costs(&session);
        let pinned_tokens = mandatory_tokens(&session, true) + costs[4] + costs[5];
        let budget = ContextBudget {
            max_tokens: Some(100),
            min_recent_rounds: 2,
            keep_task: true,
            summarize: None,
        };
        let (view, report) = compact(&session, &budget);

        assert_eq!(
            view.iter().filter_map(round_number).collect::<Vec<_>>(),
            vec![5, 6]
        );
        assert!(
            report.tokens_after > 100,
            "近期轮次下限允许不可避免的超预算"
        );
        assert_eq!(
            report.tokens_after, pinned_tokens,
            "没有余量时不得再追加台账或说明"
        );
        assert_eq!(session.snapshot(), original);
    }

    #[test]
    fn a_large_existing_summary_cannot_expand_the_compacted_view() {
        let mut session = session_with_a_fat_round();
        session.set_summary(Summary {
            covers: (1, 4),
            text: "摘要正文".repeat(5000),
            steps: vec![1],
        });
        let original = session.snapshot();
        let (view, report) = compact(&session, &budget(1200));

        assert!(
            report.tokens_after <= 1200,
            "既有摘要也需要预算: {report:?}"
        );
        assert!(!report.dropped.is_empty());
        assert_eq!(session.snapshot(), original);
        assert_eq!(CompactionReport::measure(&session, &view), report);
    }

    // ── 证据台账 ─────────────────────────────────────────────────────

    /// 台账从会话历史算出来：读过什么、写过什么、跑过什么。
    #[test]
    fn the_index_lists_what_the_session_actually_touched() {
        let mut session = Session::new(SessionId::new("s1"), "任务");
        session
            .commit_round(
                AgentTurn {
                    text: "先读".to_owned(),
                    calls: vec![ToolCall {
                        id: CallId::new("c1"),
                        name: "read_file".to_owned(),
                        arguments: json!({"path": "src/lib.rs"}),
                    }],
                },
                vec![ToolOutcome::success("内容".to_owned())],
            )
            .unwrap();
        session
            .commit_round(
                AgentTurn {
                    text: "再改".to_owned(),
                    calls: vec![ToolCall {
                        id: CallId::new("c2"),
                        name: "write_file".to_owned(),
                        arguments: json!({"path": "src/lib.rs", "content": "新内容"}),
                    }],
                },
                vec![ToolOutcome::success("写好了".to_owned())],
            )
            .unwrap();
        session
            .commit_round(
                AgentTurn {
                    text: "跑测试".to_owned(),
                    calls: vec![ToolCall {
                        id: CallId::new("c3"),
                        name: "run_command".to_owned(),
                        arguments: json!({"command": "cargo test"}),
                    }],
                },
                vec![ToolOutcome::success("通过".to_owned())],
            )
            .unwrap();

        let index = EvidenceIndex::from_session(&session);

        assert_eq!(index.files_read, vec!["src/lib.rs"]);
        assert_eq!(index.files_written, vec!["src/lib.rs"]);
        assert_eq!(index.commands_run, vec!["cargo test"]);
        assert!(index.unfinished_calls.is_empty());
    }

    /// 没成功的调用进入"未完成"清单，这一份由代码拼出，不由模型编写。
    #[test]
    fn failed_calls_land_in_the_unfinished_list() {
        let mut session = Session::new(SessionId::new("s1"), "任务");
        session
            .commit_round(
                AgentTurn {
                    text: "想读".to_owned(),
                    calls: vec![ToolCall {
                        id: CallId::new("c1"),
                        name: "read_file".to_owned(),
                        arguments: json!({"path": "missing.txt"}),
                    }],
                },
                vec![ToolOutcome::refused("路径越界".to_owned())],
            )
            .unwrap();

        let index = EvidenceIndex::from_session(&session);

        assert!(index.files_read.is_empty(), "被拒绝的读不算读过");
        assert_eq!(index.unfinished_calls.len(), 1);
        assert!(index.unfinished_calls[0].contains("read_file"));
        assert!(index.unfinished_calls[0].contains("路径越界"));
    }

    /// 同一个文件读两遍，台账里只列一次。
    #[test]
    fn the_index_deduplicates() {
        let mut session = Session::new(SessionId::new("s1"), "任务");
        for id in ["c1", "c2"] {
            session
                .commit_round(
                    AgentTurn {
                        text: "读".to_owned(),
                        calls: vec![ToolCall {
                            id: CallId::new(id),
                            name: "read_file".to_owned(),
                            arguments: json!({"path": "a.txt"}),
                        }],
                    },
                    vec![ToolOutcome::success("内容".to_owned())],
                )
                .unwrap();
        }

        assert_eq!(
            EvidenceIndex::from_session(&session).files_read,
            vec!["a.txt"]
        );
    }

    /// 批量读按一文件一项记录：台账里能逐个认出读过哪些文件。
    #[test]
    fn a_batch_read_lists_each_file_separately() {
        let mut session = Session::new(SessionId::new("s1"), "任务");
        session
            .commit_round(
                AgentTurn {
                    text: "批量读".to_owned(),
                    calls: vec![ToolCall {
                        id: CallId::new("c1"),
                        name: "read_files".to_owned(),
                        arguments: json!({"paths": ["a.rs", "b.rs"]}),
                    }],
                },
                vec![ToolOutcome::success("内容".to_owned())],
            )
            .unwrap();

        assert_eq!(
            EvidenceIndex::from_session(&session).files_read,
            vec!["a.rs", "b.rs"]
        );
    }

    /// 台账紧跟在占位说明之后呈现给模型。
    #[test]
    fn the_ledger_follows_the_placeholder() {
        let mut session = Session::new(SessionId::new("s1"), "任务");
        session
            .commit_round(
                AgentTurn {
                    text: "读".to_owned(),
                    calls: vec![ToolCall {
                        id: CallId::new("c1"),
                        name: "read_file".to_owned(),
                        arguments: json!({"path": "a.txt"}),
                    }],
                },
                vec![ToolOutcome::success("内容".repeat(1000))],
            )
            .unwrap();

        let (slices, _) = compact(&session, &budget(512));

        let texts = user_texts(&slices);
        assert!(texts[1].contains("已省略"), "第二位是占位说明: {texts:?}");
        assert!(texts[2].contains("证据台账"), "第三位是台账: {texts:?}");
        assert!(
            texts[2].contains("a.txt"),
            "台账里应列出读过的文件: {texts:?}"
        );
    }

    /// 没裁剪时不加台账：模型看到的历史完整，不需要这一段。
    #[test]
    fn no_ledger_without_compaction() {
        let session = session_with_rounds(2);
        let (slices, _) = compact(&session, &budget(100_000));

        assert_eq!(slices.len(), 3, "任务 + 两轮，没有台账");
    }

    /// 从证据事件算出的台账与从会话算出的一致。
    ///
    /// 两处判定规则必须一致：用户在 `--report` 里看到的账与模型在上下文
    /// 里看到的账出自同一份数据。
    #[test]
    fn the_index_from_events_matches_the_one_from_the_session() {
        let mut session = Session::new(SessionId::new("s1"), "任务");
        let call = ToolCall {
            id: CallId::new("c1"),
            name: "read_file".to_owned(),
            arguments: json!({"path": "a.txt"}),
        };
        session
            .commit_round(
                AgentTurn {
                    text: "读".to_owned(),
                    calls: vec![call.clone()],
                },
                vec![ToolOutcome::success("内容".to_owned())],
            )
            .unwrap();
        let events = [AuditEvent::ToolObserved {
            turn: 1,
            call,
            outcome: ToolOutcome::success("内容".to_owned()),
        }];

        assert_eq!(
            EvidenceIndex::from_events(events.iter()),
            EvidenceIndex::from_session(&session)
        );
    }
}

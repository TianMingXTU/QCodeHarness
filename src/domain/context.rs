//! 上下文预算与历史裁剪。
//!
//! 长任务的历史会无限增长：每轮都把完整历史发给模型，token 成本线性上升，
//! 最终超出模型输入上限直接报错。裁剪是"发给模型时怎么看历史"的纯函数
//! 变换——[`Session`] 本身不被修改，证据、审计与将来的恢复都依赖完整历史。
//!
//! 被裁掉的历史有两种呈现：占位说明（确定性、零成本），或一段已生成的
//! 摘要（信息量大，但摘要本身是一次模型调用，挂在 `Session` 上只做一次）。
//! `compact` 不调模型——摘要文本由调用方喂进来，这里只负责"有就用"。
//!
//! **上下文与证据分开。** 历史被裁掉之后，模型仍然需要知道
//! "这个任务动过什么"：读过哪些文件、写过哪些文件、跑过哪些命令、哪些
//! 调用没成功。这些事实由 [`EvidenceIndex`] 从会话历史**算**出来（不是另记
//! 一份账——另记一份就会与历史分叉），在占位说明之后呈现给模型；用户也能
//! 通过 `--report` 取到同一份账。

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
    /// 步骤边界是**代码**知道的：历史里每条非交接的用户消息就是一步的
    /// 交代，数一遍就知道每轮属于第几步。模型可能漏写、可能写错编号，
    /// 而"这份摘要讲的是哪几步"必须准确——下一步的模型要靠它对上自己
    /// 的进度。
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

    /// 覆盖的步骤范围（1-based，含端点）。一个都没覆盖时为 `None`。
    pub fn step_range(&self) -> Option<(u32, u32)> {
        let first = self.steps.first().copied()?;
        let last = self.steps.last().copied()?;
        Some((first, last))
    }
}

/// 摘要策略：控制"被裁掉多少轮才值得再花一次模型调用"。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SummarizePolicy {
    /// 被裁轮次达到这个数才生成/更新摘要。调太高会一直用占位说明，
    /// 调太低则摘要调用太频繁；1-2 适合短上下文预算，默认给 2。
    pub min_dropped_to_summarize: usize,
}

/// 发给模型前，历史最多占多少 token。
///
/// **token 优先，轮数只作兜底。** 轮数不是好的上限：同样一轮，读一个
/// 10 行文件与跑一条输出 5 万字的命令，token 量差三个数量级。轮数相同
/// 的两个会话，一个早就该裁、一个还远没到上限——按轮数裁要么白裁、
/// 要么照旧爆窗。
///
/// 代价是 token 数要**估算**（见 [`estimate_tokens`]）。真要用精确数就得
/// 引入分词器，那是一个新依赖，而且各家模型的分词器还不一样。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextBudget {
    /// 历史最多占多少 token。`None` 表示不按 token 裁剪。
    pub max_tokens: Option<usize>,
    /// 至少保留最近多少轮。
    ///
    /// 这是**下限**：token 预算再紧，最近这几轮也留着。模型连着几轮
    /// 看不到自己刚做过什么，就会重新问一遍、重新读一遍；而它要的
    /// 恰恰是"刚才那次调用结果是什么"。
    pub min_recent_rounds: usize,
    /// 是否始终保留最初的用户任务。强烈建议 true——模型忘了自己在干什么，
    /// 后面每一轮都会跑偏。
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
            // 默认不开摘要：多一次模型调用是不可忽略的成本，显式开启。
            summarize: None,
        }
    }
}

/// 默认的历史 token 预算。
///
/// 16k 的取法：常见模型窗口至少 32k，扣掉系统提示词、工具定义与当前
/// 轮之后，留给历史的空间就是这个量级。它比"16 轮"更贴近真实成本：
/// 一轮读文件的观察可能几百 token，一轮跑测试的观察可能上万。
pub const DEFAULT_MAX_TOKENS: usize = 16_000;

/// 默认至少保留的近期轮数。
///
/// 两轮是最低限度：一轮是"我刚做了什么"，一轮是"结果是什么"。
pub const DEFAULT_MIN_RECENT_ROUNDS: usize = 2;

/// 估算一段文本占多少 token。
///
/// **这是预算用的估算，不是计费口径。** 真实用量看 provider 报的
/// `usage`；两者在中文上能差三四倍，所以这里的数字只用来决定"要不要
/// 裁"，不用来算钱。
///
/// 口径：CJK 字符按 1 个 token 记，其余字符按 4 个字符 1 个 token 记。
/// 纯"字符数 ÷ 4"（英文的通用近似）会把中文低估三到四倍——本项目的
/// 提示词、历史与工具输出绝大多数是中文，用那个口径等于预算形同虚设。
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

/// 这个字符算不算 CJK（含中日韩标点与全角形式）。
///
/// 全角与 CJK 标点一并算进来：它们在中文文本里密度很高（每句话都带
/// 逗号句号），漏掉它们会让估算再偏一截。
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
/// **它是从会话历史算出来的，不是另记一份账。** 另记一份就会与历史
/// 分叉：模型改了主意、工具被拒绝、恢复时重跑了某个调用——账本不会
/// 跟着变，而模型看到的"我做过什么"必须是真相。
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
    /// 这是"未完成事项"的**唯一**来源：哪些调用没成功，代码知道得比
    /// 模型准——让模型自己列一份，它会漏，而漏掉的正是最要紧的
    /// "上一步那条命令到底跑没跑成"。所以模型不参与这一份，
    /// 它由代码从调用记录里拼出来。
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
            // 观察紧跟在带调用的那一轮之后（`commit_round` 保证配对）。
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
    /// 事件的来源与会话不同，但**判定规则只有一份**（[`Self::add_call`]）：
    /// 两处各写一套判定，同一个调用在会话视图与报表视图里就会被分成
    /// 不同的类，用户对着报表与模型看到的对不上。
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
    /// `outcome` 为 `None` 表示"还不知道结果"（调用已发起、结果未知）。
    /// 这种调用算未完成——它可能已经产生了副作用。
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

    /// 渲染成台账正文（不含抬头）。
    ///
    /// 抬头由调用方给：给模型看的抬头得说明"这是裁剪之后的账"，
    /// 给用户看的抬头得说明"这是这个任务动过什么"——不是同一句话。
    ///
    /// 空账本渲染成空串：没有内容就不该占模型的注意力，也不该占 token。
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

/// 内置工具的分类。
///
/// 分类靠工具名：领域层拿不到工具的注解（注解在注册表里，注册表在应用
/// 层），而"读过的文件"与"写过的文件"必须分开——台账的全部价值就在于
/// 模型能看出哪些是它读过的、哪些是它改过的。
///
/// 代价是内置工具名在这里出现了一次。名单由 [`known_tool_names`] 给出，
/// 应用层有一条测试拿它与真实注册表对照：名字拼错时台账会静默少一项，
/// 那条测试让它当场变红，而不是等到某次长任务里模型重读一遍文件才发现。
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
/// 用途只有一个：让应用层能拿它与真实注册表对照（见
/// `application::tool_registry` 的测试）。
pub fn known_tool_names() -> &'static [&'static str] {
    &["read_file", "read_files", "write_file", "edit_file", "run_command"]
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

/// 这次调用的"对象"：文件路径或命令文本。
///
/// 批量读给的是路径数组，逐个返回——台账里要是一文件一项，
/// 去重才认得出来（"a.rs、b.rs" 之后再单独读 a.rs 会变成两项）。
fn subject(call: &ToolCall) -> Vec<String> {
    if let Some(path) = call.arguments.get("path").and_then(|value| value.as_str()) {
        return vec![path.to_owned()];
    }
    if let Some(paths) = call.arguments.get("paths").and_then(|value| value.as_array()) {
        let listed: Vec<String> = paths
            .iter()
            .filter_map(|path| path.as_str().map(str::to_owned))
            .collect();
        if !listed.is_empty() {
            return listed;
        }
    }
    if let Some(command) = call.arguments.get("command").and_then(|value| value.as_str()) {
        return vec![command.to_owned()];
    }
    vec![call.name.clone()]
}

/// 调用参数的简短形式：只留关键参数，不把整份 JSON 塞进台账。
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

/// 一次裁剪的结果，报告丢掉了什么，供证据与调试使用。
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
    /// 它不小于"原历史减去被裁轮次"：占位说明与证据台账是新增的内容。
    pub tokens_after: usize,
    /// 净减掉的 token 数。
    ///
    /// 净值为 0 说明这一轮裁剪没省下东西（台账比被裁的还长）——那正是
    /// 值得看见的一个数。
    pub dropped_tokens: usize,
}

/// 把会话历史裁剪到预算内，返回保留的片段与裁剪报告。
///
/// 纯函数：不读时钟、不做 IO、不改 `session`。裁剪以"轮"为最小单位——
/// 半轮历史（有调用没观察）会让 OpenAI 兼容端点直接 400，所以
/// 预算之外的轮次连同它们的观察一起整体丢弃。
///
/// **裁剪顺序**：先按 token 总量判断要不要裁；要裁就**从最老的轮次
/// 开始丢**，但最近 [`ContextBudget::min_recent_rounds`] 轮一定留着。
/// 用户消息（任务交代、步骤边界、结论交接）永远不丢：它们是"现在在
/// 第几步""前面查出了什么"的唯一来源，丢了模型就会跑偏。
///
/// 被裁掉之后，占位说明之后紧跟一条**证据台账**：做过什么由代码从
/// 历史算出，不靠模型记得。
pub fn compact<'a>(
    session: &'a Session,
    budget: &ContextBudget,
) -> (Vec<ContextSlice<'a>>, CompactionReport) {
    let slices = session.slices();
    let costs: Vec<usize> = slices.iter().map(slice_tokens).collect();
    let tokens_before: usize = costs.iter().sum();
    let round_numbers: Vec<u32> = slices.iter().filter_map(round_number).collect();
    let rounds_before = round_numbers.len();

    let mut report = CompactionReport {
        rounds_before,
        rounds_after: rounds_before,
        dropped: Vec::new(),
        tokens_before,
        tokens_after: tokens_before,
        dropped_tokens: 0,
    };

    let Some(max_tokens) = budget.max_tokens else {
        return (slices, report);
    };
    if tokens_before <= max_tokens {
        return (slices, report);
    }

    // 每轮在切片里的位置与它的 token 代价。
    let rounds: Vec<(u32, usize)> = slices
        .iter()
        .enumerate()
        .filter_map(|(index, slice)| round_number(slice).map(|number| (number, index)))
        .collect();

    // 用户消息永远保留，它们的代价先从预算里扣掉。
    let mandatory_tokens: usize = slices
        .iter()
        .zip(&costs)
        .filter(|(slice, _)| matches!(slice, ContextSlice::User(_)))
        .map(|(_, cost)| cost)
        .sum();
    // 最近几轮一定留着：模型至少要有近期上下文。
    let floor = budget.min_recent_rounds.min(rounds.len());
    let floor_start = rounds.len() - floor;
    let floor_tokens: usize = rounds[floor_start..]
        .iter()
        .map(|(_, index)| costs[*index])
        .sum();
    let room = max_tokens.saturating_sub(mandatory_tokens + floor_tokens);

    // 从最新的一轮往前装，装不下的就是最老的那几轮。
    let mut used = 0usize;
    let mut keep_from = floor_start;
    for index in (0..floor_start).rev() {
        let cost = costs[rounds[index].1];
        if used + cost > room {
            break;
        }
        used += cost;
        keep_from = index;
    }

    let dropped: Vec<u32> = rounds[..keep_from].iter().map(|(number, _)| *number).collect();
    let dropped_set: HashSet<u32> = dropped.iter().copied().collect();

    let mut kept: Vec<ContextSlice<'a>> = Vec::with_capacity(slices.len() + 2);
    // 任务消息是历史的第一条用户消息。先它、后占位说明与台账、再保留的
    // 轮次，顺序与"人先交代任务、再补一句摘要、然后是近期工作"一致。
    if budget.keep_task {
        if let Some(ContextSlice::User(task)) = slices.first() {
            kept.push(ContextSlice::User(task.clone()));
        }
    }
    kept.push(ContextSlice::User(Cow::Owned(omitted_text(session, &dropped))));
    // 台账：上下文被裁了，"做过什么"不能跟着没。
    let index = EvidenceIndex::from_session(session);
    if !index.is_empty() {
        kept.push(ContextSlice::User(Cow::Owned(format!(
            "【本会话的证据台账】前面的对话历史被裁剪了，但下列事实仍然成立：\n{}",
            index.render()
        ))));
    }
    for (position, slice) in slices.iter().enumerate() {
        match slice {
            // 任务消息已在最前面钉住（或按 keep_task 主动裁掉），原样再放
            // 一遍只会让模型把同一件事看两遍。
            ContextSlice::User(_) if position == 0 => {}
            // 其余用户消息是步骤边界（单会话编排的产物）：它们是"现在在
            // 第几步"的唯一来源，不是历史轮次。裁掉它模型就不知道自己
            // 进行到哪了——而裁剪恰恰发生在步数最多的长任务上，正是最
            // 需要这一步信息的时候。按原位置保留，不跳到前面：把第 3 步
            // 的交代挪到第 1 步的观察之前，等于给模型一份假时间线。
            ContextSlice::User(_) => kept.push(slice.clone()),
            ContextSlice::Round { number, .. } if !dropped_set.contains(number) => {
                kept.push(slice.clone())
            }
            ContextSlice::Round { .. } => {}
        }
    }

    report.rounds_after = rounds_before - dropped.len();
    report.dropped = dropped;
    report.tokens_after = kept.iter().map(slice_tokens).sum();
    report.dropped_tokens = tokens_before.saturating_sub(report.tokens_after);
    (kept, report)
}

/// 切片的轮次号。用户消息没有轮次号。
fn round_number(slice: &ContextSlice<'_>) -> Option<u32> {
    match slice {
        ContextSlice::Round { number, .. } => Some(*number),
        ContextSlice::User(_) => None,
    }
}

/// 被省略历史处显示的文本：有覆盖全部被裁轮次的摘要就用摘要，否则用
/// 占位说明。摘要没覆盖全时宁可保守——把没被摘要的那几轮说成"已概括"
/// 会诱导模型把没看过的内容当成看过的。
fn omitted_text(session: &Session, dropped: &[u32]) -> String {
    match session.summary() {
        Some(summary) if summary.covers_all(dropped) => summary.text.clone(),
        _ => omitted_notice(dropped),
    }
}

/// 被省略历史的占位说明。
///
/// 要写清省略了哪些轮、去哪找回来：模型看到历史缺口时最常见的错是
/// 凭想象补全被删掉的内容，指路能压住这个倾向。
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
    /// 供断言"某轮的观察是否出现在裁剪结果里"。
    fn session_with_rounds(n: u32) -> Session {
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
                    vec![ToolOutcome::success(format!("观察{index}"))],
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

    /// 用户消息（任务交代、步骤交代）的 token 代价。
    ///
    /// 它们永远保留，所以永远占预算——算上限时得把它们算进去，否则
    /// 它们会挤掉一轮。
    fn mandatory_tokens(session: &Session) -> usize {
        session
            .slices()
            .iter()
            .filter(|slice| matches!(slice, ContextSlice::User(_)))
            .map(slice_tokens)
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
        // 所以上限得把它算进去——不算它就会挤掉一轮。
        let one_round = slice_tokens(&session.slices()[2]);
        let limit = one_round * 2 + mandatory_tokens(&session);
        let (slices, report) = compact(&session, &budget(limit));

        let numbers: Vec<u32> = slices.iter().filter_map(round_number).collect();
        assert_eq!(numbers, vec![5, 6], "只保留最后两轮");
        assert_eq!(report.dropped, vec![1, 2, 3, 4]);
        assert_eq!(report.rounds_before, 6);
        assert_eq!(report.rounds_after, 2);
    }

    #[test]
    fn keep_task_pins_the_task_message_first() {
        let session = session_with_rounds(4);
        let (slices, _) = compact(&session, &budget(1));

        assert!(
            matches!(&slices[0], ContextSlice::User(text) if text.as_ref() == "任务"),
            "任务消息必须排在第一位，实际 {:?}",
            slices[0]
        );
        assert!(matches!(&slices[1], ContextSlice::User(_)), "第二位是占位说明");

        // 关掉 keep_task：任务也被裁掉，占位说明顶到第一位。
        let no_task = ContextBudget {
            max_tokens: Some(1),
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

    #[test]
    fn placeholder_names_the_omitted_range() {
        let session = session_with_rounds(6);
        let (slices, _) = compact(&session, &budget(1));

        let Some(ContextSlice::User(placeholder)) = slices.get(1) else {
            panic!("第二位应当是占位说明，实际 {:?}", slices.get(1));
        };
        assert!(placeholder.contains("第 1-6 轮"), "{}", placeholder);
        assert!(placeholder.contains("共 6 轮"), "{}", placeholder);
        assert!(placeholder.contains("search_text"), "应指路 search_text: {placeholder}");
        assert!(placeholder.contains("read_file"), "应指路 read_file: {placeholder}");
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
    fn zero_token_budget_keeps_only_task_and_placeholder() {
        let session = session_with_rounds(2);
        let (slices, report) = compact(&session, &budget(0));

        assert!(slices.len() >= 2, "至少是任务 + 占位说明");
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
                        vec![ToolOutcome::success(format!("第 {step} 步的观察"))],
                    )
                    .unwrap();
            }
        }
        session
    }

    #[test]
    fn step_boundaries_survive_compaction_in_their_original_order() {
        let session = session_with_steps(2);
        // 只够装两轮：第 1、2、3 步各两轮里只有最后两轮活下来，
        // 但三条步骤交代一条都不能丢。步骤交代是用户消息，永远保留，
        // 也一定占 token——上限要把它算进去。
        let costs = round_costs(&session);
        let limit = costs[4] + costs[5] + mandatory_tokens(&session);
        let (slices, report) = compact(&session, &budget(limit));

        assert_eq!(report.dropped, vec![1, 2, 3, 4], "裁掉前四轮");
        let texts = user_texts(&slices);
        assert_eq!(texts[0], "任务", "任务交代钉在最前");
        assert!(texts[1].contains("已省略第 1-4 轮"), "第二位是占位说明: {texts:?}");
        // 第 1 步没有独立交代——任务消息就是第 1 步的交代。
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
        let session = session_with_rounds(6);
        let (slices, _) = compact(&session, &budget(1));

        let text = second_slice_text(&slices);
        assert!(text.contains("已省略第 1-6 轮"), "{text}");
        assert!(!text.contains("摘要"), "没有摘要时不应出现摘要文本: {text}");
    }

    #[test]
    fn summary_covering_all_dropped_rounds_replaces_the_placeholder() {
        let mut session = session_with_rounds(6);
        with_summary(&mut session, 1, 6);
        let (slices, _) = compact(&session, &budget(1));

        let text = second_slice_text(&slices);
        assert!(text.contains("摘要：第 1-6 轮做了什么事"), "{text}");
        assert!(!text.contains("已省略"), "摘要覆盖全部被裁轮次时不应再出现占位说明: {text}");
    }

    #[test]
    fn partial_summary_falls_back_to_the_placeholder() {
        let mut session = session_with_rounds(6);
        // 摘要只覆盖第 1-2 轮，被裁的第 3-6 轮在覆盖范围外。
        with_summary(&mut session, 1, 2);
        let (slices, _) = compact(&session, &budget(1));

        let text = second_slice_text(&slices);
        assert!(text.contains("已省略第 1-6 轮"), "{text}");
        assert!(!text.contains("摘要"), "部分覆盖时不得把没摘要的轮次说成已概括: {text}");
    }

    #[test]
    fn compact_stays_pure_when_a_summary_is_set() {
        let mut session = session_with_rounds(5);
        // 摘要要覆盖全部被裁轮次才会被用上：预算压到 1 时五轮全被裁，
        // 摘要只覆盖前三轮就得退回占位说明。
        with_summary(&mut session, 1, 5);
        let summary_before = session.summary().cloned();
        let turns_before = session.turns().len();

        let (first, _) = compact(&session, &budget(1));
        let (second, _) = compact(&session, &budget(1));

        assert_eq!(first, second, "同一输入两次裁剪结果一致（无隐藏状态变更）");
        assert_eq!(session.summary(), summary_before.as_ref(), "摘要不被裁剪改动");
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

    /// 裁剪由 token 决定，不由轮数决定：同样的轮数、同样的上限，带大
    /// 观察的那个会话留不下那么多轮。
    ///
    /// 这是"token 优先于轮数"的理由：一轮读小文件与一轮读大文件的
    /// token 量差三个数量级。按轮数裁时两个会话会得到同样的结果，
    /// 一个白裁、一个照旧爆窗。
    #[test]
    fn the_budget_is_driven_by_tokens_not_by_round_count() {
        let plain = session_with_rounds(4);
        let fat = session_with_a_fat_round();

        // 同一个 token 上限：够装下 plain 的三轮，装不下 fat 的三轮。
        // 按轮数算的话，两个会话都会留下三轮。
        let limit = round_costs(&plain)[0] * 3 + mandatory_tokens(&plain);
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

    /// 没成功的调用进"未完成"清单——这一份是代码拼的，不是模型写的。
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

        assert_eq!(EvidenceIndex::from_session(&session).files_read, vec!["a.txt"]);
    }

    /// 批量读一文件一项：台账里要能逐个认出读过哪些文件。
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
                vec![ToolOutcome::success("内容".to_owned())],
            )
            .unwrap();

        let (slices, _) = compact(&session, &budget(1));

        let texts = user_texts(&slices);
        assert!(texts[1].contains("已省略"), "第二位是占位说明: {texts:?}");
        assert!(texts[2].contains("证据台账"), "第三位是台账: {texts:?}");
        assert!(texts[2].contains("a.txt"), "台账里应列出读过的文件: {texts:?}");
    }

    /// 没裁剪时不塞台账：模型看到的历史是完整的，没必要多这一段。
    #[test]
    fn no_ledger_without_compaction() {
        let session = session_with_rounds(2);
        let (slices, _) = compact(&session, &budget(100_000));

        assert_eq!(slices.len(), 3, "任务 + 两轮，没有台账");
    }

    /// 同一份台账从证据事件算出来，与从会话算出来的一致。
    ///
    /// 两处判定规则必须一致：用户在 `--report` 里看到的账，与模型在
    /// 上下文里看到的账，不是两份数据。
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
        let events = vec![AuditEvent::ToolObserved {
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

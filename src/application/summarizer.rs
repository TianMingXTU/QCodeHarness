//! 历史摘要器：把被裁掉的轮次压成一段文字。
//!
//! 摘要是优化不是必需：生成失败一律降级为占位说明，任务继续。
//! 这里只负责"怎么问模型"，"何时值得问"的决策在运行循环
//! （`should_summarize`）——被裁轮次累积到策略阈值且现有摘要没覆盖才问。

use std::borrow::Cow;
use std::sync::Arc;

use crate::application::error::ApplicationError;
use crate::application::port::{ModelGateway, ModelRequest};
use crate::domain::agent::AgentSpec;
use crate::domain::context::Summary;
use crate::domain::evidence::TokenUsage;
use crate::domain::session::ContextSlice;

/// 一次摘要请求的输入。全部 owned：运行循环先同步渲染被裁片段，
/// 再 await 模型——借用不能跨过 await 点。
pub struct SummarizeInput {
    /// 被裁片段渲染成的文本。
    pub dropped_transcript: String,
    /// 已有的旧摘要。非空时要求模型在其基础上合并，而不是从头概括。
    pub existing: Option<Summary>,
    /// 本次摘要覆盖的轮次范围（含）。
    pub covers: (u32, u32),
    /// 被裁历史涉及的步骤号（1-based）。
    ///
    /// 由代码从会话算出：历史里每条非交接的用户消息就是一步的交代，
    /// 数一遍就知道每轮属于第几步。模型可能漏写、可能写错编号，而
    /// "这份摘要讲的是哪几步"必须准确。
    pub steps: Vec<u32>,
}

/// 历史摘要器。
pub struct Summarizer {
    model: Arc<dyn ModelGateway>,
}

/// 摘要的字数上限。摘要是给模型看的备忘，不是全文备份：
/// 太长会让"压缩"失去意义，300 字足够装下关键路径与结论。
const SUMMARY_MAX_CHARS: usize = 300;

impl Summarizer {
    pub fn new(model: Arc<dyn ModelGateway>) -> Self {
        Self { model }
    }

    /// 生成摘要，成功返回摘要与本次调用的 token 用量。
    ///
    /// # Errors
    ///
    /// 模型调用失败或摘要文本为空时返回 [`ApplicationError::SummaryFailed`]。
    /// 调用方应降级为占位说明，而不是把错误抛给任务。
    pub async fn summarize(
        &self,
        input: SummarizeInput,
    ) -> Result<(Summary, TokenUsage), ApplicationError> {
        let steps_hint = if input.steps.is_empty() {
            String::new()
        } else {
            format!("，涉及第 {} 步", describe_steps(&input.steps))
        };
        let mut prompt = format!(
            "下面是一次编码任务中被裁剪出上下文的对话历史片段（第 {from}-{to} 轮{steps_hint}）。\
             请用不超过 {SUMMARY_MAX_CHARS} 字概括，**按步骤分段**，\
             每段以「第 N 步：」开头（N 用片段里出现的步号）：\
             1) 每一步已完成的事与关键结论；\
             2) 发现的关键信息（文件路径、命令输出要点、错误信息）。\
             **不要罗列尚未完成的事**：那一份由系统按调用记录单独给出，\
             你重复写只会与它冲突。\
             只输出摘要文本本身，不要任何解释。\n",
            from = input.covers.0,
            to = input.covers.1,
        );
        if let Some(existing) = &input.existing {
            prompt.push_str(&format!(
                "\n此前已有摘要（涵盖第 {}-{} 轮）：\n{}\n请在其基础上合并下面的新片段，输出合并后的完整摘要。\n",
                existing.covers.0, existing.covers.1, existing.text,
            ));
        }
        prompt.push_str("\n【对话历史片段】\n");
        prompt.push_str(&input.dropped_transcript);

        let spec = AgentSpec::new(
            crate::domain::agent::AgentRole::Coder,
            "你是编码任务的会话记录员，负责压缩对话历史供后续轮次参考。",
        );
        let request = ModelRequest {
            history: vec![ContextSlice::User(Cow::Owned(prompt))],
            spec: &spec,
            tools: &[],
            // 摘要调用不参与前缀缓存：它每次的输入都是新裁出来的片段，
            // 给它一个键只会让 provider 把互不相干的请求路由到同一台
            // 机器，反而挤掉真正该命中的那条前缀。注意是 `None` 而不是
            // 空字符串——后者在 wire 上仍是一个 `prompt_cache_key`。
            cache_key: None,
        };
        let model = self.model.clone();
        let reply = model
            .complete(request)
            .await
            .map_err(|error| ApplicationError::SummaryFailed(error.to_string()))?;
        let text = reply.turn.text.trim().to_owned();
        if text.is_empty() {
            return Err(ApplicationError::SummaryFailed("模型返回了空摘要".to_owned()));
        }
        Ok((
            Summary {
                covers: input.covers,
                // 摘要按步骤分段：模型不按格式回答时退回原文 + 代码写的
                // 步骤抬头，两种情况下"这份摘要讲哪几步"都是清楚的。
                text: arrange_by_step(&text, &input.steps),
                steps: input.steps,
            },
            reply.usage,
        ))
    }
}

/// 步骤号列表的简短写法：`1-3` 或 `1、3`。
fn describe_steps(steps: &[u32]) -> String {
    match (steps.first(), steps.last()) {
        (Some(first), Some(last)) if first != last => format!("{first}-{last}"),
        (Some(first), Some(_)) => first.to_string(),
        _ => String::new(),
    }
}

/// 把模型返回的摘要整理成按步骤分段的文本。
///
/// **为什么要有这一步。** 摘要是给下一步的模型看的，而下一步要的是
/// "第 1 步查出了什么、第 2 步做了什么"——一整段混在一起的文字里，
/// 模型得自己猜哪句属于哪一步。分段之后，边界是现成的。
///
/// **模型不保证守格式。** 解析不出标记就退回原文加一个代码写的抬头，
/// 而不是硬掰：掰出来的段落会把不相干的内容塞进错误的步骤，比不分
/// 段更糟。
///
/// 分段之外的段（模型写了代码没算进去的步号）也留着：内容比格式重要。
fn arrange_by_step(text: &str, steps: &[u32]) -> String {
    let mut sections: Vec<(u32, String)> = Vec::new();
    let mut current: Option<(u32, String)> = None;
    for line in text.lines() {
        match split_step_marker(line) {
            Some((step, body)) => {
                if let Some((number, body)) = current.take() {
                    sections.push((number, body.trim().to_owned()));
                }
                current = Some((step, body.to_owned()));
            }
            None => {
                if let Some((_, body)) = current.as_mut() {
                    body.push('\n');
                    body.push_str(line);
                }
            }
        }
    }
    if let Some((number, body)) = current.take() {
        sections.push((number, body.trim().to_owned()));
    }

    if sections.is_empty() {
        // 模型没按格式回答：原文照用，步骤抬头由代码补。
        let described = describe_steps(steps);
        let header = if described.is_empty() {
            String::new()
        } else {
            format!("【第 {described} 步】\n")
        };
        return format!("{header}{}", text.trim());
    }

    let mut out = String::new();
    for step in steps {
        let body = sections
            .iter()
            .find(|(number, _)| number == step)
            .map(|(_, body)| body.as_str())
            .unwrap_or("（本步的内容没能概括出来）");
        out.push_str(&format!("【第 {step} 步】{body}\n"));
    }
    // 模型写了代码没算进去的步号：也留着，别丢内容。
    for (number, body) in &sections {
        if !steps.contains(number) {
            out.push_str(&format!("【第 {number} 步】{body}\n"));
        }
    }
    out.trim().to_owned()
}

/// 拆出一行里的步骤标记：`第 3 步：…`、`【第 3 步】…`、`第3步:…` 都算。
///
/// 返回步号与标记之后的正文。行首不是标记时返回 `None`。
///
/// 「第」与数字之间可以有空格：提示词里给模型的就是「第 N 步：」这种
/// 写法，解析器得认它自己要求的样子。
fn split_step_marker(line: &str) -> Option<(u32, &str)> {
    let trimmed = line.trim();
    let trimmed = trimmed
        .strip_prefix('【')
        .or_else(|| trimmed.strip_prefix('['))
        .unwrap_or(trimmed);
    let rest = trimmed.trim_start().strip_prefix('第')?;
    let rest = rest.trim_start();
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() {
        return None;
    }
    let rest = rest[digits.len()..].trim_start().strip_prefix('步')?;
    // 标记后面必须是分隔符或行尾。否则"第 3 步的观察"这种正文会被
    // 误认成标记，把后面的内容塞进错误的步骤。
    //
    // 分隔符里有收尾的括号：`【第 3 步】读了配置` 这种写法模型很爱写，
    // 认不出来就等于把整段退回原文。
    let body = rest.trim_start();
    if !(body.is_empty()
        || body.starts_with(['：', ':', '、', '）', ')', '】', ']', '.', '。', ' ', '\t']))
    {
        return None;
    }
    let body = body.trim_start_matches(['：', ':', '、', '）', ')', '】', ']', '.', '。', ' ', '\t']);
    Some((digits.parse().ok()?, body))
}

/// 把会话切片渲染成给摘要模型看的纯文本。
///
/// 只保留有信息量的部分：模型文本、调用了什么、观察到了什么。
pub fn render_slices(slices: &[ContextSlice<'_>]) -> String {
    let mut text = String::new();
    for slice in slices {
        match slice {
            ContextSlice::User(message) => {
                text.push_str("【用户】");
                text.push_str(message.as_ref());
            }
            ContextSlice::Round {
                number,
                thought,
                calls,
                observations,
            } => {
                text.push_str(&format!("\n【第 {number} 轮】模型：{thought}"));
                for call in *calls {
                    text.push_str(&format!(
                        "\n  调用 {}({})",
                        call.name,
                        serde_json::to_string(&call.arguments)
                            .unwrap_or_else(|_| "{}".to_owned())
                    ));
                }
                for outcome in *observations {
                    text.push_str(&format!("\n  观察：{}", outcome.content));
                }
            }
        }
        text.push('\n');
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::port::{BoxFuture, ModelReply};
    use crate::domain::agent::AgentTurn;
    use crate::domain::evidence::TokenUsage;

    /// 回一句固定文本的假模型，顺带记下收到的提示词。
    struct ScriptedModel {
        reply: String,
        prompts: std::sync::Mutex<Vec<String>>,
    }

    impl ScriptedModel {
        fn new(reply: &str) -> Self {
            Self {
                reply: reply.to_owned(),
                prompts: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn first_prompt(&self) -> String {
            self.prompts
                .lock()
                .unwrap()
                .first()
                .cloned()
                .expect("应调用过模型")
        }
    }

    impl ModelGateway for ScriptedModel {
        fn complete<'a>(
            &'a self,
            request: ModelRequest<'a>,
        ) -> BoxFuture<'a, Result<ModelReply, ApplicationError>> {
            Box::pin(async move {
                let prompt = request
                    .history
                    .iter()
                    .filter_map(|slice| match slice {
                        ContextSlice::User(text) => Some(text.as_ref().to_owned()),
                        ContextSlice::Round { .. } => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                self.prompts.lock().unwrap().push(prompt);
                Ok(ModelReply {
                    turn: AgentTurn {
                        text: self.reply.clone(),
                        calls: Vec::new(),
                    },
                    usage: TokenUsage::default(),
                })
            })
        }
    }

    fn input(steps: Vec<u32>) -> SummarizeInput {
        SummarizeInput {
            dropped_transcript: "【第 1 轮】模型：查了文件".to_owned(),
            existing: None,
            covers: (1, 2),
            steps,
        }
    }

    /// 摘要按步骤分段：每一步一个抬头。
    #[tokio::test]
    async fn the_summary_is_split_by_step() {
        let model = ScriptedModel::new("第 1 步：读了配置\n第 2 步：改了默认值\n");
        let summarizer = Summarizer::new(Arc::new(model));

        let (summary, _) = summarizer.summarize(input(vec![1, 2])).await.unwrap();

        assert!(summary.text.contains("【第 1 步】"), "{}", summary.text);
        assert!(summary.text.contains("读了配置"), "{}", summary.text);
        assert!(summary.text.contains("【第 2 步】"), "{}", summary.text);
        assert!(summary.text.contains("改了默认值"), "{}", summary.text);
    }

    /// 摘要记下它讲的是哪几步：由代码给，不靠模型写。
    #[tokio::test]
    async fn the_summary_records_the_steps_it_covers() {
        let model = ScriptedModel::new("第 1 步：读了配置\n");
        let summarizer = Summarizer::new(Arc::new(model));

        let (summary, _) = summarizer.summarize(input(vec![1, 2, 3])).await.unwrap();

        assert_eq!(summary.steps, vec![1, 2, 3]);
        assert_eq!(summary.covers, (1, 2));
    }

    /// 模型漏掉的一步要留个空位——不能装作那一步不存在。
    #[tokio::test]
    async fn a_step_the_model_missed_gets_a_placeholder() {
        let model = ScriptedModel::new("第 1 步：读了配置\n第 3 步：跑了测试\n");
        let summarizer = Summarizer::new(Arc::new(model));

        let (summary, _) = summarizer.summarize(input(vec![1, 2, 3])).await.unwrap();

        assert!(
            summary.text.contains("【第 2 步】"),
            "漏掉的第 2 步也要有抬头: {}",
            summary.text
        );
        assert!(summary.text.contains("没能概括出来"), "{}", summary.text);
    }

    /// 模型不守格式时退回原文加代码写的抬头，不硬掰。
    #[tokio::test]
    async fn a_model_that_ignores_the_format_still_gets_a_step_header() {
        let model = ScriptedModel::new("读了点东西，改了点东西。");
        let summarizer = Summarizer::new(Arc::new(model));

        let (summary, _) = summarizer.summarize(input(vec![1, 2])).await.unwrap();

        assert!(summary.text.contains("【第 1-2 步】"), "{}", summary.text);
        assert!(summary.text.contains("读了点东西"), "{}", summary.text);
    }

    /// 模型写了代码没算进去的步号：内容留着，别丢。
    #[tokio::test]
    async fn an_extra_step_the_model_invented_is_kept() {
        let model = ScriptedModel::new("第 1 步：读了配置\n第 9 步：模型自己加的一段\n");
        let summarizer = Summarizer::new(Arc::new(model));

        let (summary, _) = summarizer.summarize(input(vec![1])).await.unwrap();

        assert!(summary.text.contains("【第 9 步】"), "{}", summary.text);
        assert!(summary.text.contains("模型自己加的一段"), "{}", summary.text);
    }

    /// "第 3 步的观察"是正文，不是步骤标记——认错了会把内容塞错步骤。
    #[test]
    fn a_step_marker_must_be_followed_by_a_separator() {
        assert_eq!(split_step_marker("第 3 步的观察"), None);
        assert_eq!(split_step_marker("第 3 步：读了配置"), Some((3, "读了配置")));
        assert_eq!(split_step_marker("【第 3 步】读了配置"), Some((3, "读了配置")));
        assert_eq!(split_step_marker("第3步:读了配置"), Some((3, "读了配置")));
    }

    /// 问模型的话要说清要按步骤分段。
    #[tokio::test]
    async fn the_prompt_asks_for_per_step_sections() {
        let model = Arc::new(ScriptedModel::new("第 1 步：读了配置\n"));
        let summarizer = Summarizer::new(model.clone());

        let _ = summarizer.summarize(input(vec![1, 2])).await.unwrap();

        let prompt = model.first_prompt();
        assert!(prompt.contains("按步骤分段"), "{prompt}");
        assert!(prompt.contains("第 1-2 步"), "提示里要带上步号: {prompt}");
    }
}

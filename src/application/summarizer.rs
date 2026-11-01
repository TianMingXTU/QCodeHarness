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
        let mut prompt = format!(
            "下面是一次编码任务中被裁剪出上下文的对话历史片段（第 {from}-{to} 轮）。\
             请用不超过 {SUMMARY_MAX_CHARS} 字概括：\
             1) 已完成的事与关键结论；\
             2) 发现的关键信息（文件路径、命令输出要点、错误信息）；\
             3) 尚未完成的事。\
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

        let spec = AgentSpec {
            role: crate::domain::agent::AgentRole::Coder,
            system_prompt: "你是编码任务的会话记录员，负责压缩对话历史供后续轮次参考。".to_owned(),
        };
        let request = ModelRequest {
            history: vec![ContextSlice::User(Cow::Owned(prompt))],
            spec: &spec,
            tools: &[],
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
                text,
            },
            reply.usage,
        ))
    }
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

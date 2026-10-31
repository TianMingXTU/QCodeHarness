//! 接口层：CLI 参数解析与控制台播报。
//!
//! 播报走 `EvidenceSink` 而不是在循环里直接 `println!`：`AgentRunner`
//! 因此完全不知道有控制台的存在，换个前端（Web、IDE）只需换一个
//! `EvidenceSink` 实现，循环一行不用改。

use std::sync::Mutex;

use crate::application::port::{EvidenceSink, Timestamp};
use crate::domain::evidence::{AuditEvent, Evidence};
use crate::domain::tool::ToolCall;
use crate::infrastructure::evidence::memory::MemoryEvidenceSink;

/// 观察结果在控制台最多显示的字符数，超出截断；完整内容仍会回填给模型。
const OBSERVE_PREVIEW_CHARS: usize = 400;

/// 用法说明，参数缺失时打印。
pub const USAGE: &str = "用法: cargo run -- \"任务描述\"\n示例: cargo run -- \"阅读 Cargo.toml，说明本项目用了哪些依赖\"";

/// 解析出的命令行参数。
#[derive(Debug, Clone, PartialEq)]
pub struct CliArgs {
    /// 要完成的任务描述。
    pub task: String,
}

/// 取第一个位置参数作为任务。
///
/// P0 只用 `std::env::args`，不接 `clap`——一个位置参数不值得引一个依赖。
pub fn parse_args() -> Option<CliArgs> {
    std::env::args().nth(1).map(|task| CliArgs { task })
}

/// 把审计事件渲染到控制台。
///
/// 实现 `EvidenceSink` 是为了搭上 runner 已经在发的事件流：
/// 思考、行动、观察各有其事件，播报只是它们的另一种落点。
#[derive(Debug)]
pub struct ConsoleReporter {
    /// 配置的最大轮数，只用于轮次分隔线上的总数。
    max_turns: u32,
    /// 已经播报过的轮次号；换轮时补一条分隔线。
    announced_turn: Mutex<u32>,
    /// 同一份事件同时在内存里留痕，供运行结束后汇总或转存。
    trail: MemoryEvidenceSink,
}

impl ConsoleReporter {
    /// 构造播报器。
    pub fn new(max_turns: u32) -> Self {
        Self {
            max_turns,
            announced_turn: Mutex::new(0),
            trail: MemoryEvidenceSink::new(),
        }
    }

    /// 已播报并留痕的事件，按发生顺序。
    pub fn events(&self) -> Vec<Evidence> {
        self.trail.events()
    }

    /// 播报一条事件。
    fn report(&self, event: &AuditEvent) {
        self.announce_turn(event.turn());
        match event {
            AuditEvent::Thought { text, .. } => {
                if !text.trim().is_empty() {
                    println!("\n[思考] {text}");
                }
            }
            AuditEvent::ToolRequested { call, .. } => {
                println!("\n[行动] {}", render_call(call));
            }
            AuditEvent::ToolObserved { outcome, .. } => {
                println!("[观察] {}", preview(&outcome.content));
            }
            // 审批与 token 用量暂不进控制台，避免淹没主要流程。
            AuditEvent::ApprovalDecided { .. } => {}
        }
    }

    /// 轮次切换时打印分隔线。
    fn announce_turn(&self, turn: u32) {
        let mut announced = self
            .announced_turn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if turn > *announced {
            *announced = turn;
            println!("\n════ 第 {turn}/{} 轮 ════", self.max_turns);
        }
    }
}

impl EvidenceSink for ConsoleReporter {
    fn record(&self, at: Timestamp, event: AuditEvent) {
        self.report(&event);
        self.trail.record(at, event);
    }
}

/// 把一次调用渲染成 `name({...})`，与重构前的输出格式一致。
fn render_call(call: &ToolCall) -> String {
    let arguments = serde_json::to_string(&call.arguments).unwrap_or_else(|_| "{}".to_owned());
    format!("{}({})", call.name, arguments)
}

/// 截断长文本用于控制台预览。
fn preview(text: &str) -> String {
    let text = text.trim();
    let total = text.chars().count();
    if total <= OBSERVE_PREVIEW_CHARS {
        return text.to_owned();
    }
    let head: String = text.chars().take(OBSERVE_PREVIEW_CHARS).collect();
    format!("{head}\n…（共 {total} 字符，已截断显示）")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::evidence::TokenUsage;
    use crate::domain::ids::CallId;
    use crate::domain::tool::{OutcomeStatus, ToolOutcome};
    use serde_json::json;

    fn call(name: &str, arguments: serde_json::Value) -> ToolCall {
        ToolCall {
            id: CallId::new("call_1"),
            name: name.to_owned(),
            arguments,
        }
    }

    #[test]
    fn preview_truncates_long_text() {
        let long = "x".repeat(OBSERVE_PREVIEW_CHARS + 10);
        let rendered = preview(&long);

        assert!(rendered.starts_with(&"x".repeat(OBSERVE_PREVIEW_CHARS)));
        assert!(rendered.contains("已截断显示"));
    }

    #[test]
    fn preview_keeps_short_text_verbatim() {
        assert_eq!(preview("  短文本  "), "短文本");
    }

    #[test]
    fn render_call_prints_name_and_arguments() {
        let rendered = render_call(&call("read_file", json!({"path": "Cargo.toml"})));
        assert_eq!(rendered, r#"read_file({"path":"Cargo.toml"})"#);
    }

    #[test]
    fn reporter_tracks_turns() {
        let reporter = ConsoleReporter::new(20);
        reporter.record(
            Timestamp::UNIX_EPOCH,
            AuditEvent::Thought {
                turn: 1,
                text: "想".to_owned(),
                usage: TokenUsage::default(),
            },
        );
        // 同一轮不重复打印分隔线。
        reporter.record(
            Timestamp::UNIX_EPOCH,
            AuditEvent::ToolObserved {
                turn: 1,
                call: call("read_file", json!({})),
                outcome: ToolOutcome {
                    content: "内容".to_owned(),
                    status: OutcomeStatus::Success,
                },
            },
        );
        reporter.record(
            Timestamp::UNIX_EPOCH,
            AuditEvent::Thought {
                turn: 2,
                text: "再想".to_owned(),
                usage: TokenUsage::default(),
            },
        );

        assert_eq!(*reporter.announced_turn.lock().unwrap(), 2);
    }
}

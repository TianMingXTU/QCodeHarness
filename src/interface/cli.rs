//! 接口层：CLI 参数解析与控制台播报。
//!
//! 播报走 `EvidenceSink` 而不是在循环里直接 `println!`：`AgentRunner`
//! 因此完全不知道有控制台的存在，换个前端（Web、IDE）只需换一个
//! `EvidenceSink` 实现，循环一行不用改。

use std::sync::Mutex;

use crate::application::port::{EvidenceSink, Timestamp};
use crate::domain::evidence::AuditEvent;
use crate::domain::tool::ToolCall;
use crate::infrastructure::evidence::memory::MemoryEvidenceSink;

/// 观察结果在控制台最多显示的字符数，超出截断；完整内容仍会回填给模型。
const OBSERVE_PREVIEW_CHARS: usize = 400;

/// 用法说明，参数缺失时打印。
pub const USAGE: &str = "\
用法: cargo run -- [--yes] [--kind <模板>] \"任务描述\"
      cargo run -- --list-sessions
      cargo run -- [--yes] --resume <会话ID>
      cargo run -- --report [--report-dir <目录>] [--json]
      cargo run -- --benchmark
示例: cargo run -- \"阅读 Cargo.toml，说明本项目用了哪些依赖\"
      cargo run -- --yes \"非交互场景下跑同一个任务\"
      cargo run -- --list-sessions
      cargo run -- --resume <会话ID>
      cargo run -- --report
      cargo run -- --report --json
      cargo run -- --benchmark";

/// 解析出的命令行参数。
#[derive(Debug, Clone, PartialEq)]
pub struct CliArgs {
    /// 要完成的任务描述。`--resume` 沿用快照里的原任务、
    /// `--list-sessions` 只列清单，两种模式下可以缺省。
    pub task: Option<String>,
    /// 跳过人工审批，所有工具自动放行。
    pub auto_approve: bool,
    /// 要恢复的会话 ID（`--resume <id>`）。
    pub resume: Option<String>,
    /// 只列出可恢复的会话（`--list-sessions`），然后退出。
    pub list_sessions: bool,
    /// 任务模板名（`--kind <name>`），交给任务编排（P4-2）解析。
    pub kind: Option<String>,
    /// 生成运行报表（`--report`）：读证据统计成功率、工具结局与成本。
    /// 不需要模型配置。
    pub report: bool,
    /// 报表统计的证据根目录（`--report-dir <dir>`），在该目录的
    /// `.qcodeharness/sessions/` 下找证据文件；缺省为当前工作区。
    pub report_dir: Option<String>,
    /// 报表输出 JSON（`--json`），只与 `--report` 搭配有效。
    pub json: bool,
    /// 跑 `benchmarks/` 下的基准任务集并打分（`--benchmark`）。
    pub benchmark: bool,
}

/// 解析命令行参数。
pub fn parse_args() -> Option<CliArgs> {
    parse_from(std::env::args().skip(1))
}

/// 从给定参数列表解析。与 [`parse_args`] 的分工：后者读真实环境，
/// 这个纯函数可测试。
///
/// 开关可以在任何位置出现；剩下的第一个位置参数是任务描述。
/// 不接 `clap`——个位数开关不值得引一个依赖。
pub fn parse_from<I: IntoIterator<Item = String>>(argv: I) -> Option<CliArgs> {
    let mut auto_approve = false;
    let mut resume = None;
    let mut list_sessions = false;
    let mut kind = None;
    let mut report = false;
    let mut report_dir = None;
    let mut json = false;
    let mut benchmark = false;
    let mut positional: Vec<String> = Vec::new();

    let mut iter = argv.into_iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--yes" => auto_approve = true,
            "--list-sessions" => list_sessions = true,
            "--report" => report = true,
            "--json" => json = true,
            "--benchmark" => benchmark = true,
            "--report-dir" => match iter.next() {
                Some(dir) => report_dir = Some(dir),
                None => {
                    eprintln!("--report-dir 需要一个目录：--report-dir <目录>");
                    return None;
                }
            },
            "--resume" => match iter.next() {
                Some(id) => resume = Some(id),
                None => {
                    eprintln!("--resume 需要一个会话 ID：--resume <id>");
                    return None;
                }
            },
            "--kind" => match iter.next() {
                Some(name) => kind = Some(name),
                None => {
                    eprintln!("--kind 需要一个模板名：--kind <模板名>");
                    return None;
                }
            },
            other => positional.push(other.to_owned()),
        }
    }

    if positional.len() > 1 {
        eprintln!("任务描述只能是其中一个位置参数，请用引号包住完整描述");
        return None;
    }
    let task = positional.into_iter().next();
    // 报表与基准都是不接任务描述的独立模式：带着描述跑只会让人以为
    // 描述也生效了，直接拒掉。
    if task.is_some() && (report || benchmark) {
        eprintln!("--report / --benchmark 不接受任务描述");
        return None;
    }
    // 恢复模式沿用快照里的原任务：再塞一条用户消息会破坏
    // "助手轮次后面紧跟观察"的配对，也说不清哪条才是真任务。
    if task.is_some() && resume.is_some() {
        eprintln!("--resume 会沿用快照里的原任务，不能再另外指定任务描述");
        return None;
    }
    // 搭错车的开关要当场指出来，静默忽略等于悄悄吃掉用户的意图。
    if json && !report {
        eprintln!("--json 需要与 --report 搭配");
        return None;
    }
    if report_dir.is_some() && !report {
        eprintln!("--report-dir 需要与 --report 搭配");
        return None;
    }
    if benchmark && resume.is_some() {
        eprintln!("--benchmark 不能与 --resume 搭配");
        return None;
    }
    if task.is_none() && resume.is_none() && !list_sessions && !report && !benchmark {
        eprintln!("缺少任务描述；恢复请用 --resume <id>，查看请用 --report，基准请用 --benchmark\n\n{USAGE}");
        return None;
    }

    Some(CliArgs {
        task,
        auto_approve,
        resume,
        list_sessions,
        kind,
        report,
        report_dir,
        json,
        benchmark,
    })
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
            AuditEvent::PolicyEvaluated { verdict, reason, .. } => {
                // 只播报被拒绝的裁决：放行和"要审批"对读者没有新信息，
                // 而拒绝必须让人立刻看见——那是模型被拦下来的地方。
                if matches!(verdict, crate::domain::policy::PolicyVerdict::Deny) {
                    println!("[策略] {reason}");
                }
            }
            AuditEvent::ContextCompacted {
                rounds_before,
                rounds_after,
                dropped,
                ..
            } => {
                // 历史被裁剪要让人知道：从这轮起模型看不到全文了，
                // 它"忘了"早先的细节不是傻，是被裁掉的。
                if !dropped.is_empty() {
                    println!(
                        "[上下文] 历史已裁剪：{rounds_before} 轮 → {rounds_after} 轮（省略第 {}-{} 轮）",
                        dropped.first().copied().unwrap_or(0),
                        dropped.last().copied().unwrap_or(0),
                    );
                }
            }
            AuditEvent::SummaryGenerated { covers, .. } => {
                // 多花的一次调用要有出处：这行告诉读者"刚才那次慢，
                // 是在补历史摘要"。
                println!("[摘要] 已生成第 {}-{} 轮的历史摘要", covers.0, covers.1);
            }
            AuditEvent::SummaryFailed { .. } => {
                // 摘要失败只提示不报错：历史退回占位说明，任务继续跑。
                println!("[摘要] 生成失败，继续用占位说明");
            }
            AuditEvent::StepStarted { role, .. } => {
                // 每步开始时报出角色：读者要知道接下来是"谁"在干活，
                // 尤其是编排把不同角色串起来的时候。
                println!("[角色] {} 开始执行", role.name());
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

    fn argv(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn flags_can_appear_in_any_position() {
        let args = parse_from(argv(&["--kind", "review", "--yes", "检查这段代码"])).unwrap();

        assert_eq!(args.task.as_deref(), Some("检查这段代码"));
        assert!(args.auto_approve);
        assert_eq!(args.kind.as_deref(), Some("review"));
    }

    #[test]
    fn resume_takes_an_id_instead_of_a_task() {
        let args = parse_from(argv(&["--yes", "--resume", "session-1"])).unwrap();

        assert_eq!(args.resume.as_deref(), Some("session-1"));
        assert!(args.task.is_none());
        assert!(args.auto_approve);
    }

    #[test]
    fn list_sessions_needs_no_task() {
        let args = parse_from(argv(&["--list-sessions"])).unwrap();
        assert!(args.list_sessions);
    }

    #[test]
    fn resume_rejects_a_supplementary_task() {
        let parsed = parse_from(argv(&["--resume", "session-1", "再干一票"]));
        assert!(parsed.is_none(), "恢复 + 新任务会破坏历史配对，必须拒绝");
    }

    #[test]
    fn dangling_switch_values_are_rejected() {
        assert!(parse_from(argv(&["--resume"])).is_none());
        assert!(parse_from(argv(&["--kind"])).is_none());
    }

    #[test]
    fn missing_everything_is_none() {
        assert!(parse_from(std::iter::empty()).is_none());
    }

    #[test]
    fn report_flags_parse_without_a_task() {
        let args = parse_from(argv(&["--report", "--report-dir", "D:/runs", "--json"])).unwrap();

        assert!(args.report);
        assert_eq!(args.report_dir.as_deref(), Some("D:/runs"));
        assert!(args.json);
        assert!(args.task.is_none());
    }

    #[test]
    fn benchmark_flag_needs_no_task() {
        let args = parse_from(argv(&["--benchmark"])).unwrap();
        assert!(args.benchmark);
        assert!(args.task.is_none());
    }

    #[test]
    fn json_without_report_is_rejected() {
        let parsed = parse_from(argv(&["--json"]));
        assert!(parsed.is_none(), "--json 单独使用只会让用户以为生效了");
    }

    #[test]
    fn report_dir_without_report_is_rejected() {
        let parsed = parse_from(argv(&["--report-dir", "x"]));
        assert!(parsed.is_none());
    }

    #[test]
    fn task_with_report_or_benchmark_is_rejected() {
        assert!(parse_from(argv(&["--report", "修个 bug"])).is_none());
        assert!(parse_from(argv(&["--benchmark", "修个 bug"])).is_none());
    }

    #[test]
    fn benchmark_with_resume_is_rejected() {
        assert!(parse_from(argv(&["--benchmark", "--resume", "s1"])).is_none());
    }

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

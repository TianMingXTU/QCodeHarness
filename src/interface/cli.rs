//! 接口层：CLI 参数解析与控制台播报。
//!
//! 播报通过 `EvidenceSink` 输出，不在循环中直接 `println!`。`AgentRunner`
//! 因此不感知控制台；替换前端（Web、IDE）只需更换 `EvidenceSink` 实现，
//! 循环无需修改。

use std::sync::Mutex;

use crate::application::port::{EvidenceSink, Timestamp};
use crate::domain::evidence::AuditEvent;
use crate::domain::tool::ToolCall;
use crate::application::memory_evidence::MemoryEvidenceSink;

/// 观察结果在控制台最多显示的字符数，超出截断；完整内容仍会回填给模型。
const OBSERVE_PREVIEW_CHARS: usize = 400;

/// 用法说明，参数缺失时打印。
///
/// 命令名写产品名而不是 `cargo run --`：发行包里只有可执行文件，
/// 照着 `cargo run --` 抄会抄不动。从源码运行的差异放在末尾一行说明。
pub const USAGE: &str = "\
用法: QCodeHarness [--yes] [--ai-review] [--kind <模板>] \"任务描述\"
      QCodeHarness --list-sessions
      QCodeHarness [--yes] --resume <会话ID>
      QCodeHarness [--yes] [--force-worktree] --resume-task <任务ID>
      QCodeHarness --report [--report-dir <目录>] [--json]
      QCodeHarness --benchmark
      QCodeHarness --benchmark-long
      QCodeHarness --mcp-reconnect <服务器名>
      QCodeHarness --help
示例: QCodeHarness \"阅读 Cargo.toml，说明本项目用了哪些依赖\"
      QCodeHarness --yes \"非交互场景下跑同一个任务\"
      QCodeHarness --ai-review \"让模型先审一遍，拿不准的再问人\"
      QCodeHarness --list-sessions
      QCodeHarness --resume <会话ID>
      QCodeHarness --resume-task <任务ID>
      QCodeHarness --force-worktree --resume-task <任务ID>
      QCodeHarness --report
      QCodeHarness --report --json
      QCodeHarness --benchmark
      QCodeHarness --benchmark-long
      QCodeHarness --mcp-reconnect fetch
从源码运行时在命令前加 cargo run --，例：cargo run -- --report";

/// 解析出的命令行参数。
#[derive(Debug, Clone, PartialEq)]
pub struct CliArgs {
    /// 要完成的任务描述。`--resume` / `--resume-task` 沿用快照中的原任务；
    /// `--list-sessions`、报表、基准与 `--mcp-reconnect` 模式不需要。
    pub task: Option<String>,
    /// 跳过人工审批，所有工具自动放行。
    pub auto_approve: bool,
    /// 启用 AI 审批审查员：模型先判 approve/deny/human；拿不准、调用
    /// 失败、超时或恢复后的重新询问都退回交互式闸门问人。
    pub ai_review: bool,
    /// 要恢复的会话 ID（`--resume <id>`）。
    pub resume: Option<String>,
    /// 要恢复的任务 ID（`--resume-task <id>`）。
    ///
    /// 与 `--resume` 的分工：`--resume` 恢复一个 runner，从会话中断处
    /// 继续；`--resume-task` 恢复一个任务，从步骤边界继续，已完成步骤
    /// 不重做。
    pub resume_task: Option<String>,
    /// 恢复任务时，即使工作区被改动也继续（`--force-worktree`）。
    ///
    /// 工作区被改动时默认拒绝恢复。改动可能与任务无关，也可能是上一个
    /// agent 留下的半成品，只有人能判断。使用该开关表示确认继续。
    pub force_worktree: bool,
    /// 只列出可恢复的会话（`--list-sessions`），然后退出。
    pub list_sessions: bool,
    /// 任务模板名（`--kind <name>`），交给任务编排解析。
    pub kind: Option<String>,
    /// 生成运行报表（`--report`）：读取证据统计成功率、工具结局与成本，
    /// 不需要模型配置。
    pub report: bool,
    /// 报表统计的证据根目录（`--report-dir <dir>`）。在该目录的
    /// `.qcodeharness/sessions/` 下查找证据文件，缺省为当前工作区。
    pub report_dir: Option<String>,
    /// 报表输出 JSON（`--json`），只与 `--report` 搭配有效。
    pub json: bool,
    /// 跑 `benchmarks/` 下的基准任务集并打分（`--benchmark`）。
    pub benchmark: bool,
    /// 运行长任务基准集（`--benchmark-long`）。两段式执行：先耗尽预算
    /// 制造中断，再从任务快照恢复并跑完。
    pub benchmark_long: bool,
    /// 显式重连一个 MCP Server（`--mcp-reconnect <服务器名>`）。
    ///
    /// 独立模式：不运行任务，不需要模型配置。执行一次有界的握手与工具
    /// 发现，将结果写入连接审计后关闭连接退出。任务中途不会自动重试；
    /// 恢复通信需要人明确发起，与"调用超时不自动重放"一致。
    pub mcp_reconnect: Option<String>,
    /// 打印用法后正常退出（`--help` / `-h`）。
    ///
    /// 与其他开关同时出现时仅处理帮助，不运行任务，也不做模式校验。
    pub help: bool,
}

/// 解析命令行参数。
pub fn parse_args() -> Option<CliArgs> {
    parse_from(std::env::args().skip(1))
}

/// 从给定参数列表解析。与 [`parse_args`] 的分工：后者读取真实环境，
/// 本函数为纯函数，便于测试。
///
/// 开关可出现在任意位置。剩余的唯一个位置参数是任务描述。
/// 未引入 `clap`：开关数量很少，不值得增加依赖。
pub fn parse_from<I: IntoIterator<Item = String>>(argv: I) -> Option<CliArgs> {
    let mut auto_approve = false;
    let mut ai_review = false;
    let mut resume = None;
    let mut resume_task = None;
    let mut force_worktree = false;
    let mut list_sessions = false;
    let mut kind = None;
    let mut report = false;
    let mut report_dir = None;
    let mut json = false;
    let mut benchmark = false;
    let mut benchmark_long = false;
    let mut mcp_reconnect = None;
    let mut help = false;
    let mut positional: Vec<String> = Vec::new();

    let mut iter = argv.into_iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--yes" => auto_approve = true,
            "--ai-review" => ai_review = true,
            "--list-sessions" => list_sessions = true,
            "--report" => report = true,
            "--json" => json = true,
            "--benchmark" => benchmark = true,
            "--benchmark-long" => benchmark_long = true,
            "--help" | "-h" => help = true,
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
            "--force-worktree" => force_worktree = true,
            "--resume-task" => match iter.next() {
                Some(id) => resume_task = Some(id),
                None => {
                    eprintln!("--resume-task 需要一个任务 ID：--resume-task <id>");
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
            "--mcp-reconnect" => match iter.next() {
                Some(name) => mcp_reconnect = Some(name),
                None => {
                    eprintln!("--mcp-reconnect 需要一个服务器名：--mcp-reconnect <服务器名>");
                    return None;
                }
            },
            other => positional.push(other.to_owned()),
        }
    }

    // 帮助是纯查询：忽略其余开关与位置参数，也不进入后续模式校验。
    // 若与其他开关同场时报错，会让查看用法变得困难。
    if help {
        return Some(CliArgs {
            task: None,
            auto_approve: false,
            ai_review: false,
            resume: None,
            resume_task: None,
            force_worktree: false,
            list_sessions: false,
            kind: None,
            report: false,
            report_dir: None,
            json: false,
            benchmark: false,
            benchmark_long: false,
            mcp_reconnect: None,
            help: true,
        });
    }

    if positional.len() > 1 {
        eprintln!("任务描述只能是其中一个位置参数，请用引号包住完整描述");
        return None;
    }
    let task = positional.into_iter().next();
    // 报表与基准是不接受任务描述的独立模式。带描述运行会让人误以为描述
    // 生效，因此直接拒绝。
    if task.is_some() && (report || benchmark || benchmark_long) {
        eprintln!("--report / --benchmark / --benchmark-long 不接受任务描述");
        return None;
    }
    // 恢复模式沿用快照中的原任务。追加用户消息会破坏"助手轮次后紧跟
    // 观察"的配对，也无法确定真正的任务。
    if task.is_some() && resume.is_some() {
        eprintln!("--resume 会沿用快照里的原任务，不能再另外指定任务描述");
        return None;
    }
    if task.is_some() && resume_task.is_some() {
        eprintln!("--resume-task 会沿用快照里的原任务，不能再另外指定任务描述");
        return None;
    }
    // 会话级与任务级恢复不能同时使用。二者恢复的对象不同，同时指定时
    // 只有一方生效，用户不会察觉另一项被忽略。
    if resume.is_some() && resume_task.is_some() {
        eprintln!("--resume 与 --resume-task 不能同时使用：一个恢复会话，一个恢复任务");
        return None;
    }
    // 组合无效的开关应立即报错。静默忽略会丢弃用户的意图。
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
    if benchmark && resume_task.is_some() {
        eprintln!("--benchmark 不能与 --resume-task 搭配");
        return None;
    }
    // 两种基准互斥，执行方式不同（长任务基准先耗尽预算再恢复）。同时
    // 指定时只有较快的一项会被注意到。
    if benchmark && benchmark_long {
        eprintln!("--benchmark 与 --benchmark-long 不能同时使用");
        return None;
    }
    if force_worktree && resume_task.is_none() {
        eprintln!("--force-worktree 只对 --resume-task 有意义");
        return None;
    }
    // `--mcp-reconnect` 是独立模式，探测完成后退出。与其他意图（运行
    // 任务、恢复、报表、基准）同时出现时只能执行其一，静默丢弃会忽略
    // 用户指令，因此直接拒绝。
    if mcp_reconnect.is_some()
        && (task.is_some()
            || resume.is_some()
            || resume_task.is_some()
            || kind.is_some()
            || list_sessions
            || report
            || benchmark
            || benchmark_long)
    {
        eprintln!("--mcp-reconnect 是独立模式，不接任务描述，也不与其他模式同场使用");
        return None;
    }
    if task.is_none()
        && resume.is_none()
        && resume_task.is_none()
        && !list_sessions
        && !report
        && !benchmark
        && !benchmark_long
        && mcp_reconnect.is_none()
    {
        eprintln!("缺少任务描述；恢复请用 --resume <id> 或 --resume-task <id>，查看请用 --report，基准请用 --benchmark，长任务基准请用 --benchmark-long，MCP 恢复探测请用 --mcp-reconnect <服务器名>\n\n{USAGE}");
        return None;
    }

    Some(CliArgs {
        task,
        auto_approve,
        ai_review,
        resume,
        resume_task,
        force_worktree,
        list_sessions,
        kind,
        report,
        report_dir,
        json,
        benchmark,
        benchmark_long,
        mcp_reconnect,
        help,
    })
}

/// 将审计事件渲染到控制台。
///
/// 实现 `EvidenceSink` 以接入 runner 已有的事件流。思考、行动、观察
/// 各有事件，播报是其另一种输出方式。
#[derive(Debug)]
pub struct ConsoleReporter {
    /// 配置的最大轮数，只用于轮次分隔线上的总数。
    max_turns: u32,
    /// 已经播报过的轮次号；换轮时补一条分隔线。
    announced_turn: Mutex<u32>,
    /// 同一份事件同时在内存里留痕，供运行结束后汇总或转存。
    trail: MemoryEvidenceSink,
}

/// 进程保护状态后缀。未受保护时给出提示，避免无保护状态被忽略。
fn isolation_note(fact: &crate::domain::mcp::McpLifecycleFact) -> &'static str {
    if fact.process_isolation {
        ""
    } else {
        "（无进程沙箱保护）"
    }
}

/// 故障类别后缀。连接失败或恢复失败时只显示"失败"无法定位问题；启动
/// 进程失败、握手超时与工具发现超时的处理方式不同。
fn fault_note(fact: &crate::domain::mcp::McpLifecycleFact) -> String {
    match fact.fault {
        Some(kind) => format!("（{kind}）"),
        None => String::new(),
    }
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
            AuditEvent::SandboxApplied { platform, active, .. } => {
                // 生效时不播报，这是默认预期；每条命令输出一行会让人跳过
                // 所有输出。未生效必须立即显示，否则沙箱状态不会被检查。
                if !active {
                    println!("[边界] {platform}：本次执行无进程沙箱保护");
                }
            }
            AuditEvent::PolicyEvaluated { verdict, reason, .. } => {
                // 只播报拒绝的裁决。放行与要求审批不提供新信息；拒绝表示
                // 模型被拦截，需要立即显示。
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
                // 历史被裁剪时需要提示。从该轮起模型无法看到完整历史，
                // 早先细节丢失的原因是裁剪，不是模型遗忘。
                if !dropped.is_empty() {
                    println!(
                        "[上下文] 历史已裁剪：{rounds_before} 轮 → {rounds_after} 轮（省略第 {}-{} 轮）",
                        dropped.first().copied().unwrap_or(0),
                        dropped.last().copied().unwrap_or(0),
                    );
                }
            }
            AuditEvent::ContextBudgeted {
                tokens_before,
                tokens_after,
                dropped_tokens,
                ..
            } => {
                // 输出净省下的 token。轮数只表示裁剪次数，token 反映实际成本。
                if *dropped_tokens > 0 {
                    println!(
                        "[上下文] 约省下 {dropped_tokens} token（估算 {tokens_before} → {tokens_after}）",
                    );
                }
            }
            AuditEvent::SummaryGenerated { covers, .. } => {
                // 摘要调用会产生额外开销，此行说明之前的延迟来自生成
                // 历史摘要。
                println!("[摘要] 已生成第 {}-{} 轮的历史摘要", covers.0, covers.1);
            }
            AuditEvent::SummaryFailed { .. } => {
                // 摘要失败只提示，不报错。历史退回占位说明，任务继续执行。
                println!("[摘要] 生成失败，继续用占位说明");
            }
            AuditEvent::ToolCallReplayed { tool, reason, .. } => {
                // 重放提示需要显示。该调用可能已执行，程序不会自动重跑，
                // 而是把决定权交回模型。
                println!("[恢复] 工具 `{tool}` 之前已发起执行，未自动重跑：{reason}");
            }
            AuditEvent::ApprovalReasked { tool, .. } => {
                println!("[恢复] 工具 `{tool}` 的审批重新问了一次（之前问过，没答复）");
            }
            AuditEvent::StepStarted { role, .. } => {
                // 每步开始时输出角色。读者需要知道接下来由谁执行，尤其当
                // 编排串联多个角色时。
                println!("[角色] {} 开始执行", role.name());
            }
            AuditEvent::McpLifecycle { fact, .. } => match fact.kind {
                crate::domain::mcp::McpLifecycleKind::Connected => {
                    println!(
                        "[MCP] `{}` 已连接{}（第 {} 代）",
                        fact.server,
                        isolation_note(fact),
                        fact.generation
                    );
                }
                crate::domain::mcp::McpLifecycleKind::Disconnected => {
                    println!("[MCP] `{}` 连接断开{}", fact.server, isolation_note(fact));
                }
                crate::domain::mcp::McpLifecycleKind::Recovered => {
                    let changed = if fact.tools_changed {
                        "；工具清单有变化"
                    } else {
                        ""
                    };
                    println!("[MCP] `{}` 已恢复{}", fact.server, changed);
                }
                crate::domain::mcp::McpLifecycleKind::RecoveryFailed => {
                    println!("[MCP] `{}` 恢复失败{}", fact.server, fault_note(fact));
                }
                crate::domain::mcp::McpLifecycleKind::ConnectFailed => {
                    println!("[MCP] `{}` 连接失败{}", fact.server, fault_note(fact));
                }
                crate::domain::mcp::McpLifecycleKind::Closed => {
                    println!("[MCP] `{}` 已关闭", fact.server);
                }
            },
            AuditEvent::McpCallFailed { failure, .. } => {
                if failure.result_unknown {
                    // "结果未知"必须显示，因为远端可能已产生副作用。
                    println!(
                        "[MCP] `{}` 调用故障（{}），远端结果未知，未自动重放",
                        failure.server, failure.kind
                    );
                }
            }
            // 审批与 token 用量暂不输出到控制台，避免淹没主要流程。
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

/// 将调用渲染为 `name({...})`，保持既有输出格式。
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

    /// `--help` / `-h` 是用法查询，不能被当成任务描述。
    #[test]
    fn help_is_recognised_instead_of_becoming_a_task() {
        for flag in ["--help", "-h"] {
            let args = parse_from(argv(&[flag])).expect("帮助应解析成功");
            assert!(args.help, "{flag} 应置 help");
            assert!(args.task.is_none(), "{flag} 不能变成任务描述");
        }
    }

    /// 帮助优先于其他开关：不跑任务，也不因组合不合法而报错。
    #[test]
    fn help_wins_over_other_flags() {
        let args = parse_from(argv(&["--json", "--help", "别跑我"])).expect("帮助应解析成功");
        assert!(args.help);
        assert!(args.task.is_none(), "帮助模式不带任务");
        assert!(!args.json, "帮助之外的模式开关不应生效");
    }

    /// `--ai-review` 与 `--yes` 各自独立，也能同时给出。
    ///
    /// 同时给出时 main.rs 采用 `--yes`。用户已声明无需人工干预，再执行
    /// 模型审查只会浪费调用。解析层不替用户决定，只如实传递两个意图。
    #[test]
    fn ai_review_flag_stands_alone_and_combines_with_yes() {
        let alone = parse_from(argv(&["--ai-review", "跑一下测试"])).unwrap();
        assert!(alone.ai_review);
        assert!(!alone.auto_approve);
        assert_eq!(alone.task.as_deref(), Some("跑一下测试"));

        let both = parse_from(argv(&["--ai-review", "--yes", "跑一下测试"])).unwrap();
        assert!(both.ai_review);
        assert!(both.auto_approve);
    }

    /// 默认不开 AI 审查：交互式闸门仍是生产默认。
    #[test]
    fn ai_review_is_off_by_default() {
        let args = parse_from(argv(&["跑一下测试"])).unwrap();
        assert!(!args.ai_review);
    }

    #[test]
    fn resume_takes_an_id_instead_of_a_task() {
        let args = parse_from(argv(&["--yes", "--resume", "session-1"])).unwrap();

        assert_eq!(args.resume.as_deref(), Some("session-1"));
        assert!(args.task.is_none());
        assert!(args.auto_approve);
    }

    /// 任务级恢复有自己的开关。
    #[test]
    fn resume_task_takes_a_task_id() {
        let args = parse_from(argv(&["--yes", "--resume-task", "task-1"])).unwrap();

        assert_eq!(args.resume_task.as_deref(), Some("task-1"));
        assert!(args.resume.is_none());
        assert!(args.task.is_none());
    }

    /// 两种恢复不能同时给：它们恢复的是两样不同的东西。
    #[test]
    fn resume_and_resume_task_are_mutually_exclusive() {
        let parsed = parse_from(argv(&["--resume", "s1", "--resume-task", "t1"]));
        assert!(parsed.is_none(), "同时给两个恢复开关必须拒绝");
    }

    /// 任务级恢复同样沿用快照里的原任务，不接受新描述。
    #[test]
    fn resume_task_rejects_a_supplementary_task() {
        let parsed = parse_from(argv(&["--resume-task", "t1", "再干一票"]));
        assert!(parsed.is_none());
    }

    /// `--force-worktree` 只对任务级恢复有意义。
    ///
    /// 它表示确认工作区已被修改并继续。与其他模式搭配时该开关不会被
    /// 使用，应在解析阶段拒绝。
    #[test]
    fn force_worktree_needs_resume_task() {
        let parsed = parse_from(argv(&["--force-worktree", "--resume", "s1"]));
        assert!(parsed.is_none(), "--force-worktree 单独给出没有意义");

        let parsed = parse_from(argv(&["--force-worktree", "--resume-task", "t1"]));
        assert!(parsed.is_some(), "--force-worktree 与 --resume-task 搭配应当放行");
        let args = parse_from(argv(&["--force-worktree", "--resume-task", "t1"])).unwrap();
        assert!(args.force_worktree);
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

    /// `--mcp-reconnect` 是独立模式：单独可解析，与其他意图同场即拒。
    #[test]
    fn mcp_reconnect_stands_alone_and_rejects_company() {
        let args = parse_from(argv(&["--mcp-reconnect", "demo"])).unwrap();
        assert_eq!(args.mcp_reconnect.as_deref(), Some("demo"));
        assert!(args.task.is_none());

        assert!(
            parse_from(argv(&["--mcp-reconnect", "demo", "修个 bug"])).is_none(),
            "探测模式不接任务描述"
        );
        assert!(parse_from(argv(&["--mcp-reconnect", "demo", "--resume", "s1"])).is_none());
        assert!(parse_from(argv(&["--mcp-reconnect", "demo", "--report"])).is_none());
        assert!(parse_from(argv(&["--mcp-reconnect", "demo", "--benchmark"])).is_none());

        // 缺值必须拒绝，否则下一个参数会被当作任务描述，探测模式变成
        // 运行任务。
        assert!(parse_from(argv(&["--mcp-reconnect"])).is_none());

        // 审批类开关不改变探测语义。允许共存但不生效；直接拒绝反而更糟。
        let yes = parse_from(argv(&["--mcp-reconnect", "demo", "--yes"])).unwrap();
        assert!(yes.auto_approve);
        assert_eq!(yes.mcp_reconnect.as_deref(), Some("demo"));
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
                    execution: None,
                    mcp_failure: None,
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

    /// 连接事件不播报轮次。
    ///
    /// 它没有模型轮次，`turn: 0` 也不是第 0 轮。报表通过
    /// `is_lifecycle_only` 将这类日志识别为连接日志；控制台也不应输出
    /// "第 0/N 轮"分隔线，否则读者会寻找不存在的轮次。
    #[test]
    fn lifecycle_events_announce_no_turn() {
        let reporter = ConsoleReporter::new(20);
        reporter.record(
            Timestamp::UNIX_EPOCH,
            AuditEvent::McpLifecycle {
                turn: 0,
                fact: crate::domain::mcp::McpLifecycleFact {
                    server: "demo".to_owned(),
                    generation: 1,
                    kind: crate::domain::mcp::McpLifecycleKind::Disconnected,
                    fault: Some(crate::domain::mcp::McpFaultKind::Disconnected),
                    call_id: Some(crate::domain::ids::CallId::new("call_1")),
                    process_isolation: false,
                    tools_changed: false,
                },
            },
        );

        assert_eq!(
            *reporter.announced_turn.lock().unwrap(),
            0,
            "连接事件不能把轮次推进到 0 之后"
        );
        // 事件本身仍留在留痕里，供事后汇总或转存。
        let events = reporter.trail.events();
        assert_eq!(events.len(), 1);
        assert!(matches!(
            events[0].event,
            AuditEvent::McpLifecycle { turn: 0, .. }
        ));
    }

    /// 一次调用故障只播一条"结果未知"，不播成功。
    #[test]
    fn call_failure_with_unknown_result_is_announced_on_its_own_turn() {
        let reporter = ConsoleReporter::new(20);
        reporter.record(
            Timestamp::UNIX_EPOCH,
            AuditEvent::McpCallFailed {
                turn: 3,
                call_id: crate::domain::ids::CallId::new("call_9"),
                tool: "mcp__demo__send_message".to_owned(),
                failure: crate::domain::mcp::McpCallFailure {
                    server: "demo".to_owned(),
                    generation: 2,
                    kind: crate::domain::mcp::McpFaultKind::Disconnected,
                    result_unknown: true,
                    process_isolation: true,
                },
            },
        );

        assert_eq!(*reporter.announced_turn.lock().unwrap(), 3);
        assert_eq!(reporter.trail.events().len(), 1);
    }

    /// 两个提示后缀都只使用执行层提供的事实，不解析输出文本。
    ///
    /// 故障类别决定读者检查启动参数、握手还是 Server 的 `list_tools`。
    /// 只显示"失败"会让人无从判断。
    #[test]
    fn mcp_note_suffixes_come_from_structured_facts() {
        let fact = crate::domain::mcp::McpLifecycleFact {
            server: "demo".to_owned(),
            generation: 1,
            kind: crate::domain::mcp::McpLifecycleKind::ConnectFailed,
            fault: Some(crate::domain::mcp::McpFaultKind::Spawn),
            call_id: None,
            process_isolation: false,
            tools_changed: false,
        };
        assert_eq!(fault_note(&fact), "（启动进程失败）");
        assert_eq!(isolation_note(&fact), "（无进程沙箱保护）");

        let protected = crate::domain::mcp::McpLifecycleFact {
            fault: None,
            process_isolation: true,
            ..fact.clone()
        };
        assert_eq!(fault_note(&protected), "", "没有故障事实就不编一个类别");
        assert_eq!(isolation_note(&protected), "");
    }
}

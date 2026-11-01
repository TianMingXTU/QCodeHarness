//! ReAct（Reason + Act）循环的入口：薄壳。
//!
//! 分工：
//! - rig 负责模型协议适配（OpenAI 兼容的 `/chat/completions`）
//!   与工具定义的序列化，封装在 `infrastructure::model`；
//! - ReAct 循环本身在 `application::agent_run::AgentRunner`；
//! - 本文件只做三件事：读环境变量、把各层实现组装起来、把结果打印出去。
//!
//! 全部逻辑在库目标（`src/lib.rs`）里，本文件只消费它——
//! 集成测试因此能用与生产完全相同的装配路径。

use std::error::Error;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use rig::providers::openai::OpenAIConfig;
use qcode_harness::application::agent_run::{
    new_session_id, AgentDependencies, AgentRunner, RunState,
};
use qcode_harness::application::error::ApplicationError;
use qcode_harness::application::orchestrator::{EvidenceFactory, Orchestrator};
use qcode_harness::application::port::{ApprovalGate, EvidenceSink};
use qcode_harness::application::snapshot::{RunSnapshot, SessionStore};
use qcode_harness::application::tool_registry::ToolRegistry;
use qcode_harness::domain::agent::{AgentRole, AgentSpec};
use qcode_harness::domain::budget::Budget;
use qcode_harness::domain::context::ContextBudget;
use qcode_harness::domain::ids::{SessionId, TaskId};
use qcode_harness::domain::session::Turn;
use qcode_harness::domain::task::TaskKind;
use qcode_harness::domain::workspace::WorkspaceRoot;
use qcode_harness::infrastructure::approval::auto::AutoApproveGate;
use qcode_harness::infrastructure::approval::cli::CliApprovalGate;
use qcode_harness::infrastructure::clock::SystemClock;
use qcode_harness::infrastructure::evidence::jsonl::{session_path, JsonlEvidenceSink};
use qcode_harness::infrastructure::evidence::memory::MemoryEvidenceSink;
use qcode_harness::infrastructure::model::rig_chat::RigChatGateway;
use qcode_harness::infrastructure::policy;
use qcode_harness::infrastructure::session_store::JsonSessionStore;
use qcode_harness::infrastructure::tool::all_tools;
use qcode_harness::interface::cli::{self, ConsoleReporter};

/// `LLM_MAX_TURNS` 未设置时的默认最大轮数。
const DEFAULT_MAX_TURNS: u32 = 20;

/// 系统提示词：向模型说明 ReAct 工作方式与约束。
const SYSTEM_PROMPT: &str = "\
你是一个运行在受控 workspace 中的编码助手，按照 ReAct 循环完成任务：
1. 思考：简述当前掌握的信息与下一步计划；
2. 行动：调用提供的工具执行一步操作，避免一次做多余的事；
3. 观察：Harness 会把工具执行结果回填给你，基于结果继续思考。

约束：
- 文件操作只能使用 workspace 内的相对路径；
- 观察到错误时如实分析原因，不要假设操作成功；
- 任务完成、或确认无法完成时，停止调用工具，直接输出最终答案；
- 最终答案要包含：结论、依据（引用你读到的内容或执行结果）、遗留风险。";

/// 运行所需的模型服务配置，来自 `.env` 或进程环境变量。
#[derive(Debug, Clone)]
struct LlmConfig {
    /// OpenAI 兼容服务的根地址，例如 `https://api.deepseek.com/v1`。
    base_url: String,
    /// 服务的 API Key。
    api_key: String,
    /// 模型名称，例如 `deepseek-chat`。
    model: String,
    /// ReAct 循环的最大轮数，防止模型无限调用工具。
    max_turns: u32,
    /// 策略配置文件路径。未设置时用内置默认策略。
    policy_path: Option<PathBuf>,
    /// 是否跳过人工审批。`--yes` 或 `QCH_AUTO_APPROVE=1` 时为真。
    auto_approve: bool,
    /// 墙钟上限（秒）。编排模式下每步的会话各自计时（单步上限）；
    /// `--resume` 时是本次恢复运行的上限。未设置时不限时。
    max_wall_clock: Option<Duration>,
    /// 被裁历史累积到多少轮才生成一次 LLM 摘要。未设置时不做摘要
    /// （裁剪退回占位说明，P3 行为）。
    summarize_min_dropped: Option<usize>,
}

impl LlmConfig {
    /// 从环境变量读取配置。
    ///
    /// 必需变量：`LLM_BASE_URL`、`LLM_API_KEY`、`LLM_MODEL`；
    /// 可选变量：`LLM_MAX_TURNS`（默认 20）、`QCH_POLICY`（策略配置路径）、
    /// `QCH_AUTO_APPROVE`（设为 `1` 时跳过人工审批）、
    /// `QCH_MAX_WALL_CLOCK`（墙钟上限，秒；编排模式下为单步上限）。
    ///
    /// # Errors
    ///
    /// 缺少必需变量、或数值变量不是合法数字时返回说明性错误。
    fn from_env() -> Result<Self, String> {
        let required = |name: &str| -> Result<String, String> {
            std::env::var(name)
                .map_err(|_| format!("缺少环境变量 {name}，请参考 .env.example 创建 .env"))
        };
        let max_turns = match std::env::var("LLM_MAX_TURNS") {
            Ok(text) => text
                .parse()
                .map_err(|_| format!("LLM_MAX_TURNS 不是合法整数: {text}"))?,
            Err(_) => DEFAULT_MAX_TURNS,
        };
        let max_wall_clock = match std::env::var("QCH_MAX_WALL_CLOCK") {
            Ok(text) => {
                let seconds: u64 = text
                    .parse()
                    .map_err(|_| format!("QCH_MAX_WALL_CLOCK 不是合法秒数: {text}"))?;
                Some(Duration::from_secs(seconds))
            }
            Err(_) => None,
        };
        let summarize_min_dropped = match std::env::var("QCH_SUMMARIZE_MIN_DROPPED") {
            Ok(text) => {
                let count: usize = text
                    .parse()
                    .map_err(|_| format!("QCH_SUMMARIZE_MIN_DROPPED 不是合法整数: {text}"))?;
                Some(count.max(1))
            }
            Err(_) => None,
        };
        Ok(Self {
            base_url: required("LLM_BASE_URL")?,
            api_key: required("LLM_API_KEY")?,
            model: required("LLM_MODEL")?,
            max_turns,
            policy_path: std::env::var_os("QCH_POLICY").map(PathBuf::from),
            auto_approve: std::env::var("QCH_AUTO_APPROVE").as_deref() == Ok("1"),
            max_wall_clock,
            summarize_min_dropped,
        })
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    // .env 不存在时静默跳过，回退到进程环境变量。
    dotenvy::dotenv().ok();

    let Some(args) = cli::parse_args() else {
        eprintln!("{}", cli::USAGE);
        std::process::exit(2);
    };

    let workspace = std::env::current_dir()?;
    let store = Arc::new(JsonSessionStore::new(&workspace));

    // 列清单不需要模型配置：没配 API key 也应该能看有哪些会话可恢复。
    if args.list_sessions {
        return list_sessions(store.as_ref());
    }

    let config = LlmConfig::from_env()?;

    // 命令行开关优先于环境变量：显式敲 --yes 的意图比配置文件里的默认值更强。
    let auto_approve = args.auto_approve || config.auto_approve;

    // 策略配置读失败要让进程停下来：一个没人看懂的策略文件比没有策略更危险。
    let policy = policy::load(config.policy_path.as_deref())
        .map_err(|error| format!("策略配置加载失败: {error}"))?;

    // OpenAI 兼容端点必须走 chat()（/chat/completions）。
    // 不要用 completion()：它对 OpenAI 方言默认指向 Responses API（/responses），
    // DeepSeek 等兼容网关并不支持。
    let provider = OpenAIConfig::new(config.api_key.as_str())
        .with_base_url(config.base_url.as_str())
        .client();
    let model = provider.chat(config.model.as_str());

    // 工具注册表：组装入口一次给出全部内置工具，规格与执行入口已绑定。
    let registry = ToolRegistry::new(all_tools(WorkspaceRoot::new(workspace.clone())));

    let approvals: Arc<dyn ApprovalGate> = if auto_approve {
        Arc::new(AutoApproveGate)
    } else {
        Arc::new(CliApprovalGate::new())
    };
    let clock = Arc::new(SystemClock);
    let reporter = Arc::new(ConsoleReporter::new(config.max_turns));

    let mut budget = Budget::new(config.max_turns);
    if let Some(limit) = config.max_wall_clock {
        budget = budget.with_wall_clock(limit);
    }

    // 上下文预算：默认只做确定性裁剪（占位说明）；设了阈值才为被裁
    // 历史花摘要调用——摘要是多花一次模型调用的可选项，默认关。
    let context_budget = config
        .summarize_min_dropped
        .map_or(ContextBudget::default(), |min| {
            ContextBudget::default().with_summarize(min)
        });

    // ── 恢复模式：任务、系统提示词与历史都来自快照，不重新拼装。──
    if let Some(id_text) = args.resume {
        let session_id = SessionId::new(id_text);
        let Some(snapshot) = store
            .load(&session_id)
            .map_err(|error| format!("读取快照失败: {error}"))?
        else {
            eprintln!(
                "没有找到会话 {} 的快照；用 --list-sessions 查看可恢复的会话",
                session_id.as_str()
            );
            std::process::exit(1);
        };
        // 证据追加到同一份文件：恢复的运行是同一会话的延续，
        // 复盘时一个 ID 看到全貌。
        let evidence_path = session_path(&workspace, session_id.as_str());
        let evidence = fanout_evidence(&evidence_path, reporter.clone())?;
        println!(
            "恢复会话 {}（已 {} 轮，停在: {}）",
            session_id.as_str(),
            rounds_of(&snapshot),
            state_label(&snapshot.state),
        );
        let runner = match AgentRunner::resume(
            snapshot,
            AgentDependencies {
                model: Arc::new(RigChatGateway::new(model.erase())),
                tools: registry,
                approvals,
                evidence,
                clock,
                policy,
                context_budget,
                session_store: Some(store.clone()),
            },
        ) {
            Ok(runner) => runner,
            Err(error) => {
                eprintln!("{error}");
                std::process::exit(1);
            }
        };
        run_and_report(runner, &budget, &evidence_path).await;
        return Ok(());
    }

    let task = args.task.expect("parse_args 保证：非 list/resume 模式必有任务描述");

    // --kind 选模板；不传默认 bug-fix（最常见）。
    let kind_text = args.kind.as_deref().unwrap_or(TaskKind::BugFix.name());
    let Some(kind) = TaskKind::parse_name(kind_text) else {
        eprintln!("未知的任务类型 {kind_text}；可用: bug-fix / feature / review / refactor");
        std::process::exit(2);
    };

    let spec = AgentSpec {
        role: AgentRole::Coder,
        system_prompt: SYSTEM_PROMPT.to_owned(),
    };

    println!("任务: {task}");
    println!("类型: {} 模板", kind.name());
    println!("模型: {} @ {}", config.model, config.base_url);
    println!(
        "工具: {}",
        registry
            .specs()
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );
    if auto_approve {
        println!("审批: 自动放行（QCH_AUTO_APPROVE 或 --yes）");
    } else {
        println!("审批: 交互式（每次副作用工具执行前询问）");
    }

    // 每步一份证据：独立的 JSONL 文件（与会话 ID 同名）+ 一个新的控制台
    // 播报器（轮次号每步从头计，复用旧播报器会丢步骤 2-4 的分隔线）。
    let evidence_factory: EvidenceFactory = {
        let workspace = workspace.clone();
        Arc::new(move |session_id: &SessionId| {
            let path = session_path(&workspace, session_id.as_str());
            let jsonl = JsonlEvidenceSink::open(&path)?;
            Ok(Arc::new(FanoutEvidenceSink::new(vec![
                Arc::new(jsonl),
                Arc::new(ConsoleReporter::new(config.max_turns)) as Arc<dyn EvidenceSink>,
            ])) as Arc<dyn EvidenceSink>)
        })
    };

    // 任务 ID 复用会话 ID 的生成策略：唯一、可读，证据/快照同名可互查。
    let task_id = TaskId::new(new_session_id(&*clock).as_str());

    let deps = AgentDependencies {
        model: Arc::new(RigChatGateway::new(model.erase())),
        tools: registry,
        approvals,
        clock,
        policy,
        context_budget,
        // 编排器每步用工厂另造证据汇，这个占位不会被步骤用到；
        // deps 的形状要求它有值。
        evidence: Arc::new(MemoryEvidenceSink::new()),
        // 快照落盘随运行自动开启；--resume 可从任一步的会话恢复。
        session_store: Some(store.clone()),
    };
    let mut orchestrator = Orchestrator::new(deps, budget, spec, evidence_factory);
    let mut task = orchestrator.create_task(task_id, kind, &task);

    let outcome = match orchestrator.run(&mut task).await {
        Ok(outcome) => outcome,
        Err(error) => {
            eprintln!("{error}");
            // 失败时刻把已完成步骤的会话 ID 给出来：证据文件按会话 ID
            // 命名，用户拿到 ID 就能翻证据或 --resume，不用再翻目录。
            if let ApplicationError::StepFailed { step_sessions, .. } = &error {
                if !step_sessions.is_empty() {
                    eprintln!(
                        "已完成步骤的会话: {}",
                        step_sessions
                            .iter()
                            .map(|id| id.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                }
            }
            std::process::exit(1);
        }
    };

    println!("\n──── 最终答案 ────\n{}", outcome.answer);
    let (done, total) = task.progress();
    println!(
        "\n（任务 {} · {}/{} 步 · 步骤会话: {}）",
        task.id(),
        done,
        total,
        outcome
            .step_sessions
            .iter()
            .map(|id| id.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );
    Ok(())
}

/// `--list-sessions`：列出可恢复的会话与各自停在哪里。
fn list_sessions(store: &dyn qcode_harness::application::snapshot::SessionStore) -> Result<(), Box<dyn Error>> {
    let ids = store.list()?;
    if ids.is_empty() {
        println!("还没有可恢复的会话。跑完一个任务后，快照会自动存到 .qcodeharness/snapshots/。");
        return Ok(());
    }
    println!("可恢复的会话（共 {} 个）：", ids.len());
    for id in ids {
        match store.load(&id) {
            Ok(Some(snapshot)) => {
                println!(
                    "  {}  已 {} 轮  停在: {}",
                    id.as_str(),
                    rounds_of(&snapshot),
                    state_label(&snapshot.state),
                );
            }
            Ok(None) => {}
            // 单个坏文件不拖垮整个清单：标注后继续列其他的。
            Err(error) => println!("  {}  （快照不可读: {error}）", id.as_str()),
        }
    }
    Ok(())
}

/// 快照历史里已提交的轮数：Agent 消息条数即轮数（与 `Session::rounds` 同口径）。
fn rounds_of(snapshot: &RunSnapshot) -> u32 {
    snapshot
        .session
        .turns
        .iter()
        .filter(|turn| matches!(turn, Turn::Agent(_)))
        .count() as u32
}

/// 状态机位置的人话标签，给 `--list-sessions` 与 `--resume` 的提示用。
fn state_label(state: &RunState) -> &'static str {
    match state {
        RunState::AwaitingModel => "等模型回复",
        RunState::Evaluating(_) => "评估工具调用",
        RunState::AwaitingApproval(_) => "等待人工审批",
        RunState::Dispatching(_) => "执行工具中",
        RunState::Completed(_) => "已完成",
    }
}

/// 组装证据扇出：控制台播报给人看，JSONL 落盘给事后查。
/// 新任务与恢复两条入口共用。
fn fanout_evidence(
    evidence_path: &Path,
    reporter: Arc<ConsoleReporter>,
) -> std::io::Result<Arc<dyn EvidenceSink>> {
    let jsonl = JsonlEvidenceSink::open(evidence_path)?;
    Ok(Arc::new(FanoutEvidenceSink::new(vec![
        Arc::new(jsonl),
        reporter as Arc<dyn EvidenceSink>,
    ])))
}

/// 跑到底并汇报结果。新任务与恢复两条入口共用的尾巴。
async fn run_and_report(mut runner: AgentRunner, budget: &Budget, evidence_path: &Path) {
    // 错误要按 Display 打印而不是交给 Box<dyn Error> 的 Debug：错误消息里的
    // 对策指引（"调大 LLM_MAX_TURNS"这类）是写给用户看的，Debug 格式会把
    // 它变成一个结构体转储。
    let outcome = match runner.run(budget).await {
        Ok(outcome) => outcome,
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1);
        }
    };
    println!("\n──── 最终答案 ────\n{}", outcome.answer);
    println!(
        "\n（会话 {} · 共 {} 轮 · 证据已写入 {}）",
        runner.session_id().as_str(),
        outcome.turns_used,
        evidence_path.display()
    );
}

/// 把同一条事件分发给多个证据汇。
///
/// 一个坏汇不能连累其他汇：JSONL 自身已经会降级（打警告后丢弃），
/// 这里再包一层是防止将来某个汇 panic 把整个循环带崩。
struct FanoutEvidenceSink {
    sinks: Vec<Arc<dyn EvidenceSink>>,
}

impl FanoutEvidenceSink {
    /// 用一组汇构造分发器。
    fn new(sinks: Vec<Arc<dyn EvidenceSink>>) -> Self {
        Self { sinks }
    }
}

impl EvidenceSink for FanoutEvidenceSink {
    fn record(&self, at: qcode_harness::application::port::Timestamp, event: qcode_harness::domain::evidence::AuditEvent) {
        for sink in &self.sinks {
            // catch_unwind 不能跨 await，但 record 是同步的，可以包住。
            // 用一个汇的 panic 不该让任务和其余证据一起陪葬。
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                sink.record(at, event.clone());
            }));
            if result.is_err() {
                eprintln!("警告：某个证据汇处理事件时 panic，已跳过该汇");
            }
        }
    }
}

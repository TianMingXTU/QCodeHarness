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

use std::collections::HashMap;
use std::error::Error;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use rig::providers::openai::OpenAIConfig;
use qcode_harness::application::agent_run::{
    new_session_id, AgentDependencies, AgentRunner, RunState,
};
use qcode_harness::application::agent_roster::AgentRoster;
use qcode_harness::application::benchmark::{run_benchmark, run_long_benchmark};
use qcode_harness::application::error::ApplicationError;
use qcode_harness::application::mcp_lifecycle::McpLifecycleRecorder;
use qcode_harness::application::orchestrator::{EvidenceFactory, Orchestrator};
use qcode_harness::application::port::{ApprovalGate, EvidenceSink};
use qcode_harness::application::report::{render_json, render_text, BenchmarkReport};
use qcode_harness::application::snapshot::{RunSnapshot, SessionStore, TaskStore};
use qcode_harness::application::task_resume::{self, ResumeCheck, WorktreeChangePolicy};
use qcode_harness::application::tool_registry::ToolRegistry;
use qcode_harness::domain::agent::{AgentRole, Effort, Verbosity};
use qcode_harness::domain::budget::Budget;
use qcode_harness::domain::context::ContextBudget;
use qcode_harness::domain::ids::{SessionId, TaskId};
use qcode_harness::domain::session::Turn;
use qcode_harness::domain::task::TaskKind;
use qcode_harness::domain::workspace::WorkspaceRoot;
use qcode_harness::infrastructure::approval::ai_review::AiReviewGate;
use qcode_harness::infrastructure::approval::auto::AutoApproveGate;
use qcode_harness::infrastructure::approval::cli::CliApprovalGate;
use qcode_harness::infrastructure::clock::SystemClock;
use qcode_harness::infrastructure::evidence::jsonl::{session_path, JsonlEvidenceSink};
use qcode_harness::infrastructure::evidence::memory::MemoryEvidenceSink;
use qcode_harness::infrastructure::evidence::reader;
use qcode_harness::infrastructure::mcp;
use qcode_harness::infrastructure::mcp::config::McpConfig;
use qcode_harness::infrastructure::model::rig_chat::RigChatGateway;
use qcode_harness::infrastructure::policy;
use qcode_harness::infrastructure::session_store::JsonSessionStore;
use qcode_harness::infrastructure::task_store::JsonTaskStore;
use qcode_harness::infrastructure::tool::all_tools;
use qcode_harness::infrastructure::worktree::RootedWorktreeScanner;
use qcode_harness::interface::cli::{self, ConsoleReporter};
use qcode_harness::interface::instructions;

/// `LLM_MAX_TURNS` 未设置时的默认最大轮数。
const DEFAULT_MAX_TURNS: u32 = 20;

// 系统提示词随角色分发：P5 起各角色的提示词在 `AgentProfile::builtin()`
// 里（按角色提供"能做什么、不许做什么"），这里不再有全局提示词。

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
    /// 墙钟上限（秒）。P9-3 起一个任务一个会话，编排模式下它就是
    /// 整个任务的上限；`--resume` 时是本次恢复运行的上限。
    /// 未设置时不限时。
    max_wall_clock: Option<Duration>,
    /// 被裁历史累积到多少轮才生成一次 LLM 摘要。未设置时不做摘要
    /// （裁剪退回占位说明，P3 行为）。
    summarize_min_dropped: Option<usize>,
    /// 各角色专属的模型名。未设置的角色用默认模型。
    role_models: HashMap<AgentRole, String>,
    /// 各角色专属的推理档位。未设置的档位用 provider 默认。
    role_efforts: HashMap<AgentRole, Effort>,
    /// 各角色专属的输出详略档位。未设置的档位用 provider 默认。
    role_verbosities: HashMap<AgentRole, Verbosity>,
}

impl LlmConfig {
    /// 从环境变量读取配置。
    ///
    /// 必需变量：`LLM_BASE_URL`、`LLM_API_KEY`、`LLM_MODEL`；
    /// 可选变量：`LLM_MAX_TURNS`（默认 20）、`QCH_POLICY`（策略配置路径）、
    /// `QCH_AUTO_APPROVE`（设为 `1` 时跳过人工审批）、
    /// `QCH_MAX_WALL_CLOCK`（墙钟上限，秒；编排模式下为整个任务的上限）。
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
            role_models: read_role_models(),
            role_efforts: read_role_efforts()?,
            role_verbosities: read_role_verbosities()?,
        })
    }
}

/// 读各角色专属的模型名。
///
/// 变量名按 `LLM_MODEL_<角色>` 的规律拼（见
/// [`AgentRole::model_env_name`]）。没设置的角色不进表——编排器据此
/// 用运行时的默认模型，行为与 P10 逐字一致。
fn read_role_models() -> HashMap<AgentRole, String> {
    read_role_models_from(|name| std::env::var(name).ok())
}

/// 从任意取值源读各角色专属的模型名。
///
/// 逻辑与读环境变量分开，是为了能测：进程环境变量是全局状态，测试
/// 里改它会与并行跑的其他测试互相污染。取值源做成参数，测试就能喂
/// 一张假的变量表。
fn read_role_models_from(lookup: impl Fn(&str) -> Option<String>) -> HashMap<AgentRole, String> {
    let mut models = HashMap::new();
    for role in AgentRole::all() {
        if let Some(name) = lookup(role.model_env_name()) {
            let name = name.trim().to_owned();
            // 空串当作没配：环境变量存在但留空（`LLM_MODEL_CODER= `）是
            // 很常见的写法，把它当成一个叫空串的模型会让请求打到一个
            // 不存在的模型上。
            if !name.is_empty() {
                models.insert(role, name);
            }
        }
    }
    models
}

/// 读各角色专属的推理档位。
///
/// 档位写错必须让进程停下来：一个静默失效的档位配置等于"配了但没生效"，
/// 用户会以为强推理已经开上了。
fn read_role_efforts() -> Result<HashMap<AgentRole, Effort>, String> {
    read_role_efforts_from(|name| std::env::var(name).ok())
}

/// 从任意取值源读各角色专属的推理档位。
fn read_role_efforts_from(
    lookup: impl Fn(&str) -> Option<String>,
) -> Result<HashMap<AgentRole, Effort>, String> {
    let mut efforts = HashMap::new();
    for role in AgentRole::all() {
        if let Some(text) = lookup(role.effort_env_name()) {
            let effort = Effort::parse(&text).map_err(|error| {
                format!("{}: {error}", role.effort_env_name())
            })?;
            efforts.insert(role, effort);
        }
    }
    Ok(efforts)
}

/// 读各角色专属的输出详略档位。
fn read_role_verbosities() -> Result<HashMap<AgentRole, Verbosity>, String> {
    read_role_verbosities_from(|name| std::env::var(name).ok())
}

/// 从任意取值源读各角色专属的输出详略档位。
fn read_role_verbosities_from(
    lookup: impl Fn(&str) -> Option<String>,
) -> Result<HashMap<AgentRole, Verbosity>, String> {
    let mut verbosities = HashMap::new();
    for role in AgentRole::all() {
        if let Some(text) = lookup(role.verbosity_env_name()) {
            let verbosity = Verbosity::parse(&text).map_err(|error| {
                format!("{}: {error}", role.verbosity_env_name())
            })?;
            verbosities.insert(role, verbosity);
        }
    }
    Ok(verbosities)
}

/// 按配置装配内置花名册：给每个角色带上自己的模型与档位。
///
/// **模型与档位只影响"用哪个模型"，不影响"能干什么"。** 工具白名单
/// 在 [`AgentRoster::builtin`] 里定死，这里不碰——给 Tester 配一个更
/// 强的模型不等于让它能改文件。
///
/// 没配到的角色保持 `None`，跑起来的请求与 P10 逐字一致（`model` 不
/// 出现在请求里，档位由 provider 取默认）。
fn roster_with_role_models(config: &LlmConfig) -> AgentRoster {
    AgentRoster::builtin().with_role_models(
        &config.role_models,
        &config.role_efforts,
        &config.role_verbosities,
    )
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

    // 报表同样不需要模型配置：只读证据统计，没配 API key 也能出报表。
    if args.report {
        return render_report(&workspace, args.report_dir.as_deref(), args.json);
    }

    // 恢复探测同样不需要模型配置：它只回答"这个 MCP Server 现在能不能
    // 说上话"，不跑任务、不碰审批、不读会话历史。
    if let Some(server) = &args.mcp_reconnect {
        return mcp_probe(&workspace, server).await;
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

    // 上下文预算：默认只做确定性裁剪（占位说明）；设了阈值才为被裁
    // 历史花摘要调用——摘要是多花一次模型调用的可选项，默认关。
    // 基准与普通运行共用同一套预算口径。
    let context_budget = config
        .summarize_min_dropped
        .map_or(ContextBudget::default(), |min| {
            ContextBudget::default().with_summarize(min)
        });

    // ── 基准模式：跑 benchmarks/ 下的任务集并打分。放在全局工具装配
    // 之前——运行器为每个任务重建以 fixture 拷贝为根的注册表，全局
    // 注册表与 MCP 连接在这里用不上。──
    if args.benchmark || args.benchmark_long {
        // 基准无人值守：交互式审批会把整场运行挂住，强制自动放行。
        let deps = AgentDependencies {
            model: Arc::new(RigChatGateway::new(model.erase())),
            tools: ToolRegistry::new(vec![]),
            approvals: Arc::new(AutoApproveGate),
            clock: Arc::new(SystemClock),
            policy,
            roster: roster_with_role_models(&config),
            context_budget,
            evidence: Arc::new(MemoryEvidenceSink::new()),
            // 短任务基准不留快照：恢复它们只会把任务跑回当前工作区，
            // 而不是当时的 fixture 拷贝。证据照常落盘，--report 统计得到。
            // 长任务基准的运行器会在每段装配自己的快照存储（恢复是
            // 它流程的一部分），这里的 None 只是装配模板的占位。
            session_store: None,
        };
        let mut budget = Budget::new(config.max_turns);
        if let Some(limit) = config.max_wall_clock {
            budget = budget.with_wall_clock(limit);
        }
        let benchmarks_root = workspace.join("benchmarks");
        println!("基准任务集: {}", benchmarks_root.display());
        println!("模型: {} @ {}", config.model, config.base_url);
        if args.benchmark_long {
            let results =
                run_long_benchmark(&deps, &budget, &benchmarks_root, &workspace).await?;
            let passed = results.iter().filter(|result| result.passed).count();
            println!("\n通过 {passed}/{} 个长任务", results.len());
            if passed < results.len() {
                std::process::exit(1);
            }
            return Ok(());
        }
        let results = run_benchmark(&deps, &budget, &benchmarks_root, &workspace).await?;
        let passed = results.iter().filter(|result| result.passed).count();
        println!("\n通过 {passed}/{} 个任务", results.len());
        if passed < results.len() {
            // 基准是打分工具：有任务未通过就用非 0 退出码说话，
            // 自动化流水线才能感知。
            std::process::exit(1);
        }
        return Ok(());
    }

    // 工具注册表：组装入口一次给出全部内置工具，规格与执行入口已绑定。
    // MCP 工具随后并入：配置了 QCH_MCP_CONFIG 才连接，缺文件即 MCP 关闭。
    let workspace_root = WorkspaceRoot::new(workspace.clone());
    let mut tool_entries = all_tools(workspace_root);
    // 连接归拢到这里，三条正常结束路径都要显式关闭它们（见
    // close_mcp_connections）；未配置 MCP 时就是空表。
    let mut mcp_sources: Vec<mcp::McpToolSource> = Vec::new();
    if let Some(config_path) = std::env::var_os("QCH_MCP_CONFIG").map(PathBuf::from) {
        // 配置文件存在但非法要让进程停下来：用户明确想接 MCP，静默跳过
        // 会让模型拿着残缺的工具清单跑任务（与策略配置同一哲学）。
        let mcp_config = McpConfig::load(&config_path)
            .map_err(|error| format!("{error}（文件: {}）", config_path.display()))?;
        if let Some(mcp_config) = mcp_config {
            let recorder = mcp_lifecycle_recorder(&workspace);
            let (sources, warnings) = mcp::connect_all(&mcp_config, recorder).await;
            for warning in &warnings {
                eprintln!("警告：{warning}");
            }
            for source in &sources {
                println!(
                    "MCP: server `{}` 提供了 {} 个工具",
                    source.server(),
                    source.tool_count()
                );
                tool_entries.extend(source.entries());
            }
            mcp_sources = sources;
        }
    }
    let registry = ToolRegistry::new(tool_entries);

    // 审批闸门三选一，优先级从高到低：
    //   --yes                恒放行（基准与脚本化场景）
    //   --ai-review          模型先审，拿不准的退回交互式闸门
    //   默认                 交互式闸门
    // --yes 与 --ai-review 同时给出时走 --yes：用户已经声明"不用人管"，
    // 再插一个模型审查只是白花一次调用。
    let approvals: Arc<dyn ApprovalGate> = if auto_approve {
        Arc::new(AutoApproveGate)
    } else if args.ai_review {
        // 审查员的退路必须是交互式闸门：模型说"拿不准"时得有人接。
        // 与主循环共用同一个模型实例——审查就是一次普通的模型调用。
        // clone 再 erase：同一个模型实例后面主循环还要用，erase() 会移走它。
        Arc::new(AiReviewGate::new(
            Box::new(RigChatGateway::new(model.clone().erase())),
            Box::new(CliApprovalGate::new()),
        ))
    } else {
        Arc::new(CliApprovalGate::new())
    };
    let clock = Arc::new(SystemClock);
    let reporter = Arc::new(ConsoleReporter::new(config.max_turns));

    let mut budget = Budget::new(config.max_turns);
    if let Some(limit) = config.max_wall_clock {
        budget = budget.with_wall_clock(limit);
    }

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
        // 恢复的会话属于某个角色，工具集必须按同一份档案裁剪——
        // 白名单是硬边界，编排与恢复两条路径共用一条滤网（AgentRoster）。
        // 不裁的话，中断的只读步骤恢复后会带着全量注册表上场，
        // 提示词说"你没有写能力"，模型手里却握着 write_file。
        // 工具给全量：P9-3 起模型看到的工具定义对所有步骤统一（前缀缓存
        // 才命得中），角色边界由 runner 按花名册在调用层校验——恢复路径
        // 与编排路径走的是同一道校验，不因入口不同而松动。
        let runner = match AgentRunner::resume(
            snapshot,
            AgentDependencies {
                model: Arc::new(RigChatGateway::new(model.erase())),
                tools: registry,
                approvals,
                evidence,
                clock,
                policy,
                roster: roster_with_role_models(&config),
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
        close_mcp_connections(&mcp_sources).await;
        return Ok(());
    }

    // ── 任务级恢复：从步骤边界接着走，前面的步骤不重做。──
    if let Some(id_text) = args.resume_task {
        let task_id = TaskId::new(id_text);
        let task_store = JsonTaskStore::new(&workspace);
        let Some(snapshot) = task_store
            .load(&task_id)
            .map_err(|error| format!("读取任务快照失败: {error}"))?
        else {
            eprintln!(
                "没有找到任务 {} 的快照；任务 ID 在运行失败时打印过",
                task_id.as_str()
            );
            std::process::exit(1);
        };
        println!(
            "恢复任务 {}（已完成 {}/{} 步）",
            task_id.as_str(),
            snapshot.task.current_index(),
            snapshot.task.steps().len(),
        );
        // 工作区检查由编排器在恢复前做（快照里存着"当时的样子"）。
        // CLI 这边先把差异打出来，让人看得见到底改了什么——
        // 编排器默认拒绝恢复，用户看完可以加 --force-worktree 放行。
        //
        // 快照里没有清单时不比对：空清单 vs 当前清单，每个文件都是
        // "新增"，全量误报。"这一道检查没有跑"由编排器统一出声，
        // CLI 只负责把查到的差异打印出来，两处各报一遍是噪音。
        let scanner = RootedWorktreeScanner::new(&workspace);
        if let Some(ResumeCheck::WorktreeChanged { changes }) = (!snapshot.worktree.is_empty())
            .then(|| task_resume::check_worktree(&snapshot.worktree, &scanner))
        {
            let described: Vec<String> = changes.iter().map(|change| change.describe()).collect();
            eprintln!(
                "工作区在任务中断后被改动过：\n{}",
                described
                    .iter()
                    .map(|text| format!("  - {text}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            );
        }
        let evidence_factory = evidence_factory_for(&workspace, config.max_turns);
        let deps = AgentDependencies {
            model: Arc::new(RigChatGateway::new(model.erase())),
            tools: registry,
            approvals,
            clock,
            policy,
            roster: roster_with_role_models(&config),
            context_budget,
            evidence: Arc::new(MemoryEvidenceSink::new()),
            session_store: Some(store.clone()),
        };
        let mut orchestrator =
            Orchestrator::new(deps, budget, evidence_factory).with_task_store(Arc::new(task_store));
        // 工作区被改动时默认拒绝恢复。用户确认过就加 --force-worktree。
        let policy = if args.force_worktree {
            WorktreeChangePolicy::Confirmed
        } else {
            WorktreeChangePolicy::Refuse
        };
        orchestrator = orchestrator
            .with_worktree_scanner(Arc::new(RootedWorktreeScanner::new(&workspace)))
            .with_worktree_policy(policy);
        let outcome = match orchestrator.resume(snapshot).await {
            Ok(outcome) => outcome,
            Err(error) => {
                eprintln!("{error}");
                std::process::exit(1);
            }
        };
        println!("\n──── 最终答案 ────\n{}", outcome.answer);
        println!(
            "\n（任务 {} · 本次完成 {} 步 · 会话: {}）",
            task_id.as_str(),
            outcome.steps_completed,
            outcome.session_id.as_str()
        );
        close_mcp_connections(&mcp_sources).await;
        return Ok(());
    }

    let task = args.task.expect("parse_args 保证：非 list/resume 模式必有任务描述");

    // --kind 选模板；不传默认 bug-fix（最常见）。
    let kind_text = args.kind.as_deref().unwrap_or(TaskKind::BugFix.name());
    let Some(kind) = TaskKind::parse_name(kind_text) else {
        eprintln!("未知的任务类型 {kind_text}；可用: bug-fix / feature / review / refactor");
        std::process::exit(2);
    };

    // 项目指令：workspace 根上的 QCODEHARNESS.md（次级位置
    // .qcodeharness/instructions.md，P8-1 起模型读写都会被拒，所以放那里
    // 的约定不会被一次失控的 write_file 抹掉）。两个位置都没有时花名册
    // 就是内置档案，行为与没有这个特性时逐字相同。
    let roster = match instructions::load_project_instructions(&workspace) {
        Some(loaded) => {
            for warning in &loaded.warnings {
                eprintln!("警告：{warning}");
            }
            if loaded.truncated {
                eprintln!("警告：{} 超过 8 KiB，已截断后注入", loaded.source.display());
            }
            println!(
                "项目指令: {}（{} 字节）",
                loaded.source.display(),
                loaded.text.len()
            );
            roster_with_role_models(&config).with_project_instructions(&loaded.text)
        }
        None => roster_with_role_models(&config),
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
    } else if args.ai_review {
        // 说清退路：用户需要知道模型拿不准时仍然会来问他。
        println!("审批: AI 审查优先，拿不准时退回人工询问");
    } else {
        println!("审批: 交互式（每次副作用工具执行前询问）");
    }

    // 每步一份证据：独立的 JSONL 文件（与会话 ID 同名）+ 一个新的控制台
    // 播报器（轮次号每步从头计，复用旧播报器会丢步骤 2-4 的分隔线）。
    let evidence_factory = evidence_factory_for(&workspace, config.max_turns);

    // 任务 ID 复用会话 ID 的生成策略：唯一、可读，证据/快照同名可互查。
    let task_id = TaskId::new(new_session_id(&*clock).as_str());

    let deps = AgentDependencies {
        model: Arc::new(RigChatGateway::new(model.erase())),
        tools: registry,
        approvals,
        clock,
        policy,
        roster: roster.clone(),
        context_budget,
        // 编排器为任务另造证据汇（一个任务一份 JSONL），这个占位不会被
        // 步骤用到；deps 的形状要求它有值。
        evidence: Arc::new(MemoryEvidenceSink::new()),
        // 快照落盘随运行自动开启；--resume 可从任一会话恢复。
        session_store: Some(store.clone()),
    };
    let mut orchestrator = Orchestrator::with_roster(deps, budget, evidence_factory, roster)
        .with_task_store(Arc::new(JsonTaskStore::new(&workspace)))
        .with_worktree_scanner(Arc::new(RootedWorktreeScanner::new(&workspace)));
    let mut task = orchestrator.create_task(task_id.clone(), kind, &task);

    let outcome = match orchestrator.run(&mut task).await {
        Ok(outcome) => outcome,
        Err(error) => {
            eprintln!("{error}");
            // 失败时刻把会话 ID 与任务 ID 给出来：证据文件按会话 ID 命名，
            // 任务快照按任务 ID 命名。拿到 ID 就能翻证据或 --resume-task，
            // 不用再翻目录。
            if let ApplicationError::StepFailed { session_id, .. } = &error {
                eprintln!("会话 {session_id} 的证据已写入 .qcodeharness/sessions/");
            }
            eprintln!(
                "任务 {} 的快照已写入 .qcodeharness/tasks/；用 --resume-task {} 接着跑",
                task_id.as_str(),
                task_id.as_str()
            );
            std::process::exit(1);
        }
    };

    println!("\n──── 最终答案 ────\n{}", outcome.answer);
    let (done, total) = task.progress();
    println!(
        "\n（任务 {} · {}/{} 步 · 会话: {}）",
        task.id(),
        done,
        total,
        outcome.session_id.as_str()
    );
    close_mcp_connections(&mcp_sources).await;
    Ok(())
}

/// `--list-sessions`：列出可恢复的会话与各自停在哪里。
fn list_sessions(store: &dyn qcode_harness::application::snapshot::SessionStore) -> Result<(), Box<dyn Error>> {    let ids = store.list()?;
    if ids.is_empty() {
        println!("还没有可恢复的会话。跑完一个任务后，快照会自动存到 .qcodeharness/snapshots/。");
        return Ok(());
    }
    println!("可恢复的会话（共 {} 个）：", ids.len());
    for id in ids {
        match store.load(&id) {
            Ok(Some(snapshot)) => {
                println!(
                    "  {}  [{}]  已 {} 轮  停在: {}",
                    id.as_str(),
                    snapshot.spec.role.name(),
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

/// `--report`：从证据目录统计运行报表。`report_dir` 是证据根目录
/// （找它的 `.qcodeharness/sessions/`），缺省为当前工作区。
fn render_report(workspace: &Path, report_dir: Option<&str>, json: bool) -> Result<(), Box<dyn Error>> {
    let root = report_dir.map_or(workspace.to_path_buf(), PathBuf::from);
    let sessions_dir = root.join(".qcodeharness").join("sessions");
    if !sessions_dir.is_dir() {
        println!("{} 下还没有运行证据。", sessions_dir.display());
        return Ok(());
    }
    let logs = reader::read_all(&sessions_dir).map_err(|error| format!("{error}"))?;
    let report = BenchmarkReport::from_sessions(&logs);
    if json {
        let text = render_json(&report).map_err(|error| format!("{error}"))?;
        println!("{text}");
    } else {
        println!("{}", render_text(&report));
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

/// 每任务的证据汇工厂：一个任务一份 JSONL + 一个控制台播报器。
///
/// 独立成函数是因为新任务与任务级恢复两条入口都要用它——恢复路径上
/// 证据要**追加**到同一个文件（复盘时一个 ID 看到全貌），所以工厂
/// 每次都按会话 ID 打开同一个路径。
fn evidence_factory_for(workspace: &Path, max_turns: u32) -> EvidenceFactory {
    let workspace = workspace.to_path_buf();
    Arc::new(move |session_id: &SessionId| {
        let path = session_path(&workspace, session_id.as_str());
        let jsonl = JsonlEvidenceSink::open(&path)?;
        Ok(Arc::new(FanoutEvidenceSink::new(vec![
            Arc::new(jsonl),
            Arc::new(ConsoleReporter::new(max_turns)) as Arc<dyn EvidenceSink>,
        ])) as Arc<dyn EvidenceSink>)
    })
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

/// MCP 连接事件专用审计汇：`.qcodeharness/sessions/mcp-lifecycle.jsonl`。
///
/// 连接事件与工具事件分账：启动失败、断连、恢复尝试都不是工具调用，
/// 混进会话日志会让"成功/失败会话数"失真。报表侧按"整份日志只有连接
/// 事件"识别它并单列统计（application::report 的 mcp_summary）。
/// 固定文件名、跨运行追加——序号从文件里最后一条接着走（与恢复运行
/// 同一份证据文件的做法一致）。打开失败只警告不阻断：没有连接审计的
/// 运行仍然是一次诚实的运行，只是复盘时少一本账。
fn mcp_lifecycle_recorder(workspace: &Path) -> Option<Arc<McpLifecycleRecorder>> {
    let path = session_path(workspace, "mcp-lifecycle");
    match JsonlEvidenceSink::open(&path) {
        Ok(sink) => Some(Arc::new(McpLifecycleRecorder::new(
            Arc::new(sink),
            Arc::new(SystemClock),
        ))),
        Err(error) => {
            eprintln!(
                "警告：MCP 连接审计无法写入 {}（{error}），本次运行的连接事件不落盘",
                path.display()
            );
            None
        }
    }
}

/// 收尾：正常结束的三条路径共用，显式关闭所有 MCP 连接。
///
/// 失败路径靠 `std::process::exit` 离开，不经过这里——那时清理由
/// 操作系统与 Job Object 兜底（子进程已纳入 Job，随父进程终止）。
/// 这里走的是温和的一条：transport 先优雅收尾，再释放进程组兜底，
/// 两侧都留下可查的证据。
async fn close_mcp_connections(sources: &[mcp::McpToolSource]) {
    for source in sources {
        source.close().await;
    }
}

/// `--mcp-reconnect <服务器名>`：独立探测模式。
///
/// 只回答"这个 Server 现在能不能说上话"：按配置启动、握手、发现工具，
/// 把事实写进连接审计，然后显式关闭退出。有界一次，不接任务、不重放
/// 历史调用、不进任何会话；恢复出来的工具也不注入当前进程——工具要
/// 进任务，照常带任务描述启动，走完整的注册、授权与审批。
async fn mcp_probe(workspace: &Path, server: &str) -> Result<(), Box<dyn Error>> {
    let Some(config_path) = std::env::var_os("QCH_MCP_CONFIG").map(PathBuf::from) else {
        return Err("未设置 QCH_MCP_CONFIG，没有可探测的 MCP 配置".into());
    };
    let mcp_config = McpConfig::load(&config_path)
        .map_err(|error| format!("{error}（文件: {}）", config_path.display()))?;
    let Some(server_config) = mcp_config
        .as_ref()
        .and_then(|config| config.mcp_servers.iter().find(|item| item.name == server))
    else {
        let names = mcp_config.as_ref().map_or_else(String::new, |config| {
            config
                .mcp_servers
                .iter()
                .map(|item| item.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        });
        return Err(format!("MCP 配置里没有名为 `{server}` 的 Server（已配置: {names}）").into());
    };
    println!("MCP 恢复探测: server `{server}`");
    let source = mcp::McpToolSource::connect(server_config, mcp_lifecycle_recorder(workspace))
        .await
        .map_err(|error| {
            // 失败要原样带出分类：这是这个模式存在的意义——用户看到的
            // 应该就是"哪类故障、结果可知否"，不是一个被改写过的结论。
            format!("MCP Server `{server}` 恢复失败: {error}")
        })?;
    let count = source.tool_count();
    source.close().await;
    println!("MCP Server `{server}` 通信正常：发现 {count} 个工具（探测连接已关闭）");
    println!("要让工具进入任务，请照常带任务描述启动。");
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 从一张假变量表里取值：只有表里有的名字才有值。
    fn table(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let pairs = pairs.to_vec();
        move |name| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| (*value).to_owned())
        }
    }

    /// 每个角色读各自的环境变量，互不串味。
    #[test]
    fn role_models_come_from_their_own_variables() {
        let models = read_role_models_from(&table(&[
            ("LLM_MODEL_PLANNER", "planner-model"),
            ("LLM_MODEL_CODER", "coder-model"),
            ("LLM_MODEL_TESTER", "tester-model"),
            ("LLM_MODEL_REVIEWER", "reviewer-model"),
        ]));

        assert_eq!(models.len(), 4);
        assert_eq!(models[&AgentRole::Planner], "planner-model");
        assert_eq!(models[&AgentRole::Coder], "coder-model");
        assert_eq!(models[&AgentRole::Tester], "tester-model");
        assert_eq!(models[&AgentRole::Reviewer], "reviewer-model");
    }

    /// 没配的角色不进表：编排器据此让该角色用运行时的默认模型。
    #[test]
    fn an_unconfigured_role_is_absent_from_the_table() {
        let models = read_role_models_from(&table(&[("LLM_MODEL_CODER", "coder-model")]));

        assert_eq!(models.len(), 1);
        assert!(!models.contains_key(&AgentRole::Planner));
        assert!(!models.contains_key(&AgentRole::Tester));
    }

    /// 一个角色都没配时，表是空的——请求里不会出现 `model` 字段。
    ///
    /// 这是 P11 的兼容底线：默认部署的行为必须与 P10 逐字一致。
    #[test]
    fn without_any_configuration_the_tables_are_empty() {
        assert!(read_role_models_from(&table(&[])).is_empty());
        assert!(read_role_efforts_from(&table(&[])).unwrap().is_empty());
        assert!(read_role_verbosities_from(&table(&[])).unwrap().is_empty());
    }

    /// 留空的值当作没配。
    ///
    /// `LLM_MODEL_CODER=` 是很常见的写法（先占个位、以后再填）。当成一个
    /// 叫空串的模型会让请求打到一个不存在的模型上，报错还难懂。
    #[test]
    fn a_blank_value_counts_as_unset() {
        let models = read_role_models_from(&table(&[("LLM_MODEL_CODER", "   ")]));

        assert!(models.is_empty(), "空值不应被当成一个模型名");
    }

    /// 推理档位按角色读，拼错时报出是哪个变量名。
    #[test]
    fn role_efforts_are_read_per_role() {
        let efforts = read_role_efforts_from(&table(&[
            ("LLM_EFFORT_PLANNER", "high"),
            ("LLM_EFFORT_TESTER", "low"),
        ]))
        .expect("档位应能解析");

        assert_eq!(efforts[&AgentRole::Planner], Effort::High);
        assert_eq!(efforts[&AgentRole::Tester], Effort::Low);
        assert!(!efforts.contains_key(&AgentRole::Coder));
    }

    /// 档位写错要让进程停下来，并说清是哪个变量名。
    #[test]
    fn a_bad_effort_names_the_variable_that_went_wrong() {
        let error = read_role_efforts_from(&table(&[("LLM_EFFORT_CODER", "最大")]))
            .expect_err("认不出的档位应当报错");

        assert!(
            error.contains("LLM_EFFORT_CODER"),
            "报错要说清是哪个变量: {error}"
        );
    }

    /// 输出档位同样按角色读，同样在写错时报出变量名。
    #[test]
    fn role_verbosities_are_read_per_role() {
        let verbosities = read_role_verbosities_from(&table(&[
            ("LLM_VERBOSITY_REVIEWER", "high"),
            ("LLM_VERBOSITY_CODER", "low"),
        ]))
        .expect("档位应能解析");

        assert_eq!(verbosities[&AgentRole::Reviewer], Verbosity::High);
        assert_eq!(verbosities[&AgentRole::Coder], Verbosity::Low);
        assert!(!verbosities.contains_key(&AgentRole::Planner));
    }

    #[test]
    fn a_bad_verbosity_names_the_variable_that_went_wrong() {
        let error = read_role_verbosities_from(&table(&[("LLM_VERBOSITY_TESTER", "啰嗦")]))
            .expect_err("认不出的档位应当报错");

        assert!(
            error.contains("LLM_VERBOSITY_TESTER"),
            "报错要说清是哪个变量: {error}"
        );
    }

    /// 装配出的花名册：配了的角色拿到模型，没配的保持 `None`。
    #[test]
    fn the_roster_carries_the_configured_models() {
        let config = LlmConfig {
            base_url: "http://localhost".to_owned(),
            api_key: "k".to_owned(),
            model: "default-model".to_owned(),
            max_turns: 20,
            policy_path: None,
            auto_approve: false,
            max_wall_clock: None,
            summarize_min_dropped: None,
            role_models: read_role_models_from(&table(&[("LLM_MODEL_CODER", "coder-model")])),
            role_efforts: read_role_efforts_from(&table(&[("LLM_EFFORT_CODER", "high")])).unwrap(),
            role_verbosities: read_role_verbosities_from(&table(&[])).unwrap(),
        };

        let roster = roster_with_role_models(&config);

        assert_eq!(
            roster.find(AgentRole::Coder).unwrap().model.as_deref(),
            Some("coder-model")
        );
        assert_eq!(
            roster.find(AgentRole::Coder).unwrap().effort,
            Some(Effort::High)
        );
        assert_eq!(roster.find(AgentRole::Tester).unwrap().model, None);
    }
}

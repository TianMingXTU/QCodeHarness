//! ReAct（Reason + Act）循环的入口：薄壳。
//!
//! 分工：
//! - rig 负责模型协议适配（OpenAI 兼容的 `/chat/completions`）与工具
//!   定义的序列化，封装在 `infrastructure::model`；
//! - ReAct 循环在 `application::agent_run::AgentRunner`；
//! - 本文件读取环境变量、组装各层实现、打印结果。
//!
//! 全部逻辑在库目标（`src/lib.rs`）中，本文件只消费它，因此集成测试
//! 使用与生产相同的装配路径。

use std::collections::HashMap;
use std::error::Error;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use qcode_harness::application::agent_roster::AgentRoster;
use qcode_harness::application::agent_run::{
    AgentDependencies, AgentRunner, RunState, new_session_id,
};
use qcode_harness::application::context_policy::DefaultContextPolicy;
use qcode_harness::application::delegation::ChildAgentFactory;
use qcode_harness::application::error::ApplicationError;
use qcode_harness::application::extension::{ExtensionRegistry, ExtensionSelection};
use qcode_harness::application::isolation::{IsolationWorkspace, WorkspaceVerifier};
use qcode_harness::application::mcp_lifecycle::McpLifecycleRecorder;
use qcode_harness::application::memory_evidence::MemoryEvidenceSink;
use qcode_harness::application::orchestrator::{EvidenceFactory, Orchestrator};
use qcode_harness::application::port::{
    ApprovalGate, Clock, EvidenceSink, ModelGateway, NoopHooks, TimeLimiter, Timestamp,
};
use qcode_harness::application::provider_registry::ProviderRegistry;
use qcode_harness::application::report::{BenchmarkReport, render_json, render_text};
use qcode_harness::application::service::TaskService;
use qcode_harness::application::snapshot::{RunSnapshot, SessionStore, TaskStore};
use qcode_harness::application::task_resume::{self, ResumeCheck, WorktreeChangePolicy};
use qcode_harness::application::tool_registry::{ToolRegistry, ToolSource};
use qcode_harness::domain::agent::{AgentRole, Effort, Verbosity};
use qcode_harness::domain::budget::Budget;
use qcode_harness::domain::config::AppConfig;
use qcode_harness::domain::context::ContextBudget;
use qcode_harness::domain::evidence::AuditEvent;
use qcode_harness::domain::ids::{SessionId, TaskId};
use qcode_harness::domain::policy::PolicyEngine;
use qcode_harness::domain::service::{ServiceCommand, ServiceError};
use qcode_harness::domain::session::Turn;
use qcode_harness::domain::task::TaskKind;
use qcode_harness::domain::workflow::WorkflowCatalog;
use qcode_harness::domain::workspace::WorkspaceRoot;
use qcode_harness::infrastructure::approval::ai_review::AiReviewGate;
use qcode_harness::infrastructure::approval::auto::AutoApproveGate;
use qcode_harness::infrastructure::approval::cli::CliApprovalGate;
use qcode_harness::infrastructure::benchmark_runner::{
    language_summary_lines, run_benchmark, run_long_benchmark,
};
use qcode_harness::infrastructure::clock::{SystemClock, TokioTimeLimiter};
use qcode_harness::infrastructure::config;
use qcode_harness::infrastructure::delegation::{
    CommandVerifier, StructuralVerifier, SubAgentToolSource,
};
use qcode_harness::infrastructure::evidence::jsonl::{JsonlEvidenceSink, session_path};
use qcode_harness::infrastructure::evidence::reader;
use qcode_harness::infrastructure::extensions::LocalExtensionStore;
use qcode_harness::infrastructure::git_worktree::GitWorktreeManager;
use qcode_harness::infrastructure::instructions;
use qcode_harness::infrastructure::mcp;
use qcode_harness::infrastructure::mcp::config::McpConfig;
use qcode_harness::infrastructure::model::openai_compat::OpenAiCompatProvider;
use qcode_harness::infrastructure::policy;
use qcode_harness::infrastructure::service_factory::{ProductionTaskFactory, ServiceFactoryConfig};
use qcode_harness::infrastructure::session_store::JsonSessionStore;
use qcode_harness::infrastructure::task_service::LocalTaskService;
use qcode_harness::infrastructure::task_store::JsonTaskStore;
use qcode_harness::infrastructure::tool::{BuiltinToolSource, all_tools};
use qcode_harness::infrastructure::worktree::RootedWorktreeScanner;
use qcode_harness::interface::cli::{self, ConsoleReporter, FrontendMode};
use qcode_harness::interface::{stdio::StdioServer, terminal};
use std::sync::Mutex;

/// 扩展动作的审批与执行时限。工具本身还有自己的时限，这里只兜底
/// 扩展声明与审批路径，避免一次挂起的确认拖死整个任务。
const EXTENSION_TIMEOUT: Duration = Duration::from_secs(120);

/// 合并前验证命令的时限。项目测试通常比单条工具命令长，给足时间；
/// 超过就终止，结果按"未知"处理，不把半截输出当成通过。
const VERIFY_TIMEOUT: Duration = Duration::from_secs(300);

/// 扩展提示词段的标题。与项目指令分开标注：两段来源不同，冲突时
/// 模型需要知道哪一段是扩展作者写的。
const EXTENSION_INSTRUCTIONS_HEADER: &str =
    "──── 扩展指令（来自已启用的 Skill，与上述规则冲突时以项目指令为准）────";

// 系统提示词按角色分发，集中在 `AgentProfile::builtin()` 中（按角色
// 说明可用与禁止的操作），这里没有全局提示词。

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
    /// 墙钟上限（秒）。编排模式下一个任务一个会话，该值即整个任务的
    /// 上限；恢复运行从快照延续累计耗时，跨多次 `--resume` 仍按同一
    /// 上限比较。未设置时不限。
    max_wall_clock: Option<Duration>,
    /// 被裁历史累积到多少轮才生成一次 LLM 摘要。未设置时不做摘要，
    /// 被裁历史一律退回占位说明。
    summarize_min_dropped: Option<usize>,
    /// 各角色专属的模型名。未设置的角色用默认模型。
    role_models: HashMap<AgentRole, String>,
    /// 各角色专属的推理档位。未设置的档位用 provider 默认。
    role_efforts: HashMap<AgentRole, Effort>,
    /// 各角色专属的输出详略档位。未设置的档位用 provider 默认。
    role_verbosities: HashMap<AgentRole, Verbosity>,
}

impl LlmConfig {
    /// 合并文件配置与环境变量；环境变量在文件之后覆盖。
    ///
    /// 文件（用户级/项目级）可以提供 `llm.base_url`、`llm.model`、运行预算
    /// 与策略路径；`LLM_API_KEY` 始终只从环境变量读取，不落配置文件。
    /// 环境变量仍保持最高优先级，现有部署不需要改配置。
    ///
    /// # Errors
    ///
    /// 文件与环境都没有模型地址/模型名、缺少 API Key、或数值变量不是
    /// 合法数字时返回说明性错误。
    fn from_env(file: &AppConfig) -> Result<Self, String> {
        Self::from_lookup(&|name| std::env::var(name).ok(), file)
    }

    /// 从任意取值源合并配置，便于测试而不修改进程环境。
    fn from_lookup(
        lookup: &dyn Fn(&str) -> Option<String>,
        file: &AppConfig,
    ) -> Result<Self, String> {
        let required =
            |name: &str| -> Result<String, String> { lookup(name).ok_or_else(|| missing(name)) };
        let text = |name: &str| lookup(name).filter(|value| !value.trim().is_empty());
        let max_turns = match text("LLM_MAX_TURNS") {
            Some(text) => {
                let turns: u32 = text
                    .parse()
                    .map_err(|_| format!("LLM_MAX_TURNS 不是合法整数: {text}"))?;
                // 与配置文件同一套契约：0 会让任务在首轮前就报预算耗尽，
                // 静默改成 1 又会掩盖配置错误。
                if turns == 0 {
                    return Err("LLM_MAX_TURNS 必须大于 0".to_owned());
                }
                turns
            }
            None => file.run.max_turns,
        };
        let max_wall_clock = match text("QCH_MAX_WALL_CLOCK") {
            Some(text) => {
                let seconds: u64 = text
                    .parse()
                    .map_err(|_| format!("QCH_MAX_WALL_CLOCK 不是合法秒数: {text}"))?;
                if seconds == 0 {
                    return Err("QCH_MAX_WALL_CLOCK 必须大于 0".to_owned());
                }
                Some(Duration::from_secs(seconds))
            }
            None => file.run.max_wall_clock_seconds.map(Duration::from_secs),
        };
        let summarize_min_dropped = match text("QCH_SUMMARIZE_MIN_DROPPED") {
            Some(text) => {
                let count: usize = text
                    .parse()
                    .map_err(|_| format!("QCH_SUMMARIZE_MIN_DROPPED 不是合法整数: {text}"))?;
                if count == 0 {
                    return Err("QCH_SUMMARIZE_MIN_DROPPED 必须大于 0".to_owned());
                }
                Some(count)
            }
            None => file.run.summarize_min_dropped,
        };
        Ok(Self {
            // 环境变量优先：已有部署只用环境变量，文件是新加的可选层。
            base_url: match text("LLM_BASE_URL") {
                Some(value) => value,
                None => file
                    .llm
                    .base_url
                    .clone()
                    .ok_or_else(|| missing("LLM_BASE_URL"))?,
            },
            api_key: required("LLM_API_KEY")?,
            model: match text("LLM_MODEL") {
                Some(value) => value,
                None => file.llm.model.clone().ok_or_else(|| missing("LLM_MODEL"))?,
            },
            max_turns,
            policy_path: lookup("QCH_POLICY")
                .map(PathBuf::from)
                .or_else(|| file.policy_path.clone()),
            auto_approve: lookup("QCH_AUTO_APPROVE").as_deref() == Some("1"),
            max_wall_clock,
            summarize_min_dropped,
            role_models: read_role_models_from(|name| lookup(name)),
            role_efforts: read_role_efforts_from(|name| lookup(name))?,
            role_verbosities: read_role_verbosities_from(|name| lookup(name))?,
        })
    }
}

/// 缺一项模型设置时的错误：环境变量与配置文件都看过了。
fn missing(name: &str) -> String {
    format!("缺少 {name}：既没有环境变量，也没有配置文件里的对应项")
}

/// 从任意取值源读各角色专属的模型名。
///
/// 与读取环境变量的逻辑分开以便测试。进程环境变量是全局状态，测试
/// 中修改会与并行运行的其他测试互相影响。取值源作为参数后，测试可以
/// 传入模拟的变量表。
fn read_role_models_from(lookup: impl Fn(&str) -> Option<String>) -> HashMap<AgentRole, String> {
    let mut models = HashMap::new();
    for role in AgentRole::all() {
        if let Some(name) = lookup(role.model_env_name()) {
            let name = name.trim().to_owned();
            // 空串视为未配置。环境变量留空（`LLM_MODEL_CODER=`）很常见，
            // 当成空串模型名会让请求发往不存在的模型。
            if !name.is_empty() {
                models.insert(role, name);
            }
        }
    }
    models
}

/// 从任意取值源读各角色专属的推理档位。
fn read_role_efforts_from(
    lookup: impl Fn(&str) -> Option<String>,
) -> Result<HashMap<AgentRole, Effort>, String> {
    let mut efforts = HashMap::new();
    for role in AgentRole::all() {
        if let Some(text) = lookup(role.effort_env_name()) {
            let effort = Effort::parse(&text)
                .map_err(|error| format!("{}: {error}", role.effort_env_name()))?;
            efforts.insert(role, effort);
        }
    }
    Ok(efforts)
}

/// 从任意取值源读各角色专属的输出详略档位。
fn read_role_verbosities_from(
    lookup: impl Fn(&str) -> Option<String>,
) -> Result<HashMap<AgentRole, Verbosity>, String> {
    let mut verbosities = HashMap::new();
    for role in AgentRole::all() {
        if let Some(text) = lookup(role.verbosity_env_name()) {
            let verbosity = Verbosity::parse(&text)
                .map_err(|error| format!("{}: {error}", role.verbosity_env_name()))?;
            verbosities.insert(role, verbosity);
        }
    }
    Ok(verbosities)
}

/// 按配置装配内置花名册，为每个角色设置模型与档位。
///
/// 模型名与档位只决定请求使用的模型，不影响可用工具。工具白名单在
/// [`AgentRoster::builtin`] 中定义，此处不修改。为 Tester 配置更强的
/// 模型不会允许它修改文件。
///
/// 未配置的角色保持 `None`，请求中不出现 `model` 字段，档位由
/// provider 取默认值。
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

    // 帮助是纯查询：不读模型配置、不装工具，打印后正常退出。
    if args.help {
        println!("{}", cli::USAGE);
        return Ok(());
    }

    let workspace = std::env::current_dir()?;
    let store = Arc::new(JsonSessionStore::new(&workspace));

    // 配置文件先加载：后续的扩展管理、MCP 路径、策略与预算都从这里取。
    // 文件损坏时直接退出，不能用一份猜出来的配置继续跑。
    let app_config = config::load(&workspace).map_err(|error| format!("配置加载失败: {error}"))?;

    // 扩展管理是独立模式：安装、列出、启用、禁用状态，不跑任务也不需要
    // 模型配置。放在模型配置之前，没配 API Key 也能管理扩展。
    if args.list_extensions
        || args.install_extension.is_some()
        || args.enable_extension.is_some()
        || args.disable_extension.is_some()
    {
        return manage_extensions(&workspace, &args);
    }

    // 列清单不需要模型配置：没配 API key 也应该能看有哪些会话可恢复。
    if args.list_sessions {
        return list_sessions(store.as_ref());
    }

    // 报表同样不需要模型配置：只读证据统计，没配 API key 也能出报表。
    if args.report {
        return render_report(&workspace, args.report_dir.as_deref(), args.json);
    }

    // 恢复探测同样不需要模型配置：只确认这个 MCP Server 能否通信，
    // 不执行任务、不涉及审批、不读会话历史。
    if let Some(server) = &args.mcp_reconnect {
        return mcp_probe(&workspace, server, &app_config).await;
    }

    let config = LlmConfig::from_env(&app_config)?;

    // 命令行开关优先于环境变量：显式 --yes 覆盖配置默认值。
    let auto_approve = args.auto_approve || config.auto_approve;

    // 策略配置读取失败时进程退出。无法理解的策略文件比没有策略更危险。
    let policy = policy::load(config.policy_path.as_deref())
        .map_err(|error| format!("策略配置加载失败: {error}"))?;

    // 模型接入使用提供方注册表：默认（也是唯一）提供方是 OpenAI 兼容
    // chat 端点。更换服务商时实现 LlmProvider 并在此登记，循环、审批
    // 与装配不感知具体供应商。
    //
    // OpenAI 兼容端点必须使用 chat()（/chat/completions），不要使用
    // completion()：它对 OpenAI 方言默认指向 Responses API
    // （/responses），DeepSeek 等兼容网关不支持。该约束封装在
    // OpenAiCompatProvider::new 中，装配方无需了解。
    let mut providers = ProviderRegistry::new();
    providers.register(Arc::new(OpenAiCompatProvider::new(
        config.base_url.as_str(),
        config.api_key.as_str(),
        config.model.as_str(),
    )));
    let model = providers
        .default_provider()
        .expect("内置提供方已登记")
        .gateway();

    // 上下文预算：默认只做确定性裁剪（占位说明）。设置阈值后才为被裁
    // 历史调用模型生成摘要，摘要是额外一次模型调用，默认关闭。基准与
    // 普通运行使用同一套预算口径。
    let context_budget = config
        .summarize_min_dropped
        .map_or(ContextBudget::default(), |min| {
            ContextBudget::default().with_summarize(min)
        });

    let mut budget = Budget::new(config.max_turns);
    if let Some(limit) = config.max_wall_clock {
        budget = budget.with_wall_clock(limit);
    }

    // 服务入口先于一次性工具装配；stdout 归传输层所有，诊断统一写 stderr。
    if let Some(frontend) = args.frontend {
        let frontend = frontend_for_terminal(
            frontend,
            std::io::stdin().is_terminal(),
            std::io::stdout().is_terminal(),
        )?;
        let mcp_path = std::env::var_os("QCH_MCP_CONFIG")
            .map(PathBuf::from)
            .or_else(|| app_config.mcp_config_path.clone());
        let mcp = match mcp_path {
            Some(path) => McpConfig::load(&path)
                .map_err(|error| format!("{error}（文件: {}）", path.display()))?
                .unwrap_or_default(),
            None => McpConfig::default(),
        };
        let verifier: Arc<dyn WorkspaceVerifier> = match &app_config.parallel.verify_command {
            Some(command) => Arc::new(CommandVerifier::new(command.clone(), VERIFY_TIMEOUT)),
            None => Arc::new(StructuralVerifier),
        };
        let factory = ProductionTaskFactory::new(ServiceFactoryConfig {
            model,
            policy,
            roster: roster_with_role_models(&config),
            context_budget,
            budget,
            user_dir: config::user_directory(),
            extensions: LocalExtensionStore::new(&workspace),
            settings: app_config,
            mcp,
            verifier: verifier.clone(),
        });
        let service: Arc<dyn TaskService> =
            Arc::new(LocalTaskService::open(workspace, Arc::new(factory), budget, verifier).await?);
        return serve_frontend(service, frontend).await.map_err(Into::into);
    }

    // ── 基准模式：运行 benchmarks/ 下的任务集并打分。位于全局工具
    // 装配之前：运行器为每个任务重建以 fixture 拷贝为根的注册表，
    // 全局注册表与 MCP 连接在此用不上。──
    if args.benchmark || args.benchmark_long {
        // 基准无人值守：交互式审批会阻塞整个运行，强制自动放行。
        let deps = AgentDependencies {
            model: Arc::clone(&model),
            tools: ToolRegistry::new(vec![]),
            approvals: Arc::new(AutoApproveGate),
            clock: Arc::new(SystemClock),
            policy,
            roster: roster_with_role_models(&config),
            context_budget,
            evidence: Arc::new(MemoryEvidenceSink::new()),
            // 短任务基准不保存快照：恢复时任务会运行在当前工作区，而非
            // 当时的 fixture 拷贝。证据照常写入磁盘，--report 可统计。
            // 长任务基准的运行器在每段装配自己的快照存储（恢复是其流程
            // 的一部分），这里的 None 只是装配模板的占位。
            session_store: None,
            hooks: Arc::new(NoopHooks),
            context_policy: Arc::new(DefaultContextPolicy),
            additional_guards: Vec::new(),
        };
        let benchmarks_root = workspace.join("benchmarks");
        // 进度与诊断写 stderr，stdout 留给结果，JSON 才能直接交给下游解析。
        eprintln!("基准任务集: {}", benchmarks_root.display());
        eprintln!("模型: {} @ {}", config.model, config.base_url);
        if args.benchmark_long {
            let results = run_long_benchmark(&deps, &budget, &benchmarks_root, &workspace).await?;
            let passed = results.iter().filter(|result| result.passed).count();
            if args.json {
                println!("{}", serde_json::to_string_pretty(&results)?);
            } else {
                println!("\n通过 {passed}/{} 个长任务", results.len());
                for line in language_summary_lines(&results) {
                    println!("  {line}");
                }
            }
            if passed < results.len() {
                std::process::exit(1);
            }
            return Ok(());
        }
        let results = run_benchmark(&deps, &budget, &benchmarks_root, &workspace).await?;
        let passed = results.iter().filter(|result| result.passed).count();
        if args.json {
            println!("{}", serde_json::to_string_pretty(&results)?);
        } else {
            println!("\n通过 {passed}/{} 个任务", results.len());
            for line in language_summary_lines(&results) {
                println!("  {line}");
            }
        }
        if passed < results.len() {
            // 基准是打分工具：存在未通过的任务时使用非 0 退出码，
            // 自动化流水线可据此感知失败。
            std::process::exit(1);
        }
        return Ok(());
    }

    // 工具注册表按来源装配：内置工具与每个 MCP Server 各为一个来源。
    // MCP 配置取 QCH_MCP_CONFIG，未设置时用配置文件里的 mcp_config_path。
    let workspace_root = WorkspaceRoot::new(workspace.clone());
    let mut tool_sources: Vec<Arc<dyn ToolSource>> =
        vec![Arc::new(BuiltinToolSource::new(workspace_root))];
    // 所有连接汇总于此，三条正常结束路径都显式关闭（见
    // close_mcp_connections）。未配置 MCP 时为空。
    let mut mcp_sources: Vec<mcp::McpToolSource> = Vec::new();
    let mcp_config_path = std::env::var_os("QCH_MCP_CONFIG")
        .map(PathBuf::from)
        .or_else(|| app_config.mcp_config_path.clone());
    if let Some(config_path) = mcp_config_path {
        // 配置文件存在但非法时进程退出。用户明确要接入 MCP，静默跳过会让
        // 模型使用残缺的工具清单运行（与策略配置同一原则）。
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
            }
            mcp_sources = sources;
        }
    }
    // 来源顺序即工具顺序：内置在前，MCP 在后，同名工具由后到的来源
    // 覆盖（MCP 前缀 `mcp__<server>__` 使正常运行时不会与内置工具重名）。
    tool_sources.extend(
        mcp_sources
            .iter()
            .map(|source| Arc::new(source.clone()) as Arc<dyn ToolSource>),
    );
    let registry = ToolRegistry::from_sources(&tool_sources);

    // 扩展清单与授权状态。加载失败直接退出：一份读不懂的授权文件不能
    // 当作"没有扩展"继续跑，否则用户以为生效的约定会静默消失。
    let extension_store = LocalExtensionStore::new(&workspace);
    let extensions = extension_store
        .load(&registry)
        .map_err(|error| format!("扩展加载失败: {error}"))?;
    // 配置文件里的空列表表示"没写"，不额外限制选择范围；启用/禁用状态
    // 仍以安装项自身的授权为准。
    let selection = extensions
        .select_optional(
            app_config.skills.enabled.as_deref(),
            app_config.hooks.enabled.as_deref(),
        )
        .map_err(|error| format!("扩展选择失败: {error}"))?;
    let skill_tools = extensions
        .skill_tool_names(&selection)
        .map_err(|error| format!("扩展加载失败: {error}"))?;

    // 分层指令：用户级 < workspace 根 < 更接近任务目录的约定。读取失败
    // 只告警，指令是便利设施，不是安全边界。
    let base_roster = roster_with_role_models(&config);
    let mut roster = base_roster.clone();
    match instructions::load_layered_instructions(
        config::user_directory().as_deref(),
        &workspace,
        &workspace,
    ) {
        Ok(loaded) => {
            for warning in &loaded.warnings {
                eprintln!("警告：{warning}");
            }
            if !loaded.text.is_empty() {
                for source in &loaded.sources {
                    println!("项目指令: {}", source.display());
                }
                roster = roster.with_project_instructions(&loaded.text);
            }
        }
        Err(error) => eprintln!("警告：项目指令未加载（{error}）"),
    }
    let extension_instructions = extensions
        .enabled_instructions(&selection)
        .map_err(|error| format!("扩展指令加载失败: {error}"))?;
    if !extension_instructions.is_empty() {
        roster = roster
            .with_instructions_section(EXTENSION_INSTRUCTIONS_HEADER, &extension_instructions);
    }
    // 已启用的 Skill 入口对所有角色可见；Skill 背后的实际工具仍按角色
    // 白名单复核，给只读角色一个 Skill 不会让它获得写权限。
    if !skill_tools.is_empty() {
        roster = roster.with_extension_tools(&skill_tools);
    }

    // 审批闸门三选一，优先级从高到低：
    //   --yes                恒放行（基准与脚本化场景）
    //   --ai-review          模型先审，拿不准的退回交互式闸门
    //   默认                 交互式闸门
    // --yes 与 --ai-review 同时给出时采用 --yes：用户已声明无需人工
    // 确认，再插入模型审查只会多一次无效调用。
    let approval_mode = if auto_approve {
        ApprovalMode::Auto
    } else if args.ai_review {
        ApprovalMode::AiReview
    } else {
        ApprovalMode::Interactive
    };
    let approvals = approval_mode.gate(&model);
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let reporter = Arc::new(ConsoleReporter::new(config.max_turns));

    // 子任务委派：需要真实 Git 工作区根。不是仓库时跳过并说明，
    // 任务照常运行，只是模型看不到 delegate_subtasks。
    let delegate_source: Option<Arc<dyn ToolSource>> = if app_config.parallel.max_subtasks == 0 {
        println!("子任务委派: 已禁用（parallel.max_subtasks = 0）");
        None
    } else {
        match GitWorktreeManager::new(WorkspaceRoot::new(workspace.clone())).await {
            Ok(manager) => {
                // 配置了验证命令就在候选工作区真的跑一遍；没配置时只做
                // 结构校验，并在启动时说明，避免用户误以为测试已通过。
                let verifier: Arc<dyn WorkspaceVerifier> = match &app_config.parallel.verify_command
                {
                    Some(command) => {
                        println!("子任务合并前验证: {command}");
                        Arc::new(CommandVerifier::new(command.clone(), VERIFY_TIMEOUT))
                    }
                    None => {
                        println!("子任务合并前验证: 仅结构校验（未配置 parallel.verify_command）");
                        Arc::new(StructuralVerifier)
                    }
                };
                let factory = Arc::new(ChildFactory {
                    model: Arc::clone(&model),
                    policy: policy.clone(),
                    roster: base_roster.clone(),
                    context_budget,
                    user_dir: config::user_directory(),
                    approval_mode,
                    store: store.clone(),
                    sessions_root: workspace.clone(),
                });
                Some(Arc::new(SubAgentToolSource::new(
                    Arc::new(manager),
                    factory,
                    verifier,
                    budget,
                    app_config.parallel.max_subtasks,
                )) as Arc<dyn ToolSource>)
            }
            Err(error) => {
                eprintln!("警告：子任务委派不可用（{error}），任务照常运行");
                None
            }
        }
    };

    let shared = Shared {
        model,
        host_tools: registry,
        approvals,
        clock,
        policy,
        roster,
        context_budget,
        store: store.clone(),
        extensions,
        selection,
        delegate: delegate_source,
        limiter: Arc::new(
            TokioTimeLimiter::new(EXTENSION_TIMEOUT).map_err(|error| error.to_string())?,
        ),
    };

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
        // 恢复的会话属于某个角色，角色可用的工具按同一份档案约束。工具
        // 定义用全量（所有步骤一致，前缀缓存才能命中），白名单由 runner
        // 按花名册在调用层校验：装配期不按角色过滤，越权调用会被拒绝。
        let deps = assemble(&shared, evidence, snapshot.spec.role)?;
        let runner = match AgentRunner::resume(snapshot, deps) {
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
        // 工作区检查由编排器在恢复前执行，快照中保存了当时的状态。
        // CLI 先打印差异，用户确认改动后可加 --force-worktree 放行；
        // 编排器默认拒绝恢复。
        //
        // 快照中没有清单时不比对：空清单与当前清单相比，每个文件都会被
        // 视为新增，产生全量误报。"检查未执行"由编排器统一提示，CLI
        // 只打印查到的差异，两处各报一次会造成噪音。
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
        let (evidence_factory, late_evidence) = task_evidence_factory(&workspace, config.max_turns);
        let initial_role = snapshot
            .task
            .steps()
            .get(snapshot.task.current_index())
            .map_or(AgentRole::Coder, |step| step.role);
        let deps = assemble(&shared, late_evidence, initial_role)?;
        let mut orchestrator =
            Orchestrator::with_roster(deps, budget, evidence_factory, shared.roster.clone())
                .with_task_store(Arc::new(task_store));
        // 工作区被改动时默认拒绝恢复。用户确认过就加 --force-worktree。
        let worktree_policy = if args.force_worktree {
            WorktreeChangePolicy::Confirmed
        } else {
            WorktreeChangePolicy::Refuse
        };
        orchestrator = orchestrator
            .with_worktree_scanner(Arc::new(RootedWorktreeScanner::new(&workspace)))
            .with_worktree_policy(worktree_policy);
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

    let task = args
        .task
        .expect("parse_args 保证：非 list/resume 模式必有任务描述");

    // --kind 选模板；不传默认 bug-fix（最常见）。
    let kind_text = args.kind.as_deref().unwrap_or(TaskKind::BugFix.name());
    let Some(kind) = TaskKind::parse_name(kind_text) else {
        eprintln!("未知的任务类型 {kind_text}；可用: bug-fix / feature / review / refactor");
        std::process::exit(2);
    };

    println!("任务: {task}");
    println!("类型: {} 模板", kind.name());
    println!("模型: {} @ {}", config.model, config.base_url);
    if auto_approve {
        println!("审批: 自动放行（QCH_AUTO_APPROVE 或 --yes）");
    } else if args.ai_review {
        // 说明退路：模型不确定时仍会询问用户。
        println!("审批: AI 审查优先，拿不准时退回人工询问");
    } else {
        println!("审批: 交互式（每次副作用工具执行前询问）");
    }

    // 证据汇工厂：一个任务一份 JSONL（与任务会话同名），整个任务所有
    // 步骤共用。控制台播报随任务建，不随步骤建。
    let (evidence_factory, late_evidence) = task_evidence_factory(&workspace, config.max_turns);
    let initial_role = WorkflowCatalog::builtin()
        .find(kind)
        .and_then(|template| template.steps.first().map(|step| step.role))
        .unwrap_or(AgentRole::Coder);
    let deps = assemble(&shared, late_evidence, initial_role)?;
    println!(
        "工具: {}",
        deps.tools
            .specs()
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );

    // 任务 ID 复用会话 ID 的生成策略：唯一、可读，证据/快照同名可互查。
    let task_id = TaskId::new(new_session_id(&*shared.clock).as_str());

    let mut orchestrator =
        Orchestrator::with_roster(deps, budget, evidence_factory, shared.roster.clone())
            .with_task_store(Arc::new(JsonTaskStore::new(&workspace)))
            .with_worktree_scanner(Arc::new(RootedWorktreeScanner::new(&workspace)));
    let mut task = orchestrator.create_task(task_id.clone(), kind, &task);

    let outcome = match orchestrator.run(&mut task).await {
        Ok(outcome) => outcome,
        Err(error) => {
            eprintln!("{error}");
            // 失败时输出会话 ID 与任务 ID：证据文件按会话 ID 命名，任务
            // 快照按任务 ID 命名。拿到 ID 即可查看证据或执行 --resume-task，
            // 无需浏览目录。
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

/// 自动模式只有在输入、输出均为终端时进入 TUI。
fn frontend_for_terminal(
    frontend: FrontendMode,
    stdin_terminal: bool,
    stdout_terminal: bool,
) -> Result<FrontendMode, ServiceError> {
    let terminal = stdin_terminal && stdout_terminal;
    match frontend {
        FrontendMode::Interactive => Ok(if terminal {
            FrontendMode::Tui
        } else {
            FrontendMode::Text
        }),
        FrontendMode::Tui if !terminal => Err(ServiceError::new(
            "terminal_required",
            "--tui 需要标准输入和标准输出均连接终端；管道环境请用 --text 或 --stdio",
        )),
        frontend => Ok(frontend),
    }
}

/// 传输无论正常结束还是报错，都请求服务收尾，并把两侧失败合并成一个结果。
async fn finish_frontend(
    service: &dyn TaskService,
    transport: Result<(), ServiceError>,
) -> Result<(), ServiceError> {
    let shutdown_id = format!("host-shutdown-{}", new_session_id(&SystemClock));
    let shutdown = service.call(&shutdown_id, ServiceCommand::Shutdown).await;
    match (transport, shutdown) {
        (Ok(()), Ok(_)) => Ok(()),
        (Err(error), Ok(_)) | (Ok(()), Err(error)) => Err(error),
        (Err(mut error), Err(cleanup)) => {
            error
                .message
                .push_str(&format!("；服务关闭失败: {cleanup}"));
            Err(error)
        }
    }
}

/// 三条传输共用一个执行流：先跑所选传输，随后按同一收尾路径离开运行时。
async fn serve_frontend(
    service: Arc<dyn TaskService>,
    frontend: FrontendMode,
) -> Result<(), ServiceError> {
    let transport = match frontend {
        FrontendMode::Stdio => {
            StdioServer::new(service.clone())
                .serve(tokio::io::stdin(), tokio::io::stdout())
                .await
        }
        FrontendMode::Text => {
            terminal::run_text(service.clone(), tokio::io::stdin(), tokio::io::stdout()).await
        }
        FrontendMode::Tui => terminal::run_tui(service.clone()).await,
        FrontendMode::Interactive => unreachable!("前端已按终端能力解析"),
    };
    finish_frontend(service.as_ref(), transport).await
}

/// 三条任务入口共用的宿主装配。
///
/// 扩展注册表、宿主工具表与角色花名册在进程内不变，建立一次即可。每次
/// 任务另建适配器，因为适配器绑定这次任务的证据、审批与运行角色。
struct Shared {
    model: Arc<dyn ModelGateway>,
    /// 宿主工具表，不含 `skill__*` 与 `delegate_subtasks`。
    host_tools: ToolRegistry,
    approvals: Arc<dyn ApprovalGate>,
    clock: Arc<dyn Clock>,
    policy: PolicyEngine,
    /// 已注入项目指令、扩展指令与 Skill 工具入口的花名册。
    roster: AgentRoster,
    context_budget: ContextBudget,
    store: Arc<JsonSessionStore>,
    extensions: Arc<ExtensionRegistry>,
    selection: ExtensionSelection,
    delegate: Option<Arc<dyn ToolSource>>,
    /// 扩展动作的时限；由基础设施层的 tokio 计时器实现。
    limiter: Arc<dyn TimeLimiter>,
}

/// 为一次任务装配依赖：宿主工具 + 扩展适配器 + 委派工具。
///
/// `evidence` 是这次任务的证据汇。扩展适配器会一直持有它，因此编排器
/// 路径必须传入 [`LateBoundEvidence`]：真实汇要到会话 ID 生成后才存在。
fn assemble(
    shared: &Shared,
    evidence: Arc<dyn EvidenceSink>,
    initial_role: AgentRole,
) -> Result<AgentDependencies, String> {
    let host = AgentDependencies {
        model: Arc::clone(&shared.model),
        tools: shared.host_tools.clone(),
        approvals: Arc::clone(&shared.approvals),
        evidence,
        clock: Arc::clone(&shared.clock),
        policy: shared.policy.clone(),
        additional_guards: Vec::new(),
        roster: shared.roster.clone(),
        context_budget: shared.context_budget,
        context_policy: Arc::new(DefaultContextPolicy),
        session_store: Some(shared.store.clone()),
        hooks: Arc::new(NoopHooks),
    };
    // 适配器要求绑定未含 Skill 工具的宿主注册表；它按实际工具表复核每条
    // 权限，缺工具的已启用扩展在这里报错，而不是等到模型调用才发现。
    let adapters = shared
        .extensions
        .adapters(
            &host,
            initial_role,
            shared.selection.clone(),
            Arc::clone(&shared.limiter),
        )
        .map_err(|error| format!("扩展装配失败: {error}"))?;
    let mut tools =
        shared
            .host_tools
            .merged_with(&ToolRegistry::from_sources(std::slice::from_ref(
                &adapters.source,
            )));
    if let Some(delegate) = &shared.delegate {
        tools = tools.merged_with(&ToolRegistry::from_sources(std::slice::from_ref(delegate)));
    }
    Ok(AgentDependencies {
        tools,
        hooks: adapters.hooks,
        ..host
    })
}

/// 审批模式：父任务与子任务共用同一套选择，子任务另建闸门实例。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ApprovalMode {
    /// 全部放行。
    Auto,
    /// 每次副作用调用都问人。
    Interactive,
    /// 模型先审，拿不准的退回交互式闸门。
    AiReview,
}

impl ApprovalMode {
    /// 为一次运行新建闸门。
    ///
    /// 每个子运行必须有自己的实例：runner 构造时会绑定取消通道，共用
    /// 实例会让一个子任务的取消覆盖另一个的。
    fn gate(self, model: &Arc<dyn ModelGateway>) -> Arc<dyn ApprovalGate> {
        match self {
            Self::Auto => Arc::new(AutoApproveGate),
            Self::Interactive => Arc::new(CliApprovalGate::new()),
            Self::AiReview => Arc::new(AiReviewGate::new(
                Arc::clone(model),
                Box::new(CliApprovalGate::new()),
            )),
        }
    }
}

/// 为委派的子任务装配依赖。
///
/// 子任务只拿到以自己工作区为根的内置工具：扩展适配器绑定的是父任务的
/// 证据与工具表，注入子运行会让扩展动作绕过隔离写回父工作区。子运行的
/// 审批、证据、快照与取消通道全部独立，父任务的会话不受影响。
struct ChildFactory {
    model: Arc<dyn ModelGateway>,
    policy: PolicyEngine,
    /// 未注入扩展指令与 Skill 工具的基础花名册。
    roster: AgentRoster,
    context_budget: ContextBudget,
    user_dir: Option<PathBuf>,
    approval_mode: ApprovalMode,
    store: Arc<JsonSessionStore>,
    /// 父工作区根：子会话的证据与快照都落在这里，复盘时一处可查。
    sessions_root: PathBuf,
}

impl ChildAgentFactory for ChildFactory {
    fn dependencies(
        &self,
        workspace: &IsolationWorkspace,
        // 角色不在这里筛选：花名册含全部档案，runner 按 spec.role 取用并
        // 在调用层校验白名单。缺档案时子运行显式失败。
        _role: AgentRole,
        session_id: &SessionId,
        scope: &Path,
    ) -> Result<AgentDependencies, String> {
        // 子工作区是基线的一份拷贝，项目指令按它自己的根与 scope 重新
        // 加载：父任务目录下的约定不适用于另一棵目录树。加载失败只告警，
        // 与父任务同一处理方式。
        let mut roster = self.roster.clone();
        match instructions::load_layered_instructions(
            self.user_dir.as_deref(),
            &workspace.root,
            scope,
        ) {
            Ok(loaded) => {
                for warning in &loaded.warnings {
                    eprintln!("警告（子任务 {session_id}）：{warning}");
                }
                if !loaded.text.is_empty() {
                    roster = roster.with_project_instructions(&loaded.text);
                }
            }
            Err(error) => {
                eprintln!("警告（子任务 {session_id}）：项目指令未加载（{error}）");
            }
        }
        let evidence_path = session_path(&self.sessions_root, session_id.as_str());
        let evidence: Arc<dyn EvidenceSink> =
            Arc::new(JsonlEvidenceSink::open(&evidence_path).map_err(|error| {
                format!("子任务证据无法写入 {}：{error}", evidence_path.display())
            })?);
        Ok(AgentDependencies {
            model: Arc::clone(&self.model),
            tools: ToolRegistry::from_sources(&[Arc::new(BuiltinToolSource::new(
                WorkspaceRoot::new(&workspace.root),
            ))]),
            approvals: self.approval_mode.gate(&self.model),
            evidence,
            clock: Arc::new(SystemClock),
            policy: self.policy.clone(),
            additional_guards: Vec::new(),
            roster,
            context_budget: self.context_budget,
            context_policy: Arc::new(DefaultContextPolicy),
            session_store: Some(self.store.clone()),
            hooks: Arc::new(NoopHooks),
        })
    }
}

/// 延迟绑定的证据汇：扩展适配器先拿到它，真实汇在会话 ID 生成后注入。
///
/// 编排器为每个任务另建证据汇（一个任务一份 JSONL），而扩展适配器必须
/// 在任务开始前装配好。绑定前的事件直接丢弃：那段时间还没有任务在跑。
#[derive(Default)]
struct LateBoundEvidence {
    inner: Mutex<Option<Arc<dyn EvidenceSink>>>,
}

impl LateBoundEvidence {
    fn bind(&self, sink: Arc<dyn EvidenceSink>) {
        *self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(sink);
    }
}

impl EvidenceSink for LateBoundEvidence {
    fn record(&self, at: Timestamp, event: AuditEvent) {
        let sink = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        if let Some(sink) = sink {
            sink.record(at, event);
        }
    }
}

/// 任务路径的证据工厂：每建一个证据汇就同步绑定到延迟汇。
fn task_evidence_factory(
    workspace: &Path,
    max_turns: u32,
) -> (EvidenceFactory, Arc<LateBoundEvidence>) {
    let inner = evidence_factory_for(workspace, max_turns);
    let late = Arc::new(LateBoundEvidence::default());
    let bind = Arc::clone(&late);
    let factory: EvidenceFactory = Arc::new(move |session_id: &SessionId| {
        let sink = inner(session_id)?;
        bind.bind(Arc::clone(&sink));
        Ok(sink)
    });
    (factory, late)
}

/// 扩展管理：安装、列出、启用、禁用，独立于任务运行。
///
/// backing 表只装内置工具：管理扩展时没有连接 MCP，声明 MCP 工具的权限
/// 仍可安装，真正的可用性在任务装配时按实际工具表复核。
fn manage_extensions(workspace: &Path, args: &cli::CliArgs) -> Result<(), Box<dyn Error>> {
    let store = LocalExtensionStore::new(workspace);
    let backing = ToolRegistry::new(all_tools(WorkspaceRoot::new(workspace)));
    let registry = store
        .load(&backing)
        .map_err(|error| format!("扩展加载失败: {error}"))?;

    if args.list_extensions {
        let installed = registry
            .list()
            .map_err(|error| format!("扩展列表读取失败: {error}"))?;
        // 授权不在工作区里，说明位置才能让人找到"为什么换台机器就要重新批准"。
        match store.state_path() {
            Some(path) => println!("授权状态: {}", path.display()),
            None => println!("授权状态: 不可保存（系统没有提供用户配置目录）"),
        }
        if installed.is_empty() {
            println!("还没有安装扩展。用 --install-extension <清单文件> 安装。");
            return Ok(());
        }
        println!("已安装扩展（共 {} 个）：", installed.len());
        for item in installed {
            let kinds = match (item.has_skill, item.has_hook) {
                (true, true) => "Skill + Hook",
                (true, false) => "Skill",
                (false, true) => "Hook",
                (false, false) => "（无实现）",
            };
            println!(
                "  {}  [{}]  {}  —  {}",
                item.descriptor.id,
                if item.enabled { "启用" } else { "禁用" },
                kinds,
                item.descriptor.description
            );
            println!(
                "      声明权限: {}",
                join_or_none(&item.descriptor.permissions)
            );
            println!(
                "      已批准:   {}",
                join_or_none(&item.approved_permissions)
            );
        }
        return Ok(());
    }

    if let Some(source) = &args.install_extension {
        let descriptor = store.install(&registry, Path::new(source))?;
        println!(
            "已安装扩展 `{}`（默认禁用）。启用：--enable-extension {} --permissions {}",
            descriptor.id,
            descriptor.id,
            join_or_none(&descriptor.permissions)
        );
        return Ok(());
    }

    if let Some(id) = &args.enable_extension {
        store.enable(&registry, id, &args.permissions)?;
        println!(
            "已启用扩展 `{id}`，批准权限: {}",
            join_or_none(&args.permissions)
        );
        return Ok(());
    }

    if let Some(id) = &args.disable_extension {
        store.disable(&registry, id)?;
        println!("已禁用扩展 `{id}`，批准权限已清空");
        return Ok(());
    }

    unreachable!("parse_args 保证扩展管理模式至少有一项")
}

fn join_or_none(items: &[String]) -> String {
    if items.is_empty() {
        "（无）".to_owned()
    } else {
        items.join(", ")
    }
}

/// `--list-sessions`：列出可恢复的会话与各自停在哪里。
fn list_sessions(
    store: &dyn qcode_harness::application::snapshot::SessionStore,
) -> Result<(), Box<dyn Error>> {
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
fn render_report(
    workspace: &Path,
    report_dir: Option<&str>,
    json: bool,
) -> Result<(), Box<dyn Error>> {
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

/// 组装证据扇出：控制台播报给人看，JSONL 落盘给事后查。会话级
/// `--resume` 入口使用；任务级路径走 `evidence_factory_for`。
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
/// 独立成函数是因为新任务与任务级恢复两条入口都使用它。恢复路径上
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

/// 执行到底并汇报结果。会话级 `--resume` 入口的收尾；新任务与任务级
/// 恢复在各自入口内联完成同样的事。
async fn run_and_report(mut runner: AgentRunner, budget: &Budget, evidence_path: &Path) {
    // 错误按 Display 打印，不交给 Box<dyn Error> 的 Debug。错误消息中的
    // 对策指引（如"调大 LLM_MAX_TURNS"）面向用户，Debug 格式会输出为
    // 结构体转储。
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
/// 连接事件与工具事件分开记录。启动失败、断连、恢复尝试都不是工具
/// 调用，混入会话日志会使"成功/失败会话数"失真。报表按"整份日志只含
/// 连接事件"识别该文件并单独统计（application::report 的 mcp_summary）。
/// 文件名固定，跨运行追加，序号接续文件中最后一条（与恢复运行使用
/// 同一证据文件的做法一致）。打开失败只警告：没有连接审计的运行仍然
/// 有效，只是复盘时缺少这部分记录。
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
/// 失败路径通过 `std::process::exit` 离开，不经过这里。失败路径的清理
/// 取决于平台：Windows 上子进程已纳入 Job Object，随父进程终止；其他平台
/// 没有进程沙箱，异常退出后 MCP 子进程会残留，需要手工清理。
/// 正常路径先优雅关闭 transport，再释放进程组兜底，两侧都留下记录。
async fn close_mcp_connections(sources: &[mcp::McpToolSource]) {
    for source in sources {
        source.close().await;
    }
}

/// `--mcp-reconnect <服务器名>`：独立探测模式。
///
/// 只确认该 Server 当前能否通信：按配置启动、握手、发现工具，把结果
/// 写入连接审计，然后显式关闭退出。执行一次即结束，不接任务、不重放
/// 历史调用、不进入任何会话。探测到的工具不注入当前进程；要在任务中
/// 使用工具，照常带任务描述启动，走完整的注册、授权与审批。
async fn mcp_probe(
    workspace: &Path,
    server: &str,
    app_config: &AppConfig,
) -> Result<(), Box<dyn Error>> {
    let Some(config_path) = std::env::var_os("QCH_MCP_CONFIG")
        .map(PathBuf::from)
        .or_else(|| app_config.mcp_config_path.clone())
    else {
        return Err("未配置 MCP：设置 QCH_MCP_CONFIG 或在配置文件里写 mcp_config_path".into());
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
            // 失败时原样输出分类，这是该模式的目的：用户需要看到具体
            // 的故障分类与结果可知性，不需要改写过的结论。
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
/// 单个汇失败不影响其他汇。JSONL 自身会降级（打印警告后丢弃），这里
/// 再包一层，防止某个汇 panic 中断整个分发循环。
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
    fn record(
        &self,
        at: qcode_harness::application::port::Timestamp,
        event: qcode_harness::domain::evidence::AuditEvent,
    ) {
        for sink in &self.sinks {
            // catch_unwind 不能跨 await，record 是同步方法，可以包裹。
            // 单个汇 panic 不应影响任务和其余证据。
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

    #[test]
    fn service_frontend_selection_respects_both_terminal_streams() {
        for (input, output) in [(false, false), (true, false), (false, true), (true, true)] {
            assert_eq!(
                frontend_for_terminal(FrontendMode::Interactive, input, output).unwrap(),
                if input && output {
                    FrontendMode::Tui
                } else {
                    FrontendMode::Text
                },
            );
            assert_eq!(
                frontend_for_terminal(FrontendMode::Text, input, output).unwrap(),
                FrontendMode::Text
            );
            assert_eq!(
                frontend_for_terminal(FrontendMode::Stdio, input, output).unwrap(),
                FrontendMode::Stdio
            );
            assert_eq!(
                frontend_for_terminal(FrontendMode::Tui, input, output).is_ok(),
                input && output
            );
        }
    }

    #[tokio::test]
    async fn frontend_shutdown_is_awaited_after_transport_success_or_failure() {
        use qcode_harness::application::port::BoxFuture;
        use qcode_harness::application::service::EventSubscription;
        use qcode_harness::domain::service::ServiceReply;
        use std::sync::atomic::{AtomicBool, Ordering};

        struct ClosingService {
            closed: AtomicBool,
            fail: bool,
        }

        impl TaskService for ClosingService {
            fn call<'a>(
                &'a self,
                request_id: &'a str,
                command: ServiceCommand,
            ) -> BoxFuture<'a, Result<ServiceReply, ServiceError>> {
                assert!(request_id.starts_with("host-shutdown-"));
                assert_eq!(command, ServiceCommand::Shutdown);
                Box::pin(async move {
                    tokio::task::yield_now().await;
                    self.closed.store(true, Ordering::SeqCst);
                    if self.fail {
                        Err(ServiceError::new("close_failed", "关闭失败"))
                    } else {
                        Ok(ServiceReply::Shutdown)
                    }
                })
            }

            fn subscribe(&self, _: u64) -> Result<Box<dyn EventSubscription>, ServiceError> {
                unreachable!("关闭不订阅事件")
            }
        }

        for transport_fails in [false, true] {
            for shutdown_fails in [false, true] {
                let service = ClosingService {
                    closed: AtomicBool::new(false),
                    fail: shutdown_fails,
                };
                let transport = if transport_fails {
                    Err(ServiceError::new("transport_io", "写入失败"))
                } else {
                    Ok(())
                };
                let result = finish_frontend(&service, transport).await;
                assert!(service.closed.load(Ordering::SeqCst));
                match (transport_fails, shutdown_fails) {
                    (false, false) => assert!(result.is_ok()),
                    (false, true) => assert_eq!(result.unwrap_err().code, "close_failed"),
                    (true, false) => assert_eq!(result.unwrap_err().code, "transport_io"),
                    (true, true) => {
                        let error = result.unwrap_err();
                        assert_eq!(error.code, "transport_io");
                        assert!(error.message.contains("关闭失败"));
                    }
                }
            }
        }
    }

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

    /// 每个角色读取各自的环境变量，互不干扰。
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

    /// 未配置的角色不进入映射，编排器据此使用运行时默认模型。
    #[test]
    fn an_unconfigured_role_is_absent_from_the_table() {
        let models = read_role_models_from(&table(&[("LLM_MODEL_CODER", "coder-model")]));

        assert_eq!(models.len(), 1);
        assert!(!models.contains_key(&AgentRole::Planner));
        assert!(!models.contains_key(&AgentRole::Tester));
    }

    /// 未配置任何角色时映射为空，请求中不出现 `model` 字段。默认部署
    /// 下所有角色共用运行时默认模型。
    #[test]
    fn without_any_configuration_the_tables_are_empty() {
        assert!(read_role_models_from(&table(&[])).is_empty());
        assert!(read_role_efforts_from(&table(&[])).unwrap().is_empty());
        assert!(read_role_verbosities_from(&table(&[])).unwrap().is_empty());
    }

    /// 留空的值视为未配置。
    ///
    /// `LLM_MODEL_CODER=` 是常见写法（先占位后填写）。当成空串模型名
    /// 会使请求发往不存在的模型，报错信息也难以理解。
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

    // ── 文件配置与环境变量的合并 ───────────────────────────────────────

    fn file_config(text: &str) -> AppConfig {
        // 用公开的加载入口解析一段配置文本，避免在测试里复制解析逻辑。
        // 挂成显式文件（使用者自己写的），因为项目级配置不允许设置
        // 模型地址与策略路径。
        let dir = std::env::temp_dir().join(format!(
            "qch-main-config-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = dir.join("config.toml");
        std::fs::create_dir_all(&dir).expect("建临时配置目录");
        std::fs::write(&path, text).expect("写临时配置");
        let config = config::load_from_paths(&config::ConfigPaths {
            user: None,
            project: dir.join("absent-project.toml"),
            explicit: Some(path),
        })
        .expect("配置应能加载");
        std::fs::remove_dir_all(&dir).expect("清理临时配置目录");
        config
    }

    /// 文件提供模型地址与模型名，环境变量只补 API Key。
    #[test]
    fn the_file_supplies_model_settings_and_env_only_the_key() {
        let file =
            file_config("[llm]\nbase_url = 'https://file.invalid/v1'\nmodel = 'file-model'\n");
        let config = LlmConfig::from_lookup(&table(&[("LLM_API_KEY", "secret")]), &file).unwrap();

        assert_eq!(config.base_url, "https://file.invalid/v1");
        assert_eq!(config.model, "file-model");
        assert_eq!(config.api_key, "secret");
    }

    /// 环境变量在文件之后覆盖，现有部署不用改配置。
    #[test]
    fn environment_values_win_over_the_file() {
        let file = file_config(
            "[llm]\nbase_url = 'https://file.invalid/v1'\nmodel = 'file-model'\n\
             [run]\nmax_turns = 7\nmax_wall_clock_seconds = 30\n",
        );
        let config = LlmConfig::from_lookup(
            &table(&[
                ("LLM_API_KEY", "secret"),
                ("LLM_BASE_URL", "https://env.invalid/v1"),
                ("LLM_MODEL", "env-model"),
                ("LLM_MAX_TURNS", "3"),
                ("QCH_MAX_WALL_CLOCK", "90"),
            ]),
            &file,
        )
        .unwrap();

        assert_eq!(config.base_url, "https://env.invalid/v1");
        assert_eq!(config.model, "env-model");
        assert_eq!(config.max_turns, 3);
        assert_eq!(config.max_wall_clock, Some(Duration::from_secs(90)));
    }

    /// 文件里的运行预算在环境变量缺席时生效。
    #[test]
    fn run_budgets_fall_back_to_the_file() {
        let file = file_config(
            "[llm]\nbase_url = 'https://file.invalid/v1'\nmodel = 'file-model'\n\
             [run]\nmax_turns = 7\nmax_wall_clock_seconds = 30\nsummarize_min_dropped = 5\n",
        );
        let config = LlmConfig::from_lookup(&table(&[("LLM_API_KEY", "secret")]), &file).unwrap();

        assert_eq!(config.max_turns, 7);
        assert_eq!(config.max_wall_clock, Some(Duration::from_secs(30)));
        assert_eq!(config.summarize_min_dropped, Some(5));
    }

    /// 环境变量与配置文件对同一个字段用同一套范围校验。
    ///
    /// 0 轮、0 秒墙钟都会让任务在开始前就判预算耗尽；静默改成 1 则掩盖
    /// 配置错误，用户只会看到"任务莫名其妙只跑了一轮"。
    #[test]
    fn zero_budgets_are_rejected_from_both_sources() {
        let file =
            file_config("[llm]\nbase_url = 'https://file.invalid/v1'\nmodel = 'file-model'\n");
        for (name, value) in [
            ("LLM_MAX_TURNS", "0"),
            ("QCH_MAX_WALL_CLOCK", "0"),
            ("QCH_SUMMARIZE_MIN_DROPPED", "0"),
        ] {
            let error =
                LlmConfig::from_lookup(&table(&[("LLM_API_KEY", "secret"), (name, value)]), &file)
                    .expect_err("0 必须报错");
            assert!(error.contains(name), "{error}");
        }
    }

    /// 两边都没有模型地址或模型名时必须报错，不能拿空串去请求。
    #[test]
    fn missing_model_settings_fail_with_the_variable_name() {
        let file = file_config("[llm]\nbase_url = 'https://file.invalid/v1'\n");
        let error = LlmConfig::from_lookup(&table(&[("LLM_API_KEY", "secret")]), &file)
            .expect_err("缺模型名必须报错");
        assert!(error.contains("LLM_MODEL"), "{error}");

        let empty = AppConfig::default();
        let error = LlmConfig::from_lookup(&table(&[("LLM_API_KEY", "secret")]), &empty)
            .expect_err("缺地址必须报错");
        assert!(error.contains("LLM_BASE_URL"), "{error}");
    }

    /// 策略路径优先取环境变量，其次才是文件里的值。
    #[test]
    fn the_policy_path_prefers_the_environment() {
        let file = file_config(
            "policy_path = 'file-policy.toml'\n\
             [llm]\nbase_url = 'https://file.invalid/v1'\nmodel = 'file-model'\n",
        );
        // 文件里的相对路径已按定义它的文件目录解析过，这里只比较末端。
        let from_file = LlmConfig::from_lookup(&table(&[("LLM_API_KEY", "k")]), &file).unwrap();
        assert_eq!(
            from_file.policy_path.as_deref().and_then(Path::file_name),
            Some(std::ffi::OsStr::new("file-policy.toml"))
        );

        let from_env = LlmConfig::from_lookup(
            &table(&[("LLM_API_KEY", "k"), ("QCH_POLICY", "env-policy.toml")]),
            &file,
        )
        .unwrap();
        assert_eq!(from_env.policy_path, Some(PathBuf::from("env-policy.toml")));
    }
}

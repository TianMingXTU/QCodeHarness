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
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use rig::providers::openai::OpenAIConfig;
use qcode_harness::application::agent_run::AgentRunner;
use qcode_harness::application::port::{ApprovalGate, EvidenceSink};
use qcode_harness::application::tool_registry::ToolRegistry;
use qcode_harness::domain::agent::{AgentRole, AgentSpec};
use qcode_harness::domain::budget::Budget;
use qcode_harness::domain::workspace::WorkspaceRoot;
use qcode_harness::infrastructure::approval::auto::AutoApproveGate;
use qcode_harness::infrastructure::approval::cli::CliApprovalGate;
use qcode_harness::infrastructure::clock::SystemClock;
use qcode_harness::infrastructure::evidence::jsonl::{session_path, JsonlEvidenceSink};
use qcode_harness::infrastructure::model::rig_chat::RigChatGateway;
use qcode_harness::infrastructure::policy;
use qcode_harness::infrastructure::tool::all_tools;
use qcode_harness::interface::cli::{self, ConsoleReporter};

use qcode_harness::application::agent_run::AgentDependencies;

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
    /// 整体墙钟上限（秒）。未设置时不限时。
    max_wall_clock: Option<Duration>,
}

impl LlmConfig {
    /// 从环境变量读取配置。
    ///
    /// 必需变量：`LLM_BASE_URL`、`LLM_API_KEY`、`LLM_MODEL`；
    /// 可选变量：`LLM_MAX_TURNS`（默认 20）、`QCH_POLICY`（策略配置路径）、
    /// `QCH_AUTO_APPROVE`（设为 `1` 时跳过人工审批）、
    /// `QCH_MAX_WALL_CLOCK`（整体墙钟上限，秒）。
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
        Ok(Self {
            base_url: required("LLM_BASE_URL")?,
            api_key: required("LLM_API_KEY")?,
            model: required("LLM_MODEL")?,
            max_turns,
            policy_path: std::env::var_os("QCH_POLICY").map(PathBuf::from),
            auto_approve: std::env::var("QCH_AUTO_APPROVE").as_deref() == Ok("1"),
            max_wall_clock,
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

    let config = LlmConfig::from_env()?;
    let workspace = std::env::current_dir()?;

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
    let spec = AgentSpec {
        role: AgentRole::Coder,
        system_prompt: SYSTEM_PROMPT.to_owned(),
    };

    println!("任务: {}", args.task);
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

    let reporter = Arc::new(ConsoleReporter::new(config.max_turns));

    // 会话 ID 必须在这里先定下来：证据文件名要用它，而 AgentRunner
    // 是在自己内部生成 ID 的——先建 runner 再取 ID 会让证据写进
    // 一个名字对不上的文件。所以这里自己生成，再交给 runner 用。
    let clock = Arc::new(SystemClock);
    let session_id = qcode_harness::application::agent_run::new_session_id(&*clock);
    let evidence_path = session_path(&workspace, session_id.as_str());

    // 证据走两个汇：控制台播报给人看，JSONL 落盘给事后查。
    // 组合而不是二选一——运行时想看进度，出事后要能复盘，两者都需要。
    let jsonl = JsonlEvidenceSink::open(&evidence_path)?;
    let evidence = Arc::new(FanoutEvidenceSink::new(vec![
        Arc::new(jsonl),
        reporter.clone() as Arc<dyn EvidenceSink>,
    ]));

    let approvals: Arc<dyn ApprovalGate> = if auto_approve {
        Arc::new(AutoApproveGate)
    } else {
        Arc::new(CliApprovalGate::new())
    };

    let mut runner = AgentRunner::new(
        spec,
        session_id,
        args.task,
        AgentDependencies {
            model: Arc::new(RigChatGateway::new(model.erase())),
            tools: registry,
            approvals,
            evidence,
            clock,
            policy,
            context_budget: Default::default(),
        },
    );

    let mut budget = Budget::new(config.max_turns);
    if let Some(limit) = config.max_wall_clock {
        budget = budget.with_wall_clock(limit);
    }
    // 错误要按 Display 打印而不是交给 Box<dyn Error> 的 Debug：错误消息里的
    // 对策指引（"调大 LLM_MAX_TURNS"这类）是写给用户看的，Debug 格式会把
    // 它变成一个结构体转储。
    let outcome = match runner.run(&budget).await {
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

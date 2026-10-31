//! ReAct（Reason + Act）循环的入口：薄壳。
//!
//! 分工：
//! - rig 负责模型协议适配（OpenAI 兼容的 `/chat/completions`）
//!   与工具定义的序列化，封装在 `infrastructure::model`；
//! - ReAct 循环本身在 `application::agent_run::AgentRunner`；
//! - 本文件只做三件事：读环境变量、把各层实现组装起来、把结果打印出去。
//!
//! 循环里模型只提出"调用某个工具"的意图，文件读写都发生在
//! `infrastructure::tool::file` 中并限制在 workspace 内——这是 Harness
//! 掌控执行的根基。

mod application;
mod domain;
mod infrastructure;
mod interface;

use std::error::Error;
use std::sync::Arc;

use rig::providers::openai::OpenAIConfig;

use crate::application::agent_run::AgentRunner;
use crate::domain::agent::{AgentRole, AgentSpec};
use crate::domain::budget::Budget;
use crate::domain::workspace::WorkspaceRoot;
use crate::infrastructure::approval::auto::AutoApproveGate;
use crate::infrastructure::clock::SystemClock;
use crate::infrastructure::model::rig_chat::RigChatGateway;
use crate::infrastructure::tool::file::FileToolSet;
use crate::interface::cli::{self, ConsoleReporter};

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
}

impl LlmConfig {
    /// 从环境变量读取配置。
    ///
    /// 必需变量：`LLM_BASE_URL`、`LLM_API_KEY`、`LLM_MODEL`；
    /// 可选变量：`LLM_MAX_TURNS`（默认 20）。
    ///
    /// # Errors
    ///
    /// 缺少必需变量、或 `LLM_MAX_TURNS` 不是合法整数时返回说明性错误。
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
        Ok(Self {
            base_url: required("LLM_BASE_URL")?,
            api_key: required("LLM_API_KEY")?,
            model: required("LLM_MODEL")?,
            max_turns,
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

    // OpenAI 兼容端点必须走 chat()（/chat/completions）。
    // 不要用 completion()：它对 OpenAI 方言默认指向 Responses API（/responses），
    // DeepSeek 等兼容网关并不支持。
    let provider = OpenAIConfig::new(config.api_key.as_str())
        .with_base_url(config.base_url.as_str())
        .client();
    let model = provider.chat(config.model.as_str());

    let tools = FileToolSet::new(WorkspaceRoot::new(workspace));
    let spec = AgentSpec {
        role: AgentRole::Coder,
        system_prompt: SYSTEM_PROMPT.to_owned(),
        tools: FileToolSet::specs(),
    };

    println!("任务: {}", args.task);
    println!("模型: {} @ {}", config.model, config.base_url);
    println!(
        "工具: {}",
        spec.tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );

    let reporter = Arc::new(ConsoleReporter::new(config.max_turns));
    let mut runner = AgentRunner::new(
        spec,
        args.task,
        Arc::new(RigChatGateway::new(model.erase())),
        Arc::new(tools),
        Arc::new(AutoApproveGate),
        reporter.clone(),
        Arc::new(SystemClock),
    );

    let outcome = runner.run(&Budget::new(config.max_turns)).await?;
    println!("\n──── 最终答案 ────\n{}", outcome.answer);
    println!(
        "\n（会话 {} · 共 {} 轮 · 证据 {} 条）",
        runner.session_id().as_str(),
        outcome.turns_used,
        reporter.events().len()
    );
    Ok(())
}

//! 编译期插件：实现 Skill、Hook 和 Plugin，通过现有依赖接入运行。
//!
//! 运行：`cargo run --example custom-extension`。示例使用 rig 的脚本模型，
//! 无需模型凭据；read_file 与 list_dir 调用的是真实内置工具。
//!
//! Plugin 的 Rust 代码和宿主在同一进程内，属于受信代码。权限调度器检查其
//! 提交的工具动作，无法隔离插件自行编写的任意 Rust IO。数据清单不执行代码。

use std::sync::Arc;
use std::time::Duration;

use qcode_harness::application::agent_roster::AgentRoster;
use qcode_harness::application::agent_run::{AgentDependencies, AgentRunner};
use qcode_harness::application::context_policy::DefaultContextPolicy;
use qcode_harness::application::extension::{
    ExtensionRegistry, ExtensionSelection, Hook, HookEvent, Plugin, Skill,
};
use qcode_harness::application::memory_evidence::MemoryEvidenceSink;
use qcode_harness::application::port::NoopHooks;
use qcode_harness::application::tool_registry::{ToolRegistry, ToolSource};
use qcode_harness::domain::agent::{AgentRole, AgentSpec};
use qcode_harness::domain::budget::Budget;
use qcode_harness::domain::context::ContextBudget;
use qcode_harness::domain::extension::ExtensionDescriptor;
use qcode_harness::domain::ids::{CallId, SessionId};
use qcode_harness::domain::policy::PolicyEngine;
use qcode_harness::domain::tool::{OutcomeStatus, ToolCall};
use qcode_harness::domain::workspace::WorkspaceRoot;
use qcode_harness::infrastructure::approval::auto::AutoApproveGate;
use qcode_harness::infrastructure::clock::{SystemClock, TokioTimeLimiter};
use qcode_harness::infrastructure::model::rig_chat::RigChatGateway;
use qcode_harness::infrastructure::tool::BuiltinToolSource;
use rig::test_utils::{MockCompletionModel, MockTurn};
use serde_json::json;

struct ReadProject;

impl Skill for ReadProject {
    fn descriptor(&self) -> ExtensionDescriptor {
        ExtensionDescriptor {
            id: "read-project".to_owned(),
            description: "先阅读项目清单，再提出修改建议。".to_owned(),
            permissions: vec!["read_file".to_owned()],
        }
    }

    fn instructions(&self) -> &str {
        "先通过 read_file 阅读 Cargo.toml；说明所见依赖，建议必须能在文件中找到依据。"
    }
}

struct ListAfterRead;

impl Hook for ListAfterRead {
    fn descriptor(&self) -> ExtensionDescriptor {
        ExtensionDescriptor {
            id: "list-after-read".to_owned(),
            description: "读取项目清单后列出工作区根目录。".to_owned(),
            permissions: vec!["list_dir".to_owned()],
        }
    }

    fn plan(&self, event: HookEvent<'_>) -> Vec<ToolCall> {
        let HookEvent::AfterToolResult { call, outcome, .. } = event else {
            return Vec::new();
        };
        if call.name != "skill__read-project" || outcome.status != OutcomeStatus::Success {
            return Vec::new();
        }
        vec![ToolCall {
            id: CallId::new(format!("list__{}", call.id.as_str())),
            name: "list_dir".to_owned(),
            arguments: json!({"path": "."}),
        }]
    }
}

struct ProjectPlugin;

impl Plugin for ProjectPlugin {
    fn id(&self) -> &str {
        "project-plugin"
    }
    fn skills(&self) -> Vec<Arc<dyn Skill>> {
        vec![Arc::new(ReadProject)]
    }
    fn hooks(&self) -> Vec<Arc<dyn Hook>> {
        vec![Arc::new(ListAfterRead)]
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = WorkspaceRoot::new(std::env::current_dir()?);
    let builtin: Arc<dyn ToolSource> = Arc::new(BuiltinToolSource::new(root));
    let backing = ToolRegistry::from_sources(std::slice::from_ref(&builtin));
    let extensions = Arc::new(ExtensionRegistry::new(&backing));
    extensions.install_plugin(&ProjectPlugin)?;
    // 安装保持禁用，演示显式批准每个条目的实际工具名。
    extensions.enable("read-project", &["read_file".to_owned()])?;
    extensions.enable("list-after-read", &["list_dir".to_owned()])?;

    let mock = MockCompletionModel::from_turns(vec![
        MockTurn::tool_call(
            "read",
            "skill__read-project",
            json!({"tool": "read_file", "arguments": {"path": "Cargo.toml"}}),
        ),
        MockTurn::text("演示完成，文件读取结果已进入模型历史，Hook 的目录观察已记入证据。"),
    ]);
    let mut deps = AgentDependencies {
        model: Arc::new(RigChatGateway::new(mock.erase())),
        tools: backing,
        approvals: Arc::new(AutoApproveGate),
        evidence: Arc::new(MemoryEvidenceSink::new()),
        clock: Arc::new(SystemClock),
        policy: PolicyEngine::new(),
        additional_guards: Vec::new(),
        roster: AgentRoster::builtin(),
        context_budget: ContextBudget::default(),
        context_policy: Arc::new(DefaultContextPolicy),
        session_store: None,
        hooks: Arc::new(NoopHooks),
    };
    let selection = ExtensionSelection::all();
    let instructions = extensions.enabled_instructions(&selection)?;
    let adapters =
        extensions.adapters(
            &deps,
            AgentRole::Coder,
            selection,
            Arc::new(TokioTimeLimiter::new(Duration::from_secs(10))?),
        )?;
    deps.tools = ToolRegistry::from_sources(&[builtin, adapters.source]);
    deps.hooks = adapters.hooks;
    let mut runner = AgentRunner::new(
        AgentSpec::new(
            AgentRole::Coder,
            format!("{instructions}\n\n阅读项目清单。"),
        ),
        SessionId::new("custom-extension-example"),
        "读取 Cargo.toml",
        deps,
    );
    println!("{}", runner.run(&Budget::new(3)).await?.answer);
    Ok(())
}

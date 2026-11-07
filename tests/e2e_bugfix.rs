//! 端到端验收：让 agent 在一个隔离的破损仓库里修好一个失败的测试。
//!
//! 不需要 API key、不走网络：模型用 `MockCompletionModel` 脚本化，
//! 但工具全部是真的——真的读文件、真的替换、真的跑 `cargo test`。
//! 这是 P2 的总装验收：工具体系 + 注册表 + 策略 + 审批在一条真实
//! 路径上协同工作。
//!
//! 这个测试比单元测试慢（要在临时目录编译一个 crate，数秒），
//! 换来的是"修复流程确实可走通"的把握。

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use qcode_harness::application::agent_run::{AgentDependencies, AgentRunner};
use qcode_harness::application::agent_roster::AgentRoster;
use qcode_harness::application::context_policy::DefaultContextPolicy;
use qcode_harness::application::port::NoopHooks;
use qcode_harness::application::tool_registry::ToolRegistry;
use qcode_harness::domain::agent::{AgentRole, AgentSpec};
use qcode_harness::domain::budget::Budget;
use qcode_harness::domain::ids::SessionId;
use qcode_harness::domain::policy::PolicyEngine;
use qcode_harness::domain::workspace::WorkspaceRoot;
use qcode_harness::infrastructure::approval::auto::AutoApproveGate;
use qcode_harness::infrastructure::clock::SystemClock;
use qcode_harness::infrastructure::evidence::memory::MemoryEvidenceSink;
use qcode_harness::infrastructure::model::rig_chat::RigChatGateway;
use qcode_harness::infrastructure::tool::all_tools;

use rig::completion::Message;
use rig::message::UserContent;
use rig::test_utils::{MockCompletionModel, MockTurn};
use serde_json::json;

/// fixture 的 `src/lib.rs` 里的 bug 原文——edit_file 的 old_string 必须与它逐字节一致。
const BUGGY_LINE: &str = r#"format!("Hello, !")"#;
/// 修复后的样子。
const FIXED_LINE: &str = r#"format!("Hello, {name}!")"#;

/// 把 fixture 递归拷到临时目录（带进程号防并行冲突），返回拷贝根。
fn copy_fixture_to_temp() -> PathBuf {
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/broken-repo");
    let dest = std::env::temp_dir().join(format!(
        "qch-e2e-bugfix-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dest);
    copy_dir(&source, &dest).expect("拷贝 fixture");
    dest
}

/// 递归拷目录。跳过 target——万一有人在 fixture 里跑过 cargo，产物不该跟着走。
fn copy_dir(source: &Path, dest: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dest)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let name = entry.file_name();
        if name == "target" {
            continue;
        }
        let target = dest.join(&name);
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

/// 从 mock 的第 `index` 次请求历史里取最后一条 tool 结果文本。
fn last_tool_result(mock: &MockCompletionModel, index: usize) -> String {
    let requests = mock.requests();
    let history = &requests[index].chat_history;
    let Message::User { content } = history[history.len() - 1].as_ref() else {
        panic!("最后一条应是观察消息，实际是 {:?}", history.last());
    };
    let Some(UserContent::ToolResult(result)) = content.first() else {
        panic!("应当是 tool 结果内容");
    };
    result.content[0].as_text().unwrap().to_owned()
}

#[tokio::test]
async fn fixes_a_failing_test_in_an_isolated_repo() {
    let repo = copy_fixture_to_temp();

    // 确认 fixture 原样带着 bug（防止有人"顺手修好"了 fixture）。
    let lib_path = repo.join("src/lib.rs");
    let original = fs::read_to_string(&lib_path).unwrap();
    assert!(
        original.contains(BUGGY_LINE),
        "fixture 应该带着 bug，否则本测试没有意义"
    );

    // 四轮脚本：读 → 改 → 跑测试 → 总结。与一次真实修复的节奏一致。
    let mock = MockCompletionModel::from_turns(vec![
        MockTurn::tool_call("call_1", "read_file", json!({"path": "src/lib.rs"})),
        MockTurn::tool_call(
            "call_2",
            "edit_file",
            json!({"path": "src/lib.rs", "old_string": BUGGY_LINE, "new_string": FIXED_LINE}),
        ),
        MockTurn::tool_call("call_3", "run_command", json!({"command": "cargo test"})),
        MockTurn::text("已修复：greet 忘了拼名字，已改为 format!(\"Hello, {{name}}!\")，cargo test 通过。"),
    ]);

    // 全真的工具注册表：真实文件工具 + 真实 shell，不是测试替身。
    let registry = ToolRegistry::new(all_tools(WorkspaceRoot::new(&repo)));

    let mut runner = AgentRunner::new(
        AgentSpec {
            role: AgentRole::Coder,
            system_prompt: "修复失败的测试".to_owned(),
            model: None,
            effort: None,
            verbosity: None,
        },
        SessionId::new("e2e-bugfix"),
        "修复 broken-repo 里失败的测试",
        AgentDependencies {
            model: Arc::new(RigChatGateway::new(mock.clone().erase())),
            tools: registry,
            // edit_file / run_command 非只读 → 默认策略要审批；这里自动放行
            // 是集成测试的正常配置（没有 stdin 可问）。
            approvals: Arc::new(AutoApproveGate),
            evidence: Arc::new(MemoryEvidenceSink::new()),
            clock: Arc::new(SystemClock),
            policy: PolicyEngine::new(),
            roster: AgentRoster::builtin(),
            context_budget: Default::default(),
            session_store: None,
            hooks: Arc::new(NoopHooks),
            context_policy: Arc::new(DefaultContextPolicy),
        },
    );

    let outcome = runner.run(&Budget::new(10)).await.expect("运行应成功");

    // 1. 最终答案非空。
    assert!(!outcome.answer.trim().is_empty());
    assert!(outcome.answer.contains("已修复"), "{}", outcome.answer);
    assert_eq!(outcome.turns_used, 4);

    // 2. 源文件真的变了。
    let updated = fs::read_to_string(&lib_path).unwrap();
    assert!(
        updated.contains(FIXED_LINE),
        "lib.rs 应包含修复后的代码，实际:\n{updated}"
    );
    assert!(
        !updated.contains(BUGGY_LINE),
        "lib.rs 不应再包含 bug 原文，实际:\n{updated}"
    );

    // 3. run_command 的观察里带着 cargo test 的通过结果。
    //    第 4 次请求（index 3）的历史里最后一条就是 cargo test 的观察。
    let cargo_observation = last_tool_result(&mock, 3);
    assert!(
        cargo_observation.contains("test result: ok"),
        "cargo test 观察应包含 test result: ok，实际:\n{cargo_observation}"
    );

    // 清理临时仓库（编译产物不小，尽力删除即可）。
    let _ = fs::remove_dir_all(&repo);
}

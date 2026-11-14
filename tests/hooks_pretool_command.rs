//! 外部 command hook 的进程执行结果映射测试。
//!
//! 测试中直接调用 `PreToolUseHook::decide`，不经过 AgentRunner：
//! 命令退出码映射是 infrastructure 边界职责，失败必须显式拒绝。

use std::sync::Arc;
use std::time::Duration;

use qcode_harness::application::hooks::{PreToolUseHook, PreToolUseVerdict};
use qcode_harness::domain::ids::CallId;
use qcode_harness::domain::tool::ToolCall;
use qcode_harness::infrastructure::hooks_pretool::CommandPreToolUseHook;
use serde_json::json;

#[tokio::test]
async fn exit_code_2_is_a_denial() {
    let hook = Arc::new(CommandPreToolUseHook::new(vec![
        std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".to_owned()),
        "/C".to_owned(),
        "exit /b 2".to_owned(),
    ]));
    let call = ToolCall {
        id: CallId::new("call_1"),
        name: "read_file".to_owned(),
        arguments: json!({"path": "Cargo.toml"}),
    };
    assert_eq!(
        hook.decide(&call).await,
        PreToolUseVerdict::Deny("命令以退出码 2 拒绝工具调用".to_owned())
    );
}

#[tokio::test]
async fn launch_failure_is_fail_closed() {
    let hook = Arc::new(
        CommandPreToolUseHook::new(vec![
            "qcodeharness-hook-no-such-executable".to_owned()
        ])
        .with_timeout(Duration::from_secs(1)),
    );
    let call = ToolCall {
        id: CallId::new("call_1"),
        name: "read_file".to_owned(),
        arguments: json!({}),
    };
    let verdict = hook.decide(&call).await;
    assert!(
        matches!(verdict, PreToolUseVerdict::Deny(ref reason) if reason.contains("启动失败")),
        "启动失败应显式拒绝，实际：{verdict:?}"
    );
}

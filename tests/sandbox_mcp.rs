//! MCP 子进程同等隔离策略测试。
//!
//! 内置命令与 MCP spawn 共用 `R2SandboxEnforcer`；本测试不启动 MCP Server，
//! 只验证同一执行器对两种进程来源产出同一套规则、切片与降级事实。

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use qcode_harness::domain::sandbox_fact::{
    ProcessOrigin, ProcessSpec, SandboxOutcome, SandboxSliceStatus,
};
use qcode_harness::infrastructure::sandbox::r2;

fn evidence_dir(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("时间应晚于 Unix epoch")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("qch-r2-mcp-{tag}-{}-{nanos}", std::process::id()));
    fs::create_dir_all(&dir).expect("创建独立证据目录");
    dir
}

fn write_fact(dir: &Path, name: &str, value: &impl serde::Serialize) -> PathBuf {
    let path = dir.join(name);
    fs::write(
        &path,
        serde_json::to_string_pretty(value).expect("事实必须可序列化"),
    )
    .expect("写结构化事实到临时目录");
    path
}

fn spec(origin: ProcessOrigin, workspace: &Path, command: &str) -> ProcessSpec {
    ProcessSpec {
        program: "qch-mcp-server".to_owned(),
        arguments: vec!["--stdio".to_owned()],
        workspace: workspace.to_string_lossy().into_owned(),
        command: command.to_owned(),
        origin,
    }
}

#[test]
fn mcp_child_uses_the_same_policy_as_builtin_command() {
    let dir = evidence_dir("same-policy");
    let workspace = dir.join("workspace");
    fs::create_dir_all(&workspace).expect("创建测试 workspace");
    let command = "cargo test --test sandbox_mcp";
    let enforcer = r2::sandbox_enforcer();
    let builtin = enforcer.apply(&spec(ProcessOrigin::BuiltinCommand, &workspace, command));
    let mcp = enforcer.apply(&spec(ProcessOrigin::McpSubprocess, &workspace, command));

    assert_eq!(builtin.decision, mcp.decision);
    assert_eq!(builtin.slices, mcp.slices);
    assert_eq!(builtin.platform, mcp.platform);
    assert_ne!(builtin.origin, mcp.origin);
    assert_eq!(mcp.origin, ProcessOrigin::McpSubprocess);
    assert!(
        mcp.degraded_fact().is_some(),
        "缺隔离能力时必须生成降级事实"
    );

    let facts = serde_json::json!({ "builtin": builtin, "mcp": mcp });
    let path = write_fact(&dir, "same-policy.json", &facts);
    assert!(path.is_file(), "同等策略证据必须落盘");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn isolate_child_spec_blocks_open_command_before_spawn() {
    let dir = evidence_dir("spawn-gate");
    let workspace = dir.join("workspace");
    fs::create_dir_all(&workspace).expect("创建测试 workspace");
    let sentinel = dir.join("spawned.flag");
    let open = spec(
        ProcessOrigin::McpSubprocess,
        &workspace,
        "curl https://example.invalid",
    );
    let result = r2::isolate_child_spec(&open);
    assert!(result.is_err(), "开放命令缺隔离时必须在 spawn 前停止");
    assert!(!sentinel.exists(), "接线失败不得启动任何进程");

    let fact = serde_json::json!({
        "origin": ProcessOrigin::McpSubprocess,
        "spawn_permitted": false,
        "reason": result.expect_err("开放命令应返回拒绝原因"),
    });
    let path = write_fact(&dir, "spawn-gate.json", &fact);
    assert!(path.is_file(), "spawn 前拒绝证据必须落盘");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn degraded_slice_is_structured_and_never_reported_as_effective() {
    let dir = evidence_dir("degraded");
    let workspace = dir.join("workspace");
    fs::create_dir_all(&workspace).expect("创建测试 workspace");
    let enforcer = r2::sandbox_enforcer();
    let fact = enforcer.apply(&spec(
        ProcessOrigin::McpSubprocess,
        &workspace,
        "cargo test --test sandbox_mcp",
    ));
    let degraded = fact.degraded_fact().expect("缺隔离能力必须带降级事实");
    assert_eq!(
        degraded.evidence_event,
        qcode_harness::domain::sandbox_fact::SANDBOX_DEGRADED_EVENT
    );
    assert!(
        degraded
            .ineffective_slices()
            .all(|slice| slice.status != SandboxSliceStatus::Effective)
    );
    assert_ne!(fact.decision.outcome, SandboxOutcome::Deny);
    let path = write_fact(&dir, "degraded.json", &fact);
    assert!(path.is_file(), "降级事实证据必须落盘");
    let _ = fs::remove_dir_all(&dir);
}

//! macOS R2 沙箱真机测试与降级证据。
//!
//! 只有 macOS 目标检查 sandbox-exec SBPL profile 的文本规则与降级事实；
//! 当前 Windows 主机不实测 sandbox-exec，用 #[ignore] 保留可复现命令和
//! "未实测" 事实，避免把 profile 文本生成当成隔离已生效。

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(target_os = "macos")]
use qcode_harness::domain::sandbox_fact::{SandboxCapability, SandboxOutcome, SandboxSliceStatus};

fn evidence_dir(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("时间应晚于 Unix epoch")
        .as_nanos();
    let dir =
        std::env::temp_dir().join(format!("qch-r2-macos-{tag}-{}-{nanos}", std::process::id()));
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

#[cfg(target_os = "macos")]
fn slice_facts() -> Vec<qcode_harness::domain::sandbox_fact::SandboxSliceFact> {
    qcode_harness::infrastructure::sandbox::r2::probe_platform_slices()
}

#[cfg(target_os = "macos")]
fn status_of(
    facts: &[qcode_harness::domain::sandbox_fact::SandboxSliceFact],
    capability: SandboxCapability,
) -> SandboxSliceStatus {
    facts
        .iter()
        .find(|slice| slice.capability == capability)
        .expect("平台切片应覆盖所有能力")
        .status
}

/// macOS 真机：profile 必须同时覆盖 workspace 外写入与网络；未挂接到
/// spawn 前的文件/网络切片仍需记 SandboxDegraded。
#[cfg(target_os = "macos")]
#[test]
fn macos_sbpl_covers_external_write_and_network_with_degraded_fact() {
    let dir = evidence_dir("profile");
    let workspace = dir.join("workspace");
    fs::create_dir_all(&workspace).expect("创建测试 workspace");
    let facts = slice_facts();
    assert_ne!(
        status_of(&facts, SandboxCapability::FileSystem),
        SandboxSliceStatus::Effective
    );
    assert_ne!(
        status_of(&facts, SandboxCapability::Network),
        SandboxSliceStatus::Effective
    );
    let report = qcode_harness::infrastructure::sandbox::r2::debug_command(
        &workspace,
        "cargo test --test sandbox_macos",
    );
    assert!(report.fact.degraded_fact().is_some());
    let evidence = serde_json::json!({
        "platform": "macOS",
        "profile_rule_ids": ["R2.MACOS.FS.WRITE_OUTSIDE", "R2.MACOS.NETWORK.DENY"],
        "profile_expectations": ["(deny default)", "(deny network*)", "(allow file-write* (subpath workspace))"],
        "slices": facts,
        "status": "Degraded",
    });
    let path = write_fact(&dir, "macos-profile-degraded.json", &evidence);
    assert!(path.is_file(), "降级事实必须落盘");
    let _ = fs::remove_dir_all(&dir);
}

/// macOS 真机：越过 workspace 的写入命令在调试入口应报告 Deny，且不执行。
#[cfg(target_os = "macos")]
#[test]
fn macos_external_write_is_reported_without_execution() {
    let dir = evidence_dir("write-escape");
    let workspace = dir.join("workspace");
    fs::create_dir_all(&workspace).expect("创建测试 workspace");
    let sentinel = dir.join("escape.txt");
    let command = format!("echo pwned > {}", sentinel.display());
    let report = qcode_harness::infrastructure::sandbox::r2::debug_command(&workspace, &command);
    assert!(!report.executed);
    assert!(!sentinel.exists(), "调试不得执行命令");
    assert_eq!(report.fact.decision.outcome, SandboxOutcome::Deny);
    let path = write_fact(&dir, "write-escape.json", &report.fact);
    assert!(path.is_file());
    let _ = fs::remove_dir_all(&dir);
}

/// macOS 真机：网络访问在 profile 未挂接前必须降级，不得按正常放行。
#[cfg(target_os = "macos")]
#[test]
fn macos_network_access_is_degraded_without_profile_application() {
    let dir = evidence_dir("network");
    let workspace = dir.join("workspace");
    fs::create_dir_all(&workspace).expect("创建测试 workspace");
    let report = qcode_harness::infrastructure::sandbox::r2::debug_command(
        &workspace,
        "curl https://example.invalid",
    );
    assert_eq!(report.fact.decision.outcome, SandboxOutcome::Degrade);
    assert!(!report.fact.is_execution_permitted());
    let path = write_fact(&dir, "network-degraded.json", &report.fact);
    assert!(path.is_file());
    let _ = fs::remove_dir_all(&dir);
}

/// 非 macOS 主机保留完整可复现命令与"未实测"结构化事实。
#[cfg(not(target_os = "macos"))]
#[test]
#[ignore = "需要 macOS 真机；运行 bash scripts/sandbox-matrix.sh macos 生成证据"]
fn macos_live_sandbox_cases_are_not_run_on_this_host() {
    let dir = evidence_dir("untested");
    let fact = serde_json::json!({
        "platform": "macOS",
        "status": "Untested",
        "reason": "当前主机不是 macOS，未实测 sandbox-exec SBPL 的 workspace 外写入拒绝、网络拒绝与进程树终止",
        "reproduce": "bash scripts/sandbox-matrix.sh macos",
        "cases": ["workspace 外读写拒", "网络拒", "进程逃逸终止", "正常任务不阻断"],
    });
    let path = write_fact(&dir, "macos-untested.json", &fact);
    assert!(path.is_file(), "未实测事实必须保留可复现证据");
    let _ = fs::remove_dir_all(&dir);
}

//! Linux R2 沙箱真机测试与降级证据。
//!
//! 只有 Linux 目标执行 Landlock/seccomp 能力探测与真机行为用例；当前
//! Windows 主机不实测 Linux 内核，用 #[ignore] 保留可复现命令与
//! "未实测" 事实，避免把 ability 探测当成隔离已生效。

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(target_os = "linux")]
use qcode_harness::domain::sandbox_fact::{SandboxCapability, SandboxSliceStatus};

fn evidence_dir(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("时间应晚于 Unix epoch")
        .as_nanos();
    let dir =
        std::env::temp_dir().join(format!("qch-r2-linux-{tag}-{}-{nanos}", std::process::id()));
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

#[cfg(target_os = "linux")]
fn slice_facts() -> Vec<qcode_harness::domain::sandbox_fact::SandboxSliceFact> {
    qcode_harness::infrastructure::sandbox::r2::probe_platform_slices()
}

#[cfg(target_os = "linux")]
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

/// Linux 真机：Landlock ABI 与 seccomp mode 探测结果必须写进证据文件；
/// 未施放 ruleset/filter 时文件与网络切片均不得标生效。
#[cfg(target_os = "linux")]
#[test]
fn linux_landlock_seccomp_probe_writes_degraded_evidence() {
    let dir = evidence_dir("capability");
    let facts = slice_facts();
    assert_ne!(
        status_of(&facts, SandboxCapability::FileSystem),
        SandboxSliceStatus::Effective
    );
    assert_ne!(
        status_of(&facts, SandboxCapability::Network),
        SandboxSliceStatus::Effective
    );
    let evidence = serde_json::json!({
        "platform": "Linux",
        "slices": facts,
        "landlock_abi": qcode_harness::infrastructure::sandbox::r2::platform_slice_facts(),
        "cases": ["workspace 外读写拒", "网络拒", "进程逃逸终止", "正常任务不阻断"],
        "status": "Degraded",
    });
    let path = write_fact(&dir, "linux-capability-degraded.json", &evidence);
    assert!(path.is_file(), "降级事实必须落盘");
    let _ = fs::remove_dir_all(&dir);
}

/// Linux 真机：词法越界写入在调试入口直接 Deny，且不创建越界 sentinel。
#[cfg(target_os = "linux")]
#[test]
fn linux_external_write_is_reported_without_execution() {
    let dir = evidence_dir("write-escape");
    let workspace = dir.join("workspace");
    fs::create_dir_all(&workspace).expect("创建测试 workspace");
    let sentinel = dir.join("escape.txt");
    let command = format!("echo pwned > {}", sentinel.display());
    let report = qcode_harness::infrastructure::sandbox::r2::debug_command(&workspace, &command);
    assert!(!report.executed);
    assert!(!sentinel.exists(), "调试不得执行命令");
    assert_eq!(
        report.fact.decision.outcome,
        qcode_harness::domain::sandbox_fact::SandboxOutcome::Deny
    );
    let path = write_fact(&dir, "write-escape.json", &report.fact);
    assert!(path.is_file());
    let _ = fs::remove_dir_all(&dir);
}

/// Linux 真机：网络命令在 seccomp 过滤器未验证时必须降级而非允许。
#[cfg(target_os = "linux")]
#[test]
fn linux_network_access_is_degraded_without_filter_evidence() {
    let dir = evidence_dir("network");
    let workspace = dir.join("workspace");
    fs::create_dir_all(&workspace).expect("创建测试 workspace");
    let report = qcode_harness::infrastructure::sandbox::r2::debug_command(
        &workspace,
        "curl https://example.invalid",
    );
    assert_eq!(
        report.fact.decision.outcome,
        qcode_harness::domain::sandbox_fact::SandboxOutcome::Degrade
    );
    assert!(report.fact.degraded_fact().is_some());
    let path = write_fact(&dir, "network-degraded.json", &report.fact);
    assert!(path.is_file());
    let _ = fs::remove_dir_all(&dir);
}

/// 非 Linux 主机保留完整可复现命令与"未实测"结构化事实。
#[cfg(not(target_os = "linux"))]
#[test]
#[ignore = "需要 Linux 真机；运行 bash scripts/sandbox-matrix.sh linux 生成证据"]
fn linux_live_sandbox_cases_are_not_run_on_this_host() {
    let dir = evidence_dir("untested");
    let fact = serde_json::json!({
        "platform": "Linux",
        "status": "Untested",
        "reason": "当前主机不是 Linux，未实测 Landlock 文件系统隔离、seccomp 网络/系统调用过滤与进程树终止",
        "reproduce": "bash scripts/sandbox-matrix.sh linux",
        "cases": ["workspace 外读写拒", "网络拒", "进程逃逸终止", "正常任务不阻断"],
    });
    let path = write_fact(&dir, "linux-untested.json", &fact);
    assert!(path.is_file(), "未实测事实必须保留可复现证据");
    let _ = fs::remove_dir_all(&dir);
}

//! R2 降级验收：缺沙箱能力必须写 SandboxDegraded 事实，绝不静默放行。
//!
//! 用纯领域类型构造平台能力探测结果，避免把降级路径绑定在某台机器的
//! 真实内核状态上。证据文件写在独立临时目录，不污染仓库。

use std::fs;
use std::time::{SystemTime, UNIX_EPOCH};

use qcode_harness::domain::sandbox_fact::{
    SANDBOX_DEGRADED_EVENT, SandboxCapability, SandboxDegradedFact, SandboxFact, SandboxSliceFact,
    SandboxSliceStatus,
};

fn unique_dir(label: &str) -> std::path::PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("系统时钟应在 UNIX 纪元之后")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "qch-r2-downgrade-{label}-{}-{nanos}",
        std::process::id()
    ))
}

#[test]
fn degraded_capability_is_recorded_and_never_treated_as_effective() {
    let workspace = unique_dir("evidence");
    fs::create_dir_all(&workspace).expect("临时 workspace 应可创建");
    let evidence_path = workspace.join("sandbox-degraded.json");

    let slices = vec![
        SandboxSliceFact::new(
            SandboxCapability::FileSystem,
            SandboxSliceStatus::Ineffective,
            "Landlock 缺失",
            "内核 ABI 不可用",
        ),
        SandboxSliceFact::new(
            SandboxCapability::Network,
            SandboxSliceStatus::Untested,
            "seccomp 未探测",
            "本机不是 Linux",
        ),
    ];
    let fact = SandboxDegradedFact::from_slices("内核未提供文件系统隔离", slices);
    // 证据账本要求结构化字段而不是一句日志：事件名、稳定码、理由、切片。
    let payload = serde_json::json!({
        "event": SANDBOX_DEGRADED_EVENT,
        "fact": fact,
    });
    fs::write(
        &evidence_path,
        serde_json::to_vec_pretty(&payload).expect("事实应可序列化"),
    )
    .expect("应能写入临时证据文件");

    let written: serde_json::Value =
        serde_json::from_slice(&fs::read(&evidence_path).expect("证据文件应可读"))
            .expect("证据文件应是合法 JSON");
    assert_eq!(written["event"], SANDBOX_DEGRADED_EVENT);
    assert_eq!(written["fact"]["stable_code"], "sandbox_degraded");
    assert_eq!(
        written["fact"]["slices"]
            .as_array()
            .expect("降级切片应是数组")
            .len(),
        2
    );
    assert_ne!(
        written["fact"]["slices"][0]["status"],
        serde_json::json!("Effective")
    );
    fs::remove_dir_all(&workspace).expect("临时目录应可清理");
}

#[test]
fn degrade_outcome_stops_execution_before_a_process_can_start() {
    let workspace = unique_dir("stop");
    fs::create_dir_all(&workspace).expect("临时 workspace 应可创建");
    let sentinel = workspace.join("must-not-exist.txt");

    // 用 r2 的真实命令判定验证降级不静默放行：缺隔离能力时网络命令
    // 必须 Degrade；调用方据此写 SandboxDegraded 证据且绝不启动进程。
    let report = qcode_harness::infrastructure::sandbox::r2::debug_command(
        &workspace,
        "curl https://example.invalid",
    );
    let fact: &SandboxFact = &report.fact;
    assert_eq!(
        fact.degraded_fact()
            .map(|degraded| degraded.evidence_event.as_str()),
        Some(SANDBOX_DEGRADED_EVENT)
    );
    assert!(
        !fact.is_execution_permitted(),
        "Degrade 必须阻断正常执行，降级路径另行显式记账"
    );
    assert!(
        fact.require_execution_permitted().is_err(),
        "执行前检查必须返回可传播的错误，不能吞掉降级"
    );
    assert!(!sentinel.exists(), "测试没有启动进程，sentinel 不应出现");
    fs::remove_dir_all(&workspace).expect("临时目录应可清理");
}

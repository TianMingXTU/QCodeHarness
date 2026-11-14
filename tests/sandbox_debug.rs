//! R2 沙箱调试入口的行为测试。
//!
//! 用工作区外的 sentinel 文件做反证：调试接口一旦执行命令就会留下文件。
//! 临时目录与文件名带 PID 和纳秒时间戳，避免并行测试互相干扰。

use std::fs;
use std::time::{SystemTime, UNIX_EPOCH};

use qcode_harness::domain::sandbox_fact::SandboxOutcome;
use qcode_harness::infrastructure::sandbox::r2;

/// `--sandbox-debug` 背后的纯逻辑只报告规则命中，不启动命令。
#[test]
fn sandbox_debug_reports_hit_rules_without_executing_command() {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("系统时钟早于 UNIX_EPOCH")
        .as_nanos();
    let tag = format!("qch-r2-debug-{}-{nanos}", std::process::id());
    let workspace = std::env::temp_dir().join(format!("{tag}-workspace"));
    let sentinel = std::env::temp_dir().join(format!("{tag}.sentinel"));
    fs::create_dir_all(&workspace).expect("创建独立测试工作区");
    let command = format!("echo sandbox-debug > {}", sentinel.display());

    let report = r2::debug_command(&workspace, &command);

    assert!(!report.executed, "调试报告不能声称命令已执行");
    assert!(!sentinel.exists(), "调试路径创建了 sentinel 文件");
    assert_eq!(report.fact.decision.outcome, SandboxOutcome::Deny);
    assert!(
        report
            .fact
            .decision
            .matched_rules
            .iter()
            .any(|hit| hit.rule_id == "R2.FS.WRITE_ESCAPE"),
        "报告应列出命中 workspace 外写入规则，实际命中：{:?}",
        report.fact.decision.matched_rules
    );
    fs::remove_dir_all(&workspace).expect("清理独立测试工作区");
}

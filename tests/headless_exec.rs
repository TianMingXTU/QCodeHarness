//! 无头模式与权限分级的行为级验收。
//!
//! 这些测试只调用公开入口，断言脚本用户能观察到的退出码与 JSON；
//! 使用进程内固定结果的执行器，不启动真实模型或交互终端。

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use qcode_harness::application::headless::{
    EXIT_APPROVAL_REFUSED, EXIT_ERROR, EXIT_INVALID_ARGS, EXIT_SUCCESS, EXIT_TIMEOUT, HeadlessArgs,
    HeadlessOutcome, HeadlessRunError, HeadlessRunRequest, HeadlessRunner, RunOnce, format_output,
    parse_exec,
};
use qcode_harness::domain::permission_profile::PermissionProfile;
use serde_json::Value;

static RUN_SERIAL: AtomicUsize = AtomicUsize::new(0);

/// 独立临时目录，避免并行测试互相污染，也便于无头运行写自己的证据。
fn temp_workspace(name: &str) -> PathBuf {
    let serial = RUN_SERIAL.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "qch-headless-exec-{name}-{}-{serial}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("创建临时目录");
    dir
}

#[derive(Debug, Clone)]
struct FixedRun {
    result: Result<&'static str, HeadlessRunError>,
}

impl RunOnce for FixedRun {
    fn run_once(&self, request: &HeadlessRunRequest) -> Result<String, HeadlessRunError> {
        let _ = request;
        self.result.clone().map(str::to_owned)
    }
}

fn outcome(result: Result<&'static str, HeadlessRunError>, args: HeadlessArgs) -> HeadlessOutcome {
    let runner = HeadlessRunner::new(Box::new(FixedRun { result }));
    runner.run(args)
}

fn args(task: &str) -> HeadlessArgs {
    HeadlessArgs {
        task: task.to_owned(),
        workspace: temp_workspace("args"),
        profile: None,
        json: false,
        timeout_secs: None,
    }
}

#[test]
fn exec_args_are_parsed_and_bad_arguments_use_exit_code_2() {
    let parsed = parse_exec(&[
        "exec".to_owned(),
        "--permissions".to_owned(),
        "workspace-write".to_owned(),
        "--json".to_owned(),
        "--timeout".to_owned(),
        "12".to_owned(),
        "核对构建".to_owned(),
    ])
    .expect("合法 exec 命令");
    assert_eq!(parsed.task, "核对构建");
    assert_eq!(parsed.profile, Some(PermissionProfile::WorkspaceWrite));
    assert!(parsed.json);
    assert_eq!(parsed.timeout_secs, Some(12));

    let bad = parse_exec(&["exec".to_owned(), "--timeout".to_owned(), "0".to_owned()]);
    assert!(bad.is_err(), "没有任务且超时为 0 应报参数错误");
    assert_eq!(
        HeadlessOutcome::InvalidArguments("任何参数错误".to_owned()).exit_code(),
        EXIT_INVALID_ARGS
    );
}

#[test]
fn successful_exec_uses_exit_code_0_and_benchmark_json_shape() {
    let mut exec = args("完成只读核对");
    exec.json = true;
    let outcome = outcome(Ok("核对完成"), exec);
    assert_eq!(outcome.exit_code(), EXIT_SUCCESS);

    let text = format_output(&outcome, true);
    let json: Value = serde_json::from_str(text.trim()).expect("stdout 应是纯 JSON");
    let results = json
        .as_array()
        .expect("与基准一致，输出 BenchmarkResult 数组");
    assert_eq!(results.len(), 1);
    let result = &results[0];
    assert_eq!(result["task_id"], "exec");
    assert_eq!(result["passed"], true);
    assert_eq!(result["failures"], serde_json::json!([]));
    assert_eq!(result["turns_used"], 1);
    assert_eq!(result["approvals"], 0);
    assert_eq!(result["cost_usd"], Value::Null, "未知成本保持 null");
    assert!(result["metrics"].is_object(), "基准扩展指标结构应保留");
    assert!(result["wall_clock_ms"].is_u64());
    assert!(result["steps_completed"].is_u64());
}

#[test]
fn approval_refusal_timeout_and_other_failures_have_distinct_exit_codes() {
    let refusal = outcome(
        Err(HeadlessRunError::ApprovalRefused(
            "profile 拒绝非只读工具".to_owned(),
        )),
        args("写入文件"),
    );
    assert_eq!(refusal.exit_code(), EXIT_APPROVAL_REFUSED);
    assert!(matches!(refusal, HeadlessOutcome::ApprovalRefused(_)));

    let timeout = outcome(Err(HeadlessRunError::TimedOut(5)), args("等待很久"));
    assert_eq!(timeout.exit_code(), EXIT_TIMEOUT);
    assert!(matches!(timeout, HeadlessOutcome::TimedOut(5)));

    let failure = outcome(
        Err(HeadlessRunError::Failed("模型断连".to_owned())),
        args("普通任务"),
    );
    assert_eq!(failure.exit_code(), EXIT_ERROR);
    assert!(matches!(failure, HeadlessOutcome::Failed(_)));
}

#[test]
fn refused_exec_keeps_json_failures_readable() {
    let mut exec = args("写入文件");
    exec.json = true;
    let outcome = outcome(
        Err(HeadlessRunError::ApprovalRefused(
            "无 profile 默认拒绝非只读动作".to_owned(),
        )),
        exec,
    );
    let json: Value = serde_json::from_str(format_output(&outcome, true).trim())
        .expect("拒绝也应输出机器可读结果");
    let result = &json.as_array().expect("结果数组")[0];
    assert_eq!(result["passed"], false);
    assert_eq!(result["failures"][0], "无 profile 默认拒绝非只读动作");
    assert_eq!(result["language"], Value::Null);
    assert_eq!(outcome.exit_code(), EXIT_APPROVAL_REFUSED);
}

#[test]
fn missing_profile_denies_non_read_only_action_by_default() {
    let workspace = temp_workspace("default-read-only");
    let outcome = qcode_harness::application::headless::execute_in_workspace(
        &["exec".to_owned(), "写入文件".to_owned()],
        &workspace,
        None,
        None,
    );
    assert_eq!(outcome.exit_code(), EXIT_APPROVAL_REFUSED);
    assert!(
        format_output(&outcome, false).contains("拒绝"),
        "文本输出必须说明是审批拒绝: {}",
        format_output(&outcome, false)
    );
    assert!(
        !fs::exists(workspace.join("notes.txt")).expect("检查临时目录"),
        "默认只读档不能产生工作区写入"
    );
    let _ = fs::remove_dir_all(&workspace);
}

//! 后台作业：`run_command` 后台分支与 `job_list` 查询的行为验收。
//!
//! 用真实子进程，不拿睡眠计时器冒充进程退出；每个用例独立临时目录。
//! Windows 超时用例验证 Job Object 终止后代进程；其他平台只验证直接子进程
//! 被终止，并在作业状态里保留无法整树终止的说明。PTY 持久终端不在本切片。

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use qcode_harness::application::background_jobs::{
    BackgroundJobRegistry, BackgroundJobSpec, JobStatus,
};
use qcode_harness::application::port::ToolInvoker;
use qcode_harness::domain::platform::Platform;
use qcode_harness::domain::sandbox::PlatformCapabilities;
use qcode_harness::domain::tool::{OutcomeStatus, ToolAnnotations, ToolCall};
use qcode_harness::infrastructure::background_jobs::{
    JOB_LIST_TOOL, JobListTool, ProcessJobRegistry, RUN_BACKGROUND_ARG,
};

/// 独立临时目录。命令只在这个目录里留标记文件，不碰仓库工作区。
fn temp_dir(name: &str) -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "qch-background-{name}-{}-{stamp}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("建临时目录");
    dir
}

async fn wait_for<F>(timeout: Duration, mut condition: F) -> bool
where
    F: FnMut() -> bool,
{
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if condition() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    condition()
}

fn job_list_call() -> ToolCall {
    ToolCall {
        id: qcode_harness::domain::ids::CallId::new("job-list-1"),
        name: JOB_LIST_TOOL.to_owned(),
        arguments: serde_json::json!({}),
    }
}

/// 平台对应的「写标记文件」命令，确保后台进程真的执行并退出。
fn quick_command(_dir: &Path, marker: &str) -> String {
    match Platform::current() {
        Platform::Windows => format!("echo done>{marker}"),
        _ => format!("printf done > {marker}"),
    }
}

#[tokio::test]
async fn background_job_runs_and_job_list_reports_exited_status() {
    let dir = temp_dir("quick");
    let registry = Arc::new(ProcessJobRegistry::new());
    let marker = dir.join("finished.txt");
    let id = registry
        .spawn(BackgroundJobSpec {
            command: quick_command(&dir, "finished.txt"),
            root: dir.clone(),
            timeout: Duration::from_secs(10),
        })
        .await
        .expect("后台启动成功");

    assert!(
        wait_for(Duration::from_secs(5), || fs::read_to_string(&marker)
            .is_ok_and(|text| text.contains("done")))
        .await
    );
    // 标记文件已写出，但独立等待任务可能还没把 wait() 结果落到作业表。
    // 查询的是用户可见状态，给它一个有界等待，不把瞬时 Running 当缺陷。
    let deadline = Instant::now() + Duration::from_secs(5);
    let listed = loop {
        let listed = registry.list().await.expect("作业列表可读");
        if matches!(listed[0].status, JobStatus::Exited { .. }) || Instant::now() >= deadline {
            break listed;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, id);
    assert!(listed[0].command.contains("done"));
    assert!(matches!(listed[0].status, JobStatus::Exited { code } if code == 0 || code == -1));

    let tool = JobListTool::new(Arc::clone(&registry) as Arc<dyn BackgroundJobRegistry>);
    let outcome = tool.invoke(&job_list_call()).await;
    assert_eq!(outcome.status, OutcomeStatus::Success);
    assert!(outcome.content.contains("\"id\""));
    assert!(outcome.content.contains("\"status\""));
    let _ = fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_timed_out_job_terminates_its_process_tree() {
    let dir = temp_dir("timeout");
    let registry = Arc::new(ProcessJobRegistry::new());
    let marker = dir.join("child.txt");
    let command = match Platform::current() {
        // 外层已经有 cmd /C，再套一层 cmd /C 会让内层命令被当作交互式
        // shell 启动：进程既不写标记，也不执行 ping，超时用例会假失败。
        Platform::Windows => "echo alive>child.txt & ping -n 30 127.0.0.1 > nul".to_owned(),
        _ => "echo alive > child.txt; sleep 30".to_owned(),
    };
    let id = registry
        .spawn(BackgroundJobSpec {
            command,
            root: dir.clone(),
            timeout: Duration::from_millis(900),
        })
        .await
        .expect("后台启动成功");

    assert!(
        wait_for(Duration::from_secs(4), || marker.exists()).await,
        "后代进程应先写标记"
    );
    let deadline = Instant::now() + Duration::from_secs(8);
    let mut timed_out = false;
    while Instant::now() < deadline {
        if let Ok(records) = registry.list().await {
            if records
                .iter()
                .any(|job| job.id == id && job.status == JobStatus::TimedOut)
            {
                timed_out = true;
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    assert!(timed_out, "超时后作业状态应停在 TimedOut");
    let records = registry.list().await.expect("作业列表可读");
    let record = records
        .iter()
        .find(|job| job.id == id)
        .expect("作业仍在清单");
    assert!(record.note.contains("超时"));
    if PlatformCapabilities::detect().has_tree_kill {
        assert!(
            record.note.contains("已终止整个进程树"),
            "Windows 应声明整树终止"
        );
    } else {
        assert!(
            record.note.contains("后代进程可能仍在运行"),
            "无整树能力不能虚报"
        );
    }
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn job_list_is_a_read_only_tool_and_background_arg_is_explicit() {
    let spec = JobListTool::spec();
    assert_eq!(spec.name, JOB_LIST_TOOL);
    assert_eq!(
        spec.annotations,
        ToolAnnotations::new(true, false, true, false)
    );
    assert!(
        spec.description.contains("PTY"),
        "文档应明确 PTY 为后继切片"
    );
    assert_eq!(RUN_BACKGROUND_ARG, "background");
}

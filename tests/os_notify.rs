//! R1 OS 通知的集成测试。
//!
//! 临时目录中的假通知脚本负责记录调用；断言的是默认关闭、两类事件、
//! 超时后的进程清理和 OSC 9 序列这些外部可见行为。

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime};

use qcode_harness::application::notification::{
    NoopNotificationSink, NotificationConfig, NotificationEvent, NotificationPolicy,
    NotificationSink,
};
use qcode_harness::infrastructure::notification::{
    CommandNotificationSink, Osc9NotificationSink, osc9_sequence,
};

static CASE: AtomicUsize = AtomicUsize::new(0);

fn temp_dir(label: &str) -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .expect("系统时间早于 Unix 纪元")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "qch-os-notify-{label}-{}-{}-{}",
        std::process::id(),
        stamp,
        CASE.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir_all(&dir).expect("创建独立临时目录");
    dir
}

/// 参数保持为独立 argv，脚本把每次调用的 argv 追加到日志文件。
fn fake_script(dir: &Path) -> (String, Vec<String>, PathBuf) {
    let log = dir.join("calls.txt");
    let marker = dir.join("marker.txt");
    let script = dir.join("fake_notify.py");
    let source = format!(
        "import sys, pathlib, time\n\
         log = pathlib.Path({log:?})\n\
         marker = pathlib.Path({marker:?})\n\
         with log.open('a', encoding='utf-8') as f:\n\
         \x20   f.write(repr(sys.argv[1:]) + '\\n')\n\
         if '--sleep' in sys.argv:\n\
         \x20   marker.write_text('alive', encoding='utf-8')\n\
         \x20   time.sleep(30)\n",
        log = log.to_string_lossy(),
        marker = marker.to_string_lossy(),
    );
    fs::write(&script, source).expect("写假通知脚本");
    (
        python_program(),
        vec![script.to_string_lossy().into_owned()],
        log,
    )
}

fn python_program() -> String {
    for name in ["python", "python3", "py"] {
        if std::process::Command::new(name)
            .arg("--version")
            .output()
            .is_ok()
        {
            return name.to_owned();
        }
    }
    panic!("测试需要可执行的 Python 解释器");
}

fn policy_for(program: &str, args: &[String], timeout: Duration) -> NotificationPolicy {
    NotificationPolicy {
        enabled: true,
        command: NotificationConfig {
            program: program.to_owned(),
            args: args.to_vec(),
            timeout,
        },
    }
}

fn calls(log: &Path) -> Vec<String> {
    match fs::read_to_string(log) {
        Ok(text) => text.lines().map(str::to_owned).collect(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => panic!("读取调用日志失败: {error}"),
    }
}

#[tokio::test]
async fn default_policy_makes_no_external_calls() {
    let dir = temp_dir("default");
    let (program, args, log) = fake_script(&dir);
    let sink =
        CommandNotificationSink::new(NotificationPolicy::default(), program.clone(), args.clone())
            .await
            .expect("构造命令通知汇");

    sink.notify(NotificationEvent::task_completed("任务一"))
        .await
        .expect("默认关闭时不执行外部命令");
    sink.notify(NotificationEvent::approval_waiting("read_file"))
        .await
        .expect("默认关闭时不执行外部命令");

    assert!(calls(&log).is_empty(), "默认关闭必须零调用");
    assert!(!NotificationPolicy::default().enabled);
    let noop = NoopNotificationSink;
    noop.notify(NotificationEvent::task_completed("noop"))
        .await
        .expect("Noop 通知汇不产生副作用");
    fs::remove_dir_all(dir).expect("清理临时目录");
}

#[tokio::test]
async fn enabled_policy_sends_both_event_types_as_separate_argv() {
    let dir = temp_dir("events");
    let (program, args, log) = fake_script(&dir);
    let policy = policy_for(&program, &args, Duration::from_secs(5));
    let sink = CommandNotificationSink::new(policy, program, args)
        .await
        .expect("构造命令通知汇");

    sink.notify(NotificationEvent::task_completed("任务 二"))
        .await
        .expect("任务完成通知");
    sink.notify(NotificationEvent::approval_waiting("run_command"))
        .await
        .expect("审批等待通知");

    let lines = calls(&log);
    assert_eq!(lines.len(), 2, "两类事件各触发一次: {lines:?}");
    assert!(
        lines[0].contains("task_completed"),
        "事件类型应作为 argv: {}",
        lines[0]
    );
    assert!(
        lines[0].contains("任务 二"),
        "摘要应作为独立 argv: {}",
        lines[0]
    );
    assert!(
        lines[1].contains("approval_waiting"),
        "事件类型应作为 argv: {}",
        lines[1]
    );
    assert!(
        lines[1].contains("run_command"),
        "工具名应作为独立 argv: {}",
        lines[1]
    );
    for line in &lines {
        assert!(!line.contains("--sleep"), "事件不能拼成 shell 命令");
    }
    fs::remove_dir_all(dir).expect("清理临时目录");
}

#[tokio::test]
async fn timed_out_notification_leaves_no_running_process() {
    let dir = temp_dir("timeout");
    let (program, args, log) = fake_script(&dir);
    let marker = dir.join("marker.txt");
    let mut hanging_args = args.clone();
    hanging_args.push("--sleep".to_owned());
    let sink = CommandNotificationSink::new(
        policy_for(&program, &hanging_args, Duration::from_millis(400)),
        program,
        hanging_args,
    )
    .await
    .expect("构造命令通知汇");

    let error = sink
        .notify(NotificationEvent::task_completed("慢通知"))
        .await
        .expect_err("超时必须明确报错，不能静默返回成功");
    assert!(error.contains("超时"), "错误应说明超时: {error}");
    assert!(marker.exists(), "假脚本应确实启动并进入睡眠");
    let pid: u32 = fs::read_to_string(dir.join("child.pid"))
        .expect("实现应记录直接子进程 PID 供行为测试检查")
        .trim()
        .parse()
        .expect("PID 应为数字");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!process_alive(pid), "超时后直接子进程仍在运行 (pid={pid})");
    assert_eq!(calls(&log).len(), 1, "超时通知只应启动一次");
    fs::remove_dir_all(dir).expect("清理临时目录");
}

#[cfg(windows)]
fn process_alive(pid: u32) -> bool {
    use windows::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) };
    match handle {
        Ok(handle) => {
            unsafe { let _ = windows::Win32::Foundation::CloseHandle(handle); };
            true
        }
        Err(_) => false,
    }
}

#[cfg(not(windows))]
fn process_alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

#[test]
fn osc9_sequence_is_a_pure_terminal_escape() {
    let sequence = osc9_sequence(NotificationEvent::task_completed("完成"));
    assert!(
        sequence.starts_with("\u{1b}]9;"),
        "OSC 9 必须由 ESC ] 9 ; 起头: {sequence:?}"
    );
    assert!(
        sequence.ends_with("\u{7}"),
        "OSC 9 必须用 BEL 收尾: {sequence:?}"
    );
    assert!(
        sequence.contains("完成"),
        "序列应包含通知摘要: {sequence:?}"
    );
    assert_eq!(
        sequence.matches('\u{1b}').count(),
        1,
        "序列不能混入其他转义指令"
    );
}

#[tokio::test]
async fn osc9_sink_writes_sequences_to_an_injected_writer() {
    let writer = Arc::new(qcode_harness::infrastructure::notification::MemoryTerminal::default());
    let sink =
        Osc9NotificationSink::new(Arc::clone(&writer)
            as Arc<dyn qcode_harness::application::notification::TerminalWriter>);
    sink.notify(NotificationEvent::task_completed("TUI 任务"))
        .await
        .expect("TUI 通知写入");
    sink.notify(NotificationEvent::approval_waiting("write_file"))
        .await
        .expect("TUI 通知写入");
    let output = writer.output();
    assert_eq!(output.len(), 2, "两类事件各写一条 OSC 9: {output:?}");
    assert!(output[0].starts_with("\u{1b}]9;") && output[0].ends_with('\u{7}'));
    assert!(output[1].contains("write_file"));
}

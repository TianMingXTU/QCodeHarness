//! Windows R2 沙箱真实行为测试与证据文件产出。
//!
//! Windows 本机只验证既有 Job Object 的进程树终止，以及文件/网络隔离
//! 切片如实标为"未生效"；不把词法路径检查伪装成 OS 级隔离。证据写到
//! 独立临时目录，仓库不留产物。

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(target_os = "windows")]
use std::time::{Duration, Instant};

#[cfg(target_os = "windows")]
use qcode_harness::domain::sandbox_fact::{SandboxCapability, SandboxSliceStatus};
#[cfg(target_os = "windows")]
use windows::Win32::Foundation::{CloseHandle, HANDLE};
#[cfg(target_os = "windows")]
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject, TerminateJobObject,
};
#[cfg(target_os = "windows")]
use windows::Win32::System::Threading::{
    GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
};

fn evidence_dir(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("时间应晚于 Unix epoch")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("qch-r2-{tag}-{}-{nanos}", std::process::id()));
    fs::create_dir_all(&dir).expect("创建独立证据目录");
    dir
}

fn write_fact(dir: &Path, name: &str, value: &impl serde::Serialize) -> PathBuf {
    let path = dir.join(name);
    let text = serde_json::to_string_pretty(value).expect("事实必须可序列化");
    fs::write(&path, text).expect("写结构化事实到临时目录");
    path
}

fn fact_of(
    slices: &[qcode_harness::domain::sandbox_fact::SandboxSliceFact],
    capability: SandboxCapability,
) -> qcode_harness::domain::sandbox_fact::SandboxSliceFact {
    slices
        .iter()
        .find(|slice| slice.capability == capability)
        .cloned()
        .expect("平台切片应覆盖所有能力")
}

/// --doctor 与 --sandbox-debug 必须区分"进程树终止生效"和"文件/网络隔离未生效"。
#[test]
fn windows_job_object_is_effective_but_file_and_network_slices_are_not() {
    let dir = evidence_dir("windows-capability");
    let slices = qcode_harness::infrastructure::sandbox::r2::probe_platform_slices();
    let tree_kill = fact_of(&slices, SandboxCapability::ProcessTreeKill);
    let filesystem = fact_of(&slices, SandboxCapability::FileSystem);
    let network = fact_of(&slices, SandboxCapability::Network);
    assert_eq!(tree_kill.status, SandboxSliceStatus::Effective);
    assert_eq!(filesystem.status, SandboxSliceStatus::Ineffective);
    assert_eq!(network.status, SandboxSliceStatus::Ineffective);
    let path = write_fact(&dir, "capability.json", &slices);
    assert!(path.is_file(), "本机探测证据必须落盘");
    let _ = fs::remove_dir_all(&dir);
}

/// 正常任务不因新增规则判定被阻断；纯逻辑判定只返回事实，不启动进程。
#[test]
fn workspace_local_baseline_command_is_not_blocked_by_r2_policy() {
    let dir = evidence_dir("windows-baseline");
    let workspace = dir.join("workspace");
    fs::create_dir_all(&workspace).expect("创建测试 workspace");
    let report = qcode_harness::infrastructure::sandbox::r2::debug_command(
        &workspace,
        "cargo test --test sandbox_windows",
    );
    assert!(!report.executed, "调试不得执行命令");
    assert!(
        report
            .fact
            .slices
            .iter()
            .any(|slice| slice.status != SandboxSliceStatus::Effective),
        "当前 Windows 文件/网络隔离仍须如实降级"
    );
    assert!(
        report.fact.degraded_fact().is_some(),
        "降级事实必须进入证据账本"
    );
    let path = write_fact(&dir, "baseline.json", &report.fact);
    assert!(path.is_file(), "基线判定证据必须落盘");
    let _ = fs::remove_dir_all(&dir);
}

/// 词法写入越界硬拒绝，调试不执行，sentinel 文件不得出现。
#[test]
fn workspace_external_write_is_reported_and_not_executed() {
    let dir = evidence_dir("windows-write-escape");
    let workspace = dir.join("workspace");
    fs::create_dir_all(&workspace).expect("创建测试 workspace");
    let sentinel = dir.join("escape.txt");
    let command = format!("echo pwned > {}", sentinel.display());
    let report = qcode_harness::infrastructure::sandbox::r2::debug_command(&workspace, &command);
    assert!(!report.executed);
    assert!(!sentinel.exists(), "调试入口不得执行命令写越界文件");
    assert_eq!(
        report.fact.decision.outcome,
        qcode_harness::domain::sandbox_fact::SandboxOutcome::Deny
    );
    assert!(
        report
            .fact
            .decision
            .matched_rules
            .iter()
            .any(|hit| hit.rule_id
                == qcode_harness::infrastructure::sandbox::r2::RULE_FS_WRITE_ESCAPE)
    );
    let path = write_fact(&dir, "write-escape.json", &report.fact);
    assert!(path.is_file(), "越界拒绝证据必须落盘");
    let _ = fs::remove_dir_all(&dir);
}

/// 网络命令在文件/网络隔离未生效时不得按正常放行处理。
#[test]
fn network_access_without_os_isolation_is_degraded_not_allowed() {
    let dir = evidence_dir("windows-network");
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
    assert!(!report.fact.is_execution_permitted());
    assert!(report.fact.degraded_fact().is_some());
    let path = write_fact(&dir, "network-degraded.json", &report.fact);
    assert!(path.is_file(), "网络降级证据必须落盘");
    let _ = fs::remove_dir_all(&dir);
}

/// 用真实 Job Object 验证进程逃逸终止：子进程被纳入 Job 后，Job 终止
/// 会连同其后创建的整棵进程树一起结束。孙进程创建顺序由标记文件确定。
#[cfg(target_os = "windows")]
#[test]
fn job_object_terminates_descendant_process_tree() {
    use std::os::windows::io::AsRawHandle;
    use std::os::windows::process::CommandExt;

    const STILL_ACTIVE: u32 = 259;
    let dir = evidence_dir("windows-tree-kill");
    let go = dir.join("go.flag");
    let pid_file = dir.join("child.pid");
    let marker = dir.join("marker.txt");
    let script = format!(
        "while (-not (Test-Path -LiteralPath '{}')) {{ Start-Sleep -Milliseconds 50 }}; \
         $p = Start-Process -FilePath cmd -ArgumentList '/C','ping -n 300 127.0.0.1 > nul' \
         -NoNewWindow -PassThru; \
         Set-Content -LiteralPath '{}' -Value $p.Id -Encoding ascii; \
         Wait-Process -Id $p.Id",
        go.display(),
        pid_file.display(),
    );
    let mut launcher = Command::new("powershell")
        .args([
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            &script,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(0x0000_0200) // CREATE_NEW_PROCESS_GROUP：launcher 不继承测试进程的 Job。
        .spawn()
        .expect("启动 PowerShell launcher");

    let job = unsafe { CreateJobObjectW(None, None) }.expect("创建 Job Object");
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    unsafe {
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &limits as *const _ as _,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    }
    .expect("设置 Job Object 整树终止");
    unsafe { AssignProcessToJobObject(job, HANDLE(launcher.as_raw_handle())) }
        .expect("把 launcher 纳入 Job Object");
    // 纳入 Job 之后再放行，孙进程必然继承 Job 成员身份。
    fs::write(&go, b"go").expect("放行 launcher 创建孙进程");

    let deadline = Instant::now() + Duration::from_secs(30);
    let child_pid = loop {
        if let Ok(text) = fs::read_to_string(&pid_file)
            && let Ok(pid) = text.trim().parse::<u32>()
        {
            break pid;
        }
        assert!(Instant::now() < deadline, "孙进程 PID 未在 30 秒内写出");
        std::thread::sleep(Duration::from_millis(50));
    };
    unsafe { TerminateJobObject(job, 1) }.expect("终止 Job Object 整树");
    let _ = launcher.wait();
    let deadline = Instant::now() + Duration::from_secs(30);
    let alive_after_terminate = loop {
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, child_pid) };
        let alive = match handle {
            Ok(handle) => {
                let mut code = 0_u32;
                let queried = unsafe { GetExitCodeProcess(handle, &mut code) };
                let _ = unsafe { CloseHandle(handle) };
                queried.is_ok() && code == STILL_ACTIVE
            }
            Err(_) => false,
        };
        if !alive || Instant::now() >= deadline {
            break alive;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let _ = unsafe { CloseHandle(job) };
    let fact = serde_json::json!({
        "platform": "Windows",
        "case": "process_escape_terminated",
        "mechanism": "JobObject KILL_ON_JOB_CLOSE",
        "child_pid": child_pid,
        "alive_after_terminate": alive_after_terminate,
    });
    let path = write_fact(&dir, "tree-kill.json", &fact);
    assert!(path.is_file(), "进程树终止证据必须落盘");
    assert!(!alive_after_terminate, "Job 终止后孙进程仍在运行");
    assert!(!marker.exists(), "本用例没有启动任何写 marker 的命令");
    let _ = fs::remove_dir_all(&dir);
}

/// 非 Windows 目标不执行上面的真机用例，只保留明确标注的跳过理由。
#[cfg(not(target_os = "windows"))]
#[test]
#[ignore = "需要 Windows 本机；运行 scripts/sandbox-windows.ps1 生成证据"]
fn windows_job_object_live_test_is_not_run_on_this_host() {
    let dir = evidence_dir("windows-not-tested");
    let fact = serde_json::json!({
        "platform": "Windows",
        "status": "Untested",
        "reason": "当前主机不是 Windows，未实测 Job Object 与文件/网络隔离",
        "reproduce": "pwsh -File scripts/sandbox-windows.ps1",
    });
    let path = write_fact(&dir, "windows-untested.json", &fact);
    assert!(path.is_file(), "未实测事实仍须保留可复现证据");
    let _ = fs::remove_dir_all(&dir);
}

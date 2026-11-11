//! Windows Job Object 实现：进程树终止与资源上限。
//!
//! 直接使用 windows 0.62，不引入 win32job。win32job 只是几十行 unsafe
//! 的类型安全包装，版本更新滞后于 windows crate。本模块用到的 API
//! （create/assign/terminate/set information + KILL_ON_JOB_CLOSE）范围
//! 小且稳定，直接使用官方 crate 的三个子 feature 少一层间接。句柄
//! 所有权交给 `windows_core::Owned`，drop 时调用 CloseHandle，不会
//! 泄漏。

use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
    SetInformationJobObject, TerminateJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT, JOB_OBJECT_LIMIT_ACTIVE_PROCESS, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOB_OBJECT_LIMIT_PROCESS_MEMORY,
};
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE,
};
// windows crate 将 windows-core 重导出为 windows::core（句柄所有权
// 原语 Owned 在此），因此不单独声明 windows-core 依赖。
use windows::core::Owned;

use crate::domain::sandbox::SandboxPolicy;

/// 一个已配置的 Job：树内进程受策略约束，句柄关闭时整树终止。
///
/// 生命周期契约：`KILL_ON_JOB_CLOSE` 使句柄存在即沙箱存在。守卫必须
/// 存活到子进程全部结束。drop（正常结束路径）时残留进程一并终止，
/// 正常与超时两条路径的清理由同一机制保证，超时分支只多一次显式
/// 提前终止。
pub struct JobGuard {
    /// Job 句柄。`Owned` 在 drop 时 CloseHandle，触发 KILL_ON_JOB_CLOSE。
    job: Owned<HANDLE>,
}

// HANDLE 是裸指针，编译器按可能非线程安全处理。Job Object 句柄是跨线程
// 合法的系统对象（TerminateJobObject/AssignProcessToJobObject/CloseHandle
// 均无线程亲和性），这里只随 future 跨 await 传递，不并发访问。
// 缺少此实现时，run_in_workspace 的 future 不满足 ToolInvoker 的 Send。
unsafe impl Send for JobGuard {}

// MCP 客户端在独立线程阻塞等待连接收尾（tokio::task::spawn_blocking），
// JobGuard 随之跨线程移动，需要 Sync。句柄是进程级内核对象，
// TerminateJobObject/CloseHandle 均无线程亲和性；JobGuard 是单所有者，
// CloseHandle 由 Owned<HANDLE> 的 drop 保证只执行一次。
unsafe impl Sync for JobGuard {}

impl JobGuard {
    /// 创建 Job 并施加策略（内存上限、进程数上限、句柄关闭即整树终止）。
    ///
    /// `cpu_time` 不在支持范围内：`apply_limits` 明确拒绝（默认策略也不
    /// 启用），见该函数说明。
    ///
    /// # Errors
    ///
    /// Job 创建或限额设置失败时返回说明文本。失败处理由调用方决定，
    /// 两个调用点都选择 fail-closed：`run_with_policy` 拒绝执行命令并
    /// 返回 `RunError::Refused`，MCP `connect` 使该 Server 连接失败。
    /// 只有平台未实现（非 Windows）时才降级继续，并报告"沙箱未生效"。
    pub fn create(policy: &SandboxPolicy) -> Result<Self, String> {
        // 匿名 Job（不设名字）：随句柄走，不留在对象命名空间里。
        let handle = unsafe { CreateJobObjectW(None, None) }
            .map_err(|error| format!("创建 Job Object 失败: {error}"))?;

        // 必须先接管句柄：设置限额失败也要 CloseHandle。
        let job = unsafe { Owned::new(handle) };
        Self::apply_limits(&job, policy)?;
        Ok(Self { job })
    }

    /// 把策略转换为 Job 限额。失败在此转换为说明文本：
    /// `windows::core::Result` 的错误类型没有 `From<String>`，`create` 的
    /// 错误通道是 `String`，无需额外转换层。
    fn apply_limits(job: &HANDLE, policy: &SandboxPolicy) -> Result<(), String> {
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        let mut flags = JOB_OBJECT_LIMIT(0);

        // 整树终止是最后一道保证：即使调用方未调用 terminate，句柄关闭
        // （守卫 drop）时残留进程也会被终止。
        if policy.kill_tree {
            flags |= JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        }
        if let Some(bytes) = policy.per_process_memory {
            flags |= JOB_OBJECT_LIMIT_PROCESS_MEMORY;
            limits.ProcessMemoryLimit = bytes as usize;
        }
        if let Some(count) = policy.max_processes {
            flags |= JOB_OBJECT_LIMIT_ACTIVE_PROCESS;
            limits.BasicLimitInformation.ActiveProcessLimit = count;
        }
        if policy.cpu_time.is_some() {
            // 领域层默认不设 CPU 时间（编译型负载无法预估）。如需设置，
            // 应使用 PerJobUserTimeLimit（100ns 单位）。当前没有调用方
            // 传入 Some，因此明确拒绝，不实现未经验证的限额路径。
            return Err("cpu_time 限额暂未实现（默认策略不启用）".to_owned());
        }
        limits.BasicLimitInformation.LimitFlags = flags;

        unsafe {
            SetInformationJobObject(
                *job,
                JobObjectExtendedLimitInformation,
                &limits as *const _ as _,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        }
        .map_err(|error| format!("设置 Job 限额失败: {error}"))
    }

    /// 把已启动的子进程纳入 Job。
    ///
    /// # Errors
    ///
    /// assign 失败（句柄无效、权限不足）时返回说明文本。调用方降级
    /// 运行并报告"沙箱未生效"。
    ///
    /// # Safety
    ///
    /// `process_handle` 必须仍归调用方所有，且在 assign 之后不被提前
    /// 关闭。assign 不转移句柄所有权，Job 只引用它。
    ///
    /// # 纳入时机决定保护范围
    ///
    /// Windows 只在进程**创建时**按父进程当时所在的 Job 纳入它，
    /// `AssignProcessToJobObject` 不会追溯纳入此前已存在的后代。assign
    /// 晚于某个子进程的创建，该子进程就在整树终止范围之外。调用时机
    /// 是保护是否成立的前提。
    pub fn assign(&self, process_handle: HANDLE) -> Result<(), String> {
        unsafe { AssignProcessToJobObject(*self.job, process_handle) }
            .map_err(|error| format!("进程纳入 Job 失败: {error}"))
    }

    /// 按 PID 把一个**已在运行**的外部进程纳入本 Job。
    ///
    /// 用于 MCP 这类 Harness 不直接 spawn、句柄由第三方持有的进程。rmcp
    /// 的 `TokioChildProcess` 在 `serve()` 后不再暴露 `Child`，只保留
    /// `id()` 返回的 PID，所以需要通过 `OpenProcess` 取得句柄再 assign。
    /// Job 由调用方长期持有，drop 触发 `KILL_ON_JOB_CLOSE`，将纳入的
    /// 进程及其**之后**创建的子孙一并终止，这是 `child.kill()` 单独无法
    /// 做到的整树终止。
    ///
    /// # 调用时机决定保护是否成立
    ///
    /// 纳入只使目标进程**此后**创建的子进程继承 Job 成员身份。MCP
    /// Server 常由 `uvx`/`npx`/`cmd /C` 等 launcher 拉起：launcher 是
    /// 纳入的 PID，实际工作的 Server 是它的子进程。握手成功即证明该
    /// 子进程已存在，因此纳入必须在握手**之前**完成。放在握手之后时，
    /// 进入 Job 的只有 launcher，整树终止与 `active = true` 都不成立。
    /// `tests` 中两条用例分别固定"之后创建的会被终止"与"之前已存在的
    /// 不会被追溯纳入"。
    ///
    /// # Errors
    ///
    /// `OpenProcess` 无法打开该 PID（进程已退出或权限不足）、或 assign
    /// 失败时返回说明文本。调用方降级处理，记 `active = false`。
    ///
    /// **Job 创建失败**由 `JobGuard::create` 返回。创建失败可在启动任何
    /// 外部进程前拒绝；纳入失败时进程已在运行，只能如实降级。
    pub fn assign_pid(&self, pid: u32) -> Result<(), String> {
        let handle = unsafe { OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, false, pid) }
            .map_err(|error| format!("打开进程 (pid={pid}) 失败: {error}"))?;
        // Owned 保证无论 assign 成败都调用 CloseHandle。
        // AssignProcessToJobObject 不接管句柄所有权，每次漏关泄漏一个内核句柄。
        let owned = unsafe { Owned::new(handle) };
        self.assign(*owned)
    }

    /// 显式终止整树（超时分支调用）。失败不报错：KILL_ON_JOB_CLOSE 与
    /// child.kill() 还有两层保证。
    pub fn terminate_tree(&self) {
        unsafe {
            let _ = TerminateJobObject(*self.job, 1);
        }
    }
}

/// 把 tokio 子进程的原生句柄纳入 Job。
///
/// tokio 在 Windows 上由 `Child::raw_handle()` 返回进程句柄。拿不到
/// （进程已退出等）按 assign 失败处理，调用方降级。
///
/// # Errors
///
/// 句柄不可用或 assign 失败时返回说明文本。
pub fn assign_child(job: &JobGuard, child: &tokio::process::Child) -> Result<(), String> {
    let raw_handle = child
        .raw_handle()
        .ok_or_else(|| "子进程句柄不可用（进程可能已退出）".to_owned())?;
    if raw_handle.is_null() {
        return Err("子进程句柄不可用（进程可能已退出）".into());
    }
    job.assign(HANDLE(raw_handle))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::Duration;
    use windows::Win32::System::Threading::{
        GetExitCodeProcess, TerminateProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    /// `GetExitCodeProcess` 对仍在运行的进程报告的退出码。
    const STILL_ACTIVE: u32 = 259;

    /// 进程是否仍在运行。打不开句柄时按未运行处理：进程已退出与权限
    /// 不足都不代表仍在运行，测试中宁可得到确定的否定结果，也不误判。
    fn process_alive(pid: u32) -> bool {
        let handle = match unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) } {
            Ok(handle) => handle,
            Err(_) => return false,
        };
        let owned = unsafe { Owned::new(handle) };
        let mut exit_code = 0_u32;
        match unsafe { GetExitCodeProcess(*owned, &mut exit_code) } {
            Ok(()) => exit_code == STILL_ACTIVE,
            Err(_) => false,
        }
    }

    /// 测试收尾：终止有意留在 Job 外的进程，避免宿主机残留孤儿进程。
    fn terminate_by_pid(pid: u32) {
        let handle = match unsafe { OpenProcess(PROCESS_TERMINATE, false, pid) } {
            Ok(handle) => handle,
            Err(_) => return,
        };
        let owned = unsafe { Owned::new(handle) };
        let _ = unsafe { TerminateProcess(*owned, 1) };
    }

    /// 两代进程的测试脚手架。
    ///
    /// launcher 是 PowerShell 脚本，等待"放行"标记文件，收到后启动长时间
    /// 运行的孙进程（`ping -n 300`），并把孙进程 PID 写入文件。使用文件
    /// 信号而非固定延时，因为这两条测试验证的是**顺序**：先纳入还是先
    /// 创建。延时不可靠，信号结果是确定的。
    struct Fixture {
        dir: PathBuf,
        go: PathBuf,
        child_pid: PathBuf,
        launcher_err: PathBuf,
    }

    impl Fixture {
        /// `tag` 区分同一测试进程内的多个脚手架。
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "qch-sandbox-{}-{}",
                tag,
                std::process::id()
            ));
            // 上次崩溃残留的文件会让"文件已出现"变成假信号，先清理。
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("创建测试临时目录");
            Self {
                go: dir.join("go.flag"),
                child_pid: dir.join("child.pid"),
                launcher_err: dir.join("launcher.err"),
                dir,
            }
        }

        /// 启动 launcher。stdin/stdout 置空，stderr 写入文件：脚本因引号或
        /// 路径失败时，测试失败信息能直接显示原因。
        fn spawn_launcher(&self) -> tokio::process::Child {
            use std::process::Stdio;
            let script = format!(
                "while (-not (Test-Path -LiteralPath '{go}')) {{ Start-Sleep -Milliseconds 50 }}; \
                 $p = Start-Process -FilePath ping -ArgumentList '-n','300','127.0.0.1' \
                 -NoNewWindow -PassThru; \
                 Set-Content -LiteralPath '{pidf}' -Value $p.Id -Encoding ascii; \
                 Wait-Process -Id $p.Id",
                go = self.go.display(),
                pidf = self.child_pid.display(),
            );
            let stderr_file = std::fs::File::create(&self.launcher_err).expect("建 stderr 日志");
            tokio::process::Command::new("powershell")
                .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-Command", &script])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::from(stderr_file))
                .spawn()
                .expect("启动 PowerShell launcher")
        }

        /// 放行：launcher 收到标记之后才创建孙进程。
        fn signal_go(&self) {
            std::fs::write(&self.go, b"go").expect("写放行标记");
        }

        /// 等孙进程 PID 出现，即"孙进程已创建"的确定证据。
        async fn wait_for_child_pid(&self) -> u32 {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
            loop {
                if let Ok(text) = std::fs::read_to_string(&self.child_pid)
                    && let Ok(pid) = text.trim().parse::<u32>() {
                        return pid;
                    }
                if tokio::time::Instant::now() >= deadline {
                    let reason = std::fs::read_to_string(&self.launcher_err)
                        .unwrap_or_else(|_| "<空>".to_owned());
                    panic!("launcher 未在 60 秒内写出孙进程 PID，stderr: {reason}");
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }

        fn cleanup(&self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// 在 `limit` 内等待进程消失；超时则失败并输出可排查的信息。
    async fn assert_dead(pid: u32, limit: Duration, what: &str) {
        let deadline = tokio::time::Instant::now() + limit;
        while process_alive(pid) {
            if tokio::time::Instant::now() >= deadline {
                panic!("{what}：pid={pid} 在 {limit:?} 内仍在运行");
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Job 的整树终止覆盖**纳入之后**创建的子孙。MCP 路径中"Server
    /// 连同它拉起的进程一并终止"依赖这一机制。
    ///
    /// 顺序由文件信号确定：先纳入 launcher，再放行让它创建子进程。
    /// 若纳入被移到子进程创建之后，这条测试会失败，`active = true` 也将
    /// 失真（见 `assign_pid`）。
    #[tokio::test]
    async fn a_child_created_after_assignment_is_terminated_with_the_job() {
        let fixture = Fixture::new("inherit");
        let mut launcher = fixture.spawn_launcher();
        let launcher_pid = launcher.id().expect("运行中的 launcher 应有 PID");

        let job = JobGuard::create(&SandboxPolicy::default()).expect("创建 Job 应成功");
        job.assign_pid(launcher_pid)
            .expect("应能把运行中的 launcher 纳入 Job");

        fixture.signal_go();
        let child_pid = fixture.wait_for_child_pid().await;
        assert!(process_alive(child_pid), "孙进程此刻应在运行");

        drop(job); // 最后一个 Job 句柄关闭 → KILL_ON_JOB_CLOSE 整树终止
        // 约 300 秒的 ping 在 10 秒内消失，只能是被 Job 提前终止。
        assert_dead(child_pid, Duration::from_secs(10), "纳入后才创建的孙进程应被整树终止").await;
        assert_dead(launcher_pid, Duration::from_secs(10), "launcher 应被整树终止").await;
        let _ = tokio::time::timeout(Duration::from_secs(10), launcher.wait()).await;
        fixture.cleanup();
    }

    /// 反面对照：**纳入之前**已存在的子进程不会被追溯纳入，也不会被终止。
    ///
    /// 这是 Windows 的语义，与实现选择无关。它说明 MCP 必须"先纳入、
    /// 后握手"（握手成功即证明 Server 子进程已存在）。若纳入放在握手
    /// 之后，本用例的现象就会出现在线上。
    #[tokio::test]
    async fn assigning_a_parent_does_not_retroactively_cover_an_existing_child() {
        let fixture = Fixture::new("late-assign");
        let mut launcher = fixture.spawn_launcher();
        let launcher_pid = launcher.id().expect("运行中的 launcher 应有 PID");

        // 先放行并等到孙进程确实存在，再纳入 launcher。
        fixture.signal_go();
        let child_pid = fixture.wait_for_child_pid().await;

        let job = JobGuard::create(&SandboxPolicy::default()).expect("创建 Job 应成功");
        job.assign_pid(launcher_pid)
            .expect("应能把运行中的 launcher 纳入 Job");
        drop(job);

        assert_dead(
            launcher_pid,
            Duration::from_secs(10),
            "纳入的 launcher 应被整树终止",
        )
        .await;
        assert!(
            process_alive(child_pid),
            "创建在纳入之前的孙进程不该被追溯纳入，因此它必须还活着——\
             这正是握手之后才纳入时 active=true 名不副实的原因"
        );
        // 孙进程留在 Job 外，测试自己收尾，不留孤儿。
        terminate_by_pid(child_pid);
        let _ = tokio::time::timeout(Duration::from_secs(10), launcher.wait()).await;
        fixture.cleanup();
    }

    /// 打不开不存在或已退出的 PID 时返回错误说明，不 panic。调用方
    /// （MCP `connect`）依靠这个错误走"未纳入、记 `active = false`"分支。
    #[test]
    fn assigning_an_unknown_pid_is_an_error_not_a_panic() {
        let job = JobGuard::create(&SandboxPolicy::default()).expect("创建 Job 应成功");
        // PID 0 不是可 OpenProcess 的普通进程句柄目标。
        assert!(job.assign_pid(0).is_err(), "PID 0 不应被当作可纳入的活进程");
    }
}

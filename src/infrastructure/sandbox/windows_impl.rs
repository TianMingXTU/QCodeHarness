//! Windows Job Object 实现：进程树的整树终止与资源上限。
//!
//! 选型说明（windows 0.62 vs win32job 薄封装）：win32job 提供的只是
//! 这几十行 unsafe 的类型安全包装，且版本更新滞后于 windows crate；
//! 本模块用到的 API 面（create/assign/terminate/set information +
//! KILL_ON_JOB_CLOSE）小而稳定，直接用官方 windows crate 的三个子
//! feature 反而少一层间接。句柄所有权交给 `windows_core::Owned`：
//! drop 即 CloseHandle，不会泄漏。

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
// windows crate 把 windows-core 重导出为 windows::core（句柄所有权
// 原语 Owned 在这里），不单独声明 windows-core 依赖。
use windows::core::Owned;

use crate::domain::sandbox::SandboxPolicy;

/// 一个已配置好的 Job：树内进程受策略约束，句柄关闭时整树终止。
///
/// 生命周期契约：`KILL_ON_JOB_CLOSE` 意味着**句柄存在 = 沙箱存在**。
/// 守卫必须活到"子进程还在跑"的整段时间结束；drop（正常结束路径）
/// 时还挂着的残留进程被一并终止——正常与超时两条路径的清理由同一
/// 机制兜住，超时分支只是多一次显式提前终止。
pub struct JobGuard {
    /// Job 句柄。`Owned` 在 drop 时 CloseHandle，触发 KILL_ON_JOB_CLOSE。
    job: Owned<HANDLE>,
}

// HANDLE 是裸指针，编译器按"可能非线程安全"处理；但 Job Object 句柄
// 本身就是跨线程合法的系统对象（TerminateJobObject/AssignProcessToJobObject/
// CloseHandle 无线程亲和性），守卫在这里只是被跨 await 携带而不被并发访问。
// 若不补这条，run_in_workspace 的 future 无法满足 ToolInvoker 的 Send。
unsafe impl Send for JobGuard {}

// MCP 客户端把 JobGuard 放进 `Arc<dyn McpClientHandle>`（要求 Sync）长期持有，
// 只在 drop 时动句柄、从不并发调用其方法。Job Object 句柄是进程级内核对象，
// 经 `&` 跨线程共享安全，CloseHandle 由 Arc 的最后一个引用保证只发生一次。
unsafe impl Sync for JobGuard {}

impl JobGuard {
    /// 创建 Job 并施加策略（内存上限 / 进程数上限 / 句柄关闭即整树终止）。
    ///
    /// `cpu_time` 不在此列：`apply_limits` 明确拒绝它（默认策略也不启用），
    /// 详见该函数说明。
    ///
    /// # Errors
    ///
    /// Job 创建或限额设置失败时返回说明文本。**取向由调用方决定**，两个
    /// 调用点都选 fail-closed：`run_with_policy` 拒绝执行命令并返回
    /// `RunError::Refused`，MCP `connect` 让该 Server 连接失败；只有平台
    /// 本身没实现（非 Windows）才降级继续，并如实报告"沙箱未生效"。
    pub fn create(policy: &SandboxPolicy) -> Result<Self, String> {
        // 匿名 Job（不设名字）：随句柄走，不留在对象命名空间里。
        let handle = unsafe { CreateJobObjectW(None, None) }
            .map_err(|error| format!("创建 Job Object 失败: {error}"))?;

        // 必须先接管句柄：设置限额失败也要 CloseHandle。
        let job = unsafe { Owned::new(handle) };
        Self::apply_limits(&job, policy)?;
        Ok(Self { job })
    }

    /// 把策略翻译成 Job 限额。限额调用失败在这里就地转成说明文本：
    /// `windows::core::Result` 的错误类型没有 `From<String>`，调用方
    /// `create` 的错误通道本来就是 `String`，不绕一层。
    fn apply_limits(job: &HANDLE, policy: &SandboxPolicy) -> Result<(), String> {
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        let mut flags = JOB_OBJECT_LIMIT(0);

        // 整树终止是兜底里的兜底：即使调用方忘了 terminate，
        // 句柄一关（守卫 drop）残留进程也会被收掉。
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
            // 领域层默认不设 CPU 时间（编译型负载不可预估）；真要设时
            // 走 PerJobUserTimeLimit（100ns 单位）。当前没有调用方会
            // 传 Some，先明确拒绝而不是写一条没人验证过的限额路径。
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
    /// assign 失败（句柄无效、权限不足）时返回说明文本；调用方降级
    /// 运行并如实报告"沙箱未生效"。
    ///
    /// # Safety
    ///
    /// `process_handle` 必须仍归调用方所有、且在 assign 之后不会被提前
    /// 关闭；assign 不转移句柄所有权，Job 只是引用它。
    ///
    /// # 纳入时机决定保护范围
    ///
    /// Windows 只在进程**创建时**按"它的父进程当时所在的 Job"纳入它，
    /// `AssignProcessToJobObject` 不会追溯纳入之前已经存在的后代。所以
    /// assign 晚于某个子进程的创建，那个子进程就在整树终止之外——
    /// `assign_pid` 的调用时机因此不是细节，而是保护成立与否的前提。
    pub fn assign(&self, process_handle: HANDLE) -> Result<(), String> {
        unsafe { AssignProcessToJobObject(*self.job, process_handle) }
            .map_err(|error| format!("进程纳入 Job 失败: {error}"))
    }

    /// 按 PID 把一个**已在运行**的外部进程纳入本 Job。
    ///
    /// 给 MCP 这类"Harness 不亲自 spawn、句柄在别人手里"的进程用：rmcp 的
    /// `TokioChildProcess` 在 `serve()` 后就不再暴露 `Child`，只剩 `id()` 给的
    /// PID，所以只能走 `OpenProcess` 拿句柄再 assign。Job 由调用方在建好之后
    /// 长期持有——它的 drop 触发 `KILL_ON_JOB_CLOSE`，把纳入的进程连同它
    /// **之后**创建的子孙一并收掉，正是 `child.kill()` 单独做不到的整树终止。
    ///
    /// # 调用时机决定这条路径是不是真保护
    ///
    /// 纳入只能让目标进程**此后**创建的子进程继承 Job 成员身份。MCP Server
    /// 常由 `uvx`/`npx`/`cmd /C` 这类 launcher 拉起：launcher 是我们要纳入的
    /// PID，真正干活的 Server 是它的子进程。握手成功本身就证明那个子进程
    /// 已经存在，所以纳入必须在握手**之前**完成——放在握手之后，进 Job 的
    /// 只有 launcher，"整树终止"和 `active = true` 都是假的。
    /// `tests` 里两条用例分别钉住"之后创建的收得到"与"之前已存在的收不到"。
    ///
    /// # Errors
    ///
    /// `OpenProcess` 打不开该 PID（进程已退出或权限不足）、或 assign 失败时
    /// 返回说明文本；调用方按降级处理（记 `active = false`）。
    ///
    /// **Job 创建失败**不走这里——那是 `JobGuard::create` 的返回值。两者分开
    /// 是因为取向不同：创建失败能在启动任何外部进程之前拒绝，纳入失败则
    /// 进程已经在跑，只能如实降级。
    pub fn assign_pid(&self, pid: u32) -> Result<(), String> {
        let handle = unsafe { OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, false, pid) }
            .map_err(|error| format!("打开进程 (pid={pid}) 失败: {error}"))?;
        // Owned 保证无论 assign 成败都 CloseHandle：AssignProcessToJobObject 不
        // 接管进程句柄所有权，漏关就是每纳入一个进程泄漏一个内核句柄。
        let owned = unsafe { Owned::new(handle) };
        self.assign(*owned)
    }

    /// 显式终止整树（超时分支调用）。终止失败不报错：还有
    /// KILL_ON_JOB_CLOSE 和 child.kill() 两层兜底。
    pub fn terminate_tree(&self) {
        unsafe {
            let _ = TerminateJobObject(*self.job, 1);
        }
    }
}

/// 便捷函数：把 tokio 子进程的原生句柄纳入 Job。
///
/// tokio 在 Windows 上 `Child::raw_handle()` 返回进程句柄；拿不到
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

    /// 进程是否还活着。打不开句柄按"不活着"处理：进程已退出与权限不足
    /// 都不是"还在跑"，测试里宁可等一个确定的假信号，也不要误判成活着。
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

    /// 测试收尾：把有意留在 Job 外的进程收掉，不给宿主机留孤儿。
    fn terminate_by_pid(pid: u32) {
        let handle = match unsafe { OpenProcess(PROCESS_TERMINATE, false, pid) } {
            Ok(handle) => handle,
            Err(_) => return,
        };
        let owned = unsafe { Owned::new(handle) };
        let _ = unsafe { TerminateProcess(*owned, 1) };
    }

    /// 两代进程的脚手架。
    ///
    /// launcher 是 PowerShell，它等一个"放行"标记文件，收到放行才启动长跑的
    /// 孙进程（`ping -n 300`），并把孙进程 PID 写进文件。用文件信号而不是
    /// 固定延时，是因为这两条测试要证的正是**顺序**：纳入在前还是创建在前。
    /// 延时只能赌，信号是确定的。
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
            // 上一次崩溃留下的残骸会让"文件出现"变成假信号，先清干净。
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("创建测试临时目录");
            Self {
                go: dir.join("go.flag"),
                child_pid: dir.join("child.pid"),
                launcher_err: dir.join("launcher.err"),
                dir,
            }
        }

        /// 启动 launcher。它的 stdin/stdout 全部置空，stderr 落文件：脚本
        /// 因为引号或路径失败时，测试失败信息要能直接看出原因。
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
                if let Ok(text) = std::fs::read_to_string(&self.child_pid) {
                    if let Ok(pid) = text.trim().parse::<u32>() {
                        return pid;
                    }
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

    /// 在 `limit` 内等到进程消失；还在跑就失败并留下可排查的信息。
    async fn assert_dead(pid: u32, limit: Duration, what: &str) {
        let deadline = tokio::time::Instant::now() + limit;
        while process_alive(pid) {
            if tokio::time::Instant::now() >= deadline {
                panic!("{what}：pid={pid} 在 {limit:?} 内仍在运行");
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Job 的整树终止覆盖**纳入之后**创建的子孙：这是 MCP 路径声称的
    /// "Server 连同它自己拉起的进程一并收掉"赖以成立的机制。
    ///
    /// 顺序由文件信号定死：先纳入 launcher，再放行让它创建子进程。
    /// 若哪天纳入被挪到子进程创建之后，这条测试会失败——正是那次挪动
    /// 会让 `active = true` 变成假信号（见 `assign_pid`）。
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

    /// 反面对照：**纳入之前**就已存在的子进程不会被追溯纳入，也就收不掉。
    ///
    /// 这条记录的是 Windows 的语义，不是我们的实现选择：它说明为什么 MCP
    /// 必须"先纳入、后握手"（握手成功即证明 Server 子进程已存在）。
    /// 若把纳入挪回握手之后，本用例的现象就成了线上事实。
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

    /// 打不开一个不存在/已退出的 PID 时返回错误说明，而不是 panic——
    /// 调用方（MCP `connect`）正是靠这个 Err 分支走"未纳入、记 active=false"。
    #[test]
    fn assigning_an_unknown_pid_is_an_error_not_a_panic() {
        let job = JobGuard::create(&SandboxPolicy::default()).expect("创建 Job 应成功");
        // PID 0 不是可 OpenProcess 的普通进程句柄目标。
        assert!(job.assign_pid(0).is_err(), "PID 0 不应被当作可纳入的活进程");
    }
}

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
    /// Job 创建或限额设置失败时返回说明文本。**取向由调用方 `run_with_policy`
    /// 决定**：Windows 上创建失败按 fail-closed 处理——命令不执行、返回
    /// `RunError::Refused`，一个子进程都不启动；只有平台没实现（非 Windows）
    /// 或启动后 `assign` 失败才降级执行并记 `sandbox_active = false`。
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
    /// `process_handle` 必须是调用方刚 spawn、仍归自己所有的进程
    /// 句柄；assign 之后句柄所有权不转移（Job 只是引用它）。
    ///
    /// # 已知的理论竞态
    ///
    /// spawn 与 assign 之间拉起的后代可能未进入 Job。窗口长度受系统
    /// 调度影响，没有固定上界；P7 接受该竞态，不引入挂起启动机制。
    pub fn assign(&self, process_handle: HANDLE) -> Result<(), String> {
        unsafe { AssignProcessToJobObject(*self.job, process_handle) }
            .map_err(|error| format!("进程纳入 Job 失败: {error}"))
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

/// 按 PID 把一个**已经启动**的外部子进程纳入新建的 Job。
///
/// 给 MCP 这类"Harness 不亲自 spawn、句柄在别人手里"的进程用：rmcp 的
/// `TokioChildProcess` 在 `serve()` 后就不再暴露 `Child`，只剩 `id()` 给的
/// PID，所以只能走 `OpenProcess` 拿句柄再 assign。返回的 `JobGuard` 交给
/// 调用方长期持有——它的 drop 触发 `KILL_ON_JOB_CLOSE`，把 Server 连同它
/// 自己拉起的孙进程一并收掉，正是 `child.kill()` 单独做不到的整树终止。
///
/// # Errors
///
/// Job 创建失败、`OpenProcess` 打不开该 PID（进程已退出或权限不足）、或
/// assign 失败时返回说明文本；调用方按降级处理（记 `active = false`），
/// 与 shell 工具口径一致。
///
/// # 已知窗口
///
/// 从 MCP 子进程 spawn 到本函数 assign 之间，进程短暂不在 Job 内；这与
/// shell 路径 spawn→assign 的窗口同源，是已接受的取舍。
pub fn protect_pid(pid: u32, policy: &SandboxPolicy) -> Result<JobGuard, String> {
    let job = JobGuard::create(policy)?;
    let handle = unsafe { OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, false, pid) }
        .map_err(|error| format!("打开子进程 (pid={pid}) 失败: {error}"))?;
    // Owned 保证无论 assign 成败都 CloseHandle：AssignProcessToJobObject 不
    // 接管进程句柄所有权，漏关就是每连一个 Server 泄漏一个内核句柄。
    let owned = unsafe { Owned::new(handle) };
    let assigned = job.assign(*owned);
    drop(owned);
    assigned?;
    Ok(job)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// `protect_pid` 把已在运行的进程纳入 Job：guard drop 后经
    /// `KILL_ON_JOB_CLOSE` 把它连同后代一起终止。
    ///
    /// 这条测的是 MCP 收口依赖的**底层原语本身**：Server 子进程由 rmcp
    /// spawn、harness 只拿得到 PID，所以必须验证"按 PID 打开→纳入→关句柄
    /// 整树终止"这条路在真实进程上成立，而不是只验证 `connect` 没报错
    /// （`connect` 对纳入失败是优雅降级的，光看它通过证明不了纳入成功）。
    #[tokio::test]
    async fn protecting_an_existing_pid_terminates_it_when_the_guard_drops() {
        use std::process::Stdio;
        // 一个会长跑的子进程：纳入 Job 后 guard 若不能终止它，下面的 wait
        // 会一直等到 ping 自然结束（约 120 秒）而触发超时断言失败。
        let mut child = tokio::process::Command::new("cmd")
            .args(["/C", "ping -n 120 127.0.0.1 >nul"])
            .stdin(Stdio::null())
            .spawn()
            .expect("spawn cmd");
        let pid = child.id().expect("运行中的子进程应有 PID");

        // 先确认真的在跑——否则"很快返回"分不清是被杀还是压根没启动。
        assert!(
            child.try_wait().expect("try_wait 不应报错").is_none(),
            "ping 子进程此刻应仍在运行"
        );

        let guard = protect_pid(pid, &SandboxPolicy::default())
            .expect("应能把运行中的进程纳入 Job");
        drop(guard); // 最后一个 Job 句柄关闭 → KILL_ON_JOB_CLOSE 整树终止

        // 约 120 秒的进程在 10 秒内 wait 返回，只能是被 Job 提前终止。
        // 不断言退出码：Job 关闭经 TerminateProcess 收人，退出码随 Windows
        // 版本与父子结构而变，"被提前终止"这条不变量由 10 秒内返回已证。
        let status = tokio::time::timeout(Duration::from_secs(10), child.wait())
            .await
            .expect("guard drop 后子进程应在 10s 内被 Job 终止，而非跑到自然结束");
        let _ = status;
    }

    /// 打不开一个不存在/已退出的 PID 时返回错误说明，而不是 panic——
    /// 调用方（MCP `connect`）正是靠这个 Err 分支走"未纳入、记 active=false"。
    #[test]
    fn protecting_an_unknown_pid_is_an_error_not_a_panic() {
        // PID 0 不是可 OpenProcess 的普通进程句柄目标。
        let result = protect_pid(0, &SandboxPolicy::default());
        assert!(result.is_err(), "PID 0 不应被当作可纳入的活进程");
    }
}

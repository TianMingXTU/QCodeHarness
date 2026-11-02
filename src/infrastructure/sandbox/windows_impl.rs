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
// 本身就是跨线程合法的系统对象（TerminateJobObject/CloseHandle 无
// 线程亲和性），守卫在这里只是被跨 await 携带而不被并发访问。
// 若不补这条，run_in_workspace 的 future 无法满足 ToolInvoker 的 Send。
unsafe impl Send for JobGuard {}

impl JobGuard {
    /// 创建 Job 并施加策略（内存 / 进程数 / CPU 时间）。
    ///
    /// # Errors
    ///
    /// Job 创建或限额设置失败时返回说明文本；调用方降级运行
    /// （命令照跑，`sandbox_active = false`）而不是中断任务。
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

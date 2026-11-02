//! 进程沙箱策略：对 agent 发起的子进程树施加的资源与生命周期约束。
//!
//! 这是"防御纵深"的第二层：审批闸门决定"能不能跑"（策略 + 人工），
//! 沙箱决定"跑起来之后最多能祸害多少"。两层缺一不可——审批是人疲劳
//! 会失误的流程控制，沙箱是不依赖任何人清醒的机械边界。
//!
//! 本模块只有纯数据与默认值，强制执行在 infrastructure/sandbox
//! （Windows Job Object）——领域层不碰任何平台 API。

use std::time::Duration;

/// 进程沙箱策略。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxPolicy {
    /// 超时或结束时终止整个进程树（含孙进程）。
    ///
    /// 修的是真实缺陷：`child.kill()` 只杀直接子进程，
    /// `cmd /C "start /b ..."` 拉起的孙进程会活到自然结束——超时只是
    /// 让模型"以为"停了，孤儿进程继续吃宿主机资源。
    pub kill_tree: bool,
    /// 树内单个进程的内存提交上限（字节）。None = 不限。
    pub per_process_memory: Option<u64>,
    /// 树内同时存活的进程数上限。None = 不限。
    pub max_processes: Option<u32>,
    /// 树内全部进程的 CPU 时间累计上限。None = 不限。
    ///
    /// 默认不设：cargo/rustc 并行编译的 CPU 时间不可预估，设小了误杀。
    /// 进程数与内存两道边界已经兜住"失控 fork 炸弹"的主要形态。
    pub cpu_time: Option<Duration>,
}

impl Default for SandboxPolicy {
    /// 默认策略的取向：**宁可宽到不误杀，也不要紧到断任务**。
    ///
    /// 本项目自己的测试和基准都要在沙箱内跑 `cargo test`（编译
    /// fixture crate），限额设紧了最先被杀的是这个仓库自己的
    /// 工作流。沙箱的价值在"机械兜底"而不是"精细配额"：
    /// kill_tree 修的是确凿缺陷（孙进程失控），内存与进程数给的是
    /// 宽裕的天花板（正常编译远够不到，失控时是刹车不是绊索）。
    fn default() -> Self {
        Self {
            kill_tree: true,
            // 4 GiB：rustc 单进程峰值可到 2-3 GB，设小会在编译大依赖时误杀。
            per_process_memory: Some(4 * 1024 * 1024 * 1024),
            // cargo test 会并行起大量编译与测试进程；256 对正常构建
            // 远够，fork 炸弹形态被拦住。
            max_processes: Some(256),
            cpu_time: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_policy_kills_tree_and_sets_generous_ceilings() {
        let policy = SandboxPolicy::default();
        // kill_tree 是 P7 的核心价值：整树终止必须是默认行为，不能靠
        // 调用方记得开。
        assert!(policy.kill_tree);
        // 内存/进程数给天花板不设绊索：宽到正常构建永远够不到。
        assert_eq!(policy.per_process_memory, Some(4 * 1024 * 1024 * 1024));
        assert_eq!(policy.max_processes, Some(256));
        // CPU 时间不设：编译型工作负载的 CPU 时间不可预估。
        assert_eq!(policy.cpu_time, None);
    }

    #[test]
    fn none_fields_mean_unlimited() {
        let policy = SandboxPolicy {
            per_process_memory: None,
            max_processes: None,
            ..SandboxPolicy::default()
        };
        assert_eq!(policy.per_process_memory, None);
        assert_eq!(policy.max_processes, None);
    }
}

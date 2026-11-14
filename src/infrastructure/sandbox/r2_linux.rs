//! Linux R2 能力探测：Landlock 与 seccomp 的事实采集（不施放隔离）。
//!
//! 内核 ABI 与 seccomp 可用性通过只读系统接口探测。只有探测成功且真实
//! 拒绝测试证据存在时，调用方才可把切片标为生效；其余情况必须标降级。

use std::fs;
use std::path::Path;

use crate::domain::sandbox_fact::{SandboxCapability, SandboxSliceFact, SandboxSliceStatus};

/// Landlock ABI 版本节点。`landlock_create_ruleset` 不可用或该节点不可读时，
/// 明确返回 None，让上层写 SandboxDegraded 证据而不是当作文件系统已隔离。
pub fn landlock_abi() -> Option<u32> {
    if !Path::new("/proc/sys/kernel/landlock").exists() {
        return None;
    }
    let text = fs::read_to_string("/proc/sys/kernel/landlock/abi_version").ok()?;
    text.trim().parse::<u32>().ok()
}

/// seccomp 状态节点。只有 `/proc/self/status` 中 Seccomp 字段存在且不为 0，
/// 才说明当前进程处在 seccomp 模式下；这仍不等于已安装网络拒绝过滤器。
pub fn seccomp_mode() -> Option<u32> {
    let status = fs::read_to_string("/proc/self/status").ok()?;
    let value = status
        .lines()
        .find_map(|line| line.strip_prefix("Seccomp:"))
        .map(str::trim)?;
    value.parse::<u32>().ok()
}

/// Landlock 文件系统切片。有 ABI 仅表示内核特性存在；没有实际 ruleset
/// 施放证据时保持"未生效"，避免把可探测误写成已隔离。
pub fn filesystem_isolation(landlock_applied: bool) -> SandboxSliceFact {
    let abi = landlock_abi();
    let (status, backing, reason) = match (abi, landlock_applied) {
        (Some(version), true) => (
            SandboxSliceStatus::Effective,
            format!("Landlock ABI v{version} ruleset 已施放"),
            "workspace 外写入被真实拒绝，ruleset 已挂到当前进程",
        ),
        (Some(version), false) => (
            SandboxSliceStatus::Ineffective,
            format!("Landlock ABI v{version} 仅探测到，ruleset 未施放"),
            "缺内核特性或规则未挂接时必须降级并落 SandboxDegraded 证据",
        ),
        (None, _) => (
            SandboxSliceStatus::Ineffective,
            "/proc/sys/kernel/landlock/abi_version 不可读".to_owned(),
            "内核未暴露 Landlock ABI；绝不静默放行开放命令",
        ),
    };
    SandboxSliceFact::new(SandboxCapability::FileSystem, status, backing, reason)
}

/// seccomp 网络/系统调用切片。`filter_applied` 必须由真实网络拒绝用例
/// 提供，单独的 Seccomp 字段不足以证明网络已被阻断。
pub fn network_isolation(filter_applied: bool) -> SandboxSliceFact {
    let mode = seccomp_mode();
    let (status, backing, reason) = match (mode, filter_applied) {
        (Some(mode), true) if mode > 0 => (
            SandboxSliceStatus::Effective,
            format!("seccomp mode {mode} + 网络拒绝过滤器"),
            "真实网络访问被拒绝，过滤器已挂到子进程",
        ),
        (Some(mode), _) => (
            SandboxSliceStatus::Ineffective,
            format!("seccomp mode {mode}，网络过滤器未验证"),
            "能力探测不等于施放；未验证网络拒绝前必须降级",
        ),
        (None, _) => (
            SandboxSliceStatus::Ineffective,
            "/proc/self/status 的 Seccomp 字段不可读".to_owned(),
            "缺 seccomp 能力或探测失败时写 SandboxDegraded 证据",
        ),
    };
    SandboxSliceFact::new(SandboxCapability::Network, status, backing, reason)
}

/// Linux 进程树终止切片。当前 R2 未实现进程组或 cgroup 的真实整树终止，
/// 只在测试传入证据时标记生效，避免用 Job Object 概念代替 Linux 行为。
pub fn process_tree_kill(evidence: bool) -> SandboxSliceFact {
    let status = if evidence {
        SandboxSliceStatus::Effective
    } else {
        SandboxSliceStatus::Ineffective
    };
    SandboxSliceFact::new(
        SandboxCapability::ProcessTreeKill,
        status,
        "进程组/cgroup 终止证据",
        "Linux 没有 Windows Job Object；没有真机终止证据前不得写生效",
    )
}

/// MCP 子进程接入同一套 enforcer 的事实。没有接线证据时恒未生效。
pub fn mcp_subprocess(connected: bool) -> SandboxSliceFact {
    let status = if connected {
        SandboxSliceStatus::Effective
    } else {
        SandboxSliceStatus::Ineffective
    };
    SandboxSliceFact::new(
        SandboxCapability::McpSubprocess,
        status,
        "SandboxEnforcer::apply / isolate_child_spec",
        "MCP spawn 前必须调用同一 enforcer 并写入证据，未接线不得算生效",
    )
}

/// 供平台测试与真机脚本调用的只读能力汇总。参数来自真实拒绝测试；
/// 本机未跑 Linux 真机时调用方应改用 `untested()` 而非 `probe(false,false,false,false)`。
pub fn probe(
    landlock_applied: bool,
    filter_applied: bool,
    tree_kill_evidence: bool,
    mcp_connected: bool,
) -> Vec<SandboxSliceFact> {
    vec![
        process_tree_kill(tree_kill_evidence),
        filesystem_isolation(landlock_applied),
        network_isolation(filter_applied),
        mcp_subprocess(mcp_connected),
    ]
}

/// 未在本机实测的 Linux 目标统一标 Untested。证据文件生成脚本在真机
/// 跑过四个用例后才允许替换成 `probe(...)` 并附证据路径。
pub fn untested(reason: &str) -> Vec<SandboxSliceFact> {
    [
        SandboxCapability::ProcessTreeKill,
        SandboxCapability::FileSystem,
        SandboxCapability::Network,
        SandboxCapability::McpSubprocess,
    ]
    .into_iter()
    .map(|capability| {
        SandboxSliceFact::new(
            capability,
            SandboxSliceStatus::Untested,
            "Linux Landlock + seccomp 真机用例",
            reason,
        )
    })
    .collect()
}

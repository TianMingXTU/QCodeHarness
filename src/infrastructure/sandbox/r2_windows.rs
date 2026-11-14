//! Windows R2 隔离切片：如实报告现有 Job Object 与尚缺文件/网络隔离。
//!
//! 本模块只采集 Windows API 能力并产生结构化事实。AppContainer 或受限令牌
//! 涉及进程令牌与 ACL 施放，当前切片未接线，状态必须为 `Ineffective`；
//! 开放命令继续由既有策略默认拒绝，不把 Job Object 当成文件/网络隔离。

use crate::domain::sandbox_fact::{SandboxCapability, SandboxSliceFact, SandboxSliceStatus};

/// Windows 进程树终止能力。既有 `windows_impl::JobGuard` 负责 Job Object，
/// 这里通过 `PlatformCapabilities::detect()` 取同一平台事实来源，不另造判断。
pub fn process_tree_kill() -> SandboxSliceFact {
    let capabilities = crate::domain::sandbox::PlatformCapabilities::detect();
    let (status, reason) = if capabilities.has_process_sandbox && capabilities.has_tree_kill {
        (
            SandboxSliceStatus::Effective,
            "现有 Windows Job Object 已纳入子进程并负责整树终止",
        )
    } else {
        (
            SandboxSliceStatus::Ineffective,
            "平台能力探测未确认 Job Object 或整树终止",
        )
    };
    SandboxSliceFact::new(
        SandboxCapability::ProcessTreeKill,
        status,
        "Windows Job Object",
        reason,
    )
}

/// 文件系统隔离能力探测。当前没调用任何受限令牌/AppContainer ACL 创建
/// 逻辑，也没有真实拒绝测试证据，因此不能把词法路径检查算作 OS 级隔离。
pub fn filesystem_isolation() -> SandboxSliceFact {
    let capabilities = crate::domain::sandbox::PlatformCapabilities::detect();
    let (status, reason) = if capabilities.has_filesystem_isolation {
        (
            SandboxSliceStatus::Effective,
            "平台能力探测确认存在 OS 级文件系统隔离",
        )
    } else {
        (
            SandboxSliceStatus::Ineffective,
            "受限令牌/AppContainer 未接线；只有 workspace 路径词法检查",
        )
    };
    SandboxSliceFact::new(
        SandboxCapability::FileSystem,
        status,
        "受限令牌/AppContainer",
        reason,
    )
}

/// 网络隔离能力探测。当前没有 AppContainer network capability 或防火墙
/// 策略施放，网络隔离必须标未生效。
pub fn network_isolation() -> SandboxSliceFact {
    let capabilities = crate::domain::sandbox::PlatformCapabilities::detect();
    let (status, reason) = if capabilities.has_network_isolation {
        (
            SandboxSliceStatus::Effective,
            "平台能力探测确认存在 OS 级网络隔离",
        )
    } else {
        (
            SandboxSliceStatus::Ineffective,
            "AppContainer/网络策略未接线；开放命令继续默认拒绝",
        )
    };
    SandboxSliceFact::new(
        SandboxCapability::Network,
        status,
        "AppContainer/受限令牌网络策略",
        reason,
    )
}

/// MCP 子进程同等隔离切片。没有 `isolate_child_spec()` 接线证据时为未生效；
/// 主代理接入 MCP spawn 后由测试证据决定是否改为生效。
pub fn mcp_subprocess(connected: bool) -> SandboxSliceFact {
    let (status, reason) = if connected {
        (
            SandboxSliceStatus::Effective,
            "MCP spawn 前已调用 isolate_child_spec 并写入同一策略事实",
        )
    } else {
        (
            SandboxSliceStatus::Ineffective,
            "MCP spawn 未接入 SandboxEnforcer，尚无文件/网络同等隔离",
        )
    };
    SandboxSliceFact::new(
        SandboxCapability::McpSubprocess,
        status,
        "SandboxEnforcer::apply / isolate_child_spec",
        reason,
    )
}

/// 汇总 Windows 已实测与未生效切片，给 --doctor、--sandbox-debug 与证据账本。
pub fn probe() -> Vec<SandboxSliceFact> {
    vec![
        process_tree_kill(),
        filesystem_isolation(),
        network_isolation(),
        mcp_subprocess(false),
    ]
}

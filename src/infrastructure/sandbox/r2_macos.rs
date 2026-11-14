//! macOS R2 SBPL profile：只生成文本与结构化事实，不在 Windows 上假装实测。
//!
//! `sandbox-exec` 的 profile 由真机脚本在 spawn 前传给子进程。当前模块
//! 负责生成可读、可复现的 SBPL，以及"本机未实测"的降级事实。

use std::path::Path;

use crate::domain::sandbox_fact::{SandboxCapability, SandboxSliceFact, SandboxSliceStatus};

/// 生成 sandbox-exec 的 SBPL profile。默认拒绝所有操作，再显式允许
/// workspace 子树写入与必要的本地进程/系统读取；不放开 network*。
/// 路径按 SBPL 字符串字面量做最小转义，避免引号或反斜杠破坏 profile。
pub fn sbpl_profile(workspace: &Path) -> String {
    let workspace = escape_sbpl(&workspace.to_string_lossy());
    format!(
        "(version 1)\n(deny default)\n(allow process-exec (literal \"{workspace}\"))\n(allow file-read*)\n(allow file-write* (subpath \"{workspace}\"))\n(deny network*)\n(deny file-write* (outside (subpath \"{workspace}\")))\n"
    )
}

/// workspace 外写入与网络规则必须同时出现在 profile 中。测试与真机脚本
/// 都可用这段检查确认调试展示与施放依据一致，不另算一套规则。
pub fn sbpl_rule_ids() -> [&'static str; 2] {
    ["R2.MACOS.FS.WRITE_OUTSIDE", "R2.MACOS.NETWORK.DENY"]
}

/// SBPL 文件系统切片。只有真机脚本传入 profile_applied=true 且 workspace
/// 外写入被真实拒绝时，才允许写 Effective。
pub fn filesystem_isolation(profile_applied: bool) -> SandboxSliceFact {
    let status = if profile_applied {
        SandboxSliceStatus::Effective
    } else {
        SandboxSliceStatus::Ineffective
    };
    SandboxSliceFact::new(
        SandboxCapability::FileSystem,
        status,
        "sandbox-exec SBPL (allow file-write* (subpath workspace))",
        "未挂接 profile 或未跑 workspace 外写入拒绝用例时必须降级",
    )
}

/// SBPL 网络切片。profile 中有 `(deny network*)` 不代表已挂到子进程；
/// 真机网络拒绝用例通过后才标生效。
pub fn network_isolation(profile_applied: bool) -> SandboxSliceFact {
    let status = if profile_applied {
        SandboxSliceStatus::Effective
    } else {
        SandboxSliceStatus::Ineffective
    };
    SandboxSliceFact::new(
        SandboxCapability::Network,
        status,
        "sandbox-exec SBPL (deny network*)",
        "未挂接 profile 或未跑网络拒绝用例时必须降级",
    )
}

/// macOS 进程树终止切片。sandbox-exec 本身不负责整树终止，需要真机
/// 进程组终止证据；没有证据时如实写未生效。
pub fn process_tree_kill(evidence: bool) -> SandboxSliceFact {
    let status = if evidence {
        SandboxSliceStatus::Effective
    } else {
        SandboxSliceStatus::Ineffective
    };
    SandboxSliceFact::new(
        SandboxCapability::ProcessTreeKill,
        status,
        "进程组终止证据",
        "sandbox-exec profile 不等于整树终止，真机用例通过前不得写生效",
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

/// 供 macOS 真机测试调用的能力汇总。参数必须由真机拒绝用例产生。
pub fn probe(
    profile_applied: bool,
    tree_kill_evidence: bool,
    mcp_connected: bool,
) -> Vec<SandboxSliceFact> {
    vec![
        process_tree_kill(tree_kill_evidence),
        filesystem_isolation(profile_applied),
        network_isolation(profile_applied),
        mcp_subprocess(mcp_connected),
    ]
}

/// 未在本机实测的 macOS 目标统一标 Untested，并保留可复现理由。
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
            "macOS sandbox-exec SBPL 真机用例",
            reason,
        )
    })
    .collect()
}

fn escape_sbpl(raw: &str) -> String {
    raw.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_denies_network_and_only_allows_workspace_writes() {
        let profile = sbpl_profile(Path::new("/tmp/qch-r2"));
        assert!(profile.contains("(deny default)"));
        assert!(profile.contains("(deny network*)"));
        assert!(profile.contains("(allow file-write* (subpath \"/tmp/qch-r2\"))"));
        assert_eq!(sbpl_rule_ids().len(), 2);
    }
}

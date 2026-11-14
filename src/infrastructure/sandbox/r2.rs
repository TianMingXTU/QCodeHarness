//! R2 沙箱规则判定、能力探测与调试渲染。
//!
//! 命令词法与工作区根比较是纯逻辑；内核/系统能力探测放在按编译目标加载
//! 的平台文件里。未在编译目标上实测的能力一律标为"未实测"或"未生效"，
//! 不写成有效，也不据此静默放行。

use std::path::{Path, PathBuf};

use crate::domain::command::parse_command;
use crate::domain::platform::Platform;
use crate::domain::sandbox::{write_escape_beyond, write_escape_suspect};
#[cfg(target_os = "linux")]
#[path = "r2_linux.rs"]
mod linux;
#[cfg(target_os = "macos")]
#[path = "r2_macos.rs"]
mod macos;
#[cfg(target_os = "windows")]
#[path = "r2_windows.rs"]
mod windows;

use crate::domain::sandbox_fact::{
    ProcessSpec, RuleHit, SandboxCapability, SandboxDebugReport, SandboxDecision, SandboxFact,
    SandboxSliceFact, SandboxSliceStatus,
};

/// 工作区外写入规则的稳定 ID，调试输出和测试断言都引用它。
pub const RULE_FS_WRITE_ESCAPE: &str = "R2.FS.WRITE_ESCAPE";
/// 疑似越界路径升级审批规则的稳定 ID。
pub const RULE_FS_ESCAPE_SUSPECT: &str = "R2.FS.ESCAPE_SUSPECT";
/// 外部读写嫌疑规则的稳定 ID。词法模型不展开变量，只能表达嫌疑。
pub const RULE_FS_EXTERNAL_ACCESS: &str = "R2.FS.EXTERNAL_ACCESS";
/// 网络访问嫌疑规则的稳定 ID。
pub const RULE_NETWORK_ACCESS: &str = "R2.NET.ACCESS";
/// 缺少完整隔离时开放命令默认拒绝规则的稳定 ID。
pub const RULE_OPEN_COMMAND: &str = "R2.RUNTIME.OPEN_COMMAND";
/// 降级证据规则的稳定 ID。
pub const RULE_DEGRADED: &str = "R2.RUNTIME.DEGRADED";

/// 按编译目标加载的平台实现共享同一套 trait；装配层只拿到 `Arc<dyn ...>`，
/// 不直接依赖 Windows/Linux/macOS 具体模块，便于 MCP 子进程复用内置策略。
pub fn sandbox_policy() -> std::sync::Arc<dyn crate::application::sandbox_policy::SandboxPolicy> {
    std::sync::Arc::new(R2SandboxPolicy)
}

/// 返回由本模块同一份规则与事实驱动的隔离执行器。调试和 MCP 接线共用它。
pub fn sandbox_enforcer() -> std::sync::Arc<dyn crate::application::sandbox_policy::SandboxEnforcer>
{
    std::sync::Arc::new(R2SandboxEnforcer)
}

/// application 端口的默认实现。`workspace()` 取进程当前目录，避免在纯
/// 规则 trait 签名里引入 IO；调用方可用 `inspect_command` 显式传 workspace。
struct R2SandboxPolicy;

impl crate::application::sandbox_policy::SandboxPolicy for R2SandboxPolicy {
    fn inspect(&self, command: &str) -> SandboxDecision {
        let workspace = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        inspect_command(&workspace, command)
    }
}

/// application 端口的默认执行器。`apply` 只生成事实并显式检查是否允许
/// spawn；真正挂接 OS 隔离仍由 shell/MCP 装配层的精确 diff 完成。
struct R2SandboxEnforcer;

impl crate::application::sandbox_policy::SandboxEnforcer for R2SandboxEnforcer {
    fn apply(&self, process_spec: &ProcessSpec) -> SandboxFact {
        let root = Path::new(&process_spec.workspace);
        let decision = inspect_command(root, &process_spec.command);
        SandboxFact::from_apply(
            Platform::current().label(),
            process_spec.command.clone(),
            process_spec.origin,
            decision,
            probe_platform_slices(),
        )
    }

    fn debug(&self, command: &str) -> SandboxDebugReport {
        let workspace = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        debug_command(&workspace, command)
    }
}

/// 调试入口：给出命令、工作区根，只返回规则命中与判定，不执行任何命令。
///
/// `SandboxDebugReport::not_executed` 把 `executed` 固定为 false；本函数
/// 本身不接收可启动进程的句柄，从接口上排除副作用。
pub fn debug_command(workspace: &Path, command: &str) -> SandboxDebugReport {
    let decision = inspect_command(workspace, command);
    let slices = probe_platform_slices();
    let fact = SandboxFact::from_debug(Platform::current().label(), command, decision, slices);
    SandboxDebugReport::not_executed(fact)
}

/// 返回本进程所处平台的能力探测事实。真实平台文件（linux.rs/macos.rs/
/// windows.rs）负责读取内核或系统能力；本机未编译的平台只返回可复现的
/// 静态 "未实测" 事实，不能把别的目标伪装成已生效。
pub fn probe_platform_slices() -> Vec<SandboxSliceFact> {
    #[cfg(target_os = "windows")]
    {
        return windows::probe();
    }
    #[cfg(target_os = "linux")]
    {
        // 当前 R2 没有在 spawn 前挂接 Landlock ruleset/seccomp 过滤器，
        // 也没有真机拒绝证据；探测结果只用于 --doctor 与降级报告。
        return linux::probe(false, false, false, false);
    }
    #[cfg(target_os = "macos")]
    {
        // SBPL 文本可在本模块生成，但尚未挂到 spawn 前，不能写生效。
        return macos::probe(false, false, false);
    }
    #[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
    {
        return platform_slice_facts();
    }
}

/// MCP 子进程接到与内置命令同一套规则前的接线点。函数只应用纯逻辑策略：
/// 把命中的规则和降级事实写进 `ProcessSpec` 对应的 fact，并显式检查是否
/// 允许 spawn。调用方必须在 `Command::spawn` 前检查返回的 Err，且把
/// `SandboxDegradedFact` 交给证据账本记录。
pub fn isolate_child_spec(process_spec: &ProcessSpec) -> Result<SandboxFact, String> {
    let root = Path::new(&process_spec.workspace);
    let command = format!(
        "{} {}",
        process_spec.program,
        process_spec.arguments.join(" ")
    );
    let decision = inspect_command(root, &command);
    let slices = probe_platform_slices();
    let fact = SandboxFact::from_apply(
        Platform::current().label(),
        process_spec.command.clone(),
        process_spec.origin,
        decision,
        slices,
    );
    fact.require_execution_permitted()?;
    Ok(fact)
}

/// 纯逻辑规则判定。命中越界写入直接 Deny；未证明隔离生效的开放命令
/// 走 Degrade，后续执行层必须先落 `SandboxDegraded` 证据再决定是否运行。
/// 现有领域词法判定不展开变量，因此未知路径只标嫌疑，不断言已经拦截。
pub fn inspect_command(workspace: &Path, command: &str) -> SandboxDecision {
    let parsed = parse_command(command);
    let mut hits = Vec::new();

    if let Some(reason) = write_escape_beyond(command, workspace) {
        hits.push(RuleHit::new(
            RULE_FS_WRITE_ESCAPE,
            format!("workspace 外写入硬拒绝：{reason}"),
        ));
        return SandboxDecision::deny(hits, "命令字面路径已越出 workspace，调试也按拒绝报告");
    }
    if let Some(reason) = write_escape_suspect(command) {
        hits.push(RuleHit::new(RULE_FS_ESCAPE_SUSPECT, reason));
    }

    let external = parsed.segments.iter().any(|segment| {
        segment.words.iter().any(|word| {
            let candidate = word.path_candidate();
            is_absolute_path(candidate) && !is_within_workspace(candidate, workspace)
        })
    });
    let network = parsed.segments.iter().any(|segment| {
        let program = segment.program().unwrap_or_default().to_ascii_lowercase();
        let stem = program
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or(&program)
            .trim_end_matches(".exe");
        matches!(
            stem,
            "curl"
                | "wget"
                | "ssh"
                | "scp"
                | "sftp"
                | "ftp"
                | "nc"
                | "ncat"
                | "netcat"
                | "telnet"
                | "git"
                | "npm"
                | "pip"
                | "cargo"
        ) || segment.words.iter().any(|word| {
            let text = word.path_candidate().to_ascii_lowercase();
            text.starts_with("http://") || text.starts_with("https://")
        })
    });
    if external {
        hits.push(RuleHit::new(
            RULE_FS_EXTERNAL_ACCESS,
            "命令词元包含 workspace 外绝对路径；纯词法不展开变量或符号链接".to_owned(),
        ));
    }
    if network {
        hits.push(RuleHit::new(
            RULE_NETWORK_ACCESS,
            "命令词元包含网络访问程序或 URL；未证明网络隔离生效".to_owned(),
        ));
    }

    // 完整隔离尚未由平台切片证明生效。开放命令与网络/外部访问不能按正常
    // 放行处理；Degrade 不表示"允许运行"，它表示必须落降级证据后收口。
    let slices = probe_platform_slices();
    let isolation_effective = slices
        .iter()
        .all(|slice| slice.status == SandboxSliceStatus::Effective);
    if !isolation_effective {
        hits.push(RuleHit::new(
            RULE_DEGRADED,
            "当前进程的文件/网络隔离切片未证明生效；必须记录 SandboxDegraded".to_owned(),
        ));
        if external || network {
            hits.push(RuleHit::new(
                RULE_OPEN_COMMAND,
                "开放命令在缺隔离能力时默认拒绝，无沙箱能力平台不放行".to_owned(),
            ));
            return SandboxDecision::degrade(
                hits,
                "隔离能力未生效或未实测：拒绝按正常路径执行，并写入降级证据",
            )
            .with_stable_code("sandbox_unisolated_command");
        }
        if !hits.is_empty() {
            return SandboxDecision::degrade(hits, "命中嫌疑规则且隔离未生效，必须落降级证据");
        }
        // 既有策略允许只读/测试基线在缺能力时继续运行，但降级事实仍存在，
        // 执行层必须把它写入证据账本，不能当作隔离已生效。
        return SandboxDecision::allow(hits, "基线命令可进入后续策略与审批，降级事实必须记账");
    }

    SandboxDecision::allow(hits, "所有隔离切片已证明生效")
}

/// 平台能力切片。Windows 上只有现有 Job Object 的进程树终止可声称已实测；
/// 文件与网络隔离尚未接入，如实标"未生效"。Linux/macOS 当前编译目标无法
/// 在本机实测，标"未实测"，由对应平台脚本产出证据文件后再标 Effective。
/// MCP 子进程在未接线前一律标"未生效"，防止调试输出虚报同等隔离。
pub fn platform_slice_facts() -> Vec<SandboxSliceFact> {
    let platform = Platform::current();
    let target = platform.label();
    match platform {
        Platform::Windows => vec![
            SandboxSliceFact::new(
                SandboxCapability::ProcessTreeKill,
                SandboxSliceStatus::Effective,
                "Windows Job Object",
                "既有 Job Object 负责进程树终止与资源限额",
            ),
            SandboxSliceFact::new(
                SandboxCapability::FileSystem,
                SandboxSliceStatus::Ineffective,
                "受限令牌/AppContainer 未接线",
                "当前仅有词法路径判定，未形成 OS 级文件系统隔离",
            ),
            SandboxSliceFact::new(
                SandboxCapability::Network,
                SandboxSliceStatus::Ineffective,
                "AppContainer/防火墙策略未接线",
                "当前没有 OS 级网络隔离，开放命令继续默认拒绝",
            ),
            SandboxSliceFact::new(
                SandboxCapability::McpSubprocess,
                SandboxSliceStatus::Ineffective,
                "MCP spawn 尚未接入 SandboxEnforcer",
                "MCP 子进程只纳入现有 Job Object，尚无同等文件/网络隔离",
            ),
        ],
        // Linux 与 macOS 在 Windows 本机编译运行单元测试时只能给"未实测"；
        // 真机切片由 scripts/sandbox-*.sh 与 tests/sandbox_linux.rs/macos.rs
        // 生成证据后再改状态，不允许用本机猜测填 Effective。
        Platform::Linux => untested_slice_facts(
            target,
            "Landlock 文件系统 + seccomp 网络/系统调用",
            "本机未在 Linux 目标实测 Landlock/seccomp",
        ),
        Platform::MacOs => untested_slice_facts(
            target,
            "sandbox-exec SBPL 文件系统 + 网络",
            "本机未在 macOS 目标实测 sandbox-exec profile",
        ),
        Platform::Other => {
            untested_slice_facts(target, "无已实现平台机制", "当前平台没有 R2 沙箱实现")
        }
    }
}

fn untested_slice_facts(target: &str, backing: &str, reason: &str) -> Vec<SandboxSliceFact> {
    let all = [
        SandboxCapability::ProcessTreeKill,
        SandboxCapability::FileSystem,
        SandboxCapability::Network,
        SandboxCapability::McpSubprocess,
    ];
    all.into_iter()
        .map(|capability| {
            SandboxSliceFact::new(
                capability,
                SandboxSliceStatus::Untested,
                format!("{target}: {backing}"),
                reason,
            )
        })
        .collect()
}

/// 纯词法绝对路径形状。与领域层 `escape_shape` 口径一致，这里不访问文件
/// 系统；Windows 盘符、UNC 和带目录分隔符的 POSIX 绝对路径都算外部嫌疑。
fn is_absolute_path(token: &str) -> bool {
    let token = token.trim_matches(['"', '\'']);
    let bytes = token.as_bytes();
    let drive_absolute = bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'\\' || bytes[2] == b'/');
    let unc = token.starts_with("//") || token.starts_with(r"\\");
    let posix_absolute = token.starts_with('/') && token[1..].contains(['/', '\\']);
    drive_absolute
        || unc
        || posix_absolute
        || (bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':')
}

/// workspace 根前缀比较。只用于报告的"嫌疑"判定，硬拒绝仍由领域层
/// `write_escape_beyond` 承担；路径大小写规则沿用 Platform 的唯一事实。
fn is_within_workspace(token: &str, workspace: &Path) -> bool {
    let case_insensitive = Platform::current().has_case_insensitive_paths();
    let normalize = |raw: &str| {
        raw.split(['\\', '/'])
            .filter(|part| !part.is_empty() && *part != ".")
            .map(|part| {
                if case_insensitive {
                    part.to_ascii_lowercase()
                } else {
                    part.to_owned()
                }
            })
            .collect::<Vec<_>>()
    };
    let candidate = normalize(token.trim_matches(['"', '\'']));
    let root = normalize(&workspace.to_string_lossy());
    candidate.starts_with(&root)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_slices_never_claim_unimplemented_isolation() {
        if Platform::current() != Platform::Windows {
            return;
        }
        let facts = platform_slice_facts();
        for capability in [SandboxCapability::FileSystem, SandboxCapability::Network] {
            let fact = facts
                .iter()
                .find(|slice| slice.capability == capability)
                .expect("每个切片都必须有事实");
            assert_ne!(fact.status, SandboxSliceStatus::Effective);
        }
    }
}

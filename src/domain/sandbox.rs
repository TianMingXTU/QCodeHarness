//! 进程沙箱策略、执行边界元数据与命令的结构化词法边界裁决。
//!
//! 领域层只计算纯数据；资源与生命周期约束由 infrastructure/sandbox
//! 强制执行。目前只有 Windows 的 Job Object 提供进程沙箱，且它不提供
//! 文件系统或网络隔离；其他平台没有进程沙箱。平台能力见
//! [`PlatformCapabilities::detect`]。

use std::path::Path;
use std::time::Duration;

use super::command::{CommandSegment, ParsedCommand, RedirectOp, parse_command};
use super::platform::Platform;

/// 当前编译目标具备的隔离能力，不代表某次命令已经成功启用沙箱。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlatformCapabilities {
    /// 当前运行的平台。
    pub platform: Platform,
    /// 是否有进程沙箱。Windows 上是 Job Object。
    pub has_process_sandbox: bool,
    /// 超时能否终止整棵进程树（不只是直接子进程）。
    ///
    /// 与 `has_process_sandbox` 分开是因为两者不是一回事：进程沙箱管
    /// 资源上限，整树终止管超时后的清理。平台只提供前者时，超时仍会
    /// 留下继续运行的后代进程。
    pub has_tree_kill: bool,
    pub has_filesystem_isolation: bool,
    pub has_network_isolation: bool,
}

impl PlatformCapabilities {
    /// 检测当前运行的平台具备哪些隔离能力。
    ///
    /// 能力由平台决定，不由配置决定：进程沙箱与整树终止目前只有 Windows
    /// 的 Job Object 实现，其余平台没有对应机制。文件系统与网络隔离任何
    /// 平台都没有实现，因此恒为 false。
    pub fn detect() -> Self {
        let platform = Platform::current();
        let has_process_sandbox = platform == Platform::Windows;
        Self {
            platform,
            has_process_sandbox,
            has_tree_kill: has_process_sandbox,
            has_filesystem_isolation: false,
            has_network_isolation: false,
        }
    }

    /// 平台标签。写进证据的边界字段，说明这条边界由哪个平台产生。
    ///
    /// Windows 上标注具体机制（Job Object），其余平台只标平台名，因为
    /// 它们没有进程沙箱可标。标签不说明保护是否生效：生效与否由
    /// `active` 单独表示，两个字段合起来才是完整事实。
    pub fn label(&self) -> &'static str {
        match self.platform {
            Platform::Windows => "Windows JobObject",
            Platform::Linux => "Linux",
            Platform::MacOs => "macOS",
            Platform::Other => "其他系统",
        }
    }

    /// 当前平台对命令进程的保护范围，写给模型看。
    ///
    /// 这句话必须按平台生成。整树终止与资源上限是 Windows Job Object
    /// 的能力，其他平台没有对应机制；把它们写成无条件事实，模型会以为
    /// 超时一定收得干净，后台进程一定被杀。
    pub fn process_guard_summary(&self) -> String {
        if self.has_process_sandbox {
            format!(
                "命令在进程沙箱内执行：整树内存与进程数受限，{}。\
                 不要依赖超限资源或遗留后台进程。",
                if self.has_tree_kill {
                    "超时终止全部后代进程（不只是直接子进程）"
                } else {
                    "超时只终止直接子进程，后代进程可能继续运行"
                }
            )
        } else {
            format!(
                "当前平台（{}）没有进程沙箱：命令不受内存与进程数限制，\
                 超时只终止直接子进程，后代进程可能继续运行。\
                 长任务请拆小或调大 timeout_seconds，不要留下你不再等待的后台进程。",
                self.platform.label()
            )
        }
    }
}

pub const SANDBOX_DOWNGRADE_NOTICE: &str =
    "⚠️ 进程沙箱未生效：本命令在无资源限制的宿主机环境执行，请格外保守。";
pub const MCP_BOUNDARY_LABEL: &str = "MCP subprocess";

/// 执行方报告的实际边界状态，不能从命令输出文本推断。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExecutionBoundary {
    pub platform: &'static str,
    pub active: bool,
    pub boundary_checked: bool,
}

impl ExecutionBoundary {
    pub fn process(active: bool, boundary_checked: bool) -> Self {
        Self {
            platform: PlatformCapabilities::detect().label(),
            active,
            boundary_checked,
        }
    }

    /// MCP 调用的边界事实。`process_isolation` 表示 Server 进程是否在
    /// 握手**之前**纳入宿主进程沙箱（Windows 上为 Job Object）。纳入只
    /// 覆盖该进程之后创建的子孙，先后顺序决定该字段是否代表真实保护。
    /// 纳入失败、拿不到 PID 或非 Windows 平台时为 false。口径与 shell
    /// 工具一致：不虚报保护，也如实反映治理后的边界状态。
    ///
    /// `platform` 固定为 `MCP subprocess`，不使用平台标签：证据需要区分
    /// "来自外部 Server"与"内置工具"，进程隔离状态由 `active` 单独表示。
    pub fn mcp(process_isolation: bool) -> Self {
        Self {
            platform: MCP_BOUNDARY_LABEL,
            active: process_isolation,
            boundary_checked: false,
        }
    }
}

/// 进程沙箱策略。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxPolicy {
    /// 超时或结束时终止整个进程树（含孙进程）。
    ///
    /// `child.kill()` 只终止直接子进程，由它拉起的孙进程（如 Windows 的
    /// `cmd /C "start /b ..."`、POSIX shell 的后台任务）会运行到自然结束。
    /// 仅杀子进程会让模型误以为任务已停止，孤儿进程继续占用宿主机资源。
    ///
    /// 该开关只在有进程沙箱的平台（Windows Job Object）上生效。其他平台
    /// 没有整树终止能力，超时只终止直接子进程，后代进程不在清理边界内。
    pub kill_tree: bool,
    /// 树内单个进程的内存提交上限（字节）。None = 不限。
    pub per_process_memory: Option<u64>,
    /// 树内同时存活的进程数上限。None = 不限。
    pub max_processes: Option<u32>,
    /// 树内全部进程的 CPU 时间累计上限。None = 不限。
    ///
    /// 默认不设：cargo/rustc 并行编译的 CPU 时间无法预估，限制过小会
    /// 误杀。进程数与内存两项上限已覆盖失控 fork 的主要情形。
    pub cpu_time: Option<Duration>,
}

impl Default for SandboxPolicy {
    /// 默认策略偏宽，优先避免误杀。
    ///
    /// 本项目的测试和基准都在沙箱内运行 `cargo test`（编译 fixture
    /// crate），限额过紧会杀掉自身工作流。沙箱提供机械兜底，不做精细
    /// 配额：kill_tree 在有进程沙箱的平台上修复孙进程失控这一确凿缺陷，
    /// 内存与进程数设置宽裕上限，正常编译用不到，失控时起限制作用。
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

/// 判断命令是否可能把写操作延伸到 workspace 外，命中后升级审批。
///
/// 这是**词法**判定，既不是隔离也不是 shell 求值。变量拼接、命令替换、
/// 编码绕过与链接的物理目标仍可绕过，真正的进程边界由基础设施层
/// 建立。无根与有根共用同一份解析结果，差别只在 `root`：无根时所有
/// 绝对路径都视为嫌疑，有根时拒绝越出根的路径；含 `..` 成分的词元
/// 无论有无根一律拒绝（`..` 出现即嫌疑，不做归一化豁免）。
pub fn write_escape_suspect(command: &str) -> Option<String> {
    command_escape_violation(&parse_command(command), None)
}

pub fn write_escape_beyond(command: &str, root: &Path) -> Option<String> {
    command_escape_violation(&parse_command(command), Some(root))
}

/// 两道防线共用的裁决器：逐段、逐重定向、逐词元检查越界，任一处命中
/// 即拒绝。
///
/// `root` 体现两道防线有意保留的差异：无根调用没有根可参照，所有盘符
/// 绝对路径都升级审批，根内路径也再问一次；有根调用知道根的位置，拒绝
/// 越出根的路径，`..` 成分则两种模式都拒绝。审批可以多问，机械防线必须
/// 准确，两者语义不同，不能合并为一个判据。
fn command_escape_violation(parsed: &ParsedCommand, root: Option<&Path>) -> Option<String> {
    let multi_segment = parsed.segments.len() > 1;
    for (index, segment) in parsed.segments.iter().enumerate() {
        for redirect in &segment.redirects {
            // `<` 是输入重定向，不是写出 workspace；其余形态一并检查。
            if redirect.op == RedirectOp::Input {
                continue;
            }
            if let Some(reason) = path_escape_reason(&redirect.target, root) {
                return Some(segment_note(
                    format!("命令把输出重定向到 workspace 外：{reason}"),
                    segment,
                    index,
                    multi_segment,
                ));
            }
        }
        // 重定向目标也留在词元流里。现行口径是任何词元提到越界路径
        // 即触发，不能因为有了 redirect 索引就只看重定向。
        for word in &segment.words {
            let candidate = word.path_candidate();
            if let Some(reason) = path_escape_reason(candidate, root) {
                // 理由中给出完整词元（旧格式对 --flag= 也取整个词元），
                // 判据只看等号后的值，两者不同。
                let text = match write_verb(segment) {
                    Some(verb) => {
                        format!(
                            "命令用 `{verb}` 操作 workspace 外的 `{}`：{reason}",
                            word.text
                        )
                    }
                    None => format!("命令提到 workspace 外的 `{}`：{reason}", word.text),
                };
                return Some(segment_note(text, segment, index, multi_segment));
            }
        }
    }
    None
}

/// 多段命令的理由标明段号与程序；单段沿用既有文案模板，不追加段号。
/// 重定向目标由结构化模型直接收集，带空格的目标同样归为"输出重定向"。
/// `<` 是输入重定向，不套用输出重定向的理由。
fn segment_note(
    reason: String,
    segment: &CommandSegment,
    index: usize,
    multi_segment: bool,
) -> String {
    if !multi_segment {
        return reason;
    }
    let program = segment.program().unwrap_or("");
    format!("{reason}（第 {} 段，程序 `{program}`）", index + 1)
}

/// 写入型动词只用于生成可读理由，不是触发条件：任何越界路径都会触发。
/// 归属按**段**判定：`cargo test && rm D:/outside/x` 的理由应指向 `rm`，
/// 不从整条命令中挑选出现过的动词。
fn write_verb(segment: &CommandSegment) -> Option<&'static str> {
    const WRITE_VERBS: [&str; 11] = [
        "del",
        "rd",
        "rmdir",
        "move",
        "ren",
        "copy",
        "remove-item",
        "rm",
        "mv",
        "cp",
        "mkdir",
    ];
    WRITE_VERBS.into_iter().find(|verb| {
        segment
            .words
            .iter()
            .any(|word| word.text.eq_ignore_ascii_case(verb))
    })
}

fn unquote(token: &str) -> &str {
    token.trim_matches(['"', '\''])
}

/// 判断一个字面路径词元是否越界。不模拟变量或命令替换。
fn path_escape_reason(token: &str, root: Option<&Path>) -> Option<String> {
    let token = unquote(token);
    if token.is_empty()
        || token.starts_with('-')
        || matches!(
            token,
            "|" | "||" | "&" | "&&" | ";" | "(" | ")" | ">" | ">>" | "<"
        )
    {
        return None;
    }
    if token.split(['\\', '/']).any(|part| part == "..") {
        return Some(format!("`{token}` 含父目录"));
    }
    if root.is_none() {
        return escape_shape(token).then(|| format!("`{token}` 指向 workspace 外的绝对路径"));
    }
    let root = root.unwrap();
    if !is_absolute_path(token) {
        return None;
    }
    let normalized = normalize_path(token);
    let root_parts = normalize_path(&root.to_string_lossy());
    if normalized.starts_with(&root_parts) {
        return None;
    }
    Some(format!("`{token}` 不在当前 workspace 根下"))
}

/// 无 workspace 根时，所有能识别的绝对路径与设备前缀都保守视为嫌疑。
fn escape_shape(token: &str) -> bool {
    let bytes = token.as_bytes();
    let drive_absolute = bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'\\' || bytes[2] == b'/');
    let drive_relative = bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':';
    let unc = token.starts_with("//")
        || token.starts_with(r"\\")
        || token.starts_with("/\\")
        || token.starts_with("\\/");
    // 裸 `/` 判据区分开关与带目录分隔符的路径：/s 放行，/tmp/out 收紧。
    let posix_absolute = token.starts_with('/') && token[1..].contains(['/', '\\']);
    unc || drive_absolute
        || drive_relative
        || posix_absolute
        || token.starts_with('\\') && token.len() > 1
}

fn is_absolute_path(token: &str) -> bool {
    escape_shape(token)
}

/// 纯词法规范化：统一分隔符、剥离设备/UNC 前缀、抵消 `.` 与 `..`，
/// 按当前平台的文件系统语义决定是否忽略大小写。不访问文件系统。
fn normalize_path(raw: &str) -> Vec<String> {
    let trimmed = trim_device_prefix(raw);
    let case_insensitive = Platform::current().has_case_insensitive_paths();
    let mut parts: Vec<String> = Vec::new();
    for part in trimmed.split(['\\', '/']) {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            name => parts.push(if case_insensitive {
                name.to_ascii_lowercase()
            } else {
                name.to_owned()
            }),
        }
    }
    parts
}

/// `\\?\C:\x`、`//?/UNC/server/share/x` 等前缀只影响 Windows 的路径
/// 解释，不改变指向的卷或共享。UNC 的 server/share 作为普通组件比较。
fn trim_device_prefix(raw: &str) -> &str {
    let mut text = raw;
    for prefix in [r"\\?\UNC\", r"\\.\", r"\\?\", "//?/UNC/", "//./", "//?/"] {
        if let Some(rest) = text.strip_prefix(prefix) {
            text = rest;
            break;
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::command;

    /// 检测到的能力必须与当前平台一致。
    ///
    /// 平台能力是平台事实，不是配置项：Windows 有 Job Object，其余平台
    /// 没有。断言按平台分支，两个平台都真跑。
    #[test]
    fn detected_capabilities_follow_the_current_platform() {
        let capabilities = PlatformCapabilities::detect();
        assert_eq!(capabilities.platform, Platform::current());
        assert_eq!(
            capabilities.has_process_sandbox,
            capabilities.platform == Platform::Windows,
            "进程沙箱目前只有 Windows 的 Job Object 实现"
        );
        // 文件系统与网络隔离任何平台都没有实现。
        assert!(!capabilities.has_filesystem_isolation);
        assert!(!capabilities.has_network_isolation);
    }

    /// 给模型的保护说明必须与能力一致，不能声称平台做不到的事。
    ///
    /// 无进程沙箱的平台若读到"超时终止全部后代进程"，会放心地留下后台
    /// 进程。有进程沙箱的平台则必须说明资源上限。
    #[test]
    fn the_guard_summary_never_claims_more_than_the_platform_can_do() {
        let capabilities = PlatformCapabilities::detect();
        let summary = capabilities.process_guard_summary();

        if capabilities.has_process_sandbox {
            assert!(summary.contains("进程沙箱"), "应说明有沙箱: {summary}");
            if capabilities.has_tree_kill {
                assert!(summary.contains("终止全部后代进程"), "{summary}");
            } else {
                assert!(summary.contains("只终止直接子进程"), "{summary}");
            }
        } else {
            assert!(summary.contains("没有进程沙箱"), "{summary}");
            assert!(summary.contains("只终止直接子进程"), "{summary}");
            assert!(
                !summary.contains("终止全部后代进程"),
                "不能声称本平台做不到的事: {summary}"
            );
        }
    }

    #[test]
    fn default_policy_kills_tree_and_sets_generous_ceilings() {
        let policy = SandboxPolicy::default();
        // kill_tree 是核心行为：整树终止默认开启，不依赖调用方设置。
        assert!(policy.kill_tree);
        // 内存与进程数上限宽裕，正常构建不会触及。
        assert_eq!(policy.per_process_memory, Some(4 * 1024 * 1024 * 1024));
        assert_eq!(policy.max_processes, Some(256));
        // CPU 时间不设：编译型负载的 CPU 时间无法预估。
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

    // ── 越界写入嫌疑 ──────────────────────────────────────────────

    /// 真值表：越界的写入被识别，workspace 内的操作不被误判。
    ///
    /// 每条对应一种实际出现过的越界写法。
    #[test]
    fn write_escape_suspect_detects_out_of_workspace_writes_only() {
        let suspects = [
            // 重定向到其他盘：没有写入型动词，仅 `>` 即可触发。
            r"echo hi > C:\Windows\temp\x",
            r"cargo test > D:/build.log",
            r"type a > D:/x",
            // 写入型动词 + 绝对路径。
            r"Remove-Item -Recurse C:\Windows\temp\x",
            r"del C:\Windows\temp\x",
            // 写入型动词 + `..` 穿越。
            r"del ..\foo",
            r"mkdir ..\outside",
            r"rd /s /q ..\dir",
            // 追加写同样算。
            r"echo hi >> ..\out.txt",
            // UNC 共享路径。
            r"copy src \\server\share\x",
            // 无写入型动词、无 `>` 的越界。默认基线放行 `git diff *` 一族，
            // 而 `--output` 是 git 的写文件选项，仅按动词和重定向判断会
            // 漏掉。实测 `git diff --output=D:\x` 会写入文件。
            r"git diff --output=C:\Windows\temp\x",
            // 开关与值不写空格时，值跟在 `=` 后面。
            r"cargo build --target-dir=D:/cache",
            r"git diff --output D:\Windows\temp\x",
        ];
        for command in suspects {
            assert!(
                write_escape_suspect(command).is_some(),
                "`{command}` 应判定为越界写入嫌疑"
            );
        }

        let clean = [
            "cargo test",
            r"echo hi > out.txt",
            // 重定向目标在 workspace 内，命令形式像写入也不命中。
            r"echo hi>out.txt",
            "git status",
            // 网络操作不在本启发式范围内（见模块文档）。
            "git push",
            "curl -o out.txt https://example.com",
            // 词边界：`del` 不能命中 `model`。
            "cargo run --bin model.txt",
            // 删除 workspace 内的文件是策略层的事，不是越界。
            "del scratch.txt",
            // 开关不是路径：`/q` 不是根目录。
            "rd /s /q build",
        ];
        for command in clean {
            assert_eq!(
                write_escape_suspect(command),
                None,
                "`{command}` 不应被判为越界写入"
            );
        }
    }

    #[test]
    fn write_escape_suspect_covers_additional_literal_paths() {
        let commands = [
            r"copy src //server/share/x",
            r"copy src /\server/share/x",
            r"copy src \/server\share/x",
            r"echo x > //?/D:/outside",
            r"echo x > //./pipe/name",
            r"echo x > \outside",
            r"copy src D:outside",
            r"echo x > /tmp/out",
            r"git diff --output=/tmp/out",
            r"echo x > local.txt 2>/tmp/out",
        ];
        for command in commands {
            assert!(
                write_escape_suspect(command).is_some(),
                "应保守检查 `{command}`"
            );
        }
    }

    #[test]
    fn write_escape_beyond_allows_only_paths_under_the_exact_root() {
        let root = Path::new(r"D:/repo/src/..");
        // 大小写变体只差被测的 `repo` 组件：盘符大小写必须与根的写法一致，
        // 否则敏感大小写的平台上盘符先失配，测不到要测的组件。
        // 不敏感平台上故意用 `d:/REPO`，验证折叠真的生效。
        let case_variant = if Platform::current().has_case_insensitive_paths() {
            r"cargo build --manifest-path=d:/REPO/Cargo.toml"
        } else {
            r"cargo build --manifest-path=D:/repo/Cargo.toml"
        };
        let allowed = [
            r"type D:\repo\src\main.rs",
            case_variant,
            r#"type "D:\repo\sub dir\main.rs""#,
            r"echo x > D:\repo\out.txt",
            "cargo test",
            "rd /s /q build",
        ];
        for command in allowed {
            assert_eq!(
                write_escape_beyond(command, root),
                None,
                "`{command}` 应视为 workspace 内路径"
            );
        }
        // 大小写变体在敏感大小写的平台上落在根外，必须被拒。盘符与根
        // 一致，失配只可能发生在 `repo` 组件上。
        if !Platform::current().has_case_insensitive_paths() {
            assert!(
                write_escape_beyond(r"cargo build --manifest-path=D:/REPO/Cargo.toml", root)
                    .is_some(),
                "大小写敏感的平台上 `D:/REPO` 与 `D:/repo` 是两个位置，必须拒绝"
            );
        }
        let refused = [
            r"type D:\repo-evil\main.rs",
            r"type D:\repo.exe\main.rs",
            r"echo x > //server/share/x",
            r"echo x > //?/D:/outside/x",
            r"echo x > \Windows\temp\x",
            r"type D:secret.txt",
            r"echo x > /tmp/out",
            r"git diff --output=D:\other\x",
            r"type ..\outside",
            r#"type "D:\repo\..\secret""#,
        ];
        for command in refused {
            assert!(
                write_escape_beyond(command, root).is_some(),
                "`{command}` 必须被硬拒绝"
            );
        }
    }

    #[test]
    fn posix_absolute_paths_are_compared_against_a_posix_workspace_root() {
        let root = Path::new("/home/user/repo");
        assert_eq!(
            write_escape_beyond("cat /home/user/repo/src/main.rs", root),
            None
        );
        assert!(write_escape_beyond("cat /etc/passwd", root).is_some());
        assert!(write_escape_beyond("echo x > /tmp/out", root).is_some());
        assert!(write_escape_beyond("cat /home/user/repo/../secret", root).is_some());
    }

    #[test]
    fn quoted_redirect_reason_keeps_the_complete_path() {
        let reason = write_escape_suspect(r#"echo x > "D:\repo space\out.txt""#)
            .expect("绝对路径应升级审批");
        assert!(reason.contains(r"D:\repo space\out.txt"), "{reason}");
    }

    /// 词法裁决的结构化回归表：旧真值表保持不变（不放松、不误杀），
    /// 新增用例只增不减。每条注明判定依据。
    #[test]
    fn structured_adjudication_keeps_the_no_loosening_contract() {
        let root = Path::new(r"D:/repo");

        // 收紧后的新增形态：段、多重定向、--flag= 归属、引号内复合命令。
        let tightened = [
            // 越界在第 2 段。扫描整串也能命中，理由必须归到 rm 段。
            r"cargo test && rm D:/outside/x",
            // 越界在第 1 段。动词归属不能指向后面的 cargo test。
            r"rm D:/outside/x && cargo test",
            // 第二个重定向目标越界。旧实现只扫描第一个。
            r"echo x > local.txt 2> /tmp/out",
            // 分号后的 --flag= 逐段生效，等号后只看值。
            r"git diff --output=in_root ; git diff --output=D:/outside/x",
            // cmd /C "…" 的引号内复合命令，补查一层词元后必须看见重定向目标。
            r#"cmd /C "echo x > D:/outside/x""#,
            // 管道/换行/|| 的任意一段越界都拒。
            r"cargo test | copy src \\server\share\x",
            "cargo test\nrm /tmp/out",
            r"cargo test || del C:\Windows\temp\x",
            // 用户明确要求的既有拒绝形态，重构后不可丢失。
            r"cat /etc/passwd",
            r"echo x > /tmp/out",
            r"type D:\outside\x",
            r"copy src \\server\share\x",
            r"del ..\foo",
            r"git diff --output=D:\x",
            // Unicode 空白同样是词元边界。旧 tokenize 使用 is_whitespace，
            // 新词法若只认 ASCII 空格，会把整条命令粘成一个不命中的词元。
            "type\u{a0}..\\outside",
            "echo\u{a0}/etc/passwd",
            // 重定向目标本身含 `=`。目标索引不看等号，词元流仍按
            // path_candidate() 取第一个等号后的值，不能只靠 redirect 索引。
            r"echo x > X=D:/outside",
            r"echo x > --output=..\outside",
            // `<` 是输入重定向：不套"输出重定向"理由，但目标词元照常扫描。
            r"cat < /tmp/out",
            r"cat < D:\outside\x",
            // 卷根、盘符相对路径、设备前缀 UNC：三种形态都不能漏。
            r"type D:\",
            r"type D:secret.txt",
            r"type \\?\UNC\server\share\x",
            r"type //?/D:/outside/x",
            // fd 前缀的追加形态与 &> 合并输出也要按段收齐。
            r"echo x 1>>D:/outside/log",
            r"echo x 2>> /tmp/err",
            r"echo x &> /tmp/all",
            // 多段命令只由真段构成：开头链接符不留空段、不加凭空的段号。
            r"&& rm D:/outside/x",
            // `&` 只有在 `>&数字` 形态才是 fd 复制；与真实越界目标相连就是放松。
            r"echo x > local.txt&D:\outside\run.cmd",
            r"echo x > &C:\Windows\temp\x",
            r"echo x 2>&C:\Windows\temp\x",
            // heredoc 正文仍按普通词元扫描，形似外部绝对路径的文字保守拒绝。
            "cat > notes.txt <<EOF\nD:/outside/x\nEOF",
        ];
        for command in tightened {
            assert!(
                write_escape_suspect(command).is_some(),
                "`{command}` 结构词法必须仍判嫌疑"
            );
            assert!(
                write_escape_beyond(command, root).is_some(),
                "`{command}` 结构词法必须仍被硬拒绝"
            );
        }

        // 根内路径与无害命令仍然放行：收紧不能变成禁用 run_command。
        let clean = [
            "cargo test",
            r"echo hi > out.txt",
            r"echo hi>out.txt",
            "git status",
            "git push",
            "curl -o out.txt https://example.com",
            "cargo run --bin model.txt",
            "del scratch.txt",
            "rd /s /q build",
            r"echo hi > out.txt 2> out.err",
            r"echo x > D:\repo\out.txt",
            r"type D:\repo\src\main.rs",
            // 大小写变体只差 `repo` 组件：盘符大小写与根一致，
            // 敏感平台上才轮得到 `repo` 组件参与比较。
            if Platform::current().has_case_insensitive_paths() {
                r"cargo build --manifest-path=d:/REPO/Cargo.toml"
            } else {
                r"cargo build --manifest-path=D:/repo/Cargo.toml"
            },
            r#"type "D:\repo\sub dir\main.rs""#,
            r"echo x > D:\repo\out.txt && cargo test",
            // 引号里的路径文字是普通参数，不是交给 shell 再解释的命令。
            r#"git commit -m "fix C:\Users\x\note.txt""#,
            // 变量展开/命令替换不覆盖，但也不能凭空变成误拒。
            r"echo x > $OUT",
            r"echo x > %OUT%",
            // 已知词法缺口：单级 POSIX 绝对路径与 /s /q 开关形状相同，
            // 为避免误杀 Windows 开关而放行；README 记录了这个边界。
            "cat /etc",
            "type /tmp",
            // `--flag=` 只看第一个等号后的值；第二个等号后的片段不在判据内。
            r"git diff --output=a=D:\outside\x",
        ];
        for command in clean {
            assert_eq!(
                write_escape_beyond(command, root),
                None,
                "`{command}` 不应被硬拒绝"
            );
        }
    }

    /// 多段命令的理由包含段号与程序，写动词归属按段判断。
    /// 单段命令沿用旧文案模板，不带段号（审批界面与拒绝理由依赖它）。
    #[test]
    fn reasons_name_the_offending_segment_but_single_segment_text_stays_verbatim() {
        let root = Path::new(r"D:/repo");
        let first = write_escape_suspect(r"rm D:/outside/x && cargo test").expect("第 1 段越界");
        assert!(first.contains("第 1 段"), "{first}");
        assert!(first.contains("程序 `rm`"), "{first}");
        assert!(first.contains("`rm`"), "动词归属应指向 rm: {first}");

        let second =
            write_escape_beyond(r"cargo test && rm D:/outside/x", root).expect("第 2 段越界");
        assert!(second.contains("第 2 段"), "{second}");
        assert!(second.contains("程序 `rm`"), "{second}");
        assert!(second.contains("`rm` 操作 workspace 外"), "{second}");

        let single = write_escape_suspect(r"echo hi > C:\Windows\temp\x").expect("单段越界");
        assert_eq!(
            single,
            r"命令把输出重定向到 workspace 外：`C:\Windows\temp\x` 指向 workspace 外的绝对路径"
        );
        assert!(
            !single.contains("已覆盖全部"),
            "理由不得暗示覆盖所有绕过手段: {single}"
        );
    }

    /// 两道防线使用同一份解析结果：段号与越界目标一致，差别只在无根
    /// 一律视为嫌疑，有根时根内放行。
    #[test]
    fn both_defenses_share_one_parse_and_keep_the_documented_gap() {
        let parsed = parse_command(r"cargo test && type D:\repo\src\main.rs");
        let root = Path::new(r"D:/repo");

        let suspect = write_escape_suspect(r"cargo test && type D:\repo\src\main.rs")
            .expect("无根：根内绝对路径也要多问一次");
        let beyond = write_escape_beyond(r"cargo test && type D:\repo\src\main.rs", root);
        assert_eq!(beyond, None, "有根：同样的路径应放行");
        assert!(suspect.contains("第 2 段"), "{suspect}");
        assert_eq!(parsed.segments[1].program(), Some("type"));
        assert_eq!(
            parsed.segments[1].operator_before,
            Some(command::ShellOperator::And)
        );
    }

    /// 返回的原因可直接用作审批理由，用户和模型都能理解。
    #[test]
    fn write_escape_suspect_explains_what_it_found() {
        let reason = write_escape_suspect(r"echo hi > C:\Windows\temp\x").expect("应命中");

        assert!(
            reason.contains(r"C:\Windows\temp\x"),
            "原因应点名目标: {reason}"
        );
    }
}

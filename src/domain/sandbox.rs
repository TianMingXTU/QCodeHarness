//! 进程沙箱策略、执行边界元数据与命令的结构化词法边界裁决。
//!
//! 领域层只计算纯数据；资源与生命周期约束由 infrastructure/sandbox
//! 强制执行。Windows Job Object 不提供文件系统或网络隔离。

use std::path::Path;
use std::time::Duration;

use super::command::{CommandSegment, ParsedCommand, RedirectOp, parse_command};

/// 当前编译目标具备的隔离能力，不代表某次命令已经成功启用沙箱。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlatformCapabilities {
    pub has_process_sandbox: bool,
    pub has_filesystem_isolation: bool,
    pub has_network_isolation: bool,
}

impl PlatformCapabilities {
    pub fn detect() -> Self {
        Self {
            has_process_sandbox: cfg!(windows),
            has_filesystem_isolation: false,
            has_network_isolation: false,
        }
    }

    pub fn label(&self) -> &'static str {
        if self.has_process_sandbox {
            "Windows JobObject"
        } else {
            "Unsupported"
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

    /// MCP 调用的边界事实。`process_isolation` 报告 Server 进程是否在握手
    /// **之前**就被纳入了宿主进程沙箱（Windows 上是 Job Object）：纳入只
    /// 覆盖该进程之后创建的子孙，所以先后顺序决定这个字段是否为真保护。
    /// 纳入失败、拿不到 PID 或非 Windows 平台为 false。口径与 shell 工具
    /// 一致：不谎报保护，也不因为"是外部进程"就一律写 false——治理后能
    /// 反映真实边界状态。
    ///
    /// `platform` 仍是 `MCP subprocess` 而非平台标签：证据要能分辨"这来自
    /// 外部 Server"与"这是内置工具"，而 `active` 单独回答"有没有进程隔离"。
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

/// 判断命令是否可能把写操作伸到 workspace 外；命中后用于升级审批。
///
/// 这是**词法**边界判定，不是隔离，也不是 shell 求值：变量拼接、命令替换、
/// 编码绕过与链接物理目标仍可绕过；真正的进程边界由基础设施层建立。
/// 无根（P8-3）与有根（P11）共用同一份解析结果，唯一差别是 `root`：
/// 无根时所有绝对路径都算嫌疑，有根时只拒真的不在根下的绝对路径。
pub fn write_escape_suspect(command: &str) -> Option<String> {
    command_escape_violation(&parse_command(command), None)
}

pub fn write_escape_beyond(command: &str, root: &Path) -> Option<String> {
    command_escape_violation(&parse_command(command), Some(root))
}

/// 两道防线唯一的裁决器：逐段、逐重定向、逐词元查越界，任一段命中即拒。
///
/// `root` 同时表达 P8/P11 有意保留的口径落差：P8 没有 workspace 根，
/// 所有盘符绝对路径都升级审批（根内也多问一次）；P11 知道根在哪，
/// 只硬拒绝真的越界。把 `write_escape_suspect` 改成根感知会动 P8 语义
/// 与四条验收测试，本切片不消除这层落差，只消除重复切词。
fn command_escape_violation(parsed: &ParsedCommand, root: Option<&Path>) -> Option<String> {
    let multi_segment = parsed.segments.len() > 1;
    for (index, segment) in parsed.segments.iter().enumerate() {
        for redirect in &segment.redirects {
            // `<` 是输入，不是"把输出写到外面"；其余形态一视同仁。
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
        // 重定向目标也留在词元流里：P9 起"任何词元提到越界路径即触发"
        // 是收紧后的口径，不能因为有了 redirect 索引就退回只看重定向。
        for word in &segment.words {
            let candidate = word.path_candidate();
            if let Some(reason) = path_escape_reason(candidate, root) {
                // 理由里点名完整词元（旧格式对 --flag= 也是整个词元），
                // 判据则只看等号后的值——两者不能混为一谈。
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

/// 多段命令的理由要点名是哪一段、哪个程序；单段沿用三种既有文案模板，
/// 不追加段号。重定向目标由结构化模型直接收齐，带空格的目标也按
/// "输出重定向"归类；`<` 是输入，不套输出重定向的理由。
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

/// 写入型动词只用于生成可读理由；P9 起任何越界路径都会触发，不以动词为门槛。
/// 归属按**段**判定：`cargo test && rm D:/outside/x` 的理由应指向 `rm`，
/// 而不是从整条命令里挑一个恰好出现过的动词。
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

/// 纯词法规范化：统一分隔符、剥设备/UNC 装饰、抵消 `.` 与 `..`，
/// 并按目标平台决定是否忽略大小写。绝不做文件系统解析。
fn normalize_path(raw: &str) -> Vec<String> {
    let trimmed = trim_device_prefix(raw);
    let mut parts: Vec<String> = Vec::new();
    for part in trimmed.split(['\\', '/']) {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            name => parts.push(if cfg!(windows) {
                name.to_ascii_lowercase()
            } else {
                name.to_owned()
            }),
        }
    }
    parts
}

/// `\\?\C:\x`、`//?/UNC/server/share/x` 等前缀只影响 Windows 的路径解释，
/// 不改变它指向的卷或共享；UNC 的 server/share 也一并作为普通组件比较。
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

    // ── P8-3 越界写入嫌疑 ──────────────────────────────────────────────

    /// 真值表：看得见越界目标的写入要被抓出来，workspace 内的操作不打扰。
    ///
    /// 每一条对应一种模型真实的越界写法，不是为凑覆盖编的字符串。
    #[test]
    fn write_escape_suspect_detects_out_of_workspace_writes_only() {
        let suspects = [
            // 重定向到别的盘：没有写入型动词，光一个 `>` 就够了。
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
            // 不带写入型动词、也不带 `>` 的越界：P9-5 的默认基线放行了
            // `git diff *` 一族，`--output` 是 git 自己的写文件选项，
            // 只按动词和重定向判就整个漏掉——而它确实把 workspace 的
            // 内容写到了外面。本机实测 `git diff --output=D:\x` 会落盘。
            r"git diff --output=C:\Windows\temp\x",
            // 开关与值之间不写空格是常见写法，值跟在 `=` 后面。
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
            // 重定向目标就在 workspace 内，即使命令本身看着像在写。
            r"echo hi>out.txt",
            "git status",
            // 网络类操作不归这个启发式管（见模块文档的边界说明）。
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
        let allowed = [
            r"type D:\repo\src\main.rs",
            r"cargo build --manifest-path=d:/REPO/Cargo.toml",
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

    /// 词法裁决的结构化回归表：旧真值表原样保留（不放松、不误杀），
    /// 新覆盖只增不减。每条都给出"为什么这一条必须这样判"。
    #[test]
    fn structured_adjudication_keeps_the_no_loosening_contract() {
        let root = Path::new(r"D:/repo");

        // 收紧后的新增形态：段、多重定向、--flag= 归属、引号内复合命令。
        let tightened = [
            // 越界出现在第 2 段：整串扫词元也碰得到，但理由必须归到 rm 那一段。
            r"cargo test && rm D:/outside/x",
            // 越界出现在第 1 段：动词归属不能反过来指向后面的 cargo test。
            r"rm D:/outside/x && cargo test",
            // 第二个重定向目标是越界：旧实现只扫到第一个就收工。
            r"echo x > local.txt 2> /tmp/out",
            // 分号后的 --flag= 逐段生效，等号后只看值。
            r"git diff --output=in_root ; git diff --output=D:/outside/x",
            // cmd /C "…" 的引号内复合命令，补查一层词元后必须看见重定向目标。
            r#"cmd /C "echo x > D:/outside/x""#,
            // 管道/换行/|| 的任意一段越界都拒。
            r"cargo test | copy src \\server\share\x",
            "cargo test\nrm /tmp/out",
            r"cargo test || del C:\Windows\temp\x",
            // 用户点名的既有拒绝形态，重构成结构模型后一个都不许丢。
            r"cat /etc/passwd",
            r"echo x > /tmp/out",
            r"type D:\outside\x",
            r"copy src \\server\share\x",
            r"del ..\foo",
            r"git diff --output=D:\x",
            // Unicode 空白也必须是词元边界：旧 tokenize 用 is_whitespace，
            // 新词法若只认 ASCII 空格会把整条命令粘成一个不命中的词元。
            "type\u{a0}..\\outside",
            "echo\u{a0}/etc/passwd",
            // 重定向目标自己带 `=`：目标索引不看等号，但词元流仍按
            // path_candidate() 取第一个等号后的值，不能只靠 redirect 索引。
            r"echo x > X=D:/outside",
            r"echo x > --output=..\outside",
            // `<` 是输入重定向：不套"输出重定向"理由，但目标词元照常扫描。
            r"cat < /tmp/out",
            r"cat < D:\outside\x",
            // 卷根、盘符相对、设备前缀 UNC：三种形状一个都不许漏。
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
            // `&` 只有在 `>&数字` 形态才是 fd 复制；粘住真实越界目标就是放松。
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

        // 根内路径与无害命令照旧放行：不能把收紧做成"禁用 run_command"。
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
            r"cargo build --manifest-path=d:/REPO/Cargo.toml",
            r#"type "D:\repo\sub dir\main.rs""#,
            r"echo x > D:\repo\out.txt && cargo test",
            // 引号里的路径文字是普通参数，不是交给 shell 再解释的命令。
            r#"git commit -m "fix C:\Users\x\note.txt""#,
            // 变量展开/命令替换不覆盖，但也不能凭空变成误拒。
            r"echo x > $OUT",
            r"echo x > %OUT%",
            // 已知词法缺口：单级 POSIX 绝对路径与 /s /q 开关用同一个形状，
            // 为避免误杀 Windows 开关而放行；README 明确记录这个边界。
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

    /// 多段理由点名段号与程序，写动词归属按段而不是按整串；
    /// 单段沿用旧文案模板，不带段号（审批界面与拒绝理由都钉住了它）。
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

    /// 两道防线跑在同一份解析结果上：同一条命令的段号与越界目标一致，
    /// 差别只在 P8 的"无根一律嫌疑"与 P11 的"根内放行"。
    #[test]
    fn both_defenses_share_one_parse_and_keep_the_documented_gap() {
        let parsed = parse_command(r"cargo test && type D:\repo\src\main.rs");
        let root = Path::new(r"D:/repo");

        let suspect = write_escape_suspect(r"cargo test && type D:\repo\src\main.rs")
            .expect("P8 无根：根内绝对路径也要多问一次");
        let beyond = write_escape_beyond(r"cargo test && type D:\repo\src\main.rs", root);
        assert_eq!(beyond, None, "P11 知道根在哪，同样的路径应放行");
        assert!(suspect.contains("第 2 段"), "{suspect}");
        assert_eq!(parsed.segments[1].program(), Some("type"));
        assert_eq!(
            parsed.segments[1].operator_before,
            Some(command::ShellOperator::And)
        );
    }

    /// 返回的原因要能直接进审批理由：用户和模型都得看懂它在说什么。
    #[test]
    fn write_escape_suspect_explains_what_it_found() {
        let reason = write_escape_suspect(r"echo hi > C:\Windows\temp\x").expect("应命中");

        assert!(
            reason.contains(r"C:\Windows\temp\x"),
            "原因应点名目标: {reason}"
        );
    }
}

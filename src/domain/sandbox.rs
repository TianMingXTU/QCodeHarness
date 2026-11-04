//! 进程沙箱策略、执行边界元数据与命令的字面路径检查。
//!
//! 领域层只计算纯数据；资源与生命周期约束由 infrastructure/sandbox
//! 强制执行。Windows Job Object 不提供文件系统或网络隔离。

use std::path::Path;
use std::time::Duration;

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

    pub fn mcp() -> Self {
        Self {
            platform: MCP_BOUNDARY_LABEL,
            active: false,
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
/// 这是字面路径启发式，不是隔离。变量拼接、编码和链接仍可绕过；真正的
/// 进程边界由基础设施层建立。
pub fn write_escape_suspect(command: &str) -> Option<String> {
    escape_reason(command, None)
}

pub fn write_escape_beyond(command: &str, root: &Path) -> Option<String> {
    escape_reason(command, Some(root))
}

/// 两个入口共用切词与字面路径判定；无根时按 P8 的保守口径处理。
fn escape_reason(command: &str, root: Option<&Path>) -> Option<String> {
    for target in redirect_targets(command) {
        if let Some(reason) = path_escape_reason(&target, root) {
            return Some(format!("命令把输出重定向到 workspace 外：{reason}"));
        }
    }
    let target = tokenize(command).find_map(|token| {
        let candidate = token
            .split_once('=')
            .map(|(_, value)| value)
            .unwrap_or(&token);
        path_escape_reason(candidate, root).map(|reason| (token, reason))
    });
    target.map(|(target, reason)| match write_verb(command) {
        Some(verb) => format!("命令用 `{verb}` 操作 workspace 外的 `{target}`：{reason}"),
        None => format!("命令提到 workspace 外的 `{target}`：{reason}"),
    })
}

/// 命令里所有重定向目标；`echo x > a 2> /outside` 不能只看第一个。
fn redirect_targets(command: &str) -> impl Iterator<Item = String> {
    let mut tokens = tokenize(command).peekable();
    std::iter::from_fn(move || {
        while let Some(token) = tokens.next() {
            if is_redirect(&token) {
                let target = tokens.next()?;
                return Some(unquote(&target).to_owned());
            }
        }
        None
    })
}

/// 词法切分：引号内的整段路径保留，shell 链接符单独成词。
fn tokenize(command: &str) -> impl Iterator<Item = String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut chars = command.chars().peekable();

    while let Some(ch) = chars.next() {
        if let Some(q) = quote {
            if ch == q {
                quote = None;
            } else {
                current.push(ch);
            }
            continue;
        }
        match ch {
            '"' | '\'' => quote = Some(ch),
            c if c.is_whitespace() || matches!(c, '|' | '&' | ';' | '(' | ')') => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
                tokens.push(c.to_string());
            }
            '>' | '<' => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
                let mut op = ch.to_string();
                if ch == '>' && chars.peek() == Some(&'>') {
                    op.push(chars.next().unwrap());
                }
                tokens.push(op);
            }
            _ => current.push(ch),
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens.into_iter()
}

/// 写入型动词用于生成可读理由；P9 起任何越界路径都会触发，不再以动词为门槛。
fn write_verb(command: &str) -> Option<&'static str> {
    const WRITE_VERBS: [&str; 11] = [
        "del", "rd", "rmdir", "move", "ren", "copy", "remove-item",
        "rm", "mv", "cp", "mkdir",
    ];
    WRITE_VERBS
        .into_iter()
        .find(|verb| {
            tokenize(command).any(|token| token.eq_ignore_ascii_case(verb))
        })
}

fn is_redirect(token: &str) -> bool {
    matches!(token, ">" | ">>" | "1>" | "2>" | "&>")
}

fn unquote(token: &str) -> &str {
    token.trim_matches(['"', '\''])
}

/// 判断一个字面路径词元是否越界。不模拟变量或命令替换。
fn path_escape_reason(token: &str, root: Option<&Path>) -> Option<String> {
    let token = unquote(token);
    if token.is_empty()
        || token.starts_with('-')
        || matches!(token, "|" | "||" | "&" | "&&" | ";" | "(" | ")" | ">" | ">>" | "<")
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
    let drive_relative = bytes.len() >= 2
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':';
    let unc = token.starts_with("//") || token.starts_with(r"\\")
        || token.starts_with("/\\") || token.starts_with("\\/");
    // 裸 `/` 判据区分开关与带目录分隔符的路径：/s 放行，/tmp/out 收紧。
    let posix_absolute = token.starts_with('/') && token[1..].contains(['/', '\\']);
    unc || drive_absolute || drive_relative || posix_absolute
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
        assert_eq!(write_escape_beyond("cat /home/user/repo/src/main.rs", root), None);
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

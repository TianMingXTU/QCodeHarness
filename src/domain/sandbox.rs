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

/// 判断一条 shell 命令是否"疑似把写操作伸到 workspace 外"，命中则给出原因。
///
/// 存在的理由是策略层的一个真实缺口：策略按工具名和参数文本放行
/// `run_command` 之后，命令内容就不受任何约束了——模型可以敲
/// `echo x > C:\Windows\temp\y`，而策略看到的是"run_command，已放行"。
/// 于是这里补一道**升级**检查：策略说 Allow 但命令看着像越界写入时，
/// 应用层把它改判为 RequireApproval，让人再看一眼。
///
/// 这是启发式，不是隔离。它抓的是"字面上就写着越界目标"的命令，
/// 抓不住变量拼接（`set P=C:\Windows & echo x > %P%\y`）或编码绕过。
/// 真正的边界在 P7 的进程沙箱与后续的 OS 级隔离；这里的价值是把
/// 最常见的直白写法从"无声放行"变成"有人过目"。
///
/// **任何指向 workspace 外的参数都算嫌疑，不限写入型动词。** P9-5 的
/// 默认基线放行 `cargo test *` / `git diff *` 一族之后，这条检查从
/// "补漏"升格为"主防线"：`git diff --output=C:\x` 既没有写入型动词
/// 也没有 `>`，只按动词判就整个漏掉，而它确实把 workspace 的内容写到
/// 了外面（`--output` 是 git 自己的写文件选项）。动词门槛原本是为
/// `rd /s /q build` 这类带开关的命令降噪，而 [`escapes_workspace`]
/// 已经不把 `/q` 当路径——降噪交给它就够了，门槛留着只会漏。
///
/// 网络类操作（`git push`、`curl`）不在范围内：外传数据的形态太多，
/// 硬列只会制造噪音，让用户对真正的越界写入提示脱敏。
pub fn write_escape_suspect(command: &str) -> Option<String> {
    // 重定向目标越界时不需要动词：`echo hi > C:\x` 一个字型动词都没有，
    // 但它确实在 workspace 外落了盘。
    if let Some(target) = redirect_target(&command) {
        if escapes_workspace(target) {
            return Some(format!("命令把输出重定向到 workspace 外的 `{target}`"));
        }
    }

    if let Some(target) = escaping_argument(&command) {
        // 有写入型动词时理由说清"它在动什么"，没有时只说"提到了外面"——
        // 后者同样是升级的充分理由，见下面的说明。
        return Some(match write_verb(&command) {
            Some(verb) => format!("命令用 `{verb}` 操作 workspace 外的 `{target}`"),
            None => format!("命令提到 workspace 外的 `{target}`"),
        });
    }

    None
}
/// 写入型命令的动词。命中其中任意一个，后面的参数才按"写目标"审视。
///
/// 只收命令名本身，不收开关：`/s`、`-Recurse` 只是修饰，判断越界看的是
/// 目标路径。
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

/// 命令里第一个作为独立词出现的写入型动词。
fn write_verb(command: &str) -> Option<&'static str> {
    WRITE_VERBS
        .into_iter()
        .find(|verb| has_word(command, verb))
}

/// 命令里是否出现了作为独立词的 `verb`。
///
/// 按空白与常见 shell 分隔符切词：不切词的话 `del` 会命中 `model`，
/// 把一堆正常命令送去审批，用户很快就会养成"一律放行"的习惯。
fn has_word(command: &str, verb: &str) -> bool {
    // 大小写不敏感：cmdlet 与 Unix 命令都可能被写成任意大小写，
    // 漏一种写法就是一个绕过。
    split_tokens(command).any(|token| token.eq_ignore_ascii_case(verb))
}

/// `>` 或 `>>` 之后的重定向目标。有多个重定向时取第一个。
fn redirect_target(command: &str) -> Option<&str> {
    let index = command.find('>')?;
    let rest = command[index..].trim_start_matches('>');
    split_tokens(rest).next()
}

/// 第一个越出 workspace 的参数。
///
/// `--flag=value` 形态要连值一起看：`--output=C:\x` 整个词不是绝对路径
/// （开头是 `-`），只比整个词会把最常用的开关写法整个漏掉。
fn escaping_argument(command: &str) -> Option<&str> {
    split_tokens(command).find(|token| points_outside(token))
}

/// 这个词本身、或它 `=` 后面的值，是否指向 workspace 之外。
fn points_outside(token: &str) -> bool {
    if escapes_workspace(token) {
        return true;
    }
    match token.split_once('=') {
        Some((_, value)) => escapes_workspace(value),
        None => false,
    }
}

/// 按空白与常见分隔符切词。
///
/// 分隔符里故意不含 `-`：`remove-item` 是一个词。路径里的 `\` 和 `/`
/// 必须留在词内，否则 `..\foo` 会被切成两半而漏判。
///
/// 过滤空片段：`a > b` 里 `>` 两侧都是分隔符，不过滤的话第一个"词"
/// 会是空字符串，重定向目标就取错了。
fn split_tokens(command: &str) -> impl Iterator<Item = &str> {
    command
        .split(|c: char| {
            c.is_whitespace() || matches!(c, '|' | '&' | ';' | '(' | ')' | '<' | '>' | '"' | '\'')
        })
        .filter(|token| !token.is_empty())
}

/// 这个参数是否指向 workspace 之外。
///
/// 两种形态：盘符绝对路径（`C:\x`、`D:/x`，含 UNC `\\server\share`），
/// 以及 `..` 作为完整路径成分出现（`..\foo`、`a/../b`）。
///
/// 故意不把裸 `/` 开头的参数算作绝对路径：`rd /s /q build` 里的 `/s`
/// 是开关不是根目录，误报会让整条规则失去可信度。POSIX 下的穿越由
/// `..` 那一支覆盖。
fn escapes_workspace(token: &str) -> bool {
    if is_absolute_windows_path(token) || token.starts_with(r"\\") {
        return true;
    }
    token
        .split(['\\', '/'])
        .any(|component| component == "..")
}

/// 是否是 `X:\` 或 `X:/` 形态的盘符绝对路径。
///
/// 要求第三个字符是分隔符：单独的 `C:` 是"切换到 C 盘"，不是写目标。
fn is_absolute_windows_path(token: &str) -> bool {
    let bytes = token.as_bytes();
    bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'\\' || bytes[2] == b'/')
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

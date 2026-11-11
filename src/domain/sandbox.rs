//! 进程沙箱策略、执行边界元数据与命令的结构化词法边界裁决。
//!
//! 领域层只计算纯数据；资源与生命周期约束由 infrastructure/sandbox
//! 强制执行。目前只有 Windows 的 Job Object 提供进程沙箱，且它不提供
//! 文件系统或网络隔离；其他平台没有进程沙箱。平台能力见
//! [`PlatformCapabilities::detect`]。
//!
//! 无完整隔离能力时（`unisolation_statement` 为 Some），开放命令由
//! [`unisolated_decision`] 按基线名单裁决：只放行策略基线覆盖的只读
//! 与测试命令形状，其余硬拒绝，除非宿主显式设置
//! `QCH_ALLOW_UNISOLATED_COMMANDS=1`。

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

/// 宿主显式放行开放命令的环境变量。
///
/// 只放宽 [`UnisolatedDecision::Refused`] 的裁决：审批闸门、词法硬拒绝、
/// 越界写入检查与角色白名单都不受它影响。设置时未生效声明仍必须带
/// 上（`unisolation_statement`），让模型与证据看到「执行但无完整隔离」。
pub const ALLOW_UNISOLATED_COMMANDS_ENV: &str = "QCH_ALLOW_UNISOLATED_COMMANDS";

/// 稳定拒绝码。执行层用它向模型报告「该命令被策略拒绝」，程序侧按码
/// 分支，不靠解析消息文本。
pub const REFUSAL_CODE_UNISOLATED: &str = "sandbox_unisolated_command";

/// 无完整隔离能力（`unisolation_statement` 为 Some）时对一条开放命令的裁决。
///
/// 与 [`UnisolatedCommand`] 的区别：这里已经读入环境变量，执行层拿到的是
/// 可以直接落地的结论，不用再查一遍宿主配置。
/// 不派生 `Copy`：`Refused` 携带 `String` 理由。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnisolatedDecision {
    /// 允许执行。结论必须随 `unisolation_statement` 一起呈现：模型可见
    /// 文本与边界证据都要声明进程沙箱、文件隔离、网络隔离未生效。
    Allowed,
    /// 拒绝执行。`reason` 含未生效声明与可执行建议；`stable_code` 固定为
    /// [`REFUSAL_CODE_UNISOLATED`]，执行层原样带回给模型与调用方。
    Refused { reason: String, stable_code: &'static str },
}

/// 缺少文件与网络隔离时，判断一条开放命令是否允许执行。
///
/// 这是纯判定：`host_allows` 由调用方传入环境变量的当前值，函数不读
/// 环境，因此可以被任意 `PlatformCapabilities` 构造组合做单元测试，
/// 不需要真实的 Linux 或 macOS 机器。
///
/// 口径：只放行策略基线覆盖的只读与测试命令形状（[`UnisolatedCommand`]
/// 的文档列了完整清单），其余一律拒绝。`host_allows` 为 true 时拒绝
/// 全部让位给放行，但**不允许**借此省略未生效声明：`unisolation_statement`
/// 是执行层必须附带的固定文本，与本函数的裁决无关。
///
/// 不改变既有防线：词法硬拒绝（越界写入）、审批闸门、角色白名单在调用
/// 本函数之前各自生效，本函数只在它们全部放行之后做最后一道开放命令收口。
pub fn unisolated_decision(
    capabilities: &PlatformCapabilities,
    command: &ParsedCommand,
    host_allows: bool,
) -> UnisolatedDecision {
    let Some(statement) = capabilities.unisolation_statement() else {
        return UnisolatedDecision::Allowed;
    };
    match UnisolatedCommand::classify(command) {
        UnisolatedCommand::Baseline => UnisolatedDecision::Allowed,
        UnisolatedCommand::Open => {
            if host_allows {
                UnisolatedDecision::Allowed
            } else {
                UnisolatedDecision::Refused {
                    reason: unisolated_refusal_reason(statement),
                    stable_code: REFUSAL_CODE_UNISOLATED,
                }
            }
        }
    }
}

/// 拒绝理由。把「为什么拒」与「怎么办」写给模型，不依赖它再猜一次。
/// 开头固定是稳定码，程序侧可按码前缀分支。
fn unisolated_refusal_reason(statement: &str) -> String {
    format!(
        "{REFUSAL_CODE_UNISOLATED}: {statement} 开放命令在缺少文件系统与网络隔离的\
         宿主机上默认拒绝。基线允许：cargo test/check/build、go test/build/vet、\
         gofmt -l、python -m unittest、node --test、git status/diff（只读形式），\
         以及这些命令后面接 `echo` 回显退出码或 `head`/`tail`/`wc` 一类只读输出\
         过滤器。如需执行开放命令，请宿主设置 `{ALLOW_UNISOLATED_COMMANDS_ENV}=1`；\
         该开关不关闭审批、词法硬拒绝、越界写入检查或角色白名单，\
         执行时未生效声明照常附带。"
    )
}

impl PlatformCapabilities {
    /// 文件隔离或网络隔离任一缺失时，返回写进模型可见文本与证据的未生效
    /// 声明；两项都实现时返回 `None`。
    ///
    /// 声明只覆盖文件与网络两项，不虚报已生效的隔离。进程沙箱缺失的声明
    /// 由 `SANDBOX_DOWNGRADE_NOTICE` 单独承担，两个声明通道互不覆盖：
    /// 无进程沙箱的平台降级执行时两条都会出现，模型一次看全。
    pub fn unisolation_statement(&self) -> Option<&'static str> {
        if self.has_filesystem_isolation && self.has_network_isolation {
            return None;
        }
        Some(
            "⚠️ 文件系统隔离与网络隔离未生效：命令可以读写 workspace 外的文件，\
             且可以访问网络。请格外保守。",
        )
    }
}

/// 开放命令形状判定。
///
/// `Baseline` 的清单与策略基线对齐：只读 git、各语言的标准测试与构建
/// 命令、只读的格式化检查，以及这些命令后面接输出观察器的组合。清单外
/// 的任何程序都归为 `Open`：宁可让宿主多设一次环境变量，也不能在无法
/// 证明命令无害时放行。
/// 判定用 [`ParsedCommand`] 的结构化模型，不用原始文本：链接符在词法层
/// 面就把命令分段了，逐段检查比匹配整串文本可靠，也与策略基线不吞链接
/// 符的口径一致。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnisolatedCommand {
    /// 策略基线覆盖的只读与测试命令形状，允许不经宿主放行直接执行。
    ///
    /// 完整清单（重定向目标限 workspace 内）：
    /// - `cargo test *`、`cargo check *`、`cargo build *`（含 `--offline`）
    /// - `go test *`、`go build *`、`go vet *`
    /// - `gofmt -l *`（列出未格式化文件；带 `-w` 的形态不在内）
    /// - `python -m unittest *`、`python -I -m unittest *`
    /// - `node --test *`
    /// - `git status`、`git diff`（只读形式；带 `--output` 的形态不在内）
    /// - 以上命令用 `;`、`&&`、`||`、管道或换行串起来，且每一段都在清单内
    /// - 上述命令后面接 `echo` 回显，或接 `head`/`tail`/`more`/`wc`/`uniq`
    ///   截取输出（过滤器不带文件操作数）
    Baseline,
    /// 开放命令：可能写 workspace 外、可能联网、可能拉起任意程序。
    /// 在文件与网络隔离缺失时必须被拒绝，除非宿主显式放行。
    Open,
}

impl UnisolatedCommand {
    /// 按结构化命令模型分类。
    ///
    /// 整条命令逐段裁决：首段必须是基线命令，其余每段要么也是基线命令，
    /// 要么是只把上游输出转发到 stdout 的输出观察器（见
    /// `observer_baseline`）。`;`、`&&`、`||`、管道、换行与后台 `&` 都走
    /// 同一规则，不再一律拒绝：真实运行里 `go test ./... 2>&1; echo
    /// "EXIT=$?"`、`node --test 2>&1 | tail -30` 是最普通的验证方式，整条
    /// 拒绝会把正常编码任务挡在门外。任何一段落在两类之外仍然是 `Open`。
    pub fn classify(command: &ParsedCommand) -> Self {
        let mut segments = command.segments.iter();
        let Some(first) = segments.next() else {
            return UnisolatedCommand::Open;
        };
        if segment_baseline(first) != UnisolatedCommand::Baseline {
            return UnisolatedCommand::Open;
        }
        let rest_is_baseline = segments.all(|segment| {
            segment_baseline(segment) == UnisolatedCommand::Baseline
                || observer_baseline(segment)
        });
        if rest_is_baseline {
            UnisolatedCommand::Baseline
        } else {
            UnisolatedCommand::Open
        }
    }
}

/// 单段基线判定：程序名与子命令都在清单内才算基线。
fn segment_baseline(segment: &CommandSegment) -> UnisolatedCommand {
    match segment.program() {
        Some("cargo") => cargo_baseline(segment),
        Some("go") => go_baseline(segment),
        Some("gofmt") => gofmt_list_baseline(segment),
        Some("python") => python_unittest_baseline(segment),
        Some("node") => node_test_baseline(segment),
        Some("git") => git_readonly_baseline(segment),
        _ => UnisolatedCommand::Open,
    }
}

/// 输出观察器：只把上游输出或固定文本写到 stdout 的段。
///
/// `echo` 用于回显退出码与阶段标记；`head`/`tail`/`more`/`wc`/`uniq`
/// 用于截断或统计上游输出。过滤器参数只允许短选项与纯数字（`-30`、
/// `-n 40`、`-l`），带文件操作数时判为开放命令：那会把观察器变成读文件
/// 的入口，超出「观察上游输出」的范围。`sort` 有 `-o` 写文件选项，不列
/// 入清单。
fn observer_baseline(segment: &CommandSegment) -> bool {
    let Some(program) = segment.program() else {
        return false;
    };
    match program {
        "echo" => true,
        "head" | "tail" | "more" | "wc" | "uniq" => {
            let redirect_targets: Vec<&str> = segment
                .redirects
                .iter()
                .map(|redirect| redirect.target.as_str())
                .collect();
            segment.arguments().all(|word| {
                // `arguments()` 含重定向目标（如 `2>&1` 的 `&1`），它们不
                // 是文件操作数，按目标表放行。
                is_filter_option(&word.text) || redirect_targets.contains(&word.text.as_str())
            })
        }
        _ => false,
    }
}

/// 过滤器参数形状：`-n`、`-30`、`40` 这类选项与数字。其余词元按文件
/// 操作数处理，宁可判开放也不放行读文件。
fn is_filter_option(text: &str) -> bool {
    match text.strip_prefix('-') {
        Some(flag) => {
            !flag.is_empty()
                && flag
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '=' | '_'))
        }
        None => !text.is_empty() && text.chars().all(|ch| ch.is_ascii_digit()),
    }
}

/// `cargo test *`、`cargo check *`、`cargo build *`。
///
/// 子命令名必须精确匹配这三个词；`cargo run`、`cargo publish` 等是开放
/// 命令，不在基线内。带 `--manifest-path=` 指向 workspace 外的路径由
/// 越界写入检查单独拦截，本函数只判命令形状。
fn cargo_baseline(segment: &CommandSegment) -> UnisolatedCommand {
    let second = segment
        .arguments()
        .next()
        .map(|word| word.text.as_str())
        .unwrap_or("");
    if matches!(second, "test" | "check" | "build") {
        UnisolatedCommand::Baseline
    } else {
        UnisolatedCommand::Open
    }
}

/// `go test *`、`go build *`、`go vet *`。
///
/// 与 cargo 的 test/check/build 对称：本地编译、测试与静态分析都不需要
/// 网络，产物落在 workspace 与工具链缓存内。`go run`、`go get` 等仍按
/// 开放命令处理。
fn go_baseline(segment: &CommandSegment) -> UnisolatedCommand {
    let second = segment
        .arguments()
        .next()
        .map(|word| word.text.as_str())
        .unwrap_or("");
    if matches!(second, "test" | "build" | "vet") {
        UnisolatedCommand::Baseline
    } else {
        UnisolatedCommand::Open
    }
}

/// `gofmt -l *`：只列出未格式化的文件。
///
/// 带 `-w` 会改写文件，是开放命令；不带 `-l` 时 gofmt 从 stdin 读或直接
/// 改写，形状不可控，同样不在基线内。
fn gofmt_list_baseline(segment: &CommandSegment) -> UnisolatedCommand {
    let mut lists = false;
    for word in segment.arguments() {
        match word.text.as_str() {
            "-l" => lists = true,
            "-w" => return UnisolatedCommand::Open,
            _ => {}
        }
    }
    if lists {
        UnisolatedCommand::Baseline
    } else {
        UnisolatedCommand::Open
    }
}

/// `python -m unittest *` 与 `python -I -m unittest *`。
///
/// 参数顺序是固定的：`-I`（隔离解释器模式）在前，`-m` 在后，unittest
/// 是模块名。其他子命令（`-c`、`-u`、`pip` 等）都是开放命令。
fn python_unittest_baseline(segment: &CommandSegment) -> UnisolatedCommand {
    let words: Vec<&str> = segment.arguments().map(|word| word.text.as_str()).collect();
    // 两种基线形状：`-m unittest …` 与 `-I -m unittest …`。
    let is_unittest = matches!(
        (words.first().copied(), words.get(1).copied(), words.get(2).copied()),
        (Some("-m"), Some("unittest"), _) | (Some("-I"), Some("-m"), Some("unittest"))
    );
    if is_unittest {
        UnisolatedCommand::Baseline
    } else {
        UnisolatedCommand::Open
    }
}

/// `node --test *`。
fn node_test_baseline(segment: &CommandSegment) -> UnisolatedCommand {
    let second = segment
        .arguments()
        .next()
        .map(|word| word.text.as_str())
        .unwrap_or("");
    if second == "--test" {
        UnisolatedCommand::Baseline
    } else {
        UnisolatedCommand::Open
    }
}

/// `git status` 与 `git diff`（只读形式）。
///
/// 带 `--output=` 或 `--output` 的 `git diff` 会写文件，是开放命令，
/// 不在基线内。其他子命令（`push`、`commit` 等）同样是开放命令。
fn git_readonly_baseline(segment: &CommandSegment) -> UnisolatedCommand {
    let second = segment
        .arguments()
        .next()
        .map(|word| word.text.as_str())
        .unwrap_or("");
    if !matches!(second, "status" | "diff") {
        return UnisolatedCommand::Open;
    }
    // `git diff --output=...` 是写文件选项，必须按开放命令拒绝。
    let writes_file = segment
        .words
        .iter()
        .any(|word| word.flag_value.as_deref().is_some_and(|_| word.text.starts_with("--output")));
    if writes_file {
        return UnisolatedCommand::Open;
    }
    UnisolatedCommand::Baseline
}

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

    // ── 无完整隔离时的开放命令收口 ──────────────────────────────────

    /// 构造指定平台的能力：文件与网络隔离恒 false（任何平台都没实现），
    /// 进程沙箱只在 Windows 为 true。不依赖真实 Linux/macOS 机器。
    fn constructed_capabilities(platform: Platform, has_process_sandbox: bool) -> PlatformCapabilities {
        PlatformCapabilities {
            platform,
            has_process_sandbox,
            has_tree_kill: has_process_sandbox,
            has_filesystem_isolation: false,
            has_network_isolation: false,
        }
    }

    /// 模拟 Linux/macOS：无进程沙箱、无文件与网络隔离。
    fn unix_like_capabilities() -> PlatformCapabilities {
        constructed_capabilities(Platform::Linux, false)
    }

    /// 模拟 Windows：有进程沙箱，但同样无文件与网络隔离。
    fn windows_like_capabilities() -> PlatformCapabilities {
        constructed_capabilities(Platform::Windows, true)
    }

    fn parse(command: &str) -> ParsedCommand {
        parse_command(command)
    }

    /// 基线形状在默认口径下放行：cargo/go/python/node 测试命令与只读 git
    /// 照跑。缺文件与网络隔离不是理由把这些正常编码任务挡掉。
    #[test]
    fn baseline_shapes_are_allowed_by_default_without_isolation() {
        let capabilities = unix_like_capabilities();
        let baseline = [
            "cargo test",
            "cargo test --release",
            "cargo check",
            "cargo build",
            "cargo test --offline",
            "go test ./...",
            "python -m unittest",
            "python -m unittest tests.test_x",
            "python -I -m unittest",
            "node --test",
            "git status",
            "git status --porcelain=v1",
            "git diff",
            "git diff --staged",
        ];
        for command in baseline {
            let decision = unisolated_decision(&capabilities, &parse(command), false);
            assert!(
                matches!(decision, UnisolatedDecision::Allowed),
                "`{command}` 是基线形状，默认应放行，实际: {decision:?}"
            );
        }
    }

    /// 基线命令接输出观察器照常放行。这些是真实运行里最常见的验证形状：
    /// 回显退出码、截取测试输出尾部。整条拒绝会把正常编码任务挡在门外。
    #[test]
    fn baseline_with_output_observers_is_allowed() {
        let capabilities = unix_like_capabilities();
        let allowed = [
            r#"go test ./... -v 2>&1; echo "EXIT=$?""#,
            "go build ./... && echo BUILD_OK",
            "go vet ./... && echo VET_OK",
            "gofmt -l . ; echo GOFMT_EXIT=$?",
            "cargo check && cargo test",
            "cargo test | head -40",
            "node --test 2>&1 | tail -30",
            "python -m unittest 2>&1 | wc -l",
            "git status && git diff",
        ];
        for command in allowed {
            let decision = unisolated_decision(&capabilities, &parse(command), false);
            assert!(
                matches!(decision, UnisolatedDecision::Allowed),
                "`{command}` 是基线加观察器，默认应放行，实际: {decision:?}"
            );
        }
    }

    /// 观察器不等于放开任意程序：链接符后面的非基线段、带文件操作数的
    /// 过滤器、写文件的 gofmt 仍然按开放命令拒绝。
    #[test]
    fn observers_do_not_open_arbitrary_programs() {
        let capabilities = unix_like_capabilities();
        let refused = [
            "cargo test && rm -rf out",
            "cargo test | curl http://example.com",
            "cargo test | sh script.sh",
            // 首段不是基线，观察器不能单独构成基线。
            "echo hi; cargo test",
            // 过滤器带文件操作数会变成读文件入口。
            "cargo test | head secret.txt",
            "cargo test | tail -n 30 notes.md",
            // gofmt 写文件。
            "gofmt -w .",
            // 基线后面接任意脚本解释器。
            "cargo test; python -c 'print(1)'",
        ];
        for command in refused {
            let decision = unisolated_decision(&capabilities, &parse(command), false);
            assert!(
                matches!(decision, UnisolatedDecision::Refused { .. }),
                "`{command}` 不是基线加观察器，默认应拒绝，实际: {decision:?}"
            );
        }
    }

    /// 开放命令在默认口径下拒绝：缺文件与网络隔离时不能无提示裸跑。
    /// 模拟 Linux/macOS、Windows 都拒（Windows 有进程沙箱但同样缺文件与
    /// 网络隔离）。
    #[test]
    fn open_commands_are_refused_by_default_on_all_isolationless_platforms() {
        for capabilities in [unix_like_capabilities(), windows_like_capabilities()] {
            let open = [
                "echo hello",
                "curl -o out.txt https://example.com",
                "cargo run",
                "cargo publish",
                "git push",
                "git commit -m x",
                "python -c 'print(1)'",
                "python -m pip install pkg",
                "node server.js",
                "git diff --output=x.txt",
                // 链接链：基线形状后面跟开放命令，整条按开放处理。
                "cargo test && curl http://x",
                "cargo test | rm out",
            ];
            for command in open {
                let decision = unisolated_decision(&capabilities, &parse(command), false);
                match decision {
                    UnisolatedDecision::Refused {
                        reason,
                        stable_code,
                    } => {
                        assert_eq!(stable_code, REFUSAL_CODE_UNISOLATED);
                        assert!(
                            reason.starts_with(REFUSAL_CODE_UNISOLATED),
                            "拒绝理由以稳定码开头，供程序分支: {reason}"
                        );
                        assert!(
                            reason.contains(ALLOW_UNISOLATED_COMMANDS_ENV),
                            "拒绝理由给出可执行建议（开关名）: {reason}"
                        );
                        assert!(
                            reason.contains("未生效"),
                            "拒绝理由写明隔离未生效: {reason}"
                        );
                    }
                    other => panic!(
                        "`{command}` 应被默认拒绝，实际: {other:?}"
                    ),
                }
            }
        }
    }

    /// 宿主显式设开关时开放命令放行，但未生效声明照旧存在：执行仍是「无
    /// 完整隔离」，证据与模型可见文本必须声明 active=false 与原因。
    #[test]
    fn explicit_host_opt_in_allows_open_commands_but_isolation_is_still_missing() {
        let capabilities = unix_like_capabilities();
        let open = ["echo hello", "curl -o out.txt https://example.com", "git push"];
        for command in open {
            let decision = unisolated_decision(&capabilities, &parse(command), true);
            assert!(
                matches!(decision, UnisolatedDecision::Allowed),
                "`{command}` 在宿主放行时应允许执行"
            );
        }
        // 未生效声明与裁决无关：宿主放行后仍然必须附带，让证据如实反映。
        let statement = capabilities.unisolation_statement().expect("缺隔离能力必有声明");
        assert!(statement.contains("未生效"), "{statement}");
    }

    /// Windows 也有进程沙箱，但文件与网络隔离缺失时未生效声明仍要出：
    /// 收口不能只针对 Linux/macOS。
    #[test]
    fn windows_process_sandbox_does_not_silence_the_unisolation_statement() {
        let capabilities = windows_like_capabilities();
        let statement = capabilities.unisolation_statement().expect("Windows 缺文件与网络隔离");
        assert!(statement.contains("未生效"), "{statement}");
        // 开放命令默认拒，与 Linux/macOS 同口径。
        let decision = unisolated_decision(&capabilities, &parse("git push"), false);
        assert!(matches!(decision, UnisolatedDecision::Refused { .. }));
    }

    /// 两项隔离都实现时不收口：开放命令默认可执行（这是将来平台补上隔离
    /// 能力后应自动放行的语义，当前 detect() 永远走不到这里）。
    #[test]
    fn fully_isolated_platform_never_enters_the_gate() {
        let capabilities = PlatformCapabilities {
            platform: Platform::Linux,
            has_process_sandbox: true,
            has_tree_kill: true,
            has_filesystem_isolation: true,
            has_network_isolation: true,
        };
        assert!(capabilities.unisolation_statement().is_none());
        let decision = unisolated_decision(&capabilities, &parse("git push"), false);
        assert!(matches!(decision, UnisolatedDecision::Allowed));
    }
}

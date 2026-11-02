//! 审批基础设施：从 stdin 读一行的人工审批。
//!
//! 这是生产默认闸门。与 `auto` 的区别不只是"问一句"——它引入了
//! 本会话内的信任记忆（`a` 选项），并且把"读不到答案"一律当作否决。
//!
//! fail-closed 是这里唯一的设计原则：空输入、无法识别的输入、stdin
//! 关闭（EOF），全都走否决分支。宁可让模型多撞几次墙，也不能在
//! "没人看着"的时候把 destructive 工具放过去。

use std::io::{BufRead, Write};
use std::sync::Mutex;

use crate::application::port::{ApprovalGate, ApprovalRequest, BoxFuture, Decision};
use crate::domain::policy::PolicyScope;

/// 本会话内的一条信任记录。
///
/// P1 只按工具名记忆，粒度太粗：一句"允许 write_file"会连带放行之后所有
/// write_file，包括写 `Cargo.toml` 的。P8-2 把信任拆成两档——参数完全
/// 一致的精确信任，和参数文本命中某个前缀的模式信任。
enum TrustEntry {
    /// `a`：同工具且参数完全一致才放行。
    Exact {
        /// 工具名。
        tool: String,
        /// 被放行那次调用的参数原文。
        arguments: serde_json::Value,
    },
    /// `p`：同工具且参数文本命中用户给出的前缀模式。
    Prefix {
        /// 工具名。
        tool: String,
        /// 用户给出的前缀模式，落在哪个字段取决于调用带的是 command 还是 path。
        scope: PolicyScope,
    },
}

impl TrustEntry {
    /// 这条信任是否覆盖给定的调用。
    fn covers(&self, call: &crate::domain::tool::ToolCall) -> bool {
        match self {
            TrustEntry::Exact { tool, arguments } => {
                tool == &call.name && arguments == &call.arguments
            }
            TrustEntry::Prefix { tool, scope } => tool == &call.name && scope.matches(call),
        }
    }
}

/// 从标准输入读一行的人工审批闸门。
///
/// 内部持有本会话的信任记录，供 `a` / `p` 选项使用。放这里而不是放进
/// 领域层或应用层：它是"人这次坐在终端前"的会话状态，属于交互基础设施，
/// 且它随闸门实例生灭——正好和一次 `cargo run` 同寿。
pub struct CliApprovalGate {
    /// 本会话内已被放行的调用，按授予顺序排列。
    ///
    /// 用 `Vec` 而不是集合：前缀信任之间不互斥，用户可能为同一个工具
    /// 授多个前缀；查询是线性扫，长度等同于用户按过几次 `a`/`p`。
    trusted: Mutex<Vec<TrustEntry>>,
}

impl CliApprovalGate {
    /// 构造一个空的交互式闸门。
    pub fn new() -> Self {
        Self {
            trusted: Mutex::new(Vec::new()),
        }
    }

    /// 从给定 reader 读答案，据此给出裁决。
    ///
    /// 独立成函数是为了测试：测试不能抢进程的 stdin，
    /// 但可以用 `&[u8]` 冒充几行输入来喂进去。
    fn decide_from(&self, reader: &mut dyn BufRead, writer: &mut dyn Write, request: &ApprovalRequest) -> Decision {
        // 带 reason 的请求是策略之上追加的复核（典型是越界写入升级）。
        // 它的全部意义就在于"这一次必须有人过目"，所以会话信任整体失效：
        // 不查信任、不接受 `a`/`p`。否则模型把同一条越界命令再发一遍，
        // 就会被第一次授出的信任无声放行。
        let trustable = request.reason.is_none();

        if trustable {
            if let Some(note) = self.trust_note(&request.call) {
                let _ = writeln!(writer, "[审批] {} 已在会话内放行，自动通过", request.call.name);
                let _ = writeln!(writer, "        （{note}）");
                return Ok(());
            }
        }

        // `p` 之后用户可能留空取消，此时要回到问题本身而不是直接否决，
        // 所以这是一个循环而不是一次问答。
        loop {
            let _ = writeln!(writer);
            let _ = writeln!(writer, "──── 审批请求 ────");
            let _ = writeln!(writer, "工具:   {}", request.call.name);
            let _ = writeln!(
                writer,
                "参数:   {}",
                serde_json::to_string(&request.call.arguments).unwrap_or_else(|_| "{}".to_owned())
            );
            let _ = writeln!(
                writer,
                "风险:   {}",
                risk_summary(&request.annotations)
            );
            if let Some(reason) = &request.reason {
                let _ = writeln!(writer, "复核:   {reason}");
            }
            let _ = write!(writer, "批准执行？");
            let _ = write!(writer, "[y] 允许  [n] 否决  ");
            if trustable {
                let _ = write!(
                    writer,
                    "[a] 本会话允许此精确调用  [p] 本会话按前缀允许  "
                );
            } else {
                let _ = write!(writer, "（本次复核不适用会话信任）  ");
            }
            let _ = write!(writer, "[q] 中止运行: ");
            let _ = writer.flush();

            // 读不到行（EOF）时 line 为 Ok(0) 或 Err，两者都算否决。
            let mut line = String::new();
            let answer = match reader.read_line(&mut line) {
                Ok(0) | Err(_) => None,
                Ok(_) => parse_answer(line.trim()),
            };

            match answer {
                Some(Answer::Approve) => return Ok(()),
                Some(Answer::Trust) if trustable => {
                    self.trust_exact(&request.call);
                    return Ok(());
                }
                Some(Answer::Prefix) if trustable => {
                    match self.ask_prefix(reader, writer, &request.call) {
                        Some(scope) => {
                            self.trust_prefix(&request.call, scope);
                            return Ok(());
                        }
                        // 空前缀或没有可匹配的参数：既不放行也不否决，重新问。
                        None => continue,
                    }
                }
                // 复核请求不接受信任类答复：提示一句，重新问，不放行也不否决。
                Some(Answer::Trust) | Some(Answer::Prefix) => {
                    let _ = writeln!(
                        writer,
                        "[审批] 本次是策略追加的复核，不能存入会话信任；请直接答 [y] 或 [n]。"
                    );
                    continue;
                }
                Some(Answer::Deny) => return Err(ask_reason(reader, writer)),
                Some(Answer::Abort) => return Err("用户中止了本次运行".to_owned()),
                // fail-closed：空行、无法识别、EOF 全部落到这里。
                None => return Err("未给出有效答复，按策略视为否决".to_owned()),
            }
        }
    }

    /// 覆盖这个调用的信任记录的一行说明；没有则 `None`。
    ///
    /// 返回说明而不是记录本身：锁守卫是临时值，借不出引用；而调用方
    /// 要的只是"凭什么放行"那句话。
    fn trust_note(&self, call: &crate::domain::tool::ToolCall) -> Option<String> {
        let trusted = self
            .trusted
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        trusted
            .iter()
            .find(|entry| entry.covers(call))
            .map(trust_description)
    }

    /// 记录一次精确信任。
    fn trust_exact(&self, call: &crate::domain::tool::ToolCall) {
        self.trusted
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(TrustEntry::Exact {
                tool: call.name.clone(),
                arguments: call.arguments.clone(),
            });
    }

    /// 记录一次前缀信任。
    fn trust_prefix(&self, call: &crate::domain::tool::ToolCall, scope: PolicyScope) {
        self.trusted
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(TrustEntry::Prefix {
                tool: call.name.clone(),
                scope,
            });
    }

    /// 追问要放行的前缀，返回它落在哪个作用域字段上。
    ///
    /// `None` 表示用户取消（留空且没有建议值可接受），或这个调用根本没有
    /// 可匹配的参数——两者都该退回重新问，而不是记一条覆盖不全的信任。
    fn ask_prefix(
        &self,
        reader: &mut dyn BufRead,
        writer: &mut dyn Write,
        call: &crate::domain::tool::ToolCall,
    ) -> Option<PolicyScope> {
        // 没有可匹配的参数时连问都不要问：问了也给不出能用的信任，
        // 只会让用户在前缀输入框里白填一个字符串。
        if matchable_argument(call).is_none() {
            let _ = writeln!(
                writer,
                "[审批] 该工具没有命令或路径参数，无法按前缀放行；请改选 [a] 或 [y]。"
            );
            return None;
        }

        // P9-5：先算一个候选建议。它只是预填的默认值——用户敲了别的就以
        // 敲的为准，留空则接受它。给不出建议时行为与从前逐字相同。
        let suggestion = suggested_prefix(call);
        let accept_hint = match &suggestion {
            Some(candidate) => format!("回车用 `{candidate}`，或输入别的"),
            None => "回车取消".to_owned(),
        };

        let _ = writeln!(writer);
        let _ = writeln!(
            writer,
            "本会话按前缀放行：之后参数文本以该前缀开头的同类调用都不再询问。"
        );
        let _ = write!(writer, "前缀（{accept_hint}）: ");
        let _ = writer.flush();

        let mut line = String::new();
        let input = match reader.read_line(&mut line) {
            // EOF 与读取错误照旧算取消，不接受建议：fail-closed 的边界
            // 不该被一次"省打字"的便利撬开。
            Ok(0) | Err(_) => return None,
            Ok(_) => line.trim().to_owned(),
        };
        // 留空且给得出建议值：接受建议。这是 P9-5 省下的那次打字。
        // 没有建议值时 `?` 直接返回 None，走的仍是"回车取消"老路。
        let input = if input.is_empty() { suggestion? } else { input };
        scope_for(call, &input)
    }

    /// 测试用：当前信任记录的条数。
    #[cfg(test)]
    fn trusted_entries(&self) -> usize {
        self.trusted
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len()
    }
}

/// 这个调用的参数里，哪个字段能被前缀信任匹配上。
///
/// shell 是威力最大的工具，优先按命令头匹配；文件类工具没有 `command`，
/// 落到 `path`。两者都没有（git_status、多数 MCP 工具）时返回 `None`，
/// 调用方据此判定"这个工具没法按前缀放行"——空作用域会匹配一切，
/// 拿它当信任用等于放行整个工具。
fn matchable_argument(call: &crate::domain::tool::ToolCall) -> Option<&'static str> {
    ["command", "path"]
        .into_iter()
        .find(|key| call.arguments.get(*key).and_then(serde_json::Value::as_str).is_some())
}

/// 把用户输入的前缀装进调用实际带的那个参数字段。
///
/// 末尾的 `*` 由这里补上：选了 `p` 就等于声明"这是前缀"，模式语言的
/// 记法不该让使用者去记。用户已经写了 `*` 就不重复补。
fn scope_for(call: &crate::domain::tool::ToolCall, input: &str) -> Option<PolicyScope> {
    let pattern = if input.ends_with('*') {
        input.to_owned()
    } else {
        format!("{input}*")
    };
    match matchable_argument(call) {
        Some("command") => Some(PolicyScope {
            command_prefix: Some(pattern),
            ..Default::default()
        }),
        Some(_) => Some(PolicyScope {
            path_prefix: Some(pattern),
            ..Default::default()
        }),
        None => None,
    }
}

/// 从一次调用推断一个保守的前缀信任候选，供 `p` 预填（P9-5）。
///
/// `p` 的全部意义是少敲几次键盘，但候选绝不能比人愿意授的范围更宽，
/// 所以"词数要够"是硬要求：`cargo` 这种裸程序名会让会话信任膨胀到该
/// 程序的每一个子命令（包括 `cargo publish`），那不如不给建议。
///
/// - `command`：取前两个词。`cargo test --lib` → `cargo test`，覆盖
///   `cargo test --doc` 一族，但不碰 `cargo build`、`cargo publish`。
///   本次特有的参数不进候选——带上它，这条信任就永远匹配不上下一条调用。
///   带包装前缀的命令（`sudo cargo test`）取到的是 `sudo cargo`：范围仍以
///   那个程序为界，不会扩到 `sudo` 的别的子命令，但确实也涵盖
///   `sudo cargo publish`。所以候选只是**起点**——它原样印在提示框里，
///   按回车之前看得见自己授的范围，嫌宽就自己敲窄的。
/// - `path`：取所在目录。`src/domain/policy.rs` → `src/domain/`。目录边界
///   是天然的授权范围；不取"前两段路径"是因为前缀匹配不认词边界，
///   `src/domain*` 会连 `src/domain.rs` 一起吞掉。
///
/// 三种情况一律不给建议：少于两个词的命令、根目录下的文件、带 shell
/// 链接符的命令。第一种让命令头宽到整个程序；第二种连一个目录边界都没有；
/// 第三种是因为信任只覆盖命令头（`prefix_matches` 明确不让 `*` 吞 `&&`
/// 之后的部分），而人眼前那条调用是一条链——建议一个只覆盖前半截的前缀，
/// 会让人以为整条链都被记住了。
fn suggested_prefix(call: &crate::domain::tool::ToolCall) -> Option<String> {
    if let Some(command) = call.arguments.get("command").and_then(serde_json::Value::as_str) {
        if crate::domain::policy::chains_another_command(command) {
            return None;
        }
        let mut words = command.split_whitespace();
        let program = words.next()?;
        let subcommand = words.next()?;
        return Some(format!("{program} {subcommand}"));
    }
    let path = call.arguments.get("path").and_then(serde_json::Value::as_str)?;
    let (directory, _) = path.rsplit_once('/')?;
    if directory.is_empty() {
        return None;
    }
    Some(format!("{directory}/"))
}

/// 信任记录的一行摘要，打印在"自动通过"旁边。
///
/// 用户需要知道这次放行是凭哪条记忆——尤其是前缀信任，放行范围比
/// 他看到的那一条调用宽。
fn trust_description(entry: &TrustEntry) -> String {
    match entry {
        TrustEntry::Exact { .. } => "精确信任：参数完全一致".to_owned(),
        TrustEntry::Prefix { scope, .. } => match (&scope.command_prefix, &scope.path_prefix) {
            (Some(pattern), _) => format!("前缀信任：命令以 `{pattern}` 开头"),
            (None, Some(pattern)) => format!("前缀信任：路径以 `{pattern}` 开头"),
            (None, None) => "前缀信任".to_owned(),
        },
    }
}

impl Default for CliApprovalGate {
    fn default() -> Self {
        Self::new()
    }
}

impl ApprovalGate for CliApprovalGate {
    fn request<'a>(&'a self, request: &'a ApprovalRequest) -> BoxFuture<'a, Decision> {
        Box::pin(async move {
            let stdin = std::io::stdin();
            let mut reader = stdin.lock();
            let mut writer = std::io::stdout();
            self.decide_from(&mut reader, &mut writer, request)
        })
    }
}

/// 用户对一次审批的五种答复。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Answer {
    /// `y`：仅放行这一次。
    Approve,
    /// `a`：本会话内放行参数完全一致的同类调用。
    Trust,
    /// `p`：本会话内按前缀放行同类调用。
    Prefix,
    /// `n`：否决，并追问原因。
    Deny,
    /// `q`：中止整个运行。
    Abort,
}

/// 解析一行输入。`None` 表示无法识别——调用方按 fail-closed 处理。
///
/// 接受大小写与前后空白，但不接受 `yes`/`no` 全词：终端里多打一个
/// 字符就可能毁掉一次 destructive 操作，宁可让用户重打。
fn parse_answer(line: &str) -> Option<Answer> {
    match line.trim().to_ascii_lowercase().as_str() {
        "y" => Some(Answer::Approve),
        "a" => Some(Answer::Trust),
        "p" => Some(Answer::Prefix),
        "n" => Some(Answer::Deny),
        "q" => Some(Answer::Abort),
        _ => None,
    }
}

/// 追问否决原因。
///
/// 原因会作为拒绝观察回填给模型，所以空原因也要给个默认值——
/// 模型看到"被拒绝"但不知道为什么，只会盲目重试同一个调用。
fn ask_reason(reader: &mut dyn BufRead, writer: &mut dyn Write) -> String {
    let _ = write!(writer, "否决原因（回车使用默认）: ");
    let _ = writer.flush();

    let mut line = String::new();
    match reader.read_line(&mut line) {
        Ok(0) | Err(_) => "用户未提供原因".to_owned(),
        Ok(_) => {
            let reason = line.trim();
            if reason.is_empty() {
                "用户未提供原因".to_owned()
            } else {
                reason.to_owned()
            }
        }
    }
}

/// 把风险注解渲染成一行人能读的摘要。
fn risk_summary(annotations: &crate::domain::tool::ToolAnnotations) -> String {
    let mut flags = Vec::new();
    if annotations.destructive {
        flags.push("破坏性");
    }
    if annotations.open_world {
        flags.push("访问外部");
    }
    if !annotations.idempotent {
        flags.push("非幂等");
    }
    if flags.is_empty() {
        "仅写操作，无已知风险标记".to_owned()
    } else {
        flags.join(" · ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::ids::CallId;
    use crate::domain::tool::{ToolAnnotations, ToolCall};
    use serde_json::json;
    use std::io::Cursor;

    fn request(name: &str) -> ApprovalRequest {
        request_with(name, json!({"path": "src/main.rs"}))
    }

    fn request_with(name: &str, arguments: serde_json::Value) -> ApprovalRequest {
        ApprovalRequest {
            turn: 1,
            call: ToolCall {
                id: CallId::new("call_1"),
                name: name.to_owned(),
                arguments,
            },
            annotations: ToolAnnotations::new(false, true, false, false),
            reason: None,
        }
    }

    /// 策略追加的复核请求（P8-3 的越界写入升级就是这一种）。
    fn escalated_request(command: &str) -> ApprovalRequest {
        ApprovalRequest {
            reason: Some(format!(
                "策略已放行，但疑似越界写入：命令把输出重定向到 workspace 外的 `{command}`"
            )),
            ..request_with("run_command", json!({"command": command}))
        }
    }


    /// 用内存 reader/writer 跑一次审批，拿到裁决。
    fn decide(gate: &CliApprovalGate, input: &str, req: &ApprovalRequest) -> Decision {
        let mut reader = Cursor::new(input.as_bytes());
        let mut sink = Vec::new();
        gate.decide_from(&mut reader, &mut sink, req)
    }

    #[test]
    fn yes_approves_the_call() {
        let gate = CliApprovalGate::new();
        assert!(decide(&gate, "y\n", &request("write_file")).is_ok());
    }

    #[test]
    fn yes_is_case_insensitive_and_tolerates_whitespace() {
        let gate = CliApprovalGate::new();
        assert!(decide(&gate, "  Y \n", &request("write_file")).is_ok());
    }

    #[test]
    fn no_denies_with_a_reason() {
        let gate = CliApprovalGate::new();
        let decision = decide(&gate, "n\n不想改这个文件\n", &request("write_file"));

        let reason = decision.expect_err("n 应否决");
        assert_eq!(reason, "不想改这个文件");
    }

    #[test]
    fn empty_line_fails_closed() {
        let gate = CliApprovalGate::new();
        // 用户直接回车，什么都没说。
        let decision = decide(&gate, "\n", &request("write_file"));

        let reason = decision.expect_err("空输入必须否决");
        assert!(reason.contains("视为否决"), "原因应说明是 fail-closed: {reason}");
    }

    #[test]
    fn unrecognized_input_fails_closed() {
        let gate = CliApprovalGate::new();
        // "yes" 不是合法答复——见 parse_answer 的注释。
        let decision = decide(&gate, "yes\n", &request("write_file"));

        assert!(decision.is_err(), "无法识别的输入必须否决");
    }

    #[test]
    fn eof_fails_closed() {
        let gate = CliApprovalGate::new();
        // 空输入流 = stdin 已关闭。
        let decision = decide(&gate, "", &request("write_file"));

        let reason = decision.expect_err("EOF 必须否决");
        assert!(reason.contains("视为否决"), "原因应说明是 fail-closed: {reason}");
    }

    #[test]
    fn abort_reports_that_the_user_stopped_the_run() {
        let gate = CliApprovalGate::new();
        let decision = decide(&gate, "q\n", &request("write_file"));

        let reason = decision.expect_err("q 应否决");
        assert!(reason.contains("中止"), "原因应说明是用户主动中止: {reason}");
    }

    /// `a` 记住的是**这一次调用**，不是这个工具。
    ///
    /// P1 的 `a` 按工具名记忆，于是"允许 write_file"会连带放行之后所有
    /// write_file——包括写 `Cargo.toml`、写 `.env` 的那些。参数不同，
    /// 后果不同，信任的粒度必须跟后果对齐。
    #[test]
    fn trust_a_remembers_the_exact_call_not_the_tool() {
        let gate = CliApprovalGate::new();
        let req = request("write_file");

        // 第一次回答 a：本会话放行这一模一样的调用。
        assert!(decide(&gate, "a\n", &req).is_ok());
        assert_eq!(gate.trusted_entries(), 1);

        // 参数完全一致的后续调用不再需要输入——reader 是空的（EOF），
        // 如果它没记住，就会因为读不到答案而否决。
        assert!(decide(&gate, "", &req).is_ok(), "已信任的调用应自动放行");

        // 同一个工具、不同参数：仍然要问，且读不到输入时否决。
        let other = request_with("write_file", json!({"path": "Cargo.toml", "content": "x"}));
        assert!(
            decide(&gate, "", &other).is_err(),
            "参数不同的同工具调用不得被放行"
        );
    }

    /// `p` 按用户给的前缀放行：命中前缀的后续调用自动通过，其余照问。
    #[test]
    fn trust_p_covers_calls_matching_the_prefix_only() {
        let gate = CliApprovalGate::new();
        let first = request_with("run_command", json!({"command": "cargo test --nocapture"}));

        // p 之后输入前缀；p 分支自身不需要二次确认。
        assert!(decide(&gate, "p\ncargo test\n", &first).is_ok());

        // 同前缀的另一条命令自动通过。
        let same_prefix = request_with("run_command", json!({"command": "cargo test --lib"}));
        assert!(decide(&gate, "", &same_prefix).is_ok(), "前缀内的命令应自动放行");

        // 裸命令也在前缀内：用户写 "cargo test" 时没带尾部空格。
        let bare = request_with("run_command", json!({"command": "cargo test"}));
        assert!(decide(&gate, "", &bare).is_ok(), "裸命令应算前缀命中");

        // 前缀外的命令仍然要问。
        let dangerous = request_with("run_command", json!({"command": "rm -rf /"}));
        assert!(decide(&gate, "", &dangerous).is_err(), "前缀外的命令不得被放行");
    }

    /// `p` 对文件类工具放行的是路径前缀。
    #[test]
    fn trust_p_uses_the_path_argument_for_file_tools() {
        let gate = CliApprovalGate::new();
        assert!(decide(&gate, "p\nsrc/\n", &request("write_file")).is_ok());

        let inside = request_with("write_file", json!({"path": "src/lib.rs", "content": "x"}));
        assert!(decide(&gate, "", &inside).is_ok(), "路径前缀内的写入应自动放行");

        let outside = request_with("write_file", json!({"path": "docs/a.md", "content": "x"}));
        assert!(
            decide(&gate, "", &outside).is_err(),
            "路径前缀外的写入仍要审批"
        );
    }

    /// `p` 之后留空：既不否决也不记信任，回到批准问题本身。
    ///
    /// 用户敲错键选了 `p` 却不能反悔，会被迫在前缀输入框里编一个字符串——
    /// 那正是信任机制最不该有的用法。
    ///
    /// 用根目录下的文件：它给不出候选建议，所以留空走的仍是"取消"这条
    /// 老路。P9-5 之后留空的含义取决于有没有建议值——有建议时留空是
    /// 接受它（见 `accepting_the_suggestion_with_enter_trusts_the_suggested_prefix`），
    /// 那条路径上提示框已经把要授的范围原样印出来了。
    #[test]
    fn empty_prefix_answer_re_asks_without_trusting() {
        let gate = CliApprovalGate::new();
        let req = request_with("write_file", json!({"path": "main.rs"}));

        // p → 空前缀 → 重新问 → 这次答 y。
        assert!(decide(&gate, "p\n\ny\n", &req).is_ok());
        assert_eq!(gate.trusted_entries(), 0, "空前缀不得写入信任");

        // 既然没记信任，下一次照问。
        assert!(decide(&gate, "", &req).is_err());
    }

    /// 没有 command 也没有 path 参数的工具（git_status、MCP 工具）无法按
    /// 前缀放行：`p` 只能退回重新问，不能悄悄退化成"放行整个工具"。
    #[test]
    fn prefix_trust_is_unavailable_without_a_matchable_argument() {
        let gate = CliApprovalGate::new();
        let req = request_with("git_status", json!({}));

        // p → 没有可匹配参数，直接退回重新问（不消费下一行）→ 答 y 放行这一次。
        assert!(decide(&gate, "p\ny\n", &req).is_ok());
        assert_eq!(gate.trusted_entries(), 0);

        // 下一次照问。
        assert!(decide(&gate, "", &req).is_err());
    }

    #[test]
    fn plain_approval_does_not_trust_the_tool() {
        let gate = CliApprovalGate::new();
        assert!(decide(&gate, "y\n", &request("write_file")).is_ok());

        // 下一次仍需审批，且读不到输入时否决。
        assert!(decide(&gate, "", &request("write_file")).is_err());
        assert_eq!(gate.trusted_entries(), 0, "y 不应写入信任集合");
    }

    #[test]
    fn denial_reason_falls_back_when_user_gives_none() {
        let gate = CliApprovalGate::new();
        // n 之后直接回车，不写原因。
        let decision = decide(&gate, "n\n\n", &request("write_file"));

        assert_eq!(decision.expect_err("仍应否决"), "用户未提供原因");
    }

    #[test]
    fn prompt_shows_tool_arguments_and_risk() {
        let gate = CliApprovalGate::new();
        let mut reader = Cursor::new(b"y\n".as_slice());
        let mut sink = Vec::new();

        let _ = gate.decide_from(&mut reader, &mut sink, &request("write_file"));
        let printed = String::from_utf8(sink).expect("输出是 UTF-8");

        assert!(printed.contains("write_file"), "应打印工具名");
        assert!(printed.contains("src/main.rs"), "应打印参数");
        assert!(printed.contains("破坏性"), "应打印风险");
        assert!(printed.contains("[p]"), "应说明前缀信任选项");
    }

    /// 前缀输入框要把后果说清楚：它放行的是一整类调用，不是这一次。
    ///
    /// 用裸命令（`ls`）：它给不出候选建议，所以"回车取消"这条老路仍在。
    /// 有建议时的提示语由 `prefix_prompt_shows_the_suggested_candidate` 守着。
    #[test]
    fn prefix_prompt_explains_the_scope_of_the_grant() {
        let gate = CliApprovalGate::new();
        let mut reader = Cursor::new(b"p

".as_slice());
        let mut sink = Vec::new();

        let _ = gate.decide_from(&mut reader, &mut sink, &request_with("run_command", json!({"command": "ls"})));
        let printed = String::from_utf8(sink).expect("输出是 UTF-8");

        assert!(printed.contains("不再询问"), "应说明放行范围: {printed}");
        assert!(printed.contains("回车取消"), "应说明可以取消: {printed}");
    }

    /// 带 reason 的复核请求对会话信任免疫：一次都不能被 `a`/`p` 消掉。
    ///
    /// 升级的全部意义是"越界写入每次都要人过目"。若信任对它有效，
    /// 模型把同一条命令重发一遍就会被无声放行——升级白做。
    #[test]
    fn escalated_request_ignores_session_trust() {
        let gate = CliApprovalGate::new();
        let req = escalated_request(r"echo hi > C:\Windows\temp\x");

        // 先授出一条前缀信任，让它确实在会话里生效。
        let ordinary = request_with("run_command", json!({"command": "cargo test"}));
        assert!(decide(&gate, "p\ncargo\n", &ordinary).is_ok());
        assert_eq!(gate.trusted_entries(), 1);
        // 信任确实有效：同前缀的普通调用不再询问。
        let same_prefix = request_with("run_command", json!({"command": "cargo build"}));
        assert!(decide(&gate, "", &same_prefix).is_ok());

        // 复核请求照问，且 `a` 不被接受——它既不记信任也不放行。
        assert!(decide(&gate, "a\n", &req).is_err(), "复核请求不得被 `a` 放行");
        assert_eq!(gate.trusted_entries(), 1, "复核请求不得写入信任");

        // `p` 同样不被接受。
        assert!(decide(&gate, "p\ncargo\n", &req).is_err());
        assert_eq!(gate.trusted_entries(), 1, "复核请求不得写入信任");

        // 答 y 才放行，而且只放行这一次。
        assert!(decide(&gate, "y\n", &req).is_ok());
        assert!(decide(&gate, "", &req).is_err(), "复核请求每次都要问");
    }

    /// 复核请求的提示要说明为什么不给信任选项。
    #[test]
    fn escalated_prompt_shows_the_reason_and_hides_trust_options() {
        let gate = CliApprovalGate::new();
        let mut reader = Cursor::new(b"y\n".as_slice());
        let mut sink = Vec::new();

        let _ = gate.decide_from(&mut reader, &mut sink, &escalated_request(r"echo hi > C:\x"));
        let printed = String::from_utf8(sink).expect("输出是 UTF-8");

        assert!(printed.contains("复核"), "应标明这是复核: {printed}");
        assert!(printed.contains(r"C:\x"), "应点名越界目标: {printed}");
        assert!(!printed.contains("[a]"), "复核请求不应提供 [a]: {printed}");
        assert!(!printed.contains("[p]"), "复核请求不应提供 [p]: {printed}");
        assert!(printed.contains("不适用会话信任"), "应说明原因: {printed}");
    }

    #[test]
    fn risk_summary_handles_a_clean_annotation() {
        let clean = ToolAnnotations::new(false, false, true, false);
        assert_eq!(risk_summary(&clean), "仅写操作，无已知风险标记");
    }

    // ── P9-5 前缀候选建议 ──────────────────────────────────────────────

    fn call_with(name: &str, arguments: serde_json::Value) -> ToolCall {
        ToolCall {
            id: CallId::new("call_1"),
            name: name.to_owned(),
            arguments,
        }
    }

    /// 命令的候选取前两个词：够窄到不吞掉别的子命令，
    /// 也够宽到把同一条命令的参数变体一次收编。
    #[test]
    fn suggested_prefix_takes_the_first_two_words_of_a_command() {
        assert_eq!(
            suggested_prefix(&call_with("run_command", json!({"command": "cargo test --lib foo"}))),
            Some("cargo test".to_owned()),
            "参数不该进候选：候选是给后续调用用的，带上这次特有的参数就永远匹配不上"
        );
        // 恰好两个词时候选就是整条命令：省下打字，范围一点没变。
        assert_eq!(
            suggested_prefix(&call_with("run_command", json!({"command": "cargo test"}))),
            Some("cargo test".to_owned())
        );
    }

    /// 裸程序名不给建议：一个词的命令头等于把整个程序交给会话信任，
    /// `cargo` 会连 `cargo publish` 一起吞掉——那不如让用户自己敲。
    #[test]
    fn a_bare_program_name_gets_no_suggestion() {
        assert_eq!(
            suggested_prefix(&call_with("run_command", json!({"command": "ls"}))),
            None
        );
    }

    /// 路径的候选是所在目录：目录边界是天然的授权范围。
    ///
    /// 不取"前两段路径"是因为前缀匹配不认词边界，`src/domain*` 会连
    /// `src/domain.rs` 一起吞掉；带上结尾的 `/` 才精确落在目录内。
    #[test]
    fn suggested_prefix_for_a_path_is_its_directory() {
        assert_eq!(
            suggested_prefix(&call_with("write_file", json!({"path": "src/domain/policy.rs"}))),
            Some("src/domain/".to_owned())
        );
    }

    /// 就在 workspace 根上的文件没有目录可授：给建议只会变成
    /// "放行所有写入"，那是用户没说过的范围。
    #[test]
    fn a_path_at_the_workspace_root_gets_no_suggestion() {
        assert_eq!(
            suggested_prefix(&call_with("write_file", json!({"path": "main.rs"}))),
            None
        );
    }

    /// 带 shell 链接符的命令不给建议。
    ///
    /// 信任只覆盖命令头（`prefix_matches` 明确不让 `*` 吞 `&&` 之后的部分），
    /// 而人眼前这条调用是一条链——建议一个只覆盖前半截的前缀，会让人以为
    /// 整条链都被记住了，下次它照样要问。
    #[test]
    fn chained_commands_get_no_suggestion() {
        assert_eq!(
            suggested_prefix(&call_with("run_command", json!({"command": "cargo test && git push"}))),
            None
        );
    }

    /// 没有 command / path 参数的工具（git_status、多数 MCP 工具）
    /// 没有可授的范围——与既有的"无法按前缀放行"是同一个结论。
    #[test]
    fn a_tool_without_matchable_arguments_gets_no_suggestion() {
        assert_eq!(suggested_prefix(&call_with("git_status", json!({}))), None);
    }

    /// 建议值必须原样显示在输入框里。
    ///
    /// 人得看得见自己授的是什么范围，否则"回车"这个动作没有任何信息量——
    /// 他以为批的是眼前这一条，实际放开的是一整类调用。
    #[test]
    fn prefix_prompt_shows_the_suggested_candidate() {
        let gate = CliApprovalGate::new();
        let mut reader = Cursor::new(b"p\n\n".as_slice());
        let mut sink = Vec::new();
        let req = request_with("run_command", json!({"command": "cargo test --lib"}));

        let _ = gate.decide_from(&mut reader, &mut sink, &req);
        let printed = String::from_utf8(sink).expect("输出是 UTF-8");

        assert!(printed.contains("cargo test"), "应显示建议值: {printed}");
    }

    /// 选 `p` 后直接回车 = 接受建议值，不必手敲。
    #[test]
    fn accepting_the_suggestion_with_enter_trusts_the_suggested_prefix() {
        let gate = CliApprovalGate::new();
        let req = request_with("run_command", json!({"command": "cargo test --lib"}));

        // "p" 之后紧跟一个空行：接受建议。
        assert!(decide(&gate, "p\n\n", &req).is_ok());

        // 建议范围生效：同命令头的另一条调用不再询问。
        let same_head = request_with("run_command", json!({"command": "cargo test --doc"}));
        assert!(decide(&gate, "", &same_head).is_ok(), "cargo test 一族应已被信任");
        // 但换了子命令仍要问：建议值没有宽到把 cargo 整个吞掉。
        let other = request_with("run_command", json!({"command": "cargo build"}));
        assert!(decide(&gate, "", &other).is_err(), "cargo build 不在建议范围内");
    }

    /// 用户仍然可以敲自己的前缀：建议只是省一次打字，不是替人决定。
    #[test]
    fn typing_a_prefix_still_overrides_the_suggestion() {
        let gate = CliApprovalGate::new();
        let req = request_with("run_command", json!({"command": "cargo test --lib"}));

        assert!(decide(&gate, "p\ncargo\n", &req).is_ok());

        let under_cargo = request_with("run_command", json!({"command": "cargo build"}));
        assert!(decide(&gate, "", &under_cargo).is_ok(), "用户敲的前缀优先");
        let cargo_test = request_with("run_command", json!({"command": "cargo test"}));
        assert!(decide(&gate, "", &cargo_test).is_ok());
    }
}

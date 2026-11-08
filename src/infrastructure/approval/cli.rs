//! 审批基础设施：从 stdin 读取人工审批。
//!
//! 生产默认闸门。与 `auto` 的区别在于本会话内的信任记忆（`a` / `p`），
//! 以及读不到答案时一律否决。
//!
//! fail-closed 是这里的唯一原则：空输入、无法识别的输入、stdin 关闭
//! （EOF）全部走否决分支。即使模型因此多试几次，也不能在无人看管时
//! 放行 destructive 工具。

use std::io::{BufRead, Write};
use std::sync::Mutex;

use crate::application::port::{
    ApprovalGate, ApprovalRequest, BoxFuture, CancelHandle, Decision,
};
use crate::domain::policy::PolicyScope;

/// 本会话内的一条信任记录。
///
/// 信任粒度与后果对齐：按调用记录，不按工具名记录。按工具名放行
/// write_file 会连带放行写 `Cargo.toml` 的调用，参数不同，后果不同。
/// 因此分两档：参数完全一致的精确信任，以及参数文本命中前缀的模式信任。
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
        /// 用户给出的前缀模式。匹配 command 还是 path 取决于调用参数。
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

/// 从标准输入读取一行的人工审批闸门。
///
/// 内部保存本会话的信任记录，供 `a` / `p` 使用。它表示"人这次坐在终端前"
/// 的会话状态，属于交互基础设施，生命周期与一次 `cargo run` 相同，
/// 因此不放在领域层或应用层。
pub struct CliApprovalGate {
    /// 本会话内已授予的信任记录，按授予顺序排列。
    ///
    /// 用 `Vec`，不用集合：前缀信任可以重叠，同一工具可能有多个前缀。
    /// 查询是线性扫描，长度等于已授予的信任条数。
    trusted: Mutex<Vec<TrustEntry>>,
    /// 用户按下 `q` 时请求取消运行。
    ///
    /// `None` 表示没有取消通道：闸门只否决当前调用，用户要中止运行得由
    /// 调用方自己处理。用 `Mutex` 是因为通道由 runner 在构造后绑定，而
    /// 闸门本身要放进 `Arc` 共享。
    cancel: Mutex<Option<CancelHandle>>,
}

impl CliApprovalGate {
    /// 构造空的交互式闸门。
    pub fn new() -> Self {
        Self {
            trusted: Mutex::new(Vec::new()),
            cancel: Mutex::new(None),
        }
    }

    /// 从给定 reader 读取答案并给出裁决。
    ///
    /// 独立成函数便于测试：测试不能抢占进程的 stdin，但可以用 `&[u8]`
    /// 提供输入。
    fn decide_from(&self, reader: &mut dyn BufRead, writer: &mut dyn Write, request: &ApprovalRequest) -> Decision {
        // 带 reason 的请求是策略追加的复核，典型场景是越界写入升级。
        // 这类请求必须每次由人过目，因此会话信任整体失效：不查信任，
        // 也不接受 `a` / `p`。否则模型重发同一条越界命令时，会被第一次
        // 授出的信任静默放行。
        //
        // 恢复后重新询问的请求同样不适用会话信任。上一次的提问发生在
        // 上一个进程里且未获答复，不能当作"人已批准"。
        let trustable = request.reason.is_none() && !request.resumed;

        if trustable {
            if let Some(note) = self.trust_note(&request.call) {
                let _ = writeln!(writer, "[审批] {} 已在会话内放行，自动通过", request.call.name);
                let _ = writeln!(writer, "        （{note}）");
                return Ok(());
            }
        }

        // `p` 之后用户可能留空取消，此时回到主问题，因此用循环处理。
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
            if request.resumed {
                // 必须说明"上次已问过但未答复"，否则用户可能以为程序出错。
                let _ = writeln!(
                    writer,
                    "注意:   这是恢复后的重新询问——这个审批请求之前问过，那时没答复。"
                );
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

            // EOF 时 read_line 返回 Ok(0) 或 Err，两者都算否决。
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
                        // 空前缀或没有可匹配参数：不放行也不否决，重新询问。
                        None => continue,
                    }
                }
                // 复核请求不接受信任类答复：提示后重新询问，不放行也不否决。
                Some(Answer::Trust) | Some(Answer::Prefix) => {
                    let _ = writeln!(
                        writer,
                        "[审批] 本次是策略追加的复核，不能存入会话信任；请直接答 [y] 或 [n]。"
                    );
                    continue;
                }
                Some(Answer::Deny) => return Err(ask_reason(reader, writer)),
                Some(Answer::Abort) => {
                    // 中止整个运行，不是只否决这次调用。
                    //
                    // 有取消通道时请求取消，模型不会看到拒绝观察，运行在
                    // 下一个安全点停下。没有通道时只能否决这次调用：把它
                    // 说成"已中止运行"是骗用户，运行还会继续。
                    let cancel = self
                        .cancel
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .clone();
                    if let Some(cancel) = cancel {
                        cancel.cancel();
                        return Err("用户中止了运行".to_owned());
                    }
                    return Err("用户中止了本次调用（未绑定取消通道，运行继续）".to_owned());
                }
                // fail-closed：空行、无法识别或 EOF 都走这里。
                None => return Err("未给出有效答复，按策略视为否决".to_owned()),
            }
        }
    }

    /// 覆盖该调用的信任记录说明；没有则返回 `None`。
    ///
    /// 返回说明字符串：锁守卫是临时值，无法借出引用；调用方只需要
    /// 放行依据的文字。
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

    /// 追问要放行的前缀，返回它落在哪个作用域字段。
    ///
    /// `None` 表示用户取消（留空且没有可接受的建议值），或该调用没有
    /// 可匹配参数。两种情况都应退回重新询问，避免记录覆盖面不全的信任。
    fn ask_prefix(
        &self,
        reader: &mut dyn BufRead,
        writer: &mut dyn Write,
        call: &crate::domain::tool::ToolCall,
    ) -> Option<PolicyScope> {
        // 没有可匹配参数时不要追问：问也得不到可用的信任，只会让用户
        // 输入一个无效字符串。
        if matchable_argument(call).is_none() {
            let _ = writeln!(
                writer,
                "[审批] 该工具没有命令或路径参数，无法按前缀放行；请改选 [a] 或 [y]。"
            );
            return None;
        }

        // 先计算候选建议作为默认值：用户输入其他内容时以输入为准，
        // 直接回车则接受建议。
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
            // EOF 和读取错误仍视为取消，不接受建议。fail-closed 边界
            // 不能为省一次输入而放宽。
            Ok(0) | Err(_) => return None,
            Ok(_) => line.trim().to_owned(),
        };
        // 留空且存在建议值时接受建议；没有建议值时 `?` 返回 None，
        // 仍按回车取消处理。
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

/// 返回该调用中可被前缀信任匹配的参数字段。
///
/// shell 类工具优先匹配命令头；文件类工具没有 `command`，退到 `path`。
/// 两者都没有时返回 `None`（如 git_status、多数 MCP 工具），调用方据此
/// 判定无法按前缀放行。空作用域会匹配一切，等同放行整个工具。
fn matchable_argument(call: &crate::domain::tool::ToolCall) -> Option<&'static str> {
    ["command", "path"]
        .into_iter()
        .find(|key| call.arguments.get(*key).and_then(serde_json::Value::as_str).is_some())
}

/// 把用户输入的前缀写入调用实际携带的参数字段。
///
/// 末尾的 `*` 由这里补上：选择 `p` 即表示这是前缀，用户不需要记住模式
/// 语法。输入已以 `*` 结尾时不重复添加。
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

/// 从一次调用推断保守的前缀信任候选，供 `p` 预填。
///
/// `p` 用于减少输入，但候选不能比用户愿意授予的范围更宽，因此词数必须
/// 足够。裸程序名（如 `cargo`）会把会话信任扩大到该程序的每个子命令，
/// 包括 `cargo publish`，这种情况不给建议。
///
/// - `command`：取前两个词。`cargo test --lib` → `cargo test`，覆盖
///   `cargo test --doc` 一族，但不覆盖 `cargo build`、`cargo publish`。
///   本次调用特有的参数不进入候选，否则该信任无法匹配后续调用。
///   带包装前缀的命令（`sudo cargo test`）取到 `sudo cargo`：范围仍以该
///   程序为界，不会扩到 `sudo` 的其他子命令，但会覆盖 `sudo cargo publish`。
///   候选只是**起点**：它原样显示在提示框中，回车前用户能看到授予范围，
///   过宽时可自行输入更窄的前缀。
/// - `path`：取所在目录。`src/domain/policy.rs` → `src/domain/`。目录边界
///   是天然的授权范围。不取"前两段路径"，因为前缀匹配不认词边界，
///   `src/domain*` 会同时匹配 `src/domain.rs`。
///
/// 三种情况不给建议：少于两个词的命令、根目录下的文件、带 shell 链接符
/// 的命令。第一种的命令头会宽到整个程序；第二种没有目录边界；第三种是
/// 因为信任只覆盖命令头（`prefix_matches` 不让 `*` 匹配 `&&` 之后的部分），
/// 而用户面前的调用是一条链，只覆盖前半截的前缀会让人误以为整条链都被
/// 记住。
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

/// 信任记录的一行摘要，打印在"自动通过"旁。
///
/// 用户需要知道本次放行的依据。前缀信任的放行范围比眼前这条调用宽。
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

    /// 绑定取消通道，之后用户按 `q` 能真正中止整个运行。
    fn bind_cancel(&self, cancel: CancelHandle) {
        *self.cancel.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(cancel);
    }
}

/// 用户对审批的五种答复。
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
    ///
    /// 绑定取消通道时请求取消，运行在下一个安全点停下；未绑定时只否决
    /// 当前调用，提示语会如实说明这一点。
    Abort,
}

/// 解析一行输入。`None` 表示无法识别，调用方按 fail-closed 处理。
///
/// 接受大小写差异与首尾空白，不接受 `yes` / `no` 全词。多输入一个字符
/// 可能误批破坏性操作，因此要求精确答复。
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
/// 原因会作为拒绝观察回填给模型，空原因也必须有默认值。模型只看到
/// "被拒绝"而不知道原因时，往往会重试同一个调用。
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

/// 把风险注解渲染成单行可读摘要。
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
            resumed: false,
        }
    }

    /// 策略追加的复核请求：策略已放行，但要求人再确认。
    fn escalated_request(command: &str) -> ApprovalRequest {
        ApprovalRequest {
            reason: Some(format!(
                "策略已放行，但疑似越界写入：命令把输出重定向到 workspace 外的 `{command}`"
            )),
            ..request_with("run_command", json!({"command": command}))
        }
    }

    /// 恢复后重新询问的审批请求。
    fn resumed_request(name: &str) -> ApprovalRequest {
        ApprovalRequest {
            resumed: true,
            ..request_with(name, json!({"path": "src/main.rs"}))
        }
    }


    /// 用内存 reader/writer 执行一次审批，返回裁决。
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
        // 用户直接回车，未输入内容。
        let decision = decide(&gate, "\n", &request("write_file"));

        let reason = decision.expect_err("空输入必须否决");
        assert!(reason.contains("视为否决"), "原因应说明是 fail-closed: {reason}");
    }

    #[test]
    fn unrecognized_input_fails_closed() {
        let gate = CliApprovalGate::new();
        // "yes" 不是合法答复，见 parse_answer 的注释。
        let decision = decide(&gate, "yes\n", &request("write_file"));

        assert!(decision.is_err(), "无法识别的输入必须否决");
    }

    #[test]
    fn eof_fails_closed() {
        let gate = CliApprovalGate::new();
        // 空输入流表示 stdin 已关闭。
        let decision = decide(&gate, "", &request("write_file"));

        let reason = decision.expect_err("EOF 必须否决");
        assert!(reason.contains("视为否决"), "原因应说明是 fail-closed: {reason}");
    }

    /// `q` 在绑定了取消通道时真正中止运行。
    ///
    /// 提示语写的是"中止运行"，行为必须对得上：只否决当前调用却让运行
    /// 继续，是骗用户。绑定通道后按 `q` 会请求取消，运行在下一个安全点
    /// 停下。
    #[test]
    fn abort_requests_cancellation_when_a_cancel_channel_is_bound() {
        let gate = CliApprovalGate::new();
        let cancel = CancelHandle::default();
        gate.bind_cancel(cancel.clone());

        let decision = decide(&gate, "q\n", &request("write_file"));

        let reason = decision.expect_err("q 应否决");
        assert!(reason.contains("中止"), "原因应说明是用户主动中止: {reason}");
        assert!(
            cancel.is_cancelled(),
            "绑定取消通道后，q 必须真正请求中止运行"
        );
    }

    /// 没有取消通道时，`q` 只能否决当前调用，并如实说明运行会继续。
    #[test]
    fn abort_without_a_cancel_channel_denies_only_this_call() {
        let gate = CliApprovalGate::new();
        let decision = decide(&gate, "q\n", &request("write_file"));

        let reason = decision.expect_err("q 应否决");
        assert!(reason.contains("中止"), "原因应说明是用户主动中止: {reason}");
        assert!(
            reason.contains("运行继续"),
            "无法中止运行时要如实说明: {reason}"
        );
    }

    /// `a` 记录的是**这一次调用**，不是整个工具。
    ///
    /// 按工具名记录会让"允许 write_file"放行之后所有 write_file，
    /// 包括写 `Cargo.toml`、`.env` 的调用。参数不同，后果不同。
    #[test]
    fn trust_a_remembers_the_exact_call_not_the_tool() {
        let gate = CliApprovalGate::new();
        let req = request("write_file");

        // 第一次回答 a：本会话放行完全相同的调用。
        assert!(decide(&gate, "a\n", &req).is_ok());
        assert_eq!(gate.trusted_entries(), 1);

        // 参数相同的后续调用不再需要输入。reader 为空（EOF），若未记住
        // 则会被否决。
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

    /// `p` 之后留空：不放行也不记录信任，回到主审批问题。
    ///
    /// 用户误选 `p` 后应能反悔，不应被迫在前缀输入框里编一个字符串。
    ///
    /// 这里用根目录下的文件，没有候选建议，留空按取消处理。留空的含义
    /// 取决于是否存在建议值：有建议时留空表示接受（见
    /// `accepting_the_suggestion_with_enter_trusts_the_suggested_prefix`），
    /// 该路径的提示框已原样显示授予范围。
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

    /// 没有 command 或 path 参数的工具（git_status、MCP 工具）无法按
    /// 前缀放行：`p` 退回重新询问，不能退化成放行整个工具。
    #[test]
    fn prefix_trust_is_unavailable_without_a_matchable_argument() {
        let gate = CliApprovalGate::new();
        let req = request_with("git_status", json!({}));

        // p → 没有可匹配参数，退回重新询问（不消费下一行）；随后答 y 放行本次。
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

    /// 前缀输入框要说明后果：放行的是一类调用，不是本次调用。
    ///
    /// 使用裸命令 `ls`，没有候选建议，因此仍会出现"回车取消"提示。
    /// 有建议时的提示由 `prefix_prompt_shows_the_suggested_candidate` 覆盖。
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

    /// 带 reason 的复核请求不受会话信任影响：`a` / `p` 均不能放行。
    ///
    /// 升级要求每次越界写入都由人过目。若信任对复核有效，模型重发同一条
    /// 命令就会被静默放行，升级失效。
    #[test]
    fn escalated_request_ignores_session_trust() {
        let gate = CliApprovalGate::new();
        let req = escalated_request(r"echo hi > C:\Windows\temp\x");

        // 先授予一条前缀信任，确认它在会话内生效。
        let ordinary = request_with("run_command", json!({"command": "cargo test"}));
        assert!(decide(&gate, "p\ncargo\n", &ordinary).is_ok());
        assert_eq!(gate.trusted_entries(), 1);
        // 信任确实有效：同前缀的普通调用不再询问。
        let same_prefix = request_with("run_command", json!({"command": "cargo build"}));
        assert!(decide(&gate, "", &same_prefix).is_ok());

        // 复核请求仍会询问；`a` 不被接受，既不记信任也不放行。
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

    /// 重新询问时，提示必须说明"之前问过且未答复"。
    ///
    /// 否则用户会以为程序出错，可能在未看清的情况下误按 y。
    #[test]
    fn a_resumed_request_shows_that_it_was_asked_before() {
        let gate = CliApprovalGate::new();
        let mut reader = Cursor::new(b"y\n".as_slice());
        let mut sink = Vec::new();

        let _ = gate.decide_from(&mut reader, &mut sink, &resumed_request("write_file"));
        let printed = String::from_utf8(sink).expect("输出是 UTF-8");

        assert!(printed.contains("之前问过"), "应说明之前问过: {printed}");
        assert!(printed.contains("没答复"), "应说明那次没答复: {printed}");
    }

    /// 重新询问不适用会话信任：上次未答复不能视为本次已批准。
    ///
    /// 信任记录表示"人点过 yes"。恢复后的询问是另一次决定，用户没有
    /// 看到提示（它在上一个进程中已关闭）。用旧信任放行新请求，会把
    /// "未批准"当成"已批准"。
    #[test]
    fn a_resumed_request_ignores_session_trust() {
        let gate = CliApprovalGate::new();
        // 先授予一条精确信任：同工具同参数在普通请求中会自动放行。
        assert!(decide(&gate, "a\n", &request("write_file")).is_ok());
        assert_eq!(gate.trusted_entries(), 1);

        // 同样参数，但这次是重新询问：必须照问，读不到输入就否决。
        let decision = decide(&gate, "", &resumed_request("write_file"));
        assert!(
            decision.is_err(),
            "重新询问不得被会话信任免掉"
        );
    }

    #[test]
    fn risk_summary_handles_a_clean_annotation() {
        let clean = ToolAnnotations::new(false, false, true, false);
        assert_eq!(risk_summary(&clean), "仅写操作，无已知风险标记");
    }

    // ── 前缀候选建议 ──────────────────────────────────────────────────

    fn call_with(name: &str, arguments: serde_json::Value) -> ToolCall {
        ToolCall {
            id: CallId::new("call_1"),
            name: name.to_owned(),
            arguments,
        }
    }

    /// 命令候选取前两个词：窄到不覆盖其他子命令，宽到覆盖同一命令的
    /// 参数变体。
    #[test]
    fn suggested_prefix_takes_the_first_two_words_of_a_command() {
        assert_eq!(
            suggested_prefix(&call_with("run_command", json!({"command": "cargo test --lib foo"}))),
            Some("cargo test".to_owned()),
            "参数不该进候选：候选是给后续调用用的，带上这次特有的参数就永远匹配不上"
        );
        // 恰好两个词时，候选就是整条命令，范围不变。
        assert_eq!(
            suggested_prefix(&call_with("run_command", json!({"command": "cargo test"}))),
            Some("cargo test".to_owned())
        );
    }

    /// 裸程序名不给建议：单词命令头会把整个程序交给会话信任，
    /// `cargo` 会连 `cargo publish` 一起放行。
    #[test]
    fn a_bare_program_name_gets_no_suggestion() {
        assert_eq!(
            suggested_prefix(&call_with("run_command", json!({"command": "ls"}))),
            None
        );
    }

    /// 路径候选取所在目录：目录边界是天然的授权范围。
    ///
    /// 不取"前两段路径"，因为前缀匹配不认词边界，`src/domain*` 会同时
    /// 匹配 `src/domain.rs`；结尾带 `/` 才能精确落在目录内。
    #[test]
    fn suggested_prefix_for_a_path_is_its_directory() {
        assert_eq!(
            suggested_prefix(&call_with("write_file", json!({"path": "src/domain/policy.rs"}))),
            Some("src/domain/".to_owned())
        );
    }

    /// workspace 根目录下的文件没有目录可授予：建议会变成"放行所有写入"，
    /// 超出用户表达的范围。
    #[test]
    fn a_path_at_the_workspace_root_gets_no_suggestion() {
        assert_eq!(
            suggested_prefix(&call_with("write_file", json!({"path": "main.rs"}))),
            None
        );
    }

    /// 带 shell 链接符的命令不给建议。
    ///
    /// 信任只覆盖命令头（`prefix_matches` 不让 `*` 匹配 `&&` 之后的部分），
    /// 而用户面前的调用是一条链。只覆盖前半截的前缀会让人误以为整条链
    /// 都被记住，实际下次仍会询问。
    #[test]
    fn chained_commands_get_no_suggestion() {
        assert_eq!(
            suggested_prefix(&call_with("run_command", json!({"command": "cargo test && git push"}))),
            None
        );
    }

    /// 没有 command / path 参数的工具（git_status、多数 MCP 工具）没有
    /// 可授予的范围，结论与"无法按前缀放行"一致。
    #[test]
    fn a_tool_without_matchable_arguments_gets_no_suggestion() {
        assert_eq!(suggested_prefix(&call_with("git_status", json!({}))), None);
    }

    /// 建议值必须原样显示在输入框里。
    ///
    /// 用户需要看到授予范围。否则回车没有信息量：用户以为批准了眼前一条，
    /// 实际放开的是一类调用。
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

    /// 选 `p` 后直接回车表示接受建议值，无需手动输入。
    #[test]
    fn accepting_the_suggestion_with_enter_trusts_the_suggested_prefix() {
        let gate = CliApprovalGate::new();
        let req = request_with("run_command", json!({"command": "cargo test --lib"}));

        // "p" 之后紧跟一个空行：接受建议。
        assert!(decide(&gate, "p\n\n", &req).is_ok());

        // 建议范围生效：同命令头的另一条调用不再询问。
        let same_head = request_with("run_command", json!({"command": "cargo test --doc"}));
        assert!(decide(&gate, "", &same_head).is_ok(), "cargo test 一族应已被信任");
        // 换子命令仍会询问：建议范围没有覆盖整个 cargo。
        let other = request_with("run_command", json!({"command": "cargo build"}));
        assert!(decide(&gate, "", &other).is_err(), "cargo build 不在建议范围内");
    }

    /// 用户可以输入自己的前缀：建议只减少输入，不代替用户决定。
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

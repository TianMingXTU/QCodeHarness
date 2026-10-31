//! 审批基础设施：从 stdin 读一行的人工审批。
//!
//! 这是生产默认闸门。与 `auto` 的区别不只是"问一句"——它引入了
//! 本会话内的信任记忆（`a` 选项），并且把"读不到答案"一律当作否决。
//!
//! fail-closed 是这里唯一的设计原则：空输入、无法识别的输入、stdin
//! 关闭（EOF），全都走否决分支。宁可让模型多撞几次墙，也不能在
//! "没人看着"的时候把 destructive 工具放过去。

use std::collections::HashSet;
use std::io::{BufRead, Write};
use std::sync::Mutex;

use crate::application::port::{ApprovalGate, ApprovalRequest, BoxFuture, Decision};

/// 从标准输入读一行的人工审批闸门。
///
/// 内部持有一份"本会话已批准的工具名"集合，供 `a` 选项使用。
/// 放这里而不是放进领域层或应用层：它是"人这次坐在终端前"的会话状态，
/// 属于交互基础设施，且它随闸门实例生灭——正好和一次 `cargo run` 同寿。
pub struct CliApprovalGate {
    /// 本会话内已被 `a` 放行的工具名。
    ///
    /// 用 `HashSet` 而不是 `Vec`：工具名数量少但会被反复查询，
    /// 而且"已批准"是一个集合语义，用 Vec 会诱导出线性查找。
    trusted: Mutex<HashSet<String>>,
}

impl CliApprovalGate {
    /// 构造一个空的交互式闸门。
    pub fn new() -> Self {
        Self {
            trusted: Mutex::new(HashSet::new()),
        }
    }

    /// 从给定 reader 读一行答案，据此给出裁决。
    ///
    /// 独立成函数是为了测试：测试不能抢进程的 stdin，
    /// 但可以用 `&[u8]` 冒充一行输入来喂进去。
    ///
    /// 返回值语义与 [`Decision`] 一致，但额外带上"是否记住这个工具"。
    fn decide_from(
        &self,
        reader: &mut dyn BufRead,
        writer: &mut dyn Write,
        request: &ApprovalRequest,
    ) -> (Decision, bool) {
        // 已信任的工具不再打扰用户。
        if self.is_trusted(&request.call.name) {
            let _ = writeln!(writer, "[审批] {} 已在会话内放行，自动通过", request.call.name);
            return (Ok(()), false);
        }

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
        let _ = write!(writer, "批准执行？[y] 允许  [n] 否决  [a] 本会话允许此工具  [q] 中止运行: ");
        let _ = writer.flush();

        // 读不到行（EOF）时 line 为 Ok(0) 或 Err，两者都算否决。
        let mut line = String::new();
        let read = reader.read_line(&mut line);
        let answer = match read {
            Ok(0) | Err(_) => None,
            Ok(_) => parse_answer(line.trim()),
        };

        match answer {
            Some(Answer::Approve) => (Ok(()), false),
            Some(Answer::Trust) => {
                self.trust(&request.call.name);
                (Ok(()), false)
            }
            Some(Answer::Deny) => {
                let reason = ask_reason(reader, writer);
                (Err(reason), false)
            }
            Some(Answer::Abort) => (
                Err("用户中止了本次运行".to_owned()),
                false,
            ),
            // fail-closed：空行、无法识别、EOF 全部落到这里。
            None => (
                Err("未给出有效答复，按策略视为否决".to_owned()),
                false,
            ),
        }
    }

    /// 该工具名是否已被 `a` 放行。
    fn is_trusted(&self, tool: &str) -> bool {
        self.trusted
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains(tool)
    }

    /// 把工具名加入本会话信任集合。
    fn trust(&self, tool: &str) {
        self.trusted
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(tool.to_owned());
    }

    /// 测试用：当前已信任的工具名。
    #[cfg(test)]
    fn trusted_tools(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .trusted
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .cloned()
            .collect();
        names.sort();
        names
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
            // 第二个返回值是"是否要记住"，由 decide_from 内部直接处理，
            // 这里只取裁决本身。
            let (decision, _) = self.decide_from(&mut reader, &mut writer, request);
            decision
        })
    }
}

/// 用户对一次审批的四种答复。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Answer {
    /// `y`：仅放行这一次。
    Approve,
    /// `a`：本会话内放行同名工具。
    Trust,
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
        ApprovalRequest {
            turn: 1,
            call: ToolCall {
                id: CallId::new("call_1"),
                name: name.to_owned(),
                arguments: json!({"path": "src/main.rs"}),
            },
            annotations: ToolAnnotations::new(false, true, false, false),
        }
    }

    /// 用内存 reader/writer 跑一次审批，拿到裁决。
    fn decide(gate: &CliApprovalGate, input: &str, req: &ApprovalRequest) -> Decision {
        let mut reader = Cursor::new(input.as_bytes());
        let mut sink = Vec::new();
        let (decision, _) = gate.decide_from(&mut reader, &mut sink, req);
        decision
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

    #[test]
    fn trust_remembers_the_tool_for_later_calls() {
        let gate = CliApprovalGate::new();
        let req = request("write_file");

        // 第一次回答 a：本会话放行。
        assert!(decide(&gate, "a\n", &req).is_ok());
        assert_eq!(gate.trusted_tools(), vec!["write_file".to_owned()]);

        // 第二次同一个工具不再需要输入——reader 是空的（EOF），
        // 如果它没记住，就会因为读不到答案而否决。
        assert!(decide(&gate, "", &req).is_ok(), "已信任的工具应自动放行");

        // 换一个工具仍然要问。
        assert!(decide(&gate, "", &request("delete_file")).is_err());
    }

    #[test]
    fn plain_approval_does_not_trust_the_tool() {
        let gate = CliApprovalGate::new();
        assert!(decide(&gate, "y\n", &request("write_file")).is_ok());

        // 下一次仍需审批，且读不到输入时否决。
        assert!(decide(&gate, "", &request("write_file")).is_err());
        assert!(gate.trusted_tools().is_empty(), "y 不应写入信任集合");
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
    }

    #[test]
    fn risk_summary_handles_a_clean_annotation() {
        let clean = ToolAnnotations::new(false, false, true, false);
        assert_eq!(risk_summary(&clean), "仅写操作，无已知风险标记");
    }
}

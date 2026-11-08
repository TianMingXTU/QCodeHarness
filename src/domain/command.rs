//! shell 命令的结构化词法模型，纯数据，不做 shell 求值。
//!
//! 只描述命令文本包含哪些段、词元、重定向与 flag 值，供 `domain::sandbox`
//! 的两道边界防线共用。它不是 shell 解析器，也不提供隔离：不展开
//! `$OUT`/`%OUT%`，不执行反引号或 `$(…)`，不解码编码路径，不访问文件
//! 系统，不解析符号链接与 junction 的物理目标。引号只影响分词，不代表
//! 运行时求值结果。
//!
//! 词法处理偏保守：宁可把可疑文本纳入检查，也不让真实的路径词元从模型中
//! 消失。重定向目标因此同时保留在有序词元流中。"任何词元提到 workspace 外
//! 路径即触发"是收紧后的口径，结构化处理不应放松它。

/// 段与段之间的链接符。它们是词法边界，不是普通词元。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellOperator {
    /// `&&`
    And,
    /// `||`
    Or,
    /// `;`
    Sequence,
    /// `|`
    Pipe,
    /// 单个 `&`，后台执行
    Background,
    /// 换行
    Newline,
}

/// 重定向操作。fd 前缀（`1>` / `2>`）并入操作本身，不另造词元。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RedirectOp {
    /// `>` / `1>` / `2>`
    Output,
    /// `>>` / `1>>` / `2>>`
    Append,
    /// `<` / `<<`。输入不是写操作，裁决时不套用输出重定向的理由。
    Input,
    /// `&>`：stdout 与 stderr 一起写。
    OutputAndError,
}

/// 一个词元。`text` 已剥掉外层引号，但保留引号内完整内容（含空格）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandWord {
    pub text: String,
    /// 是否曾被引号包住（整词或其中一段）。
    pub quoted: bool,
    /// 第一个 `=` 之后的值；`--flag=<值>` 的判据只看这里，不看整个词元。
    pub flag_value: Option<String>,
    /// 来自引号内复合命令的一层补查，不是外层的原生词元。
    pub embedded: bool,
}

impl CommandWord {
    fn new(text: String, quoted: bool, embedded: bool) -> Self {
        let flag_value = text.split_once('=').map(|(_, value)| value.to_owned());
        Self {
            text,
            quoted,
            flag_value,
            embedded,
        }
    }

    /// 路径判据检查的字面值。存在 `=` 时只取等号后的值，与逐词元检查的
    /// 口径一致。
    pub fn path_candidate(&self) -> &str {
        self.flag_value.as_deref().unwrap_or(&self.text)
    }
}

/// 一处重定向及其目标词元。目标同时留在 [`CommandSegment::words`] 里。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Redirect {
    pub op: RedirectOp,
    pub target: String,
}

/// 一段命令：一个程序、它的参数词元、以及本段收齐的全部重定向。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSegment {
    /// 常规命令的首段为 `None`；以链接符开头的命令（`&& cargo test`）首段
    /// 也带该链接符；后续段记录把它与前面分开的链接符。
    pub operator_before: Option<ShellOperator>,
    /// 有序词元流，含程序、参数、重定向目标与嵌套补查词元。
    pub words: Vec<CommandWord>,
    /// 本段全部重定向，逐段裁决时逐个查目标而不只看第一个。
    pub redirects: Vec<Redirect>,
}

impl CommandSegment {
    /// 本段程序名（原生词元里的第一个；嵌套补查词元不参与）。
    pub fn program(&self) -> Option<&str> {
        self.words
            .iter()
            .find(|word| !word.embedded)
            .map(|word| word.text.as_str())
    }

    /// 程序之后的参数词元（含重定向目标，与"任何词元都查"的口径一致）。
    pub fn arguments(&self) -> impl Iterator<Item = &CommandWord> {
        self.words.iter().filter(|word| !word.embedded).skip(1)
    }

    /// 本段所有 `--flag=<值>` 形态的值，供结构化断言与裁决复用。
    pub fn flag_values(&self) -> impl Iterator<Item = &str> {
        self.words
            .iter()
            .filter_map(|word| word.flag_value.as_deref())
    }
}

/// 整条命令的结构化模型。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedCommand {
    pub segments: Vec<CommandSegment>,
}

/// 把命令文本切成结构化模型。纯词法，无 IO、无展开、无文件系统解析。
pub fn parse_command(command: &str) -> ParsedCommand {
    parse_at_depth(command, 0)
}

/// 词法扫描的中间形态：尚未做引号内复合命令的嵌套补查。
struct DraftWord {
    text: String,
    quoted: bool,
}

struct DraftSegment {
    operator_before: Option<ShellOperator>,
    words: Vec<DraftWord>,
    redirects: Vec<Redirect>,
}

/// 只在外层（depth 0）做一次补查。递归展开引号内的引号会使模型无限增长；
/// 需要覆盖的场景是 `cmd /C "echo x > D:\outside\x"`。
fn parse_at_depth(command: &str, depth: u8) -> ParsedCommand {
    let segments = lex(command)
        .into_iter()
        .map(|draft| {
            let mut words: Vec<CommandWord> = draft
                .words
                .iter()
                .map(|word| CommandWord::new(word.text.clone(), word.quoted, false))
                .collect();
            let mut redirects = draft.redirects;
            // 只对"shell 启动器加一段带引号的命令文本"补查一层。
            // `cmd /C "echo x > D:\outside\x"` 是真实的绕过形式，而
            // `git commit -m "提到 C:\Users\x 的说明"` 是普通文本。
            // 无差别递归会把提交说明中的路径误判为越界。
            if depth == 0
                && draft
                    .words
                    .first()
                    .is_some_and(|word| is_shell_launcher(&word.text))
            {
                for word in &draft.words {
                    if word.quoted && looks_like_compound(&word.text) {
                        let nested = parse_at_depth(&word.text, depth + 1);
                        for nested_segment in nested.segments {
                            for nested_word in nested_segment.words {
                                words.push(CommandWord {
                                    embedded: true,
                                    ..nested_word
                                });
                            }
                            redirects.extend(nested_segment.redirects);
                        }
                    }
                }
            }
            CommandSegment {
                operator_before: draft.operator_before,
                words,
                redirects,
            }
        })
        .collect();
    ParsedCommand { segments }
}

/// 判断带引号的词是否含空白或 shell 操作符，是则按复合命令再切一层词元。
///
/// 这只是一次补查。原词仍作为完整候选先被裁决，因此带空格的完整路径不会
/// 在理由中被拆开（由 `quoted_redirect_reason_keeps_the_complete_path`
/// 测试锁定）。
fn looks_like_compound(text: &str) -> bool {
    text.chars()
        .any(|ch| ch.is_whitespace() || matches!(ch, '&' | '|' | ';' | '>' | '<' | '\n'))
}

/// 会把后续整段文本交给 shell 解释的程序。只有它们的引号内容按复合命令
/// 补查；其他工具的参数是普通文本。
fn is_shell_launcher(program: &str) -> bool {
    let name = program
        .rsplit(['\\', '/'])
        .next()
        .unwrap_or(program)
        .to_ascii_lowercase();
    let stem = name.trim_end_matches(".exe");
    matches!(stem, "cmd" | "sh" | "bash" | "zsh" | "powershell" | "pwsh")
}

/// 单遍词法扫描。反斜杠与反引号按普通字符处理。引入转义处理会放松
/// Windows 路径的现有判据。
fn lex(command: &str) -> Vec<DraftSegment> {
    let mut segments: Vec<DraftSegment> = Vec::new();
    let mut operator_before: Option<ShellOperator> = None;
    let mut words: Vec<DraftWord> = Vec::new();
    let mut redirects: Vec<Redirect> = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut quote: Option<char> = None;
    let mut pending: Option<RedirectOp> = None;
    let mut chars = command.chars().peekable();

    while let Some(ch) = chars.next() {
        if let Some(closing) = quote {
            if ch == closing {
                quote = None;
            } else {
                current.push(ch);
            }
            continue;
        }
        match ch {
            '"' | '\'' => {
                quoted = true;
                quote = Some(ch);
            }
            '\n' => {
                flush_word(
                    &mut words,
                    &mut redirects,
                    &mut current,
                    &mut quoted,
                    &mut pending,
                );
                end_segment(
                    &mut segments,
                    &mut operator_before,
                    &mut words,
                    &mut redirects,
                    &mut pending,
                    ShellOperator::Newline,
                );
            }
            c if c.is_whitespace() => flush_word(
                &mut words,
                &mut redirects,
                &mut current,
                &mut quoted,
                &mut pending,
            ),
            '(' | ')' => {
                flush_word(
                    &mut words,
                    &mut redirects,
                    &mut current,
                    &mut quoted,
                    &mut pending,
                );
                // 旧 tokenize 会把括号单独成词；保留它，判据侧统一跳过。
                words.push(DraftWord {
                    text: ch.to_string(),
                    quoted: false,
                });
            }
            '&' => match chars.peek().copied() {
                Some('&') => {
                    chars.next();
                    flush_word(
                        &mut words,
                        &mut redirects,
                        &mut current,
                        &mut quoted,
                        &mut pending,
                    );
                    end_segment(
                        &mut segments,
                        &mut operator_before,
                        &mut words,
                        &mut redirects,
                        &mut pending,
                        ShellOperator::And,
                    );
                }
                Some('>') => {
                    chars.next();
                    let op = if chars.peek() == Some(&'>') {
                        chars.next();
                        RedirectOp::Append
                    } else {
                        RedirectOp::OutputAndError
                    };
                    drop_fd_prefix(&mut current);
                    flush_word(
                        &mut words,
                        &mut redirects,
                        &mut current,
                        &mut quoted,
                        &mut pending,
                    );
                    pending = Some(op);
                }
                // `2>&1` 中的 `&` 表示 fd 复制，仅在以下条件成立时按此
                // 处理：有重定向等待目标（pending）、当前词元为空、且 `&`
                // 后紧跟数字。数字并入目标词元（`&1`）。否则 `&` 是后台
                // 链接符，必须结束当前目标并分段。例如
                // `echo x > local.txt&D:\outside\run.cmd` 中 `&` 后是路径，
                // 并入 current 会让真实越界目标从模型中消失。
                Some(digit) if digit.is_ascii_digit()
                    && pending.is_some() && current.is_empty() =>
                {
                    // 只落下 `&`：数字留给下一轮循环并进 current。
                    current.push('&');
                }
                _ => {
                    flush_word(
                        &mut words,
                        &mut redirects,
                        &mut current,
                        &mut quoted,
                        &mut pending,
                    );
                    end_segment(
                        &mut segments,
                        &mut operator_before,
                        &mut words,
                        &mut redirects,
                        &mut pending,
                        ShellOperator::Background,
                    );
                }
            },
            '|' => {
                flush_word(
                    &mut words,
                    &mut redirects,
                    &mut current,
                    &mut quoted,
                    &mut pending,
                );
                let op = if chars.peek() == Some(&'|') {
                    chars.next();
                    ShellOperator::Or
                } else {
                    ShellOperator::Pipe
                };
                end_segment(
                    &mut segments,
                    &mut operator_before,
                    &mut words,
                    &mut redirects,
                    &mut pending,
                    op,
                );
            }
            ';' => {
                flush_word(
                    &mut words,
                    &mut redirects,
                    &mut current,
                    &mut quoted,
                    &mut pending,
                );
                end_segment(
                    &mut segments,
                    &mut operator_before,
                    &mut words,
                    &mut redirects,
                    &mut pending,
                    ShellOperator::Sequence,
                );
            }
            '>' | '<' => {
                drop_fd_prefix(&mut current);
                flush_word(
                    &mut words,
                    &mut redirects,
                    &mut current,
                    &mut quoted,
                    &mut pending,
                );
                if ch == '>' {
                    let op = if chars.peek() == Some(&'>') {
                        chars.next();
                        RedirectOp::Append
                    } else {
                        RedirectOp::Output
                    };
                    pending = Some(op);
                } else {
                    if chars.peek() == Some(&'<') {
                        chars.next();
                    }
                    pending = Some(RedirectOp::Input);
                }
            }
            other => current.push(other),
        }
    }
    flush_word(
        &mut words,
        &mut redirects,
        &mut current,
        &mut quoted,
        &mut pending,
    );
    // 命令以链接符结尾时不保留空尾段。否则单段命令的理由会多出
    // "（第 2 段…）"，违反单段不带段号的约定。
    if !words.is_empty() || !redirects.is_empty() || segments.is_empty() {
        segments.push(DraftSegment {
            operator_before,
            words,
            redirects,
        });
    }
    segments
}

/// 结束当前词元；若前面挂着重定向操作，这个词元就是它的目标。
fn flush_word(
    words: &mut Vec<DraftWord>,
    redirects: &mut Vec<Redirect>,
    current: &mut String,
    quoted: &mut bool,
    pending: &mut Option<RedirectOp>,
) {
    let was_quoted = std::mem::replace(quoted, false);
    if current.is_empty() {
        // 空引号目标没有路径可判，但仍要保留这次重定向，否则 pending 会被
        // 下一个操作覆盖，重定向目标列表将不完整。只处理显式空引号词；
        // 重定向后的普通空格仍需等待真正的目标词元。
        if was_quoted {
            if let Some(op) = pending.take() {
                redirects.push(Redirect {
                    op,
                    target: String::new(),
                });
            }
        }
        return;
    }
    let text = std::mem::take(current);
    if let Some(op) = pending.take() {
        redirects.push(Redirect {
            op,
            target: text.clone(),
        });
    }
    words.push(DraftWord {
        text,
        quoted: was_quoted,
    });
}

/// `1>` / `2>` 的 fd 前缀属于重定向操作，不是命令参数。
fn drop_fd_prefix(current: &mut String) {
    if matches!(current.as_str(), "1" | "2") {
        current.clear();
    }
}

/// 收束当前段，并记下下一段由哪个链接符引出。
fn end_segment(
    segments: &mut Vec<DraftSegment>,
    operator_before: &mut Option<ShellOperator>,
    words: &mut Vec<DraftWord>,
    redirects: &mut Vec<Redirect>,
    pending: &mut Option<RedirectOp>,
    operator: ShellOperator,
) {
    // 开头是链接符时不保留空的前导段。否则只有一条命令也会被算作第 2 段，
    // 多出段号。与尾随链接符的处理对称。
    if !words.is_empty() || !redirects.is_empty() || !segments.is_empty() {
        segments.push(DraftSegment {
            operator_before: *operator_before,
            words: std::mem::take(words),
            redirects: std::mem::take(redirects),
        });
    }
    *operator_before = Some(operator);
    *pending = None;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 取一段的全部词元文本（含嵌套补查词元），供表驱动断言。
    fn words(parsed: &ParsedCommand, segment: usize) -> Vec<&str> {
        parsed.segments[segment]
            .words
            .iter()
            .map(|word| word.text.as_str())
            .collect()
    }

    fn single(command: &str) -> ParsedCommand {
        let parsed = parse_command(command);
        assert_eq!(
            parsed.segments.len(),
            1,
            "`{command}` 应只有一段: {parsed:#?}"
        );
        parsed
    }

    /// 程序 / 参数 / 链接符 / flag 值的基本形状。
    #[test]
    fn a_plain_command_has_a_program_arguments_and_no_redirects() {
        let parsed = single("cargo test --release");
        let segment = &parsed.segments[0];
        assert_eq!(segment.operator_before, None);
        assert_eq!(segment.program(), Some("cargo"));
        assert_eq!(
            segment
                .arguments()
                .map(|word| word.text.as_str())
                .collect::<Vec<_>>(),
            vec!["test", "--release"]
        );
        assert!(segment.redirects.is_empty());
        assert!(segment.flag_values().next().is_none());
    }

    /// `=` 之后的值单独可见，且判据只看值不看整个词元。
    #[test]
    fn flag_values_are_exposed_separately_from_the_flag_name() {
        let parsed = single(r"git diff --output=C:\Windows\temp\x --stat");
        let segment = &parsed.segments[0];
        assert_eq!(
            segment.flag_values().collect::<Vec<_>>(),
            vec![r"C:\Windows\temp\x"]
        );
        let flag = &segment.words[2];
        assert_eq!(flag.text, r"--output=C:\Windows\temp\x");
        assert_eq!(flag.path_candidate(), r"C:\Windows\temp\x");
    }

    /// 引号内整段路径不空格切碎；相邻引号片与普通片合成一个词元。
    #[test]
    fn quoted_paths_stay_one_word_and_adjacent_fragments_concatenate() {
        let parsed = single(r#"type "D:\repo space\out.txt""#);
        assert_eq!(words(&parsed, 0)[1], r"D:\repo space\out.txt");
        assert!(parsed.segments[0].words[1].quoted);

        let parsed = single(r#"echo "a b"c > out.txt"#);
        assert_eq!(words(&parsed, 0)[1], "a bc");
    }

    /// 引号内的链接符不分段；普通程序的引号文本不做复合命令补查，
    /// shell 启动器（cmd /C "…"）才补一层词元与重定向。
    #[test]
    fn operators_inside_quotes_do_not_split_segments() {
        let parsed = single(r#"echo "a && b" > out.txt"#);
        let segment = &parsed.segments[0];
        assert_eq!(segment.words[1].text, "a && b");
        assert_eq!(segment.redirects.len(), 1);
        assert!(
            !segment.words.iter().any(|word| word.embedded),
            "echo 的参数不是交给 shell 再解释的命令文本"
        );

        // 提交说明里提到绝对路径是普通文本：补查不得把它变成越界拒绝。
        let parsed = single(r#"git commit -m "fix C:\Users\x\note.txt""#);
        assert_eq!(parsed.segments[0].words[3].text, r"fix C:\Users\x\note.txt");
        assert!(!parsed.segments[0].words.iter().any(|word| word.embedded));
    }

    /// 所有重定向形态都要有目标；目标同时留在词元流里。
    #[test]
    fn every_redirect_form_is_collected_with_its_target() {
        let cases: &[(&str, RedirectOp, &str)] = &[
            (r"echo x > out.txt", RedirectOp::Output, "out.txt"),
            (r"echo x >> out.txt", RedirectOp::Append, "out.txt"),
            (r"echo x 1> out.txt", RedirectOp::Output, "out.txt"),
            (r"echo x 2> err.txt", RedirectOp::Output, "err.txt"),
            (r"echo x 1>> out.txt", RedirectOp::Append, "out.txt"),
            (r"echo x 2>> err.txt", RedirectOp::Append, "err.txt"),
            (r"echo x &> all.txt", RedirectOp::OutputAndError, "all.txt"),
            (r"cat < in.txt", RedirectOp::Input, "in.txt"),
            (r"cat << EOF", RedirectOp::Input, "EOF"),
            (r"echo hi>out.txt", RedirectOp::Output, "out.txt"),
            (r"echo x 2>&1", RedirectOp::Output, "&1"),
        ];
        for (command, op, target) in cases {
            let parsed = single(command);
            let segment = &parsed.segments[0];
            assert_eq!(
                segment
                    .redirects
                    .first()
                    .map(|redirect| (&redirect.op, redirect.target.as_str())),
                Some((op, *target)),
                "`{command}` 的重定向形态: {segment:#?}"
            );
            assert!(
                segment.words.iter().any(|word| word.text == *target),
                "`{command}` 的目标也必须留在词元流里"
            );
        }
    }

    /// 一段里的多个重定向要全部收齐，不能只看第一个。
    #[test]
    fn all_redirect_targets_of_one_segment_are_collected() {
        let parsed = single("echo x > local.txt 2>/tmp/out");
        let targets: Vec<&str> = parsed.segments[0]
            .redirects
            .iter()
            .map(|redirect| redirect.target.as_str())
            .collect();
        assert_eq!(targets, vec!["local.txt", "/tmp/out"]);

        // 空引号目标没有路径，但仍占一条重定向；后面的目标不被覆盖。
        let parsed = single(r#"echo x > "" 2> /tmp/out"#);
        let targets: Vec<(&RedirectOp, &str)> = parsed.segments[0]
            .redirects
            .iter()
            .map(|redirect| (&redirect.op, redirect.target.as_str()))
            .collect();
        assert_eq!(
            targets,
            vec![
                (&RedirectOp::Output, ""),
                (&RedirectOp::Output, "/tmp/out"),
            ]
        );
    }

    /// `&& || ; | &` 与换行都分段，且记录段前操作符与各自的程序。
    #[test]
    fn every_shell_operator_starts_a_new_segment() {
        let cases: &[(&str, ShellOperator, &str)] = &[
            ("cargo test && rm x", ShellOperator::And, "rm"),
            ("cargo test || rm x", ShellOperator::Or, "rm"),
            ("cargo test ; rm x", ShellOperator::Sequence, "rm"),
            ("cargo test | rm x", ShellOperator::Pipe, "rm"),
            ("cargo test & rm x", ShellOperator::Background, "rm"),
            ("cargo test\nrm x", ShellOperator::Newline, "rm"),
        ];
        for (command, operator, program) in cases {
            let parsed = parse_command(command);
            assert_eq!(parsed.segments.len(), 2, "`{command}` 应分两段");
            assert_eq!(parsed.segments[1].operator_before, Some(*operator));
            assert_eq!(parsed.segments[1].program(), Some(*program));
            assert_eq!(parsed.segments[0].program(), Some("cargo"));
        }
    }

    /// 首尾链接符都不留空段：单段命令的理由因此仍不带段号。
    #[test]
    fn trailing_operators_do_not_add_an_empty_segment() {
        for command in [
            "cargo test &&",
            "cargo test\n",
            "cargo test;",
            "&& cargo test",
            "; cargo test",
            "",
        ] {
            assert_eq!(
                parse_command(command).segments.len(),
                1,
                "`{command}` 不该多出空段"
            );
        }
        assert_eq!(
            parse_command("&& cargo test").segments[0].operator_before,
            Some(ShellOperator::And)
        );
    }

    /// 括号单独成词，路径判据侧统一跳过；引号内复合命令只补查一层。
    #[test]
    fn parentheses_are_words_and_nested_quotes_are_bounded() {
        let parsed = single("(echo hi)");
        assert_eq!(words(&parsed, 0), vec!["(", "echo", "hi", ")"]);

        let parsed = single(r#"cmd /C "echo x > D:\outside\x""#);
        let segment = &parsed.segments[0];
        assert_eq!(segment.program(), Some("cmd"), "嵌套词元不改程序名");
        assert!(
            segment
                .words
                .iter()
                .any(|word| word.embedded && word.text == r"D:\outside\x")
        );
        assert!(
            segment
                .redirects
                .iter()
                .any(|redirect| redirect.target == r"D:\outside\x")
        );
        // 深度上限 1：嵌套补查词元自身不再触发下一层补查。
        assert!(segment.words.iter().all(|word| word.text != "echo x > D:"));
    }

    /// 不平衡引号不吞词元：末尾累积的内容照样成一个词元。
    #[test]
    fn an_unbalanced_quote_does_not_swallow_the_word() {
        let parsed = single(r#"type "..\secret"#);
        assert_eq!(words(&parsed, 0)[1], r"..\secret");
    }

    /// 命令替换、反引号、`$()` 与变量都不展开。括号独立成词（与旧词法
    /// 一致），其中的绝对路径词元仍保留在模型中。
    #[test]
    fn substitutions_are_not_evaluated_but_their_words_remain_visible() {
        assert_eq!(
            words(&single("echo $(cat /etc/passwd)"), 0),
            vec!["echo", "$", "(", "cat", "/etc/passwd", ")"]
        );
        assert_eq!(
            words(&single("echo `cat /etc/passwd`"), 0),
            vec!["echo", "`cat", "/etc/passwd`"]
        );
        assert_eq!(
            words(&single("echo x > $OUT"), 0),
            vec!["echo", "x", "$OUT"]
        );
        assert_eq!(
            words(&single("echo x > %OUT%"), 0),
            vec!["echo", "x", "%OUT%"]
        );
    }

    /// 只有 shell 启动器的引号命令文本才补查。普通引号路径仍是一个词元，
    /// 不会被嵌套词法拆开，根内放行与完整理由依赖这一点。
    #[test]
    fn shell_launcher_quotes_get_one_bounded_nested_pass() {
        let parsed = single(r#"cmd /C "echo x > D:\outside\x""#);
        let segment = &parsed.segments[0];
        assert_eq!(words(&parsed, 0)[2], r"echo x > D:\outside\x");
        assert!(
            segment
                .words
                .iter()
                .any(|word| word.embedded && word.text == r"D:\outside\x")
        );
        assert!(
            segment
                .redirects
                .iter()
                .any(|redirect| redirect.target == r"D:\outside\x")
        );

        let parsed = single(r#"type "D:\repo\sub dir\main.rs""#);
        assert_eq!(words(&parsed, 0)[1], r"D:\repo\sub dir\main.rs");
        assert!(!parsed.segments[0].words.iter().any(|word| word.embedded));
    }
}

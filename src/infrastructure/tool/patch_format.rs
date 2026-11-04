//! 补丁格式的解析与匹配：纯逻辑，不碰文件系统。
//!
//! 拆出来的理由：补丁格式是**协议**——模型看到什么、我们认什么，
//! 与执行（读文件、写盘、路径校验）是两件事。分开之后，格式的每个
//! 歧义都能用纯输入输出测到，不必先摆一个临时目录。
//!
//! 格式取 Codex 的 `*** Begin Patch` 风格：
//!
//! ```text
//! *** Begin Patch
//! *** Update File: src/lib.rs
//! @@ fn greet(name: &str) -> String
//! -    format!("Hello, !")
//! +    format!("Hello, {name}!")
//! *** Add File: src/new.rs
//! +fn new() {}
//! *** Delete File: src/old.rs
//! *** End Patch
//! ```
//!
//! 三种前缀标记行前缀（`+` / `-` / 空格）决定这一行进旧片段还是新片段：
//! 上下文行进两边，删除行只进旧片段，新增行只进新片段。

/// 补丁文本的起止标记。
const BEGIN_MARKER: &str = "*** Begin Patch";
const END_MARKER: &str = "*** End Patch";

/// 补丁里的一个操作。
#[derive(Debug, Clone, PartialEq)]
pub enum Operation {
    /// 修改已有文件：一组改动块按顺序应用。
    Update {
        /// workspace 内的相对路径。
        path: String,
        /// 改动块，按出现顺序应用。
        hunks: Vec<Hunk>,
    },
    /// 新建文件。
    Add {
        /// workspace 内的相对路径。
        path: String,
        /// 文件的每一行（不含换行符）。
        lines: Vec<String>,
    },
    /// 删除文件。
    Delete {
        /// workspace 内的相对路径。
        path: String,
    },
}

/// 更新操作里的一个改动块。
#[derive(Debug, Clone, PartialEq)]
pub struct Hunk {
    /// `@@` 之后的定位提示。
    ///
    /// 给出时只在它**之后**的文本里找匹配：这是"跳到某个函数再改"的
    /// 语义。没给就在整个文件里找。
    pub anchor: Option<String>,
    /// 旧文本片段：上下文行与被删除的行。
    pub old: Vec<String>,
    /// 新文本片段：上下文行与新增的行。
    pub new: Vec<String>,
}

/// 解析一段补丁文本，返回其中的操作。
///
/// 失败时给出**可操作的**说明：模型据此能改对再来一次，而不是猜。
/// 常见的错法（漏了 Begin/End、路径后多了字、行前缀写错）都各自点名。
pub fn parse_patch(text: &str) -> Result<Vec<Operation>, String> {
    let lines: Vec<&str> = text.lines().collect();
    let trimmed: Vec<&str> = lines
        .iter()
        .map(|line| line.trim_end())
        .collect();

    if trimmed.first().map(|line| *line) != Some(BEGIN_MARKER) {
        return Err(format!(
            "补丁必须以 `{BEGIN_MARKER}` 开头（实际开头是 `{}`）",
            trimmed.first().copied().unwrap_or("<空>")
        ));
    }
    if trimmed.last().map(|line| *line) != Some(END_MARKER) {
        return Err(format!(
            "补丁必须以 `{END_MARKER}` 结尾（实际结尾是 `{}`）",
            trimmed.last().copied().unwrap_or("<空>")
        ));
    }

    let body = &trimmed[1..trimmed.len() - 1];
    let mut operations: Vec<Operation> = Vec::new();
    let mut index = 0usize;
    while index < body.len() {
        let line = body[index];
        if line.trim().is_empty() {
            index += 1;
            continue;
        }
        if let Some(path) = line.strip_prefix("*** Update File:") {
            let path = path.trim().to_owned();
            if path.is_empty() {
                return Err("`*** Update File:` 后面必须有路径".to_owned());
            }
            let (hunks, consumed) = parse_hunks(&body[index + 1..])?;
            operations.push(Operation::Update { path, hunks });
            index += 1 + consumed;
            continue;
        }
        if let Some(path) = line.strip_prefix("*** Add File:") {
            let path = path.trim().to_owned();
            if path.is_empty() {
                return Err("`*** Add File:` 后面必须有路径".to_owned());
            }
            let (lines, consumed) = parse_added_lines(&body[index + 1..])?;
            operations.push(Operation::Add { path, lines });
            index += 1 + consumed;
            continue;
        }
        if let Some(path) = line.strip_prefix("*** Delete File:") {
            let path = path.trim().to_owned();
            if path.is_empty() {
                return Err("`*** Delete File:` 后面必须有路径".to_owned());
            }
            operations.push(Operation::Delete { path });
            index += 1;
            continue;
        }
        if line.starts_with("***") {
            return Err(format!(
                "看不懂的操作标记 `{line}`：只认 `*** Update File:`、`*** Add File:`、`*** Delete File:`"
            ));
        }
        return Err(format!(
            "补丁正文里出现了既不是操作标记也不是补丁行的内容：`{line}`"
        ));
    }

    if operations.is_empty() {
        return Err("补丁里没有任何操作".to_owned());
    }
    Ok(operations)
}

/// 解析 Update 块的内容：若干改动块。
///
/// 返回改动块与消耗的行数。
fn parse_hunks(body: &[&str]) -> Result<(Vec<Hunk>, usize), String> {
    let mut hunks: Vec<Hunk> = Vec::new();
    let mut current: Option<Hunk> = None;
    let mut consumed = 0usize;

    for line in body {
        // 到下一个操作标记为止：改动块不能跨过操作边界——否则下一个
        // 操作的头部与内容会被当成当前块的上下文行。
        if line.starts_with("*** ") {
            break;
        }
        consumed += 1;
        if let Some(anchor) = line.strip_prefix("@@") {
            // 新改动块开始：上一块收尾。
            if let Some(hunk) = current.take() {
                hunks.push(finish_hunk(hunk)?);
            }
            let anchor = anchor.trim();
            current = Some(Hunk {
                anchor: if anchor.is_empty() {
                    None
                } else {
                    Some(anchor.to_owned())
                },
                old: Vec::new(),
                new: Vec::new(),
            });
            continue;
        }
        // `@@` 之前的内容（没有 @@ 头）也要能构成一个改动块。
        let Some(hunk) = current.as_mut() else {
            current = Some(Hunk {
                anchor: None,
                old: Vec::new(),
                new: Vec::new(),
            });
            let hunk = current.as_mut().expect("刚放进去了");
            apply_line_prefix(hunk, line)?;
            continue;
        };
        apply_line_prefix(hunk, line)?;
    }

    if let Some(hunk) = current.take() {
        hunks.push(finish_hunk(hunk)?);
    }
    if hunks.is_empty() {
        return Err("`*** Update File:` 后面没有任何改动内容".to_owned());
    }
    Ok((hunks, consumed))
}

/// 按行前缀把一行分派到旧片段或新片段。
///
/// 前缀的含义：`+` 是新增、`-` 是删除，**其余整行都是上下文**，而
/// 上下文的行首空白是原文的一部分。
///
/// 这里有个容易搞错的地方：Codex 格式里上下文行也常写成"一个空格 +
/// 原文"，那个空格是**前缀标记**，不是内容。但模型写补丁时并不总是
/// 带上它（尤其空行根本带不了）。两种写法都要认，且必须给出同一个
/// 结果——否则同一个补丁会因为写法不同而匹配上/匹配不上。
///
/// 判定办法：行首的空格若**只有一个**，且后面紧跟非空格内容，那它
/// 更像前缀标记；否则它是原文的缩进。这条判定有歧义（原文本来就以
/// 一个空格开头时），但那种情况极少见，而"上下文行的缩进被吃掉"
/// 会让每一次匹配都失败——后者是常态，前者是例外。
fn apply_line_prefix(hunk: &mut Hunk, line: &str) -> Result<(), String> {
    if let Some(text) = line.strip_prefix('+') {
        hunk.new.push(text.to_owned());
        return Ok(());
    }
    if let Some(text) = line.strip_prefix('-') {
        hunk.old.push(text.to_owned());
        return Ok(());
    }
    // 上下文行：剥掉"恰好一个"的前缀空格。
    let text = match line.strip_prefix(' ') {
        Some(rest) if !rest.is_empty() && !rest.starts_with(' ') => rest,
        _ => line,
    };
    hunk.old.push(text.to_owned());
    hunk.new.push(text.to_owned());
    Ok(())
}

/// 收尾一个改动块：旧片段不能为空，否则这个块什么都不改。
fn finish_hunk(hunk: Hunk) -> Result<Hunk, String> {
    if hunk.old.is_empty() {
        return Err("改动块没有要替换的内容（只有 `+` 行，没有 `-` 或上下文行）".to_owned());
    }
    if hunk.old == hunk.new {
        return Err("改动块的新旧内容相同，无需修改".to_owned());
    }
    Ok(hunk)
}

/// 解析 Add 块的内容：文件的每一行。
///
/// 返回行与消耗的行数。
fn parse_added_lines(body: &[&str]) -> Result<(Vec<String>, usize), String> {
    let mut lines: Vec<String> = Vec::new();
    let mut consumed = 0usize;
    for line in body {
        // 到下一个操作标记为止。
        if line.starts_with("*** ") {
            break;
        }
        consumed += 1;
        if line.trim().is_empty() {
            lines.push(String::new());
            continue;
        }
        let Some(text) = line.strip_prefix('+') else {
            return Err(format!(
                "新建文件的内容每行都要以 `+` 开头（实际是 `{line}`）"
            ));
        };
        lines.push(text.to_owned());
    }
    Ok((lines, consumed))
}

/// 把一个 Update 的改动块按顺序应用到文件内容上。
///
/// 每个改动块在**上一个块应用之后**的内容上找匹配：改动块之间可能
/// 互相影响行号，顺序应用才能对上。
///
/// 失败时给出可操作的说明，并附上文件的相关片段让模型能纠正。
pub fn apply_hunks(content: &str, hunks: &[Hunk]) -> Result<String, String> {
    let ends_with_newline = content.ends_with('\n');
    let mut lines: Vec<String> = content.split('\n').map(str::to_owned).collect();
    if ends_with_newline {
        // `split` 对以换行结尾的内容会多出一个空串尾巴。
        lines.pop();
    }

    for (number, hunk) in hunks.iter().enumerate() {
        let start = match &hunk.anchor {
            Some(anchor) => match lines.iter().position(|line| line.contains(anchor.as_str())) {
                Some(position) => position,
                None => {
                    return Err(format!(
                        "第 {} 个改动块定位不到 `@@ {anchor}`：文件里没有这一处",
                        number + 1
                    ));
                }
            },
            None => 0,
        };
        let position = find_unique(&lines, &hunk.old, start).map_err(|reason| {
            format!(
                "第 {} 个改动块{reason}\n文件当前内容：\n{}",
                number + 1,
                excerpt(&lines, start)
            )
        })?;
        lines.splice(position..position + hunk.old.len(), hunk.new.iter().cloned());
    }

    let mut result = lines.join("\n");
    if ends_with_newline && !result.is_empty() {
        result.push('\n');
    }
    Ok(result)
}

/// 在 `lines` 里找 `old` 片段的唯一匹配位置。
///
/// 找不到与匹配到多处都算失败：猜一次就可能改错地方。
fn find_unique(lines: &[String], old: &[String], start: usize) -> Result<usize, String> {
    let mut positions: Vec<usize> = Vec::new();
    if old.len() <= lines.len() {
        for index in start..=lines.len() - old.len() {
            if lines[index..index + old.len()] == *old {
                positions.push(index);
            }
        }
    }
    match positions.len() {
        0 => Err("在文件里找不到要替换的内容（上下文与文件内容不一致）".to_owned()),
        1 => Ok(positions[0]),
        n => Err(format!(
            "匹配到 {n} 处，请提供更长的上下文以唯一定位"
        )),
    }
}

/// 摘出文件的相关片段附在错误说明里，让模型知道文件实际长什么样。
///
/// 只给片段而不是全文：全文可能很大，而模型要看的正是"我以为的位置"
/// 与"实际的位置"的差别。
fn excerpt(lines: &[String], around: usize) -> String {
    let start = around.saturating_sub(3);
    let end = (around + 8).min(lines.len());
    if start >= end {
        return "（文件为空）".to_owned();
    }
    lines[start..end]
        .iter()
        .enumerate()
        .map(|(offset, line)| format!("  {}| {}", start + offset + 1, line))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn patch(text: &str) -> Vec<Operation> {
        parse_patch(text).expect("补丁应当能解析")
    }

    /// 更新一个文件里的一处。
    ///
    /// 补丁文本用 `concat!` 逐行拼而不是 `\` 续行：续行会吃掉下一行的
    /// 行首空白，而补丁的上下文行对空白**逐字敏感**——那正是"上下文不
    /// 匹配"要抓的东西，测试自己却把它吃了，测的就不是真的了。
    #[test]
    fn parses_an_update_with_context_lines() {
        let operations = patch(concat!(
            "*** Begin Patch\n",
            "*** Update File: src/lib.rs\n",
            "@@ fn greet()\n",
            "    let x = 1;\n",
            "-    let x = 2;\n",
            "+    let x = 3;\n",
            "*** End Patch",
        ));

        let Operation::Update { path, hunks } = &operations[0] else {
            panic!("应当是更新操作，实际 {operations:?}")
        };
        assert_eq!(path, "src/lib.rs");
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].anchor.as_deref(), Some("fn greet()"));
        // 上下文行进两边，删除行只进旧片段，新增行只进新片段。
        assert_eq!(hunks[0].old, vec!["    let x = 1;", "    let x = 2;"]);
        assert_eq!(hunks[0].new, vec!["    let x = 1;", "    let x = 3;"]);
    }

    /// 空行常常不带空格前缀：Codex 的补丁里这是常态，必须认。
    #[test]
    fn a_bare_blank_line_is_a_context_line() {
        let operations = patch(concat!(
            "*** Begin Patch\n",
            "*** Update File: a.txt\n",
            "-old\n",
            "\n",
            "+new\n",
            "*** End Patch",
        ));

        let Operation::Update { hunks, .. } = &operations[0] else {
            panic!("应当是更新操作")
        };
        assert_eq!(hunks[0].old, vec!["old", ""]);
        assert_eq!(hunks[0].new, vec!["", "new"]);
    }

    /// 上下文行的两种写法必须得到同一个结果。
    ///
    /// Codex 格式把上下文写成"一个空格 + 原文"，那个空格是前缀标记；
    /// 模型手写时常常直接写原文（缩进原样）。两种写法指的是同一段
    /// 原文——判据不同的话，同一个补丁会因为写法不同而匹配上/匹配不上。
    #[test]
    fn a_context_line_reads_the_same_with_or_without_the_prefix_space() {
        let with_marker = patch(concat!(
            "*** Begin Patch\n",
            "*** Update File: a.txt\n",
            " fn main() {\n",
            "-    let x = 1;\n",
            "+    let x = 2;\n",
            "*** End Patch",
        ));
        let without_marker = patch(concat!(
            "*** Begin Patch\n",
            "*** Update File: a.txt\n",
            "fn main() {\n",
            "-    let x = 1;\n",
            "+    let x = 2;\n",
            "*** End Patch",
        ));

        let Operation::Update { hunks: a, .. } = &with_marker[0] else {
            panic!("应当是更新操作")
        };
        let Operation::Update { hunks: b, .. } = &without_marker[0] else {
            panic!("应当是更新操作")
        };
        assert_eq!(a[0].old, b[0].old, "两种写法的上下文片段必须一致");
        assert_eq!(a[0].new, b[0].new);
        assert_eq!(a[0].old, vec!["fn main() {", "    let x = 1;"]);
    }

    #[test]
    fn parses_add_and_delete() {
        let operations = patch(concat!(
            "*** Begin Patch\n",
            "*** Add File: src/new.rs\n",
            "+fn new() {}\n",
            "+// 第二行\n",
            "*** Delete File: src/old.rs\n",
            "*** End Patch",
        ));

        assert_eq!(
            operations[0],
            Operation::Add {
                path: "src/new.rs".to_owned(),
                lines: vec!["fn new() {}".to_owned(), "// 第二行".to_owned()],
            }
        );
        assert_eq!(
            operations[1],
            Operation::Delete {
                path: "src/old.rs".to_owned()
            }
        );
    }

    /// 缺了起止标记必须明确报错：模型据此能改对再来一次。
    #[test]
    fn missing_markers_are_reported_actionably() {
        let no_begin = parse_patch("*** Update File: a.txt\n- x\n+ y\n*** End Patch");
        assert!(
            no_begin.as_ref().unwrap_err().contains("Begin Patch"),
            "实际: {:?}",
            no_begin.err()
        );

        let no_end = parse_patch("*** Begin Patch\n*** Update File: a.txt\n- x\n+ y\n");
        assert!(
            no_end.as_ref().unwrap_err().contains("End Patch"),
            "实际: {:?}",
            no_end.err()
        );

        let empty = parse_patch("*** Begin Patch\n*** End Patch");
        assert!(
            empty.as_ref().unwrap_err().contains("没有任何操作"),
            "实际: {:?}",
            empty.err()
        );
    }

    /// 认不出来的操作标记要原样报出来：模型能照着改。
    #[test]
    fn an_unknown_operation_marker_is_quoted_back() {
        let error = parse_patch(
            "*** Begin Patch\n\
             *** Rename File: a.txt\n\
             *** End Patch",
        )
        .unwrap_err();

        assert!(error.contains("Rename File"), "实际: {error}");
        assert!(error.contains("Update File"), "应说清只认哪三种: {error}");
    }

    /// 新建文件的内容行必须带 `+`：漏了就是模型没按格式写。
    #[test]
    fn added_lines_must_carry_the_plus_prefix() {
        let error = parse_patch(
            "*** Begin Patch\n\
             *** Add File: src/new.rs\n\
             fn new() {}\n\
             *** End Patch",
        )
        .unwrap_err();

        assert!(error.contains("要以 `+` 开头"), "实际: {error}");
    }

    /// 应用改动块：命中的那段被替换，其余原样保留。
    #[test]
    fn applies_a_hunk_to_the_matching_place() {
        let content = "fn main() {\n    let x = 1;\n    let x = 2;\n    println!(\"{x}\");\n}\n";
        let hunks = vec![Hunk {
            anchor: None,
            old: vec!["    let x = 1;".to_owned(), "    let x = 2;".to_owned()],
            new: vec!["    let x = 1;".to_owned(), "    let x = 3;".to_owned()],
        }];

        let updated = apply_hunks(content, &hunks).unwrap();

        assert_eq!(
            updated,
            "fn main() {\n    let x = 1;\n    let x = 3;\n    println!(\"{x}\");\n}\n"
        );
    }

    /// 上下文对不上：失败，且带上文件片段让模型能纠正。
    #[test]
    fn a_context_mismatch_fails_with_the_actual_content() {
        let content = "fn main() {\n    let x = 1;\n}\n";
        let hunks = vec![Hunk {
            anchor: None,
            old: vec!["    let x = 999;".to_owned()],
            new: vec!["    let x = 1;".to_owned()],
        }];

        let error = apply_hunks(content, &hunks).unwrap_err();

        assert!(error.contains("找不到"), "实际: {error}");
        assert!(error.contains("let x = 1;"), "应附上文件实际内容: {error}");
    }

    /// 上下文有歧义：多处匹配时拒绝改。
    #[test]
    fn an_ambiguous_context_is_refused() {
        let content = "let x = 1;\nlet x = 1;\n";
        let hunks = vec![Hunk {
            anchor: None,
            old: vec!["let x = 1;".to_owned()],
            new: vec!["let x = 2;".to_owned()],
        }];

        let error = apply_hunks(content, &hunks).unwrap_err();

        assert!(error.contains("匹配到 2 处"), "实际: {error}");
        assert!(error.contains("更长的上下文"), "应提示怎么改: {error}");
    }

    /// 锚点把搜索范围限定在它之后：同名代码在锚点之前不该被误改。
    #[test]
    fn the_anchor_limits_the_search_to_what_follows_it() {
        let content = "fn first() {\n    let x = 1;\n}\nfn second() {\n    let x = 1;\n}\n";
        let hunks = vec![Hunk {
            anchor: Some("fn second()".to_owned()),
            old: vec!["    let x = 1;".to_owned()],
            new: vec!["    let x = 2;".to_owned()],
        }];

        let updated = apply_hunks(content, &hunks).unwrap();

        // 锚点之后的那处被改，锚点之前的原封不动。
        assert!(updated.contains("fn first() {\n    let x = 1;\n}"));
        assert!(updated.contains("fn second() {\n    let x = 2;\n}"));
    }

    /// 锚点不存在：明确失败，不去别处乱找。
    #[test]
    fn a_missing_anchor_is_reported() {
        let content = "fn main() {}\n";
        let hunks = vec![Hunk {
            anchor: Some("fn nowhere()".to_owned()),
            old: vec!["fn main() {}".to_owned()],
            new: vec!["fn main() { }".to_owned()],
        }];

        let error = apply_hunks(content, &hunks).unwrap_err();

        assert!(error.contains("定位不到"), "实际: {error}");
        assert!(error.contains("fn nowhere()"), "应点出找不到的锚点: {error}");
    }

    /// 多个改动块按顺序应用：第二个块在第一个的结果上找。
    #[test]
    fn several_hunks_apply_in_order() {
        let content = "a\nb\nc\n";
        let hunks = vec![
            Hunk {
                anchor: None,
                old: vec!["a".to_owned()],
                new: vec!["A".to_owned()],
            },
            Hunk {
                anchor: None,
                old: vec!["c".to_owned()],
                new: vec!["C".to_owned()],
            },
        ];

        assert_eq!(apply_hunks(content, &hunks).unwrap(), "A\nb\nC\n");
    }

    /// 文件末尾有没有换行要原样保留：补丁不该顺手改动它。
    #[test]
    fn the_trailing_newline_is_preserved() {
        let with_newline = "a\nb\n";
        let without_newline = "a\nb";
        let hunks = vec![Hunk {
            anchor: None,
            old: vec!["a".to_owned()],
            new: vec!["A".to_owned()],
        }];

        assert_eq!(apply_hunks(with_newline, &hunks).unwrap(), "A\nb\n");
        assert_eq!(apply_hunks(without_newline, &hunks).unwrap(), "A\nb");
    }

    /// 新旧内容相同的改动块没有意义：直接拒绝，不让它白跑一趟。
    #[test]
    fn a_no_op_hunk_is_refused() {
        let error = parse_patch(
            "*** Begin Patch\n\
             *** Update File: a.txt\n\
             -same\n\
             +same\n\
             *** End Patch",
        )
        .unwrap_err();

        assert!(error.contains("无需修改"), "实际: {error}");
    }
}

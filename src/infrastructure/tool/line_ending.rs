//! 文件行尾的识别与统一。
//!
//! 行尾是文件的属性，不是内容的属性。Windows 上 git 默认把文本文件检出
//! 成 CRLF，Linux 与 macOS 上是 LF；同一份代码在两个平台上的磁盘内容
//! 逐字节不同。
//!
//! 工具读到的模型文本与磁盘上的行尾因此可能对不上：模型写补丁用 LF，
//! 文件在 Windows 上是 CRLF。比较前不统一口径，逐行精确匹配会永远失配。
//!
//! 三步是固定用法：读入后 [`LineEnding::detect`] 判风格，[`LineEnding::to_lf`]
//! 统一成 LF 再比较，写回前 [`LineEnding::restore`] 还原成原风格。

/// 文本文件的行尾风格。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineEnding {
    /// Unix 风格：行尾是 `\n`。
    Lf,
    /// Windows 风格：行尾是 `\r\n`。
    CrLf,
}

impl LineEnding {
    /// 判别内容的行尾风格。
    ///
    /// 内容里出现过 `\r\n` 就按 CRLF 算，否则按 LF 算。行尾混用的文件
    /// 少见，真出现时按 CRLF 处理：把整份文件统一成一种风格不会改掉
    /// 任何一行的实际内容。
    pub fn detect(content: &str) -> Self {
        if content.contains("\r\n") {
            Self::CrLf
        } else {
            Self::Lf
        }
    }

    /// 判别一个文件的行尾风格。
    ///
    /// 分块往后读，直到看见 `\r\n` 或读到文件末尾。行尾风格在文件内是
    /// 统一的，但 `\r\n` 出现的位置没有保证：首行超过一整块的长行文件
    /// （压缩过的脚本、数据行）会在第一块里看不到任何 `\r\n`，只看开头
    /// 一块就会把 CRLF 误判成 LF。
    ///
    /// 按字节找 `\r\n`，不解码成字符串：多字节字符恰好跨过读取边界时，
    /// 解码会失败，按 LF 处理就把 CRLF 文件误判成 LF。`\r\n` 是 ASCII，
    /// 在字节层面找它不受编码影响。上一块的末尾与下一块的开头可能正好
    /// 拼成一个 `\r\n`，跨块的那一对也要认出来。
    ///
    /// 读不出内容时按 LF 算。
    pub async fn detect_in_file(path: &std::path::Path) -> Self {
        use tokio::io::AsyncReadExt;

        let Ok(mut file) = tokio::fs::File::open(path).await else {
            return Self::Lf;
        };
        let mut buffer = vec![0u8; 4096];
        // 上一块最后一个字节：它可能与下一块的首字节组成 `\r\n`。
        let mut carry: Option<u8> = None;
        loop {
            let read = match file.read(&mut buffer).await {
                Ok(0) => return Self::Lf,
                Ok(read) => read,
                Err(_) => return Self::Lf,
            };
            if carry == Some(b'\r') && buffer[0] == b'\n' {
                return Self::CrLf;
            }
            if buffer[..read].windows(2).any(|pair| pair == b"\r\n") {
                return Self::CrLf;
            }
            carry = buffer[read - 1..read].first().copied();
            // 读到的字节数少于请求量，说明已经到文件末尾。
            if read < buffer.len() {
                return Self::Lf;
            }
        }
    }

    /// 把内容统一成 LF。
    ///
    /// 只有 `\r\n` 会被改写。单独的 `\r` 不动：它不是行尾，可能是内容
    /// 本身的一部分。
    pub fn to_lf(content: &str) -> String {
        if content.contains("\r\n") {
            content.replace("\r\n", "\n")
        } else {
            content.to_owned()
        }
    }

    /// 把 LF 内容还原成该风格。
    ///
    /// 入参必须是 [`LineEnding::to_lf`] 的产物（或本来就是 LF）。已经
    /// 是 `\r\n` 的内容不能直接传进来，否则会变成 `\r\r\n`。
    pub fn restore(content: &str, ending: Self) -> String {
        match ending {
            Self::Lf => content.to_owned(),
            Self::CrLf => content.replace('\n', "\r\n"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 风格判别：出现过 CRLF 就是 CRLF，否则是 LF。
    #[test]
    fn detects_the_line_ending_style() {
        assert_eq!(LineEnding::detect("a\r\nb\r\n"), LineEnding::CrLf);
        assert_eq!(LineEnding::detect("a\nb\n"), LineEnding::Lf);
        assert_eq!(LineEnding::detect("a\nb\r\n"), LineEnding::CrLf);
        assert_eq!(LineEnding::detect(""), LineEnding::Lf);
    }

    /// 统一与还原要能原样回到起点：内容本身不能被改动。
    #[test]
    fn normalizing_and_restoring_round_trips() {
        let crlf = "a\r\nb\r\n";
        let lf = "a\nb\n";

        assert_eq!(
            LineEnding::restore(&LineEnding::to_lf(crlf), LineEnding::CrLf),
            crlf
        );
        assert_eq!(
            LineEnding::restore(&LineEnding::to_lf(lf), LineEnding::Lf),
            lf
        );
    }

    /// 单独的 `\r` 不是行尾，不能被改写。
    #[test]
    fn a_lone_carriage_return_is_left_alone() {
        let content = "a\rb\n";

        assert_eq!(LineEnding::to_lf(content), "a\rb\n");
    }

    /// 建一个临时目录，返回路径。目录名带用例名：共用一个目录时，
    /// 并行跑的用例会互删对方还没读完的文件。
    fn temp_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("qch-eol-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建临时目录");
        dir
    }

    /// 从文件判风格：读文件开头的一段就够。
    #[tokio::test]
    async fn detects_the_line_ending_style_of_a_file() {
        let dir = temp_dir("basic");

        let crlf = dir.join("crlf.txt");
        std::fs::write(&crlf, b"a\r\nb\r\n").expect("写 CRLF 文件");
        assert_eq!(
            LineEnding::detect_in_file(&crlf).await,
            LineEnding::CrLf,
            "CRLF 文件应被认出来"
        );

        let lf = dir.join("lf.txt");
        std::fs::write(&lf, b"a\nb\n").expect("写 LF 文件");
        assert_eq!(
            LineEnding::detect_in_file(&lf).await,
            LineEnding::Lf,
            "LF 文件应被认出来"
        );

        assert_eq!(
            LineEnding::detect_in_file(&dir.join("missing.txt")).await,
            LineEnding::Lf,
            "读不出的文件按 LF 处理"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 多字节字符跨过读取边界时，仍要认出 CRLF。
    ///
    /// 按字符串解码会在这里失败（半个字符），判成 LF 就把 CRLF 文件写坏了。
    /// `\r\n` 是 ASCII，按字节找它不受编码影响。
    #[tokio::test]
    async fn a_multibyte_char_at_the_read_boundary_does_not_hide_crlf() {
        let dir = temp_dir("multibyte");

        // 4096 字节的读取窗口：让多字节字符正好跨过边界，且 `\r\n`
        // 落在窗口之内。
        let mut content = "中".repeat(1364).into_bytes();
        assert!(content.len() < 4096, "前半段要落在窗口内");
        content.extend_from_slice(b"\r\n");
        content.extend_from_slice("文".repeat(100).as_bytes());
        let path = dir.join("multibyte.txt");
        std::fs::write(&path, &content).expect("写多字节文件");

        // 前缀解码会在边界处截断多字节字符，这正是旧实现的失败点。
        let prefix = &content[..4096];
        assert!(
            std::str::from_utf8(prefix).is_err(),
            "夹具必须让边界落在多字节字符中间"
        );

        assert_eq!(
            LineEnding::detect_in_file(&path).await,
            LineEnding::CrLf,
            "多字节字符跨过边界不该把 CRLF 误判成 LF"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 首行超过一整块时，CRLF 可能在第一块之外才出现。
    ///
    /// 只看开头一块就会漏掉它，把 CRLF 误判成 LF，落盘时整个文件的行尾
    /// 被改写成 LF。
    #[tokio::test]
    async fn a_crlf_after_a_long_first_line_is_still_crlf() {
        let dir = temp_dir("longline");

        let mut content = vec![b'a'; 5000];
        content.extend_from_slice(b"\r\n");
        content.extend_from_slice(b"tail\r\n");
        let path = dir.join("long-line.txt");
        std::fs::write(&path, &content).expect("写长行文件");

        assert_eq!(
            LineEnding::detect_in_file(&path).await,
            LineEnding::CrLf,
            "首行超过一整块时不该漏掉后面的 CRLF"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `\r\n` 正好跨在两块之间时也算数。
    ///
    /// `\r` 是上一块的最后一个字节，`\n` 是下一块的第一个字节：两块分开
    /// 看都凑不出一对 `\r\n`。
    #[tokio::test]
    async fn a_crlf_straddling_the_read_boundary_is_still_crlf() {
        let dir = temp_dir("straddle");

        // `\r` 落在第 4095 字节（第一块的最后一位），`\n` 落在第 4096 字节。
        let mut content = vec![b'a'; 4095];
        content.extend_from_slice(b"\r\n");
        content.extend_from_slice(b"tail\r\n");
        let path = dir.join("straddle.txt");
        std::fs::write(&path, &content).expect("写跨块文件");

        assert_eq!(
            LineEnding::detect_in_file(&path).await,
            LineEnding::CrLf,
            "跨块的 CRLF 不该被漏掉"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}

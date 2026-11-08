//! 项目指令文件：把 workspace 中的项目约定注入系统提示词。
//!
//! 对应 Claude Code 的 `CLAUDE.md` 与 Codex 的 `AGENTS.md`。模型每次运行
//! 都应了解项目自身的约定；否则只能沿用通用习惯，而通用习惯与项目约定
//! 冲突时模型不会察觉。
//!
//! 读取文件属于 IO，因此放在基础设施层。应用层的 `AgentRoster` 只接收
//! 已读入的正文，并拼接到各角色的提示词之后。

use std::path::{Path, PathBuf};

/// 项目指令的字节上限。
///
/// 8 KiB 是在"能写完项目关键约定"与"每轮重复消耗上下文"之间的折中。
/// 指令进入系统提示词，每个角色每轮都会重发，成本随轮数放大。
const MAX_INSTRUCTION_BYTES: usize = 8 * 1024;

/// workspace 根下的项目指令文件。
const ROOT_INSTRUCTIONS: &str = "QCODEHARNESS.md";

/// 次级位置：harness 自己的目录。
///
/// `.qcodeharness` 是受保护路径，agent 的读写都会被拒绝，因此放在这里的
/// 指令不会被一次误写的 `write_file` 覆盖。两个位置都存在时使用根目录
/// 版本：它更接近项目的 README，也是使用者最先查看的位置。
const SECONDARY_INSTRUCTIONS: &str = ".qcodeharness/instructions.md";

/// 项目指令的加载结果。
#[derive(Debug, Clone, PartialEq)]
pub struct ProjectInstructions {
    /// 注入用的正文，已按上限截断。
    pub text: String,
    /// 来源文件，供日志与报告说明。
    pub source: PathBuf,
    /// 是否被截断。
    pub truncated: bool,
    /// 读取过程中的告警，调用方负责提示用户。
    pub warnings: Vec<String>,
}

/// 从 workspace 根加载项目指令。
///
/// 按顺序查找两个位置，读取第一个存在的文件。两个位置都不存在时返回
/// `None`，此时提示词只包含内置档案。文件存在但读取失败时返回空正文与
/// 告警，不尝试下一个位置。
pub fn load_project_instructions(root: &Path) -> Option<ProjectInstructions> {
    for relative in [ROOT_INSTRUCTIONS, SECONDARY_INSTRUCTIONS] {
        let path = root.join(relative);
        if !path.is_file() {
            continue;
        }
        return Some(read_instructions(&path));
    }
    None
}

/// 读一个指令文件并按上限截断。
fn read_instructions(path: &Path) -> ProjectInstructions {
    let mut warnings = Vec::new();
    let (text, truncated) = match std::fs::read_to_string(path) {
        Ok(text) => truncate_to_limit(text),
        Err(error) => {
            // 读取失败不返回错误：指令文件是便利设施，不是安全边界。
            // 但必须告警，静默跳过会让使用者误以为约定已生效。
            warnings.push(format!("{} 存在但读不了（{error}），本次不使用项目指令", path.display()));
            (String::new(), false)
        }
    };
    ProjectInstructions {
        text,
        source: path.to_path_buf(),
        truncated,
        warnings,
    }
}

/// 将正文按上限截断并追加截断提示，返回文本与是否发生截断。
///
/// 按字符边界切分。多字节字符被截成半个会使字符串不是合法 UTF-8，而
/// 提示词随后要序列化为 JSON。
fn truncate_to_limit(text: String) -> (String, bool) {
    if text.len() <= MAX_INSTRUCTION_BYTES {
        return (text, false);
    }
    let mut end = MAX_INSTRUCTION_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut truncated = text[..end].to_owned();
    truncated.push_str("\n……（项目指令超过 8 KiB，此处截断；请精简该文件）");
    (truncated, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// 每个测试一个独立根目录，避开并行测试互相删目录。
    fn root(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("qch-instr-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("建临时目录");
        dir
    }

    #[test]
    fn no_instruction_file_means_no_instructions() {
        // 兼容性承诺：文件不存在时加载器返回 None，提示词与内置档案
        // 逐字相同。
        assert_eq!(load_project_instructions(&root("absent")), None);
    }

    #[test]
    fn root_file_is_loaded() {
        let dir = root("root");
        fs::write(dir.join(ROOT_INSTRUCTIONS), "提交信息用中文\n").expect("写指令");

        let loaded = load_project_instructions(&dir).expect("应加载到指令");

        assert_eq!(loaded.text, "提交信息用中文\n");
        assert_eq!(loaded.source, dir.join(ROOT_INSTRUCTIONS));
        assert!(!loaded.truncated);
        assert!(loaded.warnings.is_empty());
    }

    /// 根目录优先：使用者最先查找 workspace 根的 `QCODEHARNESS.md`；
    /// 次级位置只是模型无法修改的备份，不应替代它。
    #[test]
    fn root_file_wins_over_the_secondary_location() {
        let dir = root("both");
        fs::write(dir.join(ROOT_INSTRUCTIONS), "根上的约定\n").expect("写根指令");
        fs::create_dir_all(dir.join(".qcodeharness")).expect("建目录");
        fs::write(dir.join(SECONDARY_INSTRUCTIONS), "次级的约定\n").expect("写次级指令");

        let loaded = load_project_instructions(&dir).expect("应加载到指令");

        assert_eq!(loaded.text, "根上的约定\n");
        assert_eq!(loaded.source, dir.join(ROOT_INSTRUCTIONS));
    }

    /// 根上没有时用次级位置。
    #[test]
    fn secondary_location_is_used_when_root_is_absent() {
        let dir = root("secondary");
        fs::create_dir_all(dir.join(".qcodeharness")).expect("建目录");
        fs::write(dir.join(SECONDARY_INSTRUCTIONS), "模型改不动的约定\n").expect("写次级指令");

        let loaded = load_project_instructions(&dir).expect("应加载到指令");

        assert_eq!(loaded.text, "模型改不动的约定\n");
        assert_eq!(loaded.source, dir.join(SECONDARY_INSTRUCTIONS));
    }

    /// 超限要截断，并留下看得见的说明。
    #[test]
    fn oversized_instructions_are_truncated_with_a_notice() {
        let dir = root("oversize");
        fs::write(dir.join(ROOT_INSTRUCTIONS), "x".repeat(MAX_INSTRUCTION_BYTES * 2)).expect("写指令");

        let loaded = load_project_instructions(&dir).expect("应加载到指令");

        assert!(loaded.truncated);
        assert!(loaded.text.len() > MAX_INSTRUCTION_BYTES, "截断说明本身要留得下");
        assert!(loaded.text.len() < MAX_INSTRUCTION_BYTES * 2, "不应原样放进来的两倍长");
        assert!(loaded.text.contains("截断"), "应说明被截断: {}", loaded.text);
        assert!(loaded.text.starts_with('x'), "正文应在说明之前");
    }

    /// 恰好等于上限不截断：边界不能差一个字节。
    #[test]
    fn instructions_exactly_at_the_limit_are_not_truncated() {
        let dir = root("exact");
        fs::write(dir.join(ROOT_INSTRUCTIONS), "x".repeat(MAX_INSTRUCTION_BYTES)).expect("写指令");

        let loaded = load_project_instructions(&dir).expect("应加载到指令");

        assert!(!loaded.truncated);
        assert_eq!(loaded.text.len(), MAX_INSTRUCTION_BYTES);
    }

    /// 截断点落在多字节字符内部时必须回退到字符边界。
    ///
    /// 中文指令正好位于 8 KiB 边界时，按字节硬切会产生半个 UTF-8 序列，
    /// 导致后续 JSON 序列化失败。
    #[test]
    fn truncation_respects_character_boundaries() {
        let dir = root("utf8");
        // 一个汉字 3 字节，8192 不是 3 的倍数，硬切必然切在汉字中间。
        let body = "约".repeat(MAX_INSTRUCTION_BYTES / 3 + 100);
        fs::write(dir.join(ROOT_INSTRUCTIONS), &body).expect("写指令");

        let loaded = load_project_instructions(&dir).expect("应加载到指令");

        assert!(loaded.truncated);
        // 能原样读回来就说明没有产生孤儿字节。
        assert!(loaded.text.contains('约'));
        assert!(!loaded.text.contains('\u{FFFD}'), "不应出现替换字符");
    }

    /// 无法按 UTF-8 读取的文件必须告警，不能静默跳过。
    ///
    /// Windows 上用记事本以 ANSI 编码保存的 `QCODEHARNESS.md` 是 GBK
    /// 字节流，`read_to_string` 会失败。静默跳过会让使用者以为约定已
    /// 生效，而模型未收到任何内容。
    #[test]
    fn non_utf8_instruction_file_warns_instead_of_failing_silently() {
        let dir = root("gbk");
        // 0xFF 不是合法的 UTF-8 起始字节。
        fs::write(dir.join(ROOT_INSTRUCTIONS), [0xFF, 0xFE, 0x00]).expect("写坏编码");

        let loaded = load_project_instructions(&dir).expect("是普通文件即尝试读取");

        assert!(loaded.text.is_empty());
        assert_eq!(loaded.warnings.len(), 1, "应有一条告警: {:?}", loaded.warnings);
        assert!(loaded.warnings[0].contains("读不了"), "{:?}", loaded.warnings);
    }
}

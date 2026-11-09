//! 分层指令文件：把用户、workspace 与任务目录的约定注入系统提示词。
//!
//! 对应 Claude Code 的 `CLAUDE.md` 与 Codex 的 `AGENTS.md`。模型每次运行
//! 都应了解项目自身的约定；否则只能沿用通用习惯，而通用习惯与项目约定
//! 冲突时模型不会察觉。
//!
//! 读取文件属于 IO，因此放在基础设施层。应用层的 `AgentRoster` 只接收
//! 已读入的正文，并拼接到各角色的提示词之后。

use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Component, Path, PathBuf};

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

/// 合并正文的总上限，包含来源标题与截断说明。
const MAX_LAYERED_BYTES: usize = 32 * 1024;
const FILE_TRUNCATION_NOTICE: &str = "\n……（项目指令超过 8 KiB，此处截断；请精简该文件）";
const TOTAL_TRUNCATION_NOTICE: &str = "\n……（分层指令超过 32 KiB，此处截断；后续指令未加载）";
const LAYER_PREAMBLE: &str = "# 分层任务指令\n\n优先级从低到高：用户级 < workspace 根 < 更接近任务 scope 的目录。发生冲突时采用适用范围内优先级更高的约定。\n目录规则仅适用于标注目录及其子目录，不适用于同级或其他目录。指令不更改工具权限、审批或受保护路径。\n";

/// 按优先级合并后的指令及其诊断信息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayeredInstructions {
    /// 注入提示词的正文，含来源、适用目录及优先级，最多 32 KiB。
    pub text: String,
    /// 已选择并尝试读取的文件，顺序与正文一致；总量截断后不再读后续文件。
    pub sources: Vec<PathBuf>,
    /// 读取、编码或截断告警，调用方负责提示用户。
    pub warnings: Vec<String>,
}

/// 加载用户约定及 workspace 到任务 scope 的目录指令。
///
/// `user_dir` 是显式传入的 qcodeharness 配置目录，依次查找其中的
/// `QCODEHARNESS.md`、`instructions.md`。未传入时不探测环境或父仓库；
/// 子代理可由装配层传入父任务的同一个用户目录。workspace 及各级目录
/// 依次查找 `QCODEHARNESS.md`、`.qcodeharness/instructions.md`，每层只取
/// 第一个存在的位置，workspace 根只加载一次。
///
/// 相对 `scope` 按 workspace 解析，允许 `.`。可选指令缺失时不告警；
/// 文件读取失败时保留来源与告警，不以 fallback 掩盖错误。每个文件只读
/// 8 KiB 加一个探测字节，正文保持 UTF-8 完整；总量达到上限后停止读取。
/// 此函数用于任务装配，不修改工具权限或运行中的目录选择。
///
/// # Errors
///
/// workspace 或 scope 不存在、不是目录、含 `..`，scope 在词法上越界，
/// 或目录／指令链接的物理路径逃出各自根目录时返回错误。无法检查可选
/// 文件是否存在或解析其链接时也返回带路径的错误，避免静默丢失约定。
pub fn load_layered_instructions(
    user_dir: Option<&Path>,
    workspace: &Path,
    scope: &Path,
) -> Result<LayeredInstructions, String> {
    let workspace = normalized_absolute(workspace)?;
    let canonical_workspace = canonical_directory(&workspace, None)?;
    let scope = if scope.is_absolute() {
        normalized_absolute(scope)?
    } else {
        if scope.has_root()
            || scope
                .components()
                .any(|part| matches!(part, Component::Prefix(_)))
        {
            return Err(format!(
                "scope 必须是完整绝对路径或 workspace 相对路径：{}",
                scope.display()
            ));
        }
        normalized_absolute(&workspace.join(scope))?
    };
    // 一律按物理路径比较：workspace 与 scope 可能一个走符号链接、一个走真实
    // 路径（macOS 的 /var 就是链接），只做词法比较会把合法 scope 判成越界。
    // 目录本身仍用调用方给出的词法路径，日志与来源才对得上输入。
    let canonical_scope = canonical_directory(&scope, None)?;
    if !canonical_scope.starts_with(&canonical_workspace) {
        return Err(format!(
            "任务 scope 只允许位于 workspace 内：{}",
            scope.display()
        ));
    }
    let relative = canonical_scope
        .strip_prefix(&canonical_workspace)
        .expect("已确认 scope 位于 workspace 内");

    let mut directories = vec![workspace.clone()];
    let mut directory = workspace;
    for part in relative.components() {
        directory.push(part);
        // 逐级校验物理边界：目录链接即使没有指令文件也必须被拒绝，否则
        // 缺失文件会让越界链接静默通过。
        canonical_directory(&directory, Some(&canonical_workspace))?;
        directories.push(directory.clone());
    }
    let mut loaded = LayeredInstructions {
        text: String::new(),
        sources: Vec::new(),
        warnings: Vec::new(),
    };
    // 同一物理文件可能经不同词法路径进入（用户目录与 workspace 重叠、目录
    // 链接指回根）；按物理身份去重，避免同一份约定重复占用 32 KiB 预算。
    let mut seen = BTreeSet::new();
    if let Some(user_dir) = user_dir {
        let directory = normalized_absolute(user_dir)?;
        if optional_path_exists(&directory, None)? {
            let boundary = canonical_directory(&directory, None)?;
            if !append_layer(
                &mut loaded,
                &mut seen,
                &directory,
                &boundary,
                1,
                true,
            )? {
                return Ok(loaded);
            }
        }
    }
    for (depth, directory) in directories.iter().enumerate() {
        if !append_layer(
            &mut loaded,
            &mut seen,
            directory,
            &canonical_workspace,
            depth + 2,
            false,
        )? {
            break;
        }
    }
    Ok(loaded)
}


fn normalized_absolute(path: &Path) -> Result<PathBuf, String> {
    if path
        .components()
        .any(|part| matches!(part, Component::ParentDir))
    {
        return Err(format!("指令路径不允许包含 ..：{}", path.display()));
    }
    std::path::absolute(path)
        .map(|absolute| absolute.components().collect())
        .map_err(|error| format!("无法解析指令路径 {}：{error}", path.display()))
}

fn canonical_directory(path: &Path, boundary: Option<&Path>) -> Result<PathBuf, String> {
    let physical = fs::canonicalize(path)
        .map_err(|error| format!("无法解析指令目录 {}：{error}", path.display()))?;
    if let Some(boundary) = boundary {
        require_within(&physical, boundary, path)?;
    }
    let metadata = fs::metadata(&physical)
        .map_err(|error| format!("无法检查指令目录 {}：{error}", path.display()))?;
    if !metadata.is_dir() {
        return Err(format!("指令目录必须是已存在的目录：{}", path.display()));
    }
    Ok(physical)
}

fn require_within(physical: &Path, boundary: &Path, original: &Path) -> Result<(), String> {
    if physical.starts_with(boundary) {
        Ok(())
    } else {
        Err(format!(
            "指令路径越出允许的根目录：{} → {}",
            original.display(),
            physical.display()
        ))
    }
}

/// 检查链接本身，避免把断链当作不存在；在访问子路径前逐级检查物理边界。
fn optional_path_exists(path: &Path, boundary: Option<&Path>) -> Result<bool, String> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(format!("无法检查指令路径 {}：{error}", path.display())),
        Ok(_) => {}
    }
    let physical = fs::canonicalize(path)
        .map_err(|error| format!("无法解析指令路径 {}：{error}", path.display()))?;
    if let Some(boundary) = boundary {
        require_within(&physical, boundary, path)?;
    }
    Ok(true)
}

fn append_layer(
    loaded: &mut LayeredInstructions,
    seen: &mut BTreeSet<PathBuf>,
    directory: &Path,
    boundary: &Path,
    priority: usize,
    user_layer: bool,
) -> Result<bool, String> {
    let fallback = if user_layer {
        "instructions.md"
    } else {
        SECONDARY_INSTRUCTIONS
    };
    for relative in [ROOT_INSTRUCTIONS, fallback] {
        let mut path = directory.to_path_buf();
        let mut exists = true;
        for part in Path::new(relative).components() {
            path.push(part);
            if !optional_path_exists(&path, Some(boundary))? {
                exists = false;
                break;
            }
        }
        if !exists {
            continue;
        }
        let metadata = fs::metadata(&path)
            .map_err(|error| format!("无法检查指令文件 {}：{error}", path.display()))?;
        if !metadata.is_file() {
            loaded.warnings.push(format!(
                "{} 指令路径不是普通文件，本次不使用该层",
                path.display()
            ));
            return Ok(true);
        }
        // 去重键用物理路径：同一文件经 junction 或重叠用户目录出现多次时，
        // 只保留优先级最高（最先出现）的那一份。
        let identity = fs::canonicalize(&path)
            .map_err(|error| format!("无法解析指令文件 {}：{error}", path.display()))?;
        if !seen.insert(identity) {
            return Ok(true);
        }
        loaded.sources.push(path.clone());
        let instructions = read_instructions(&path);
        loaded.warnings.extend(instructions.warnings);
        if instructions.text.is_empty() {
            return Ok(true);
        }
        let kind = if user_layer {
            "用户级"
        } else if priority == 2 {
            "workspace 根"
        } else {
            "目录级"
        };
        let mut section = format!(
            "\n## 优先级 {priority}（{kind}）\n来源：{}\n",
            path.display()
        );
        if user_layer {
            section.push_str("适用范围：当前任务。\n\n");
        } else {
            section.push_str(&format!(
                "仅适用于目录 {} 及其子目录。\n\n",
                directory.display()
            ));
        }
        section.push_str(&instructions.text);
        if loaded.text.is_empty() {
            loaded.text.push_str(LAYER_PREAMBLE);
        }
        return Ok(append_bounded(loaded, &section));
    }
    Ok(true)
}

/// 总量包含标题及告警文字；已有正文也预留截断说明的位置。
fn append_bounded(loaded: &mut LayeredInstructions, section: &str) -> bool {
    if loaded.text.len() + section.len() <= MAX_LAYERED_BYTES {
        loaded.text.push_str(section);
        return true;
    }
    let body_limit = MAX_LAYERED_BYTES - TOTAL_TRUNCATION_NOTICE.len();
    if loaded.text.len() > body_limit {
        let end = char_boundary_before(&loaded.text, body_limit);
        loaded.text.truncate(end);
    } else {
        let remaining = body_limit - loaded.text.len();
        let end = char_boundary_before(section, remaining);
        loaded.text.push_str(&section[..end]);
    }
    loaded.text.push_str(TOTAL_TRUNCATION_NOTICE);
    loaded
        .warnings
        .push("分层指令超过 32 KiB，合并正文已截断，后续指令未加载".to_owned());
    false
}

fn char_boundary_before(text: &str, limit: usize) -> usize {
    let mut end = limit.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    end
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
    let (mut text, truncated) = match read_bounded_utf8(path) {
        Ok(result) => result,
        Err(error) => {
            // 读取失败不返回错误：指令文件是便利设施，不是安全边界。
            // 但必须告警，静默跳过会让使用者误以为约定已生效。
            warnings.push(format!(
                "{} 存在但读不了（{error}），本次不使用项目指令",
                path.display()
            ));
            (String::new(), false)
        }
    };
    if truncated {
        text.push_str(FILE_TRUNCATION_NOTICE);
        warnings.push(format!("{} 超过 8 KiB，项目指令已截断", path.display()));
    }
    ProjectInstructions {
        text,
        source: path.to_path_buf(),
        truncated,
        warnings,
    }
}

fn read_bounded_utf8(path: &Path) -> Result<(String, bool), String> {
    let file = File::open(path).map_err(|error| error.to_string())?;
    let mut bytes = Vec::with_capacity(MAX_INSTRUCTION_BYTES + 1);
    file.take((MAX_INSTRUCTION_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    let truncated = bytes.len() > MAX_INSTRUCTION_BYTES;
    let prefix = &bytes[..bytes.len().min(MAX_INSTRUCTION_BYTES)];
    let text = match std::str::from_utf8(prefix) {
        Ok(text) => text,
        // 只允许限量读取造成的末尾残缺，正文内部的非法 UTF-8 仍需告警。
        Err(error) if truncated && error.error_len().is_none() => {
            std::str::from_utf8(&prefix[..error.valid_up_to()])
                .map_err(|error| error.to_string())?
        }
        Err(error) => return Err(error.to_string()),
    };
    Ok((text.to_owned(), truncated))
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
        fs::write(
            dir.join(ROOT_INSTRUCTIONS),
            "x".repeat(MAX_INSTRUCTION_BYTES * 2),
        )
        .expect("写指令");

        let loaded = load_project_instructions(&dir).expect("应加载到指令");

        assert!(loaded.truncated);
        assert!(
            loaded.text.len() > MAX_INSTRUCTION_BYTES,
            "截断说明本身要留得下"
        );
        assert!(
            loaded.text.len() < MAX_INSTRUCTION_BYTES * 2,
            "不应原样放进来的两倍长"
        );
        assert!(
            loaded.text.contains("截断"),
            "应说明被截断: {}",
            loaded.text
        );
        assert!(loaded.text.starts_with('x'), "正文应在说明之前");
    }

    /// 恰好等于上限不截断：边界不能差一个字节。
    #[test]
    fn instructions_exactly_at_the_limit_are_not_truncated() {
        let dir = root("exact");
        fs::write(
            dir.join(ROOT_INSTRUCTIONS),
            "x".repeat(MAX_INSTRUCTION_BYTES),
        )
        .expect("写指令");

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
    /// 字节流，按 UTF-8 解码会失败。静默跳过会让使用者以为约定已
    /// 生效，而模型未收到任何内容。
    #[test]
    fn non_utf8_instruction_file_warns_instead_of_failing_silently() {
        let dir = root("gbk");
        // 0xFF 不是合法的 UTF-8 起始字节。
        fs::write(dir.join(ROOT_INSTRUCTIONS), [0xFF, 0xFE, 0x00]).expect("写坏编码");

        let loaded = load_project_instructions(&dir).expect("是普通文件即尝试读取");

        assert!(loaded.text.is_empty());
        assert_eq!(
            loaded.warnings.len(),
            1,
            "应有一条告警: {:?}",
            loaded.warnings
        );
        assert!(
            loaded.warnings[0].contains("读不了"),
            "{:?}",
            loaded.warnings
        );
    }
}

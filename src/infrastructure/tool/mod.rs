//! 工具基础设施：全部内置工具与组装入口。
//!
//! 一个工具一个结构体，不用统一 dispatch：各工具需要的配置不同（只有
//! `run_command` 需要超时和输出上限），合成一个结构体会让所有工具共享
//! 大量用不上的字段。新增工具只需两步：写好结构体，在 [`all_tools`]
//! 登记一行。

pub mod apply_patch;
pub mod edit;
pub mod file;
pub mod git;
pub mod patch_format;
pub mod search;
pub mod shell;

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::application::tool_registry::{ToolEntry, ToolSource};
use crate::domain::tool::ToolOutcome;
use crate::domain::workspace::{is_protected_path, PathRejected, WorkspacePath, WorkspaceRoot};

/// 内置工具来源：把 [`all_tools`] 包成具名来源。
///
/// 装配方把它与 MCP 等来源一起交给
/// [`ToolRegistry::from_sources`](crate::application::tool_registry::ToolRegistry::from_sources)。
/// 工具来源在装配层可见，注册表不需要知道。
pub struct BuiltinToolSource {
    root: WorkspaceRoot,
}

impl BuiltinToolSource {
    /// 用 workspace 根构造：内置工具全部限定在这个根内。
    pub fn new(root: WorkspaceRoot) -> Self {
        Self { root }
    }
}

impl ToolSource for BuiltinToolSource {
    fn name(&self) -> &str {
        "builtin"
    }

    fn tools(&self) -> Vec<ToolEntry> {
        all_tools(self.root.clone())
    }
}

/// 组装全部内置工具。
///
/// 规格与执行入口在这里绑定为 [`ToolEntry`]。调用方（`main.rs`、集成
/// 测试）拿到后可直接装入
/// [`crate::application::tool_registry::ToolRegistry`]，不会出现"有 spec
/// 但没有执行入口"的登记项。
pub fn all_tools(root: WorkspaceRoot) -> Vec<ToolEntry> {
    vec![
        // 文件工具：读、列、整体写。各自独立成结构体，配置只有 root。
        ToolEntry {
            spec: file::ReadFile::spec(),
            invoker: Arc::new(file::ReadFile::new(root.clone())),
        },
        ToolEntry {
            spec: file::ReadFiles::spec(),
            invoker: Arc::new(file::ReadFiles::new(root.clone())),
        },
        ToolEntry {
            spec: file::ListDir::spec(),
            invoker: Arc::new(file::ListDir::new(root.clone())),
        },
        ToolEntry {
            spec: file::WriteFile::spec(),
            invoker: Arc::new(file::WriteFile::new(root.clone())),
        },
        ToolEntry {
            spec: search::SearchText::spec(),
            invoker: Arc::new(search::SearchText::new(root.clone())),
        },
        ToolEntry {
            spec: edit::EditFile::spec(),
            invoker: Arc::new(edit::EditFile::new(root.clone())),
        },
        // apply_patch 紧挨 edit_file：一个改一处，一个改多处。两个改文件的
        // 工具相邻，模型更容易注意到分工。
        ToolEntry {
            spec: apply_patch::ApplyPatch::spec(),
            invoker: Arc::new(apply_patch::ApplyPatch::new(root.clone())),
        },
        ToolEntry {
            spec: shell::RunCommand::spec(),
            invoker: Arc::new(shell::RunCommand::new(root.clone())),
        },
        ToolEntry {
            spec: git::GitStatus::spec(),
            invoker: Arc::new(git::GitStatus::new(root.clone())),
        },
        ToolEntry {
            spec: git::GitDiff::spec(),
            invoker: Arc::new(git::GitDiff::new(root)),
        },
    ]
}

/// 把模型给出的 JSON 参数反序列化为具体工具的参数结构。
///
/// 解析失败归为 `failed`，不归为 `refused`：参数错误是模型可以自行修正
/// 的普通错误。错误信息带上 serde 的说明，模型重试时知道缺了什么。
pub(crate) fn parse_args<A: DeserializeOwned>(arguments: &Value) -> Result<A, String> {
    serde_json::from_value(arguments.clone()).map_err(|error| format!("参数解析失败: {error}"))
}

/// 解析 workspace 内的相对路径，越界或受保护时返回拒绝观察。
///
/// 所有操作文件的工具共用这条校验路径，"越界即拒绝、受保护即拒绝"只有
/// 一种实现。
pub(crate) fn resolve_in(root: &WorkspaceRoot, raw: &str) -> Result<WorkspacePath, ToolOutcome> {
    let resolved =
        WorkspacePath::resolve(root, raw).map_err(|rejected| ToolOutcome::refused(rejected.to_string()))?;
    reject_links_out_of_scope(resolved.as_path(), root.as_path(), raw)
        .map_err(|rejected| ToolOutcome::refused(rejected.to_string()))?;
    Ok(resolved)
}

/// 取 `resolved` 的物理路径：从它开始向上找第一个真实存在的祖先，
/// 规范化后把剩余后缀接回。
///
/// 不能只 canonicalize `resolved` 本身：新建文件时目标尚不存在，
/// canonicalize 会失败，而链接可能藏在已存在的父目录上。
/// `write_file("jgit/evil")` 里的 `jgit` 是 junction，文件本身不存在。
fn physical_path(resolved: &Path) -> Option<PathBuf> {
    let mut tail: Vec<OsString> = Vec::new();
    let mut current = resolved.to_path_buf();
    loop {
        if let Ok(canonical) = std::fs::canonicalize(&current) {
            return Some(tail.iter().rev().fold(canonical, |path, part| path.join(part)));
        }
        tail.push(current.file_name()?.to_os_string());
        if !current.pop() {
            return None;
        }
    }
}

/// 解析路径里的符号链接与 junction，再校验一次边界。
///
/// [`WorkspacePath::resolve`] 是纯词法校验：领域层不碰文件系统，这是项目
/// 的分层边界。词法校验管不住链接。在 workspace 里给 `.git` 建一个
/// junction（`mklink /J jgit .git`，不需要管理员权限），词法上
/// `jgit/config` 完全合法，落盘时却写在真实的 `.git/config` 上。agent
/// 因此能读到 `.git/config`（常含带令牌的 remote URL），也能改写版本库
/// 元数据和自己的审计证据，而审批界面只显示 `jgit/config`。
///
/// 链接只能在这一层解析，因为 canonicalize 需要读文件系统。
///
/// 规范化失败（路径不存在、盘符不对）时不追加拒绝：新建文件时目标本来
/// 就不存在，词法校验的结论仍然成立，多拒一次只会挡掉正常写入。
fn reject_links_out_of_scope(resolved: &Path, root: &Path, raw: &str) -> Result<(), PathRejected> {
    let Some(physical) = physical_path(resolved) else {
        return Ok(());
    };
    let Ok(canonical_root) = std::fs::canonicalize(root) else {
        return Ok(());
    };
    let Ok(relative) = physical.strip_prefix(&canonical_root) else {
        return Err(PathRejected::Escape { raw: raw.to_owned() });
    };
    // 规范化后重新判断受保护：链接指向 .git 时，物理路径的首组件是
    // .git，词法一侧看到的却是 junction 的名字。
    if is_protected_path(relative) {
        return Err(PathRejected::Protected { raw: raw.to_owned() });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;

    /// 建一个真实 workspace：包含 `.git`、`.env`、`.qcodeharness`，各放
    /// 一份可识别的内容。这些测试依赖真实文件系统，mock 路径没有意义。
    fn real_workspace(name: &str) -> (PathBuf, WorkspaceRoot) {
        let dir = std::env::temp_dir().join(format!("qch-links-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join(".git")).unwrap();
        fs::create_dir_all(dir.join(".qcodeharness/sessions")).unwrap();
        fs::write(dir.join(".git/config"), "SECRET_GIT_CONFIG\n").unwrap();
        fs::write(dir.join(".env"), "SECRET_TOKEN=abc123\n").unwrap();
        fs::write(
            dir.join(".qcodeharness/sessions/s1.jsonl"),
            "SECRET_SESSION_EVIDENCE\n",
        )
        .unwrap();
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::write(dir.join("src/main.rs"), "fn main() {}\n").unwrap();
        let root = WorkspaceRoot::new(&dir);
        (dir, root)
    }

    /// junction 是这类攻击的载体：`mklink /J` 即可创建，不需要管理员权限。
    fn link_dir(workspace: &Path, link: &str, target: &Path) {
        let output = Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(workspace.join(link))
            .arg(target)
            .output()
            .expect("调起 cmd 建 junction");
        assert!(
            output.status.success(),
            "建 junction 失败: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// 词法校验管不住 junction：`.git` 的链接建在 workspace 里，
    /// `jgit/config` 词法合法，落盘却写在真实的 `.git/config` 上。
    ///
    /// 读方向能拿到 `.git/config` 的内容（常含带令牌的 remote URL），
    /// 写方向能改写版本库元数据。审批界面只显示 `jgit/config`，两步都
    /// 看不出在操作 `.git`。
    #[test]
    fn a_junction_into_a_protected_directory_is_refused() {
        let (dir, root) = real_workspace("junction");
        link_dir(&dir, "jgit", &dir.join(".git"));
        link_dir(&dir, "jq", &dir.join(".qcodeharness"));

        for path in [
            "jgit/config",
            "jgit",
            "jq/sessions/s1.jsonl",
            // 多套一层目录也一样：链接在中间那段。
            "jgit/refs/heads/main",
        ] {
            let refused = resolve_in(&root, path).expect_err("经 junction 落到受保护路径，应被拒绝");
            assert!(
                refused.content.contains("受保护"),
                "`{path}` 应按受保护拒绝，实际: {}",
                refused.content
            );
        }

        // 反向：workspace 内的正常文件不受影响。
        assert!(resolve_in(&root, "src/main.rs").is_ok(), "正常文件应放行");
        let _ = fs::remove_dir_all(&dir);
    }

    /// 链接指向 workspace 外同样拒绝：`jgit` 指向 `..\outside`，
    /// 词法上 `jgit/x` 仍是普通相对路径。
    #[test]
    fn a_junction_out_of_the_workspace_is_refused() {
        let (dir, root) = real_workspace("escape");
        let outside = std::env::temp_dir().join(format!("qch-outside-{}", std::process::id()));
        let _ = fs::remove_dir_all(&outside);
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("secret.txt"), "OUTSIDE\n").unwrap();
        link_dir(&dir, "jout", &outside);

        let refused = resolve_in(&root, "jout/secret.txt").expect_err("经 junction 逃出 workspace 应被拒绝");
        assert!(
            refused.content.contains("只允许"),
            "应按越界拒绝，实际: {}",
            refused.content
        );
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&outside);
    }

    /// 不存在的路径不追加拒绝：新建文件时目标本来就不存在，
    /// 多拒一次只会挡掉正常写入。
    #[test]
    fn a_path_that_does_not_exist_yet_is_still_allowed() {
        let (dir, root) = real_workspace("newfile");

        assert!(resolve_in(&root, "src/brand_new.rs").is_ok(), "新文件应放行");
        assert!(resolve_in(&root, "new_dir/x.txt").is_ok(), "新目录下的新文件应放行");
        let _ = fs::remove_dir_all(&dir);
    }

    /// 新建文件的目标藏在 junction 后面也要拒绝：文件本身尚不存在，
    /// canonicalize 它自己会失败，必须从已存在的父目录向上查找链接。
    #[test]
    fn a_new_file_behind_a_junction_is_refused() {
        let (dir, root) = real_workspace("newfile-junction");
        link_dir(&dir, "jgit", &dir.join(".git"));

        let refused =
            resolve_in(&root, "jgit/hooks/pre-commit").expect_err("经 junction 往 .git 里写新文件应被拒绝");
        assert!(
            refused.content.contains("受保护"),
            "应按受保护拒绝，实际: {}",
            refused.content
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// `read_files` 必须登记进注册表，且紧挨 `read_file`。
    ///
    /// 漏登记的表现是"模型看得见却调不动"：规格由同一个 `spec()` 产出，
    /// 所以看得见；执行入口没注册时，调用只会得到一条"未注册"的拒绝。
    /// 相邻放置让工具清单更连贯，两个读工具分居两端时，模型更难注意到
    /// 批量入口。
    #[test]
    fn read_files_is_registered_next_to_read_file() {
        let names: Vec<String> = all_tools(WorkspaceRoot::new(std::env::temp_dir()))
            .into_iter()
            .map(|entry| entry.spec.name)
            .collect();

        let batch = names
            .iter()
            .position(|name| name == "read_files")
            .expect("read_files 应登记进注册表");
        let single = names
            .iter()
            .position(|name| name == "read_file")
            .expect("read_file 应登记进注册表");
        assert_eq!(batch, single + 1, "read_files 应紧挨 read_file: {names:?}");
    }

    /// `apply_patch` 必须登记进注册表，且紧挨 `edit_file`。
    ///
    /// 理由同 read_files。相邻放置让"改一处"与"改多处"两个工具在清单上
    /// 连贯，模型更容易注意到分工。
    #[test]
    fn apply_patch_is_registered_next_to_edit_file() {
        let names: Vec<String> = all_tools(WorkspaceRoot::new(std::env::temp_dir()))
            .into_iter()
            .map(|entry| entry.spec.name)
            .collect();

        let patch = names
            .iter()
            .position(|name| name == "apply_patch")
            .expect("apply_patch 应登记进注册表");
        let edit = names
            .iter()
            .position(|name| name == "edit_file")
            .expect("edit_file 应登记进注册表");
        assert_eq!(patch, edit + 1, "apply_patch 应紧挨 edit_file: {names:?}");
    }

    /// 工具描述之间互相指路。
    ///
    /// 模型对工具分工的认识全部来自描述：读工具要说明"改代码用
    /// apply_patch"，写工具要说明"改多处用 apply_patch"，apply_patch
    /// 要指回 edit_file 与 write_file。缺任何一环，模型会在该用补丁时
    /// 逐处 edit_file，或用 write_file 整体覆盖。
    #[test]
    fn tool_descriptions_cross_reference_apply_patch() {
        for name in ["read_file", "read_files", "edit_file", "write_file"] {
            let spec = all_tools(WorkspaceRoot::new(std::env::temp_dir()))
                .into_iter()
                .find(|entry| entry.spec.name == name)
                .expect("工具应已登记")
                .spec;
            assert!(
                spec.description.contains("apply_patch"),
                "{name} 的描述应指路 apply_patch: {}",
                spec.description
            );
        }

        let patch = all_tools(WorkspaceRoot::new(std::env::temp_dir()))
            .into_iter()
            .find(|entry| entry.spec.name == "apply_patch")
            .expect("apply_patch 应已登记")
            .spec;
        for referenced in ["edit_file", "write_file"] {
            assert!(
                patch.description.contains(referenced),
                "apply_patch 的描述应指路 {referenced}: {}",
                patch.description
            );
        }
    }
}

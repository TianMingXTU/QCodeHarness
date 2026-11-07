//! Workspace 抽象：受控执行环境的根目录与路径。
//!
//! 核心不变量：所有文件操作必须限制在 workspace 内，拒绝绝对路径和 `..` 等越界成分。
//! 这个不变量通过 `WorkspacePath` 类型在构造时强制执行——拿到一个
//! `WorkspacePath` 就等于拿到"已经验过界"的保证，下游不必再检查一遍。

use std::path::{Component, Path, PathBuf};

/// 工作区根目录。
///
/// 只做"基准点"，不校验存在性、也不做规范化：目录由 `interface` 层从进程
/// 工作目录取得并交进来，领域层不该去碰文件系统。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceRoot(PathBuf);

impl WorkspaceRoot {
    /// 用给定目录作为根。
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self(path.into())
    }

    /// 只读访问根目录路径。
    pub fn as_path(&self) -> &Path {
        &self.0
    }
}

/// workspace 路径被拒绝的原因。
///
/// 分成两个变体不是给模型看两种措辞，而是让调用方与测试能区分"路径算错了"
/// 和"这条路本来就不许走"：越界换个写法可能就合法，受保护路径则不会——
/// 把后者说成前者，会引导模型反复重写路径去撞同一堵墙。
#[derive(Debug, thiserror::Error)]
pub enum PathRejected {
    /// 路径试图逃出 workspace：绝对路径、`..`、根目录或盘符前缀。
    #[error("拒绝访问 `{raw}`：只允许 workspace 内的相对路径")]
    Escape {
        /// 模型给出的原始路径文本，原样回显便于模型自行纠正。
        raw: String,
    },
    /// 路径落在受保护区域内，agent 不可读写，也不因策略放行而解禁。
    #[error("拒绝访问 `{raw}`：受保护路径，agent 不可读写")]
    Protected {
        /// 模型给出的原始路径文本。
        raw: String,
    },
}

/// 判断单个路径组件的名字是否命中受保护名单。
///
/// 末尾的空格和点要先剥掉：NTFS 等文件系统就是这么做的，`.env ` 与
/// `.env` 落在磁盘上是同一个文件。不先归一化，模型把路径写成 `.env `
/// 就能读到凭据文件、覆盖 `.env`，而审批界面上显示的仍是 `.env `——
/// 人很难认出那就是 `.env`。这不是假想的攻击面，是 Windows 路径解析的
/// 既定行为。
fn is_protected_name(name: &str) -> bool {
    let name = name.trim_end_matches([' ', '.']).to_ascii_lowercase();
    matches!(name.as_str(), ".git" | ".qcodeharness" | ".env") || name.starts_with(".env.")
}

/// 判断一条 workspace 相对路径是否落在受保护区域内。
///
/// `.git` 是版本库元数据（改它就是改历史），`.qcodeharness` 是本工具自己的
/// 会话与快照（改它就是改审计证据），`.env*` 按约定放本地凭据。三者被 agent
/// 碰到都意味着运行状态被污染或凭据外泄，因此读和写一并拒绝。
///
/// 只看首组件：`sub/.git` 是子项目自己的仓库，不在保护范围——保护的是
/// workspace 根上那几个 agent 不该碰的位置，不是所有同名目录。
///
/// `.` 要跳过：`./.git/config` 与 `.git/config` 指向同一处，不能因为多了
/// 一层当前目录前缀就绕过保护。
///
/// 大小写不敏感：Windows 与 macOS 的文件系统默认如此，`.GIT/config` 与
/// `.git/config` 是同一处。若按字节精确比较，把路径改写成大写就能读到
/// `.env`、改写 `.git/config`——而"这两个位置 agent 碰不得"正是这里唯一
/// 要守的东西。比较前统一转小写（ASCII 小写化不改变字节长度，分配一次）。
///
/// 单独公开而不是藏在 `resolve` 里：infrastructure 层解析完符号链接后，
/// 要拿物理路径再问一次这个问题（见 `tool::resolve_in`）。
pub fn is_protected_path(path: &Path) -> bool {
    let first = path
        .components()
        .find(|component| !matches!(component, Component::CurDir));
    match first {
        Some(Component::Normal(name)) => is_protected_name(&name.to_string_lossy()),
        _ => false,
    }
}

/// workspace 内的一条已验证路径。
///
/// 字段私有：只能通过 [`WorkspacePath::resolve`] 构造，因此不可能存在
/// 绕过校验的实例。这是"构造即合法"的含义。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspacePath(PathBuf);

impl WorkspacePath {
    /// 把模型给出的路径解析到 workspace 内。
    ///
    /// 这里只做词法校验，不解析符号链接；符号链接与 junction 的物理
    /// 目标由 infrastructure 层解析后再问一次 [`is_protected_path`]
    /// （见 `tool::resolve_in`），两层合起来才是完整的越界防线。
    ///
    /// 受保护路径（见 [`is_protected`]）在这里一并拒绝，而不是留给策略层：
    /// 策略是给使用者调配的调剂，而"agent 不能改版本库元数据和审计证据"
    /// 是不该被任何配置放宽的底线。
    ///
    /// # Errors
    ///
    /// 路径为绝对路径、或含 `..` / 根 / 前缀等越界成分时返回
    /// [`PathRejected::Escape`]；路径落在受保护区域时返回
    /// [`PathRejected::Protected`]。
    pub fn resolve(root: &WorkspaceRoot, raw: &str) -> Result<Self, PathRejected> {
        let relative = Path::new(raw);
        let escapes = relative.is_absolute()
            || relative.components().any(|component| {
                matches!(
                    component,
                    Component::ParentDir | Component::RootDir | Component::Prefix(_)
                )
            });
        if escapes {
            return Err(PathRejected::Escape { raw: raw.to_owned() });
        }
        if is_protected_path(relative) {
            return Err(PathRejected::Protected { raw: raw.to_owned() });
        }
        Ok(Self(root.0.join(relative)))
    }

    /// 只读访问已解析的绝对路径。
    pub fn as_path(&self) -> &Path {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 词法校验不要求 workspace 真实存在。
    fn workspace() -> WorkspaceRoot {
        WorkspaceRoot::new("D:/tmp/demo-workspace")
    }

    #[test]
    fn relative_path_joins_workspace() {
        let resolved = WorkspacePath::resolve(&workspace(), "src/main.rs").unwrap();
        assert_eq!(resolved.as_path(), workspace().as_path().join("src/main.rs"));
    }

    #[test]
    fn rejects_absolute_paths() {
        assert!(WorkspacePath::resolve(&workspace(), "C:/Windows/system32").is_err());
        // `/etc/passwd` 在 Windows 上不是 is_absolute，但含 RootDir，同样要拒绝。
        assert!(WorkspacePath::resolve(&workspace(), "/etc/passwd").is_err());
    }

    #[test]
    fn rejects_parent_dir_escape() {
        assert!(WorkspacePath::resolve(&workspace(), "../secrets.txt").is_err());
        assert!(WorkspacePath::resolve(&workspace(), "src/../../outside.txt").is_err());
    }

    #[test]
    fn allows_current_dir_component() {
        let resolved = WorkspacePath::resolve(&workspace(), "./README.md").unwrap();
        assert_eq!(resolved.as_path(), workspace().as_path().join("./README.md"));
    }

    // ── 受保护路径 ────────────────────────────────────────────────────

    /// 受保护路径的真值表：`.git`、`.qcodeharness`、`.env*` 一律拒，
    /// 名字相似但不是它们的（`.gitignore`、`.github`、`environments/`）一律放。
    /// 大小写变形不能绕过保护。
    ///
    /// Windows 与 macOS 的文件系统默认大小写不敏感：`.GIT/config` 与
    /// `.git/config` 指向同一处。`is_protected` 若只做精确比较，模型把
    /// 路径改写成大写就能读到 `.env`、改写 `.git/config`——而受保护路径
    /// 的全部意义就是"这两个位置 agent 碰不得"。
    #[test]
    fn protected_paths_are_matched_case_insensitively() {
        for rejected in [
            ".GIT/config",
            ".Git/config",
            ".ENV",
            ".Env.local",
            ".QCODEHARNESS/sessions/x.jsonl",
            "./.GIT/config",
        ] {
            assert!(
                WorkspacePath::resolve(&workspace(), rejected).is_err(),
                "`{rejected}` 是受保护路径的大小写变形，应被拒绝"
            );
        }
        // 反向：只是名字长得像的仍然放行。
        for allowed in [".gitignore", ".GITHUB/workflows/ci.yml", ".ENVRC.d/keep.js"] {
            assert!(
                WorkspacePath::resolve(&workspace(), allowed).is_ok(),
                "`{allowed}` 不该被误杀"
            );
        }
    }

    #[test]
    fn protected_paths_are_rejected_and_lookalikes_are_not() {
        for rejected in [
            ".git/config",
            ".git",
            "./.git/config",
            ".qcodeharness/sessions/x.jsonl",
            ".env",
            ".env.local",
        ] {
            assert!(
                WorkspacePath::resolve(&workspace(), rejected).is_err(),
                "`{rejected}` 应被拒绝"
            );
        }
        for allowed in [
            ".gitignore",
            ".github/workflows/ci.yml",
            // 首组件是 environments 而不是 .env：凭据文件放进普通目录不该被误杀。
            "environments/prod.txt",
            // .envrc.d 以 .env 开头但不是 .env 或 .env.*，放行。
            ".envrc.d/keep.txt",
            "src/main.rs",
        ] {
            assert!(
                WorkspacePath::resolve(&workspace(), allowed).is_ok(),
                "`{allowed}` 不应被误杀"
            );
        }
    }

    /// 尾随空格和点不能绕过保护。
    ///
    /// NTFS 会剥掉路径组件末尾的空格和点，`.env ` 与 `.env` 落在磁盘上是
    /// 同一个文件。`read_file(".env ")` 若不做归一化就会成功返回凭据内容，
    /// `write_file(".env ", ...)` 就能覆盖真实的 `.env`；审批界面上显示的
    /// 路径是 `".env "`，人几乎不可能认出它就是 `.env`。
    #[test]
    fn protected_names_survive_trailing_spaces_and_dots() {
        for rejected in [".env ", ".ENV  ", ".env .", ".env.", ".env..", ".git ", ".GIT ."] {
            assert!(
                WorkspacePath::resolve(&workspace(), rejected).is_err(),
                "`{rejected}` 归一化后命中受保护名单，应被拒绝"
            );
        }
        // 反向：尾巴上没有空格/点的，以及剥完之后仍然不像的，都要放行。
        for allowed in [".envx", ".envrc", ".envrc.d/keep.txt", "notes .txt", "src/main.rs"] {
            assert!(
                WorkspacePath::resolve(&workspace(), allowed).is_ok(),
                "`{allowed}` 不该被误杀"
            );
        }
    }

    /// 受保护与越界必须是两种拒绝：模型看到的信息不同，它能采取的行动也不同。
    ///
    /// 越界是"路径算错了"，换个写法可能就对了；受保护是"这条路本来就不许走"，
    /// 引导它重写只会换来一轮无用的重试。
    #[test]
    fn protected_path_reports_a_reason_distinct_from_escape() {
        let protected = WorkspacePath::resolve(&workspace(), ".git/config").unwrap_err();
        assert!(
            matches!(protected, PathRejected::Protected { .. }),
            "`.git/config` 应按受保护拒绝，实际: {protected}"
        );
        assert!(protected.to_string().contains("受保护"), "{protected}");

        let escaped = WorkspacePath::resolve(&workspace(), "../outside.txt").unwrap_err();
        assert!(
            matches!(escaped, PathRejected::Escape { .. }),
            "越界仍是越界，实际: {escaped}"
        );
    }
}

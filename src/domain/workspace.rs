//! Workspace 抽象：受控执行环境的根目录与路径。
//!
//! 核心不变量：所有文件操作限制在 workspace 内，拒绝绝对路径和 `..`
//! 等越界成分。该不变量由 `WorkspacePath` 在构造时强制保证，持有实例
//! 即表示路径已验证，下游无需重复检查。

use std::path::{Component, Path, PathBuf};

/// 工作区根目录。
///
/// 仅作为基准点，不校验存在性，也不做规范化。目录由 `interface` 层从
/// 进程工作目录取得后传入，领域层不访问文件系统。
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
/// 两个变体让调用方与测试区分"路径写法错误"与"路径本来就不允许访问"：
/// 越界换个写法可能合法，受保护路径不会。把受保护路径报成越界，会引导
/// 模型反复重写路径。
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
/// 先剥离末尾的空格和点：NTFS 等文件系统按此规则解析，`.env ` 与
/// `.env` 指向同一文件。不归一化时，模型写成 `.env ` 即可读到凭据
/// 文件或覆盖 `.env`，而审批界面显示的仍是 `.env `，用户难以辨认。
/// 这是 Windows 路径解析的既有行为，实际可利用。
///
/// 剥离在所有平台都执行，口径与下面的大小写折叠一致：Windows 是风险
/// 的实际来源，Linux 上剥离只是多拒几个形似 `.env` 的路径，代价是一次
/// 误拒。
fn is_protected_name(name: &str) -> bool {
    let name = name.trim_end_matches([' ', '.']).to_ascii_lowercase();
    matches!(name.as_str(), ".git" | ".qcodeharness" | ".env") || name.starts_with(".env.")
}

/// 判断一条 workspace 相对路径是否落在受保护区域内。
///
/// `.git` 是版本库元数据，修改会影响历史；`.qcodeharness` 保存本工具的
/// 会话与快照，修改会影响审计证据；`.env*` 按约定存放本地凭据。agent
/// 访问这三类路径都会污染运行状态或泄露凭据，因此读和写一并拒绝。
///
/// 只检查首组件：`sub/.git` 是子项目自己的仓库，不在保护范围内。保护的
/// 是 workspace 根上的这些位置，不是所有同名目录。
///
/// 跳过 `.` 组件：`./.git/config` 与 `.git/config` 指向同一位置，不能
/// 因为多了当前目录前缀而绕过保护。
///
/// 大小写不敏感：Windows 与 macOS 的文件系统默认如此，`.GIT/config` 与
/// `.git/config` 指向同一位置。按字节精确比较时，改写大小写即可访问
/// `.env` 或 `.git/config`。比较前统一转小写（ASCII 小写化不改变字节
/// 长度，只需一次分配）。
///
/// 所有平台都按不敏感比较，不只 Windows 与 macOS：Linux 上多拒掉几个
/// 形似 `.GIT` 的路径，代价是一次误拒；少拒掉一个则可能泄露凭据。
/// 方向不对称时取保守的一边。
///
/// 单独公开供 infrastructure 层使用：解析符号链接后需要拿物理路径再
/// 检查一次（见 `tool::resolve_in`）。
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
/// 字段私有，只能通过 [`WorkspacePath::resolve`] 构造，不存在绕过校验
/// 的实例。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspacePath(PathBuf);

impl WorkspacePath {
    /// 把模型给出的路径解析到 workspace 内。
    ///
    /// 只做词法校验，不解析符号链接。符号链接与 junction 的物理目标由
    /// infrastructure 层解析后再检查一次 [`is_protected_path`]（见
    /// `tool::resolve_in`），两层共同构成完整的越界防线。
    ///
    /// 受保护路径（见 [`is_protected_path`]）在这里拒绝，不交给策略层。
    /// 策略允许用户按需调整，而"agent 不能修改版本库元数据和审计证据"
    /// 的底线不能被任何配置放宽。
    ///
    /// # Errors
    ///
    /// 路径为绝对路径，或含 `..`、根、前缀等越界成分时返回
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
    use crate::domain::platform::Platform;

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
        // 带根分量的路径在任何平台上都是绝对路径，必须被拒绝。
        assert!(
            WorkspacePath::resolve(&workspace(), "/etc/passwd").is_err(),
            "带根分量的路径必须被拒绝"
        );
        // 盘符路径只在 Windows 上算绝对路径。POSIX 上 `C:/Windows/system32`
        // 是普通相对路径，落在 workspace 内，不是越界——判据跟着平台语义走。
        let drive_path = "C:/Windows/system32";
        if Platform::current() == Platform::Windows {
            assert!(
                WorkspacePath::resolve(&workspace(), drive_path).is_err(),
                "Windows 上盘符路径是绝对路径，必须被拒绝"
            );
        } else {
            let resolved = WorkspacePath::resolve(&workspace(), drive_path)
                .expect("POSIX 上盘符路径是相对路径，落在 workspace 内");
            assert_eq!(resolved.as_path(), workspace().as_path().join(drive_path));
        }
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

    /// 受保护路径真值表：`.git`、`.qcodeharness`、`.env*` 被拒绝，
    /// 名字相似的 `.gitignore`、`.github`、`environments/` 放行。
    /// 大小写变形不能绕过保护。
    ///
    /// Windows 与 macOS 的文件系统默认大小写不敏感，`.GIT/config` 与
    /// `.git/config` 指向同一位置。Linux 上两者是不同的文件，但保护按
    /// 同一口径比对：宁可多拒，不能给大小写绕过留口子。
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
        // 反向用例：仅名字相似的路径放行。
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
            // 首组件是 environments，不是 .env：普通目录不应被误判。
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
    /// NTFS 会剥离路径组件末尾的空格和点，`.env ` 与 `.env` 指向同一
    /// 文件。不做归一化时，`read_file(".env ")` 会返回凭据内容，
    /// `write_file(".env ", ...)` 会覆盖真实的 `.env`，而审批界面显示的
    /// 路径是 `".env "`，用户很难认出它。
    #[test]
    fn protected_names_survive_trailing_spaces_and_dots() {
        for rejected in [".env ", ".ENV  ", ".env .", ".env.", ".env..", ".git ", ".GIT ."] {
            assert!(
                WorkspacePath::resolve(&workspace(), rejected).is_err(),
                "`{rejected}` 归一化后命中受保护名单，应被拒绝"
            );
        }
        // 反向用例：末尾无空格或点、剥离后不匹配的路径都放行。
        for allowed in [".envx", ".envrc", ".envrc.d/keep.txt", "notes .txt", "src/main.rs"] {
            assert!(
                WorkspacePath::resolve(&workspace(), allowed).is_ok(),
                "`{allowed}` 不该被误杀"
            );
        }
    }

    /// 受保护与越界返回不同的拒绝类型，模型可据此采取不同行动。
    ///
    /// 越界是路径写法错误，换个写法可能合法；受保护路径不能访问，引导
    /// 模型重写只会产生无效重试。
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

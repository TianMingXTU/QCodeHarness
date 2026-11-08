//! 工作区清单扫描：把 workspace 里的文件扫成一份可比对的清单。
//!
//! 用于任务级恢复前的"工作区有没有被人动过"检查（见
//! `domain::worktree`）。这里负责 IO：遍历目录、读文件、计算指纹、
//! 按路径排序。
//!
//! 跳过以下内容：
//!
//! - `.git`、`.qcodeharness`：版本库元数据与本工具的运行状态。它们每次
//!   运行都在变化，计入会让每次恢复都报告"工作区被改过"，检查随即失效。
//! - `target`：构建产物。体积大、变动频繁，而且不是人改的。
//! - 超过大小上限的文件：几百兆的文件读进内存计算指纹没有意义，还会把
//!   一次检查拖成一次长任务。

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::application::worktree::WorktreeScanner;
use crate::domain::worktree::{content_hash, WorktreeEntry, WorktreeManifest};

/// 参与清单的文件大小上限。超过它只记路径与大小，不算内容指纹。
pub const MAX_HASHED_FILE_BYTES: u64 = 4 * 1024 * 1024;

/// 扫描时跳过的目录名。
///
/// 这些目录不进入清单：它们是版本库元数据、构建产物或本工具的运行状态，
/// 都不能作为"人改过工作区"的证据。
const SKIPPED_DIRS: &[&str] = &[".git", ".qcodeharness", "target"];

/// 扫描 workspace，返回清单。
///
/// 读取失败的文件（权限不足、被占用）不中断扫描，跳过并继续。清单因此
/// 可能少几个文件。对恢复检查的含义：若同一文件在快照扫描与恢复扫描时
/// 都读失败，它的真实改动不会体现在 diff 里，检查可能得出"没改动"。
/// 调用方（编排器）在快照清单为空或扫描失败时已降级为警告，这里的跳过
/// 只影响单个文件，比整份清单缺失的影响小。
///
/// # Errors
///
/// workspace 根目录无法读取时返回 IO 错误。
pub fn scan_workspace(root: &Path) -> std::io::Result<WorktreeManifest> {
    // 根目录要单独确认可读：整份清单都来自这里。像子目录那样"读不出
    // 就跳过"，会把"无法检查"伪装成"工作区为空"，恢复端会拿空清单比对
    // 出"没改动"。
    let _ = fs::read_dir(root)?;
    let mut entries = Vec::new();
    walk(root, root, &mut entries)?;
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(WorktreeManifest { entries })
}

/// 递归遍历一个目录，把文件收进 `entries`。
fn walk(root: &Path, dir: &Path, entries: &mut Vec<WorktreeEntry>) -> std::io::Result<()> {
    let read = match fs::read_dir(dir) {
        Ok(read) => read,
        // 目录读不出来（权限、被删）：跳过这一支，继续扫别的。
        Err(_) => return Ok(()),
    };
    for entry in read {
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();

        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            if SKIPPED_DIRS.contains(&name.as_ref()) {
                continue;
            }
            walk(root, &path, entries)?;
            continue;
        }
        // 不跟随符号链接：跟随可能走出 workspace，也可能成环。链接本身
        // 也不进清单，它不是独立的内容。
        if !file_type.is_file() {
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        let size = metadata.len();
        let relative = relative_path(root, &path);
        // 超大文件只记路径与大小。内容哈希需要把整个文件读进内存，且
        // 对"有没有被人动过"没有额外贡献。
        let hash = if size > MAX_HASHED_FILE_BYTES {
            0
        } else {
            match fs::read(&path) {
                Ok(bytes) => content_hash(&bytes),
                Err(_) => continue,
            }
        };
        entries.push(WorktreeEntry { path: relative, hash, size });
    }
    Ok(())
}

/// 相对 workspace 根的路径，分隔符统一成 `/`。
fn relative_path(root: &Path, path: &Path) -> String {
    let relative = path.strip_prefix(root).unwrap_or(path);
    relative
        .components()
        .map(|component| component.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

/// 以指定 workspace 根扫描的清单器。
///
/// 恢复时要扫描当时的工作区，不是当前进程的工作目录。基准跑在 fixture
/// 拷贝上，恢复也要扫描同一个拷贝。
#[derive(Debug, Clone)]
pub struct RootedWorktreeScanner {
    root: PathBuf,
}

impl RootedWorktreeScanner {
    /// 以 `root` 为工作区根构造。
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
}

impl WorktreeScanner for RootedWorktreeScanner {
    fn scan(&self) -> Result<WorktreeManifest, String> {
        scan_workspace(&self.root).map_err(|error| error.to_string())
    }
}

/// 永远返回空清单的扫描器。
///
/// 用于拿不到工作区根的场景（如单元测试里的编排器装配）。此时"工作区
/// 有没有被动过"无从判断，返回空清单表示"没发现改动"，检查不会误报。
#[derive(Debug, Clone, Default)]
pub struct NoopWorktreeScanner;

impl WorktreeScanner for NoopWorktreeScanner {
    fn scan(&self) -> Result<WorktreeManifest, String> {
        Ok(WorktreeManifest::default())
    }
}

/// 把扫描器包成 `Arc`，供依赖注入。
pub fn shared(scanner: impl WorktreeScanner + Send + Sync + 'static) -> Arc<dyn WorktreeScanner> {
    Arc::new(scanner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_workspace(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("qch-manifest-{}-{}", name, std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("建临时目录");
        dir
    }

    #[test]
    fn scan_finds_every_file_sorted_by_path() {
        let root = temp_workspace("basic");
        fs::write(root.join("b.txt"), "bbb").unwrap();
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::write(root.join("sub").join("c.txt"), "ccc").unwrap();
        fs::write(root.join("a.txt"), "aaa").unwrap();

        let manifest = scan_workspace(&root).unwrap();

        let paths: Vec<&str> = manifest.entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, vec!["a.txt", "b.txt", "sub/c.txt"], "应按路径排序");
        assert_eq!(manifest.len(), 3);
    }

    /// 两次扫描同一份工作区必须得到逐字相同的清单。
    ///
    /// 不一致时清单比对会报告"工作区被改过"，实际没有任何变化。检查一旦
    /// 误报，人就会忽略它。
    #[test]
    fn two_scans_of_the_same_tree_agree() {
        let root = temp_workspace("stable");
        fs::write(root.join("a.txt"), "aaa").unwrap();
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::write(root.join("sub").join("b.txt"), "bbb").unwrap();

        let first = scan_workspace(&root).unwrap();
        let second = scan_workspace(&root).unwrap();

        assert_eq!(first, second);
    }

    /// 内容变了一定要看得出来。
    #[test]
    fn a_changed_file_shows_up_as_a_modification() {
        let root = temp_workspace("changed");
        fs::write(root.join("a.txt"), "原内容").unwrap();
        let before = scan_workspace(&root).unwrap();

        fs::write(root.join("a.txt"), "新内容").unwrap();
        let after = scan_workspace(&root).unwrap();

        let changes = before.diff(&after);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].describe(), "修改 a.txt");
    }

    /// 版本库元数据、构建产物与本工具的状态都不进清单。
    ///
    /// 它们每次运行都在变化。计入后恢复检查永远有差异，这个检查就失效了。
    #[test]
    fn noisy_directories_are_skipped() {
        let root = temp_workspace("noisy");
        fs::write(root.join("keep.txt"), "内容").unwrap();
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::write(root.join(".git").join("HEAD"), "ref: refs/heads/main").unwrap();
        fs::create_dir_all(root.join("target")).unwrap();
        fs::write(root.join("target").join("big.bin"), "产物").unwrap();
        fs::create_dir_all(root.join(".qcodeharness")).unwrap();
        fs::write(root.join(".qcodeharness").join("state.json"), "{}").unwrap();

        let manifest = scan_workspace(&root).unwrap();

        let paths: Vec<&str> = manifest.entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, vec!["keep.txt"], "噪声目录不该进清单: {paths:?}");
    }

    /// 大文件不读进内存算指纹：只记路径与大小。
    #[test]
    fn oversized_files_are_listed_without_a_content_hash() {
        let root = temp_workspace("big");
        let big = vec![b'x'; (MAX_HASHED_FILE_BYTES + 1) as usize];
        fs::write(root.join("big.bin"), &big).unwrap();

        let manifest = scan_workspace(&root).unwrap();

        assert_eq!(manifest.len(), 1);
        assert_eq!(manifest.entries[0].hash, 0, "超大文件不算内容哈希");
        assert!(manifest.entries[0].size > MAX_HASHED_FILE_BYTES);
    }

    /// 空工作区扫出空清单，不报错。
    #[test]
    fn an_empty_workspace_scans_to_an_empty_manifest() {
        let root = temp_workspace("empty");
        let manifest = scan_workspace(&root).unwrap();
        assert!(manifest.is_empty());
    }
}

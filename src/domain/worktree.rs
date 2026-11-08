//! 工作区清单：判断工作区是否被改动过的纯数据。
//!
//! 任务级恢复的前提是工作区保持离开时的样子。中途被改过，恢复后的
//! 模型会按过期计划继续干活，看到的文件内容与磁盘不符。
//!
//! 选用清单，不用 `git status`：workspace 常常不是 git 仓库（基准的
//! fixture 拷贝、临时工作目录都是），git status 在那里只会报"不是
//! 仓库"，检查没有发生。清单不依赖版本控制；扫描的覆盖范围（跳过的
//! 目录、大小上限、读失败的文件）见 `infrastructure::worktree`。
//!
//! 扫描要碰文件系统（基础设施层），比对是纯函数。

use serde::{Deserialize, Serialize};

/// 工作区里某个文件的内容指纹。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeEntry {
    /// 相对 workspace 根的路径，分隔符统一为 `/`。
    ///
    /// Windows 与 Linux 的路径写法不同，统一分隔符后清单才能跨机器
    /// 比对。
    pub path: String,
    /// 内容指纹。
    pub hash: u64,
    /// 字节数，便于人眼核对差异。
    pub size: u64,
}

/// 工作区在某一时刻的清单。
///
/// 条目按路径排序：两次扫描同一份工作区必须得到完全相同的清单，否则
/// 扫描顺序的抖动会破坏"没动过"的判断。
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct WorktreeManifest {
    /// 参与检查的文件，按路径升序。
    pub entries: Vec<WorktreeEntry>,
}

/// 清单之间的一处差异。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorktreeChange {
    /// 快照之后新增的文件。
    Added {
        /// 相对路径。
        path: String,
    },
    /// 快照之后被删除的文件。
    Removed {
        /// 相对路径。
        path: String,
    },
    /// 快照之后内容变化的文件。
    Modified {
        /// 相对路径。
        path: String,
    },
}

impl WorktreeChange {
    /// 差异的一句话描述，给人看。
    pub fn describe(&self) -> String {
        match self {
            Self::Added { path } => format!("新增 {path}"),
            Self::Removed { path } => format!("删除 {path}"),
            Self::Modified { path } => format!("修改 {path}"),
        }
    }
}

impl WorktreeManifest {
    /// 比对两份清单，返回差异。
    ///
    /// `self` 是快照时的清单，`other` 是当前清单。返回空表示工作区
    /// 自快照以来未被改动。
    ///
    /// 两份清单都按路径升序，用双指针线性扫描即可，不必建哈希表。
    pub fn diff(&self, other: &WorktreeManifest) -> Vec<WorktreeChange> {
        let mut changes = Vec::new();
        let mut left = self.entries.iter().peekable();
        let mut right = other.entries.iter().peekable();
        loop {
            match (left.peek(), right.peek()) {
                (None, None) => break,
                (Some(old), None) => {
                    changes.push(WorktreeChange::Removed {
                        path: old.path.clone(),
                    });
                    left.next();
                }
                (None, Some(new)) => {
                    changes.push(WorktreeChange::Added {
                        path: new.path.clone(),
                    });
                    right.next();
                }
                (Some(old), Some(new)) => match old.path.cmp(&new.path) {
                    std::cmp::Ordering::Equal => {
                        if old.hash != new.hash {
                            changes.push(WorktreeChange::Modified {
                                path: old.path.clone(),
                            });
                        }
                        left.next();
                        right.next();
                    }
                    std::cmp::Ordering::Less => {
                        changes.push(WorktreeChange::Removed {
                            path: old.path.clone(),
                        });
                        left.next();
                    }
                    std::cmp::Ordering::Greater => {
                        changes.push(WorktreeChange::Added {
                            path: new.path.clone(),
                        });
                        right.next();
                    }
                },
            }
        }
        changes
    }

    /// 文件总数。给报表用。
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// 清单是否为空。
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// FNV-1a 64 位哈希。
///
/// 只用于判断文件内容是否变化，不是安全边界，无需密码学哈希。碰撞
/// 概率不影响"是否被改过"的判断。
///
/// 不引入 `sha2` 等依赖：为非安全用途的指纹增加一个加密库不划算。
pub fn content_hash(bytes: &[u8]) -> u64 {
    const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET_BASIS;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str, hash: u64) -> WorktreeEntry {
        WorktreeEntry {
            path: path.to_owned(),
            hash,
            size: 1,
        }
    }

    fn manifest(entries: Vec<WorktreeEntry>) -> WorktreeManifest {
        WorktreeManifest { entries }
    }

    #[test]
    fn identical_manifests_have_no_diff() {
        let base = manifest(vec![entry("a.txt", 1), entry("b.txt", 2)]);
        let now = manifest(vec![entry("a.txt", 1), entry("b.txt", 2)]);

        assert!(base.diff(&now).is_empty());
    }

    #[test]
    fn added_removed_and_modified_are_all_reported() {
        let base = manifest(vec![entry("a.txt", 1), entry("b.txt", 2)]);
        let now = manifest(vec![entry("a.txt", 9), entry("c.txt", 3)]);

        let changes = base.diff(&now);

        assert_eq!(
            changes,
            vec![
                WorktreeChange::Modified {
                    path: "a.txt".to_owned()
                },
                WorktreeChange::Removed {
                    path: "b.txt".to_owned()
                },
                WorktreeChange::Added {
                    path: "c.txt".to_owned()
                },
            ]
        );
        assert_eq!(changes[0].describe(), "修改 a.txt");
    }

    /// 两个空清单之间没有差异，对应工作区从未有过文件。
    #[test]
    fn empty_manifests_match() {
        assert!(manifest(vec![]).diff(&manifest(vec![])).is_empty());
    }

    /// 同一内容得到同一指纹，不同内容得到不同指纹。
    ///
    /// 指纹是判断"是否被改过"的唯一依据。同一内容得到不同指纹会让每次
    /// 恢复都误报改动；不同内容得到相同指纹会漏掉真实改动。两个方向都
    /// 要覆盖。
    #[test]
    fn content_hash_is_stable_and_discriminating() {
        assert_eq!(content_hash(b"hello"), content_hash(b"hello"));
        assert_ne!(content_hash(b"hello"), content_hash(b"hellp"));
        assert_ne!(content_hash(b"hello"), content_hash(b"hello "));
        // 空内容也有确定的指纹，不能与任何内容撞上。
        assert_ne!(content_hash(b""), content_hash(b"x"));
    }
}

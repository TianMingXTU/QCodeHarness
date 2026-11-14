//! 跨会话记忆文件的落盘实现。
//!
//! 索引与事实文件分别写临时文件后原子替换，避免中途失败留下半截索引。
//! 目录由调用方给出，测试可用独立临时目录；不在本模块猜测用户目录。

use std::fs;
use std::path::Path;

use crate::application::memory::MemoryWriter;
use crate::domain::memory::MemoryFile;

/// 把一条事实写入 `MEMORY.md` 与同名事实文件。
pub struct FileMemoryWriter;

impl MemoryWriter for FileMemoryWriter {
    fn write(&self, root: &Path, file: &MemoryFile) -> Result<(), String> {
        fs::create_dir_all(root).map_err(|error| format!("创建记忆目录失败: {error}"))?;
        let index = root.join("MEMORY.md");
        let fact = root.join(file.fact_relative_path());
        write_atomic(&index, &file.index_text_after())?;
        write_atomic(&fact, file.fact_text())?;
        Ok(())
    }
}

/// 同目录临时文件 + rename：Windows 上目标存在时 rename 失败，因此先删
/// 目标再改名；这个窗口内索引短暂缺失，比留下半截文本更可诊断。
fn write_atomic(path: &Path, text: &str) -> Result<(), String> {
    let temp = path.with_extension("tmp");
    fs::write(&temp, text).map_err(|error| format!("写 {} 失败: {error}", temp.display()))?;
    if path.exists() {
        fs::remove_file(path).map_err(|error| format!("替换 {} 失败: {error}", path.display()))?;
    }
    fs::rename(&temp, path).map_err(|error| format!("落盘 {} 失败: {error}", path.display()))?;
    Ok(())
}

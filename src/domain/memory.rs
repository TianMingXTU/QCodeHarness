//! 跨会话记忆文件的纯模型。
//!
//! 对齐 MEMORY.md 语义：索引文件一行一个事实链接，事实正文放在独立文件。
//! 本模块只处理文本格式与内容校验，不读写磁盘；落盘由基础设施完成。

use serde::{Deserialize, Serialize};

/// 记忆格式错误。空字段与不安全文件名都不得进入文件写入路径。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemoryFileError {
    EmptySlug,
    EmptySummary,
    EmptyFact,
    UnsafeSlug(String),
}

impl std::fmt::Display for MemoryFileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptySlug => write!(f, "记忆文件名不能为空"),
            Self::EmptySummary => write!(f, "索引摘要不能为空"),
            Self::EmptyFact => write!(f, "事实正文不能为空"),
            Self::UnsafeSlug(slug) => write!(f, "记忆文件名不安全: {slug}"),
        }
    }
}

/// 一条记忆事实。slug 同时是独立事实文件的名字。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryEntry {
    pub slug: String,
    pub summary: String,
    pub fact: String,
}

impl MemoryEntry {
    /// 校验字段。slug 只允许安全字符，避免写入索引目录之外的路径。
    pub fn validate(&self) -> Result<(), MemoryFileError> {
        if self.slug.trim().is_empty() {
            return Err(MemoryFileError::EmptySlug);
        }
        if !self
            .slug
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
        {
            return Err(MemoryFileError::UnsafeSlug(self.slug.clone()));
        }
        if self.summary.trim().is_empty() {
            return Err(MemoryFileError::EmptySummary);
        }
        if self.fact.trim().is_empty() {
            return Err(MemoryFileError::EmptyFact);
        }
        Ok(())
    }
}

/// MEMORY.md 的一行索引。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryIndexLine {
    pub slug: String,
    pub summary: String,
}

impl MemoryIndexLine {
    /// 渲染成 MEMORY.md 的链接行，中文摘要不截断、不改写。
    pub fn render(&self) -> String {
        format!("- [{}]({}.md) —— {}", self.summary, self.slug, self.summary)
    }
}

/// 一次记忆写入的纯文本产物：索引文件内容与事实文件内容。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemoryFile {
    pub entry: MemoryEntry,
    pub index_line: MemoryIndexLine,
    pub index_text_before: String,
}

impl MemoryFile {
    /// 从既有索引文本和一条事实生成待写文本，字段校验在写盘前完成。
    pub fn new(
        entry: MemoryEntry,
        index_text_before: impl Into<String>,
    ) -> Result<Self, MemoryFileError> {
        entry.validate()?;
        let index_line = MemoryIndexLine {
            slug: entry.slug.clone(),
            summary: entry.summary.clone(),
        };
        Ok(Self {
            entry,
            index_line,
            index_text_before: index_text_before.into(),
        })
    }

    /// 没有既有索引时使用 MEMORY.md 的标准标题。
    pub fn with_entry(entry: MemoryEntry) -> Result<Self, MemoryFileError> {
        Self::new(entry, "# Memory Index\n")
    }

    pub fn index_line(&self) -> &MemoryIndexLine {
        &self.index_line
    }

    /// 合并索引，重复 slug 按最新事实覆盖，保持原有行序。
    pub fn index_text_after(&self) -> String {
        let marker = format!("]({}.md)", self.index_line.slug);
        let mut lines = self
            .index_text_before
            .lines()
            .filter(|line| !line.contains(&marker))
            .map(str::to_owned)
            .collect::<Vec<_>>();
        lines.push(self.index_line.render());
        let mut text = lines.join("\n");
        text.push('\n');
        text
    }

    /// 事实文件的相对路径与正文。
    pub fn fact_relative_path(&self) -> String {
        format!("{}.md", self.entry.slug)
    }

    pub fn fact_text(&self) -> &str {
        &self.entry.fact
    }
}

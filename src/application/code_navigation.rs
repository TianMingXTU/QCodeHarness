//! 只读结构化代码导航的纯契约。
//!
//! 这不是完整语言服务器：不建 AST、不解析类型、不跨文件重命名。基于
//! `ignore` walker 与 `regex` 的最小符号索引只给出文件、行号、符号名、
//! 引用位置，供模型缩小阅读范围。所有查询都是只读。

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// 一次扫描命中的符号定义或引用。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SymbolHit {
    pub symbol: String,
    pub kind: SymbolKind,
    pub path: PathBuf,
    pub line: usize,
    pub column: usize,
    pub text: String,
}

/// 定义与引用分开标注。正则只识别常见声明形态，不宣称语言语义完备。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SymbolKind {
    Definition,
    Reference,
}

/// 代码导航的查询条件。`root` 由调用方限定到 workspace 之内。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NavigationQuery {
    pub root: PathBuf,
    pub symbol: String,
    pub only_definitions: bool,
}

/// 图片输入只做证据登记：路径、字节数、SHA-256。不解析像素、不猜内容。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageEvidence {
    pub path: PathBuf,
    pub bytes: u64,
    pub digest: String,
    pub note: String,
}

impl ImageEvidence {
    /// 校验路径和摘要非空。摘要由上层计算，避免 domain 依赖文件 IO。
    pub fn validate(&self) -> Result<(), String> {
        if self.path.as_os_str().is_empty() {
            return Err("图片路径不能为空".to_owned());
        }
        if self.digest.trim().is_empty() {
            return Err("图片摘要不能为空".to_owned());
        }
        if self.digest.len() != 64 || !self.digest.chars().all(|ch| ch.is_ascii_hexdigit()) {
            return Err("图片摘要必须是 64 位十六进制 SHA-256".to_owned());
        }
        Ok(())
    }
}

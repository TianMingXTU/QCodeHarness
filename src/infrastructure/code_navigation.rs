//! 基于 `ignore`/`regex` 的最小只读符号索引。
//!
//! 明确限制：不做完整 LSP、不建 Tree-sitter AST、不解析类型与作用域。
//! 定义按常见 `fn/function/def/class/struct/impl` 声明识别，引用按整词
//! 匹配；注释、字符串与同名局部变量都可能产生误报，结果只供导航。

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use ignore::WalkBuilder;
use regex::Regex;
use serde_json::{Value, json};

use crate::application::code_navigation::{ImageEvidence, NavigationQuery, SymbolHit, SymbolKind};
use crate::application::port::{BoxFuture, ToolInvoker};
use crate::application::tool_registry::{ToolEntry, ToolSource};
use crate::domain::tool::{ToolAnnotations, ToolCall, ToolOutcome, ToolSpec};

/// `code_navigation` 工具名。
pub const CODE_NAVIGATION_TOOL: &str = "code_navigation";
/// `image_evidence` 工具名。图片只登记证据，不解析像素。
pub const IMAGE_EVIDENCE_TOOL: &str = "image_evidence";

/// 明确写在工具描述里的限制，避免模型把正则结果当编译器语义。
pub const LIMITATION: &str =
    "最小正则符号索引，不是完整 LSP/Tree-sitter；不解析类型、作用域与跨文件语义";

/// 只读扫描 workspace，返回定义与引用的位置清单。
pub fn search(query: &NavigationQuery) -> Result<Vec<SymbolHit>, String> {
    if query.symbol.trim().is_empty()
        || !query
            .symbol
            .chars()
            .all(|ch| ch.is_alphanumeric() || ch == '_')
    {
        return Err("符号名只允许字母、数字与下划线".to_owned());
    }
    let symbol = regex::escape(&query.symbol);
    let definition = Regex::new(&format!(
        r"(?i)^\s*(?:pub\s+)?(?:async\s+)?(?:fn|function|def|class|struct|enum|interface|trait|impl|type)\s+[^\w]*{symbol}\b"
    ))
    .map_err(|error| format!("构造定义正则失败: {error}"))?;
    let reference = Regex::new(&format!(r"\b{symbol}\b"))
        .map_err(|error| format!("构造引用正则失败: {error}"))?;

    let mut hits = Vec::new();
    let mut seen = HashSet::new();
    for entry in WalkBuilder::new(&query.root).hidden(false).build() {
        let entry = entry.map_err(|error| format!("遍历工作区失败: {error}"))?;
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        let path = entry.path();
        if !is_supported_source(path) {
            continue;
        }
        let text = fs::read_to_string(path)
            .map_err(|error| format!("读取 {} 失败: {error}", path.display()))?;
        for (index, line) in text.lines().enumerate() {
            let column = reference
                .find(line)
                .map(|found| found.start() + 1)
                .unwrap_or(1);
            let kind = if definition.is_match(line) {
                SymbolKind::Definition
            } else if reference.is_match(line) {
                SymbolKind::Reference
            } else {
                continue;
            };
            if query.only_definitions && kind != SymbolKind::Definition {
                continue;
            }
            let key = (path.to_path_buf(), index + 1, kind);
            if !seen.insert(key) {
                continue;
            }
            hits.push(SymbolHit {
                symbol: query.symbol.clone(),
                kind,
                path: relative_path(&query.root, path),
                line: index + 1,
                column,
                text: line.trim().to_owned(),
            });
        }
    }
    Ok(hits)
}

fn is_supported_source(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|ext| ext.to_str()),
        Some("rs" | "js" | "ts" | "jsx" | "tsx" | "py" | "go" | "java" | "c" | "h" | "cpp" | "cc")
    )
}

fn relative_path(root: &Path, path: &Path) -> PathBuf {
    path.strip_prefix(root).unwrap_or(path).to_path_buf()
}

/// SHA-256 的纯 Rust 实现，避免引入新依赖；只用于证据登记。
pub fn sha256_hex(bytes: &[u8]) -> String {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut state = [
        0x6a09e667u32,
        0xbb67ae85,
        0x3c6ef372,
        0xa54ff53a,
        0x510e527f,
        0x9b05688c,
        0x1f83d9ab,
        0x5be0cd19,
    ];
    let bit_len = (bytes.len() as u64).wrapping_mul(8);
    let mut data = bytes.to_vec();
    data.push(0x80);
    while data.len() % 64 != 56 {
        data.push(0);
    }
    data.extend_from_slice(&bit_len.to_be_bytes());
    for block in data.chunks_exact(64) {
        let mut w = [0u32; 64];
        for (i, word) in w.iter_mut().enumerate().take(16) {
            *word = u32::from_be_bytes([
                block[i * 4],
                block[i * 4 + 1],
                block[i * 4 + 2],
                block[i * 4 + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = state;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = h
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }
        for (slot, value) in state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *slot = slot.wrapping_add(value);
        }
    }
    let mut hex = String::with_capacity(64);
    for word in state {
        hex.push_str(&format!("{word:08x}"));
    }
    hex
}

/// 只读图片证据登记。参数只收路径；真实字节与摘要由本工具从文件读取。
pub fn image_evidence(path: &Path) -> Result<ImageEvidence, String> {
    let bytes =
        fs::read(path).map_err(|error| format!("读取图片 {} 失败: {error}", path.display()))?;
    let digest = sha256_hex(&bytes);
    let evidence = ImageEvidence {
        path: path.to_path_buf(),
        bytes: bytes.len() as u64,
        digest,
        note: "只登记路径/字节数/SHA-256，不解析像素，也不宣称理解图像内容".to_owned(),
    };
    evidence.validate()?;
    Ok(evidence)
}

/// `code_navigation` 的只读工具。
pub struct CodeNavigationTool {
    root: PathBuf,
}

impl CodeNavigationTool {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn spec() -> ToolSpec {
        ToolSpec {
            name: CODE_NAVIGATION_TOOL.to_owned(),
            description: format!(
                "只读符号级搜索：返回定义/引用的文件、行号、列号与原文。{LIMITATION}"
            ),
            parameters: json!({
                "type": "object",
                "properties": {
                    "symbol": {"type": "string", "description": "符号名，只允许字母数字下划线"},
                    "only_definitions": {"type": "boolean", "description": "只看定义，默认 false"}
                },
                "required": ["symbol"]
            }),
            annotations: ToolAnnotations::new(true, false, true, false),
        }
    }
}

impl ToolInvoker for CodeNavigationTool {
    fn invoke<'a>(&'a self, call: &'a ToolCall) -> BoxFuture<'a, ToolOutcome> {
        Box::pin(async move {
            let Value::Object(arguments) = &call.arguments else {
                return ToolOutcome::failed("code_navigation 参数必须是对象");
            };
            let Some(symbol) = arguments.get("symbol").and_then(Value::as_str) else {
                return ToolOutcome::failed("code_navigation 缺少 symbol");
            };
            let only_definitions = arguments
                .get("only_definitions")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let root = self.root.clone();
            let symbol = symbol.to_owned();
            let hits = match tokio::task::spawn_blocking(move || {
                search(&NavigationQuery {
                    root,
                    symbol,
                    only_definitions,
                })
            })
            .await
            {
                Ok(Ok(hits)) => hits,
                Ok(Err(error)) => return ToolOutcome::failed(error),
                Err(error) => return ToolOutcome::failed(format!("符号索引任务异常: {error}")),
            };
            ToolOutcome::success(
                serde_json::to_string_pretty(&hits)
                    .unwrap_or_else(|error| format!("序列化符号索引失败: {error}")),
            )
        })
    }
}

/// `image_evidence` 只返回登记字段，绝不返回图片内容或模型解释。
pub struct ImageEvidenceTool;

impl ImageEvidenceTool {
    pub fn spec() -> ToolSpec {
        ToolSpec {
            name: IMAGE_EVIDENCE_TOOL.to_owned(),
            description: "只读登记图片输入证据（路径、字节数、SHA-256），不解析像素或图片内容"
                .to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {"path": {"type": "string", "description": "workspace 内图片相对路径"}},
                "required": ["path"]
            }),
            annotations: ToolAnnotations::new(true, false, true, false),
        }
    }
}

impl ToolInvoker for ImageEvidenceTool {
    fn invoke<'a>(&'a self, call: &'a ToolCall) -> BoxFuture<'a, ToolOutcome> {
        Box::pin(async move {
            let Value::Object(arguments) = &call.arguments else {
                return ToolOutcome::failed("image_evidence 参数必须是对象");
            };
            let Some(path) = arguments.get("path").and_then(Value::as_str) else {
                return ToolOutcome::failed("image_evidence 缺少 path");
            };
            let path = PathBuf::from(path);
            let evidence = match tokio::task::spawn_blocking(move || image_evidence(&path)).await {
                Ok(Ok(evidence)) => evidence,
                Ok(Err(error)) => return ToolOutcome::failed(error),
                Err(error) => return ToolOutcome::failed(format!("图片证据任务异常: {error}")),
            };
            ToolOutcome::success(
                serde_json::to_string(&evidence)
                    .unwrap_or_else(|error| format!("序列化图片证据失败: {error}")),
            )
        })
    }
}

/// 两个只读工具的注册表来源，交给装配层接 ToolGuard/审批链。
pub struct CodeNavigationToolSource {
    root: PathBuf,
}

impl CodeNavigationToolSource {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
}

impl ToolSource for CodeNavigationToolSource {
    fn name(&self) -> &str {
        "code_navigation"
    }

    fn tools(&self) -> Vec<ToolEntry> {
        vec![
            ToolEntry {
                spec: CodeNavigationTool::spec(),
                invoker: std::sync::Arc::new(CodeNavigationTool::new(self.root.clone())),
            },
            ToolEntry {
                spec: ImageEvidenceTool::spec(),
                invoker: std::sync::Arc::new(ImageEvidenceTool),
            },
        ]
    }
}

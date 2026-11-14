//! LSP / Tree-sitter 结构化导航与多模态图片证据的行为验收。
//!
//! 测试使用独立临时目录与只读工具。明确覆盖：只读声明、符号定义/引用定位、
//! 图片证据只登记路径/字节数/SHA-256、不解析像素，以及实现限制标注。

use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use qcode_harness::application::code_navigation::{ImageEvidence, NavigationQuery, SymbolKind};
use qcode_harness::application::port::ToolInvoker;
use qcode_harness::application::tool_registry::ToolSource;
use qcode_harness::domain::ids::CallId;
use qcode_harness::domain::tool::{ToolAnnotations, ToolCall};
use qcode_harness::infrastructure::code_navigation::{
    CODE_NAVIGATION_TOOL, CodeNavigationToolSource, IMAGE_EVIDENCE_TOOL,
    ImageEvidenceTool, LIMITATION, image_evidence, search, sha256_hex,
};

fn temp_dir(name: &str) -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("qch-nav-{name}-{}-{stamp}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("建临时目录");
    dir
}

fn call(name: &str, arguments: serde_json::Value) -> ToolCall {
    ToolCall {
        id: CallId::new(format!("{name}-1")),
        name: name.to_owned(),
        arguments,
    }
}

#[test]
fn navigation_finds_definitions_and_references_without_writing() {
    let dir = temp_dir("symbols");
    let source = "\npub fn compute(value: u32) -> u32 {\n    value + 1\n}\n\nfn caller() -> u32 {\n    compute(3)\n}\n";
    fs::write(dir.join("lib.rs"), source).expect("写 fixture");
    let mut query = NavigationQuery {
        root: dir.clone(),
        symbol: "compute".to_owned(),
        only_definitions: false,
    };
    let hits = search(&query).expect("符号索引成功");
    assert!(
        hits.iter()
            .any(|hit| hit.kind == SymbolKind::Definition && hit.line == 2)
    );
    assert!(
        hits.iter()
            .any(|hit| hit.kind == SymbolKind::Reference && hit.line == 7)
    );
    assert!(hits.iter().all(|hit| hit.path == PathBuf::from("lib.rs")));

    query.only_definitions = true;
    let definitions = search(&query).expect("定义索引成功");
    assert!(
        definitions
            .iter()
            .all(|hit| hit.kind == SymbolKind::Definition)
    );
    let before = fs::read_to_string(dir.join("lib.rs")).expect("源文件仍存在");
    assert_eq!(before, source, "只读查询不能改源文件");
    assert!(!dir.join("compute.rs").exists(), "只读查询不能新增文件");
    let _ = fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn image_evidence_records_path_size_and_digest_only() {
    let dir = temp_dir("image");
    let bytes = b"\x89PNG\r\n\x1a\nnot-a-real-pixel-payload";
    let image = dir.join("input.png");
    fs::write(&image, bytes).expect("写图片 fixture");
    let evidence = image_evidence(&image).expect("证据可登记");
    assert_eq!(evidence.bytes, bytes.len() as u64);
    assert_eq!(evidence.digest, sha256_hex(bytes));
    evidence.validate().expect("摘要合法");
    assert_eq!(
        sha256_hex(b"abc"),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );

    let tool = ImageEvidenceTool;
    let outcome = tool
        .invoke(&call(
            IMAGE_EVIDENCE_TOOL,
            serde_json::json!({ "path": image.to_string_lossy() }),
        ))
        .await;
    assert_eq!(
        outcome.status,
        qcode_harness::domain::tool::OutcomeStatus::Success
    );
    let recorded: ImageEvidence = serde_json::from_str(&outcome.content).expect("输出是证据结构");
    assert_eq!(recorded, evidence);
    assert!(
        !outcome.content.to_lowercase().contains("pixel payload"),
        "不得返回或解释图片内容"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn both_tools_are_read_only_and_the_limitation_is_explicit() {
    let source = CodeNavigationToolSource::new(".");
    let specs = source
        .tools()
        .into_iter()
        .map(|entry| entry.spec)
        .collect::<Vec<_>>();
    assert_eq!(specs.len(), 2);
    assert!(specs.iter().any(|spec| spec.name == CODE_NAVIGATION_TOOL));
    assert!(specs.iter().any(|spec| spec.name == IMAGE_EVIDENCE_TOOL));
    assert!(
        specs
            .iter()
            .all(|spec| spec.annotations == ToolAnnotations::new(true, false, true, false))
    );
    assert!(specs[0].description.contains(LIMITATION));
    assert!(LIMITATION.contains("不是完整 LSP/Tree-sitter"));
    assert_eq!(source.name(), "code_navigation");
}

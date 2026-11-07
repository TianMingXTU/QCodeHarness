//! 自定义工具来源：把一批工具接进 Harness 的注册表。
//!
//! 运行：`cargo run --example custom-tool-source`
//!
//! `ToolSource` 只回答"这个来源提供哪些登记项"；登记项把模型可见的规格
//! 与真正的执行入口绑在一起，注册表按来源顺序合并，同名工具后到的来源
//! 胜出。这里给内置工具加一个只读的 `char_count`。

use std::sync::Arc;

use qcode_harness::application::port::{BoxFuture, ToolInvoker};
use qcode_harness::application::tool_registry::{ToolEntry, ToolRegistry, ToolSource};
use qcode_harness::domain::ids::CallId;
use qcode_harness::domain::tool::{ToolAnnotations, ToolCall, ToolOutcome, ToolSpec};
use qcode_harness::domain::workspace::WorkspaceRoot;
use qcode_harness::infrastructure::tool::BuiltinToolSource;
use serde_json::json;

/// 只读的演示工具：统计参数文本的字符数。
struct CharCount;

impl ToolInvoker for CharCount {
    fn invoke<'a>(&'a self, call: &'a ToolCall) -> BoxFuture<'a, ToolOutcome> {
        Box::pin(async move {
            let text = call
                .arguments
                .get("text")
                .and_then(|value| value.as_str())
                .unwrap_or_default();
            ToolOutcome::success(format!("{} 个字符", text.chars().count()))
        })
    }
}

/// 演示来源：规格与执行入口成对登记。
struct CharCountSource;

impl ToolSource for CharCountSource {
    fn name(&self) -> &str {
        "demo"
    }

    fn tools(&self) -> Vec<ToolEntry> {
        vec![ToolEntry {
            spec: ToolSpec {
                name: "char_count".to_owned(),
                description: "统计一段文本的字符数。".to_owned(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "text": {"type": "string", "description": "要统计的文本"}
                    },
                    "required": ["text"]
                }),
                annotations: ToolAnnotations::new(true, false, true, false),
            },
            invoker: Arc::new(CharCount),
        }]
    }
}

#[tokio::main]
async fn main() {
    let root = WorkspaceRoot::new(std::env::current_dir().expect("当前目录可用"));
    let sources: Vec<Arc<dyn ToolSource>> = vec![
        Arc::new(BuiltinToolSource::new(root)),
        Arc::new(CharCountSource),
    ];
    let registry = ToolRegistry::from_sources(&sources);

    let names: Vec<String> = registry
        .specs()
        .into_iter()
        .map(|spec| spec.name)
        .collect();
    println!("注册表里有 {} 个工具: {names:?}", names.len());

    let call = ToolCall {
        id: CallId::new("call_1"),
        name: "char_count".to_owned(),
        arguments: json!({"text": "hello 世界"}),
    };
    println!("char_count 结果: {}", registry.invoke(&call).await.content);
}

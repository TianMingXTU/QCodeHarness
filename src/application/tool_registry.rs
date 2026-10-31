//! 工具注册表：模型可见的规格与真正的执行入口合一。
//!
//! P1 之前 `AgentSpec.tools` 与 `invoker` 是两个要手工保持同步的东西——
//! spec 说模型能看到某工具，invoker 才知道谁执行它。加工具时要改两处，
//! 漏改一处的表现是：模型看得见却执行不了，或者执行得了但模型不知道。
//! 收敛成一个注册表后，"加一个工具"只在一处发生。

use std::sync::Arc;

use crate::domain::tool::{ToolCall, ToolOutcome, ToolSpec};
use crate::application::port::{BoxFuture, ToolInvoker};

/// 一个工具的完整登记：模型看到的规格 + 真正执行它的入口。
///
/// 两个字段绑在一起，spec 与执行器就不可能各自漂移。
pub struct ToolEntry {
    /// 模型可见的规格（名字、描述、参数 schema、风险注解）。
    pub spec: ToolSpec,
    /// 这个工具的执行入口。每个工具可以有自己的配置——
    /// 比如 `run_command` 需要超时与输出上限，而 `read_file` 只要 workspace 根。
    pub invoker: Arc<dyn ToolInvoker>,
}

/// 工具注册表。
///
/// 用 `Vec` 而不是 `HashMap`：工具数量是个位数到十几个，线性查找没有
/// 可观测的开销；而 `Vec` 保序，"注册表里有哪些工具"能以注册顺序呈现，
/// 调试时工具列表是稳定的。
pub struct ToolRegistry {
    entries: Vec<ToolEntry>,
}

impl ToolRegistry {
    /// 用一组登记项构造注册表。
    ///
    /// 名字重复时后者覆盖前者：装配代码可以无痛替换某个工具的实现，
    /// 而不必先显式移除旧的。
    pub fn new(entries: Vec<ToolEntry>) -> Self {
        let mut registry = Self {
            entries: Vec::with_capacity(entries.len()),
        };
        for entry in entries {
            match registry
                .entries
                .iter_mut()
                .find(|existing| existing.spec.name == entry.spec.name)
            {
                Some(existing) => *existing = entry,
                None => registry.entries.push(entry),
            }
        }
        registry
    }

    /// 模型可见的全部规格。
    ///
    /// 克隆而不是借出：调用方（rig 防腐层）要把它们序列化进请求，
    /// 返回所有权省去一次借用传递。
    pub fn specs(&self) -> Vec<ToolSpec> {
        self.entries.iter().map(|entry| entry.spec.clone()).collect()
    }

    /// 按名字查登记项。查不到说明模型指名了不存在的工具。
    pub fn find(&self, name: &str) -> Option<&ToolEntry> {
        self.entries.iter().find(|entry| entry.spec.name == name)
    }

    /// 统一的执行入口：找到登记项就交给它执行，找不到给拒绝观察。
    ///
    /// 放在注册表而不是 `AgentRunner`：分发是"注册表"自己的职责，
    /// 循环不该关心名字背后是谁在干活。
    pub async fn invoke(&self, call: &ToolCall) -> ToolOutcome {
        match self.find(&call.name) {
            Some(entry) => entry.invoker.invoke(call).await,
            None => ToolOutcome::refused(format!("工具 `{}` 未注册", call.name)),
        }
    }
}

impl ToolInvoker for ToolRegistry {
    fn invoke<'a>(&'a self, call: &'a ToolCall) -> BoxFuture<'a, ToolOutcome> {
        Box::pin(self.invoke(call))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::ids::CallId;
    use crate::domain::tool::ToolAnnotations;
    use serde_json::json;

    /// 固定回复的假执行器。
    struct RecordingInvoker {
        reply: &'static str,
    }

    impl RecordingInvoker {
        fn new(reply: &'static str) -> Self {
            Self { reply }
        }
    }

    impl ToolInvoker for RecordingInvoker {
        fn invoke<'a>(&'a self, _call: &'a ToolCall) -> BoxFuture<'a, ToolOutcome> {
            Box::pin(async { ToolOutcome::success(self.reply) })
        }
    }

    fn spec(name: &str) -> ToolSpec {
        ToolSpec {
            name: name.to_owned(),
            description: "测试工具".to_owned(),
            parameters: json!({"type": "object"}),
            annotations: ToolAnnotations::new(true, false, true, false),
        }
    }

    fn call(name: &str) -> ToolCall {
        ToolCall {
            id: CallId::new("call_1"),
            name: name.to_owned(),
            arguments: json!({}),
        }
    }

    #[test]
    fn specs_returns_all_registered_specs_in_order() {
        let registry = ToolRegistry::new(vec![
            ToolEntry {
                spec: spec("read_file"),
                invoker: Arc::new(RecordingInvoker::new("a")),
            },
            ToolEntry {
                spec: spec("write_file"),
                invoker: Arc::new(RecordingInvoker::new("b")),
            },
        ]);

        let names: Vec<String> = registry
            .specs()
            .iter()
            .map(|s| s.name.clone())
            .collect();
        assert_eq!(names, vec!["read_file", "write_file"]);
    }

    #[tokio::test]
    async fn invoke_routes_to_the_right_invoker() {
        let registry = ToolRegistry::new(vec![
            ToolEntry {
                spec: spec("read_file"),
                invoker: Arc::new(RecordingInvoker::new("文件内容")),
            },
            ToolEntry {
                spec: spec("write_file"),
                invoker: Arc::new(RecordingInvoker::new("写入结果")),
            },
        ]);

        let read = registry.invoke(&call("read_file")).await;
        let write = registry.invoke(&call("write_file")).await;

        assert_eq!(read.content, "文件内容");
        assert_eq!(write.content, "写入结果");
    }

    #[tokio::test]
    async fn invoke_refuses_unknown_tool_instead_of_panicking() {
        let registry = ToolRegistry::new(vec![]);

        let outcome = registry.invoke(&call("rm_rf")).await;

        assert_eq!(outcome.status, crate::domain::tool::OutcomeStatus::Refused);
        assert!(outcome.content.contains("rm_rf"));
        assert!(outcome.is_error());
    }

    #[test]
    fn duplicate_names_keep_the_later_entry() {
        // 后注册的同名工具覆盖先注册的：装配代码可以无痛替换某个工具的实现。
        let registry = ToolRegistry::new(vec![
            ToolEntry {
                spec: spec("read_file"),
                invoker: Arc::new(RecordingInvoker::new("旧实现")),
            },
            ToolEntry {
                spec: spec("read_file"),
                invoker: Arc::new(RecordingInvoker::new("新实现")),
            },
        ]);

        assert_eq!(registry.specs().len(), 1, "同名去重后只剩一个规格");
    }
}

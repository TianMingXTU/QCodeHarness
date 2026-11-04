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
///
/// 可克隆：编排器（P4）每步要用同一套依赖克隆出新的 runner，
/// `Arc` 的克隆只是引用计数 +1，真正的工具实现不会跟着复制。
#[derive(Clone)]
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
#[derive(Clone)]
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

    /// 证据台账的分类表要与真实注册表对得上。
    ///
    /// 两头都要守：表里的名字拼错了，台账会静默少记一项；真实工具没
    /// 进表、也没说清为什么不记，新工具就会悄悄消失在台账里——用户
    /// 看报表以为"这个任务没碰过那些文件"。
    ///
    /// 所以真实工具要么被分类，要么在"明确不记"名单里写清理由。
    #[test]
    fn the_evidence_ledger_knows_every_real_tool() {
        let registry = ToolRegistry::new(crate::infrastructure::tool::all_tools(
            crate::domain::workspace::WorkspaceRoot::new(std::env::temp_dir()),
        ));
        let specs = registry.specs();
        let registered: Vec<&str> = specs.iter().map(|spec| spec.name.as_str()).collect();
        let known = crate::domain::context::known_tool_names();

        for name in known {
            assert!(
                registered.contains(name),
                "台账分类表里的 `{name}` 不是真实注册的工具；已注册: {registered:?}"
            );
        }

        // 明确不记的工具，以及不记的理由。
        let deliberately_untracked: &[(&str, &str)] = &[
            ("list_dir", "列目录不读文件内容，台账记的是文件"),
            ("search_text", "按模式扫文件，命中的文件不固定"),
            ("git_status", "查仓库状态，不指向具体文件"),
            ("git_diff", "看的是改动，不是读过哪个文件"),
            // apply_patch 的参数是一整块补丁文本，没有 path/paths 字段，
            // 台账的 subject() 取不到对象，只能退化成工具名本身——把
            // "apply_patch" 当文件名记进"写过的文件"是错的。动过哪些
            // 文件要从补丁文本里解析，那是基础设施层的事，台账宁可少记
            // 一类也不记错。
            ("apply_patch", "参数是补丁文本，没有路径字段，台账取不到对象"),
        ];
        for (name, reason) in deliberately_untracked {
            assert!(
                registered.contains(name),
                "`{name}` 应当是真实注册的工具: {registered:?}"
            );
            assert!(
                !known.contains(name),
                "`{name}` 已进分类表，明确不记的理由（{reason}）要一并更新"
            );
        }

        for name in &registered {
            assert!(
                known.contains(name) || deliberately_untracked.iter().any(|(untracked, _)| untracked == name),
                "新工具 `{name}` 没进台账分类表：要么加进分类表，要么在明确不记的名单里写清理由"
            );
        }
    }
}

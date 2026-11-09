//! 工具注册表：模型可见的规格与执行入口合在一起。
//!
//! 两者分开维护（工具白名单与 `invoker` 各一份）需要手工同步。漏改一处
//! 会导致模型看得见工具却无法执行，或工具可执行但模型不知道。合并到
//! 注册表后，新增工具只需改一处。
//!
//! 角色白名单不在这里：它在 `domain::agent_profile` 的角色档案里，按
//! 调用时校验，不在装配期过滤（见该模块的说明）。

use std::sync::Arc;

use crate::domain::tool::{ToolCall, ToolOutcome, ToolSpec};
use crate::application::port::{BoxFuture, ToolInvoker};

/// 一个工具的完整登记：模型可见的规格与执行入口。
///
/// 两个字段绑定在一起，规格和执行器不会各自漂移。
///
/// 可克隆：编排器每一步要用同一套依赖克隆新的 runner。`Arc` 克隆只增加
/// 引用计数，工具实现不会复制。
#[derive(Clone)]
pub struct ToolEntry {
    /// 模型可见的规格（名字、描述、参数 schema、风险注解）。
    pub spec: ToolSpec,
    /// 执行入口。每个工具可自带配置，例如 `run_command` 需要超时和输出上限，
    /// `read_file` 只需要 workspace 根。
    pub invoker: Arc<dyn ToolInvoker>,
}

/// 工具来源：一批登记项加来源名。
///
/// 注册表只关心有哪些工具；来源名记录工具出处（内置、某个 MCP Server、
/// 第三方工具包），便于排查工具清单。实现方返回的登记项必须规格与执行
/// 入口成对；这是 [`ToolEntry`] 的不变量，来源只负责批量提供。
pub trait ToolSource: Send + Sync {
    /// 来源名，供装配和诊断识别来源。
    ///
    /// 合并时**不比较名字**：按装配顺序展开，同名工具以后到的来源为准
    /// （见 [`ToolRegistry::from_sources`]）。
    fn name(&self) -> &str;

    /// 这个来源提供的全部登记项。
    fn tools(&self) -> Vec<ToolEntry>;
}

/// 工具注册表。
///
/// 用 `Vec`，不用 `HashMap`：工具数量在个位数到十几个之间，线性查找没有
/// 可观测开销。`Vec` 还保持注册顺序，调试时工具列表稳定。
#[derive(Clone)]
pub struct ToolRegistry {
    entries: Vec<ToolEntry>,
}

impl ToolRegistry {
    /// 用一组登记项构造注册表。
    ///
    /// 名字重复时后者覆盖前者，装配代码可直接替换某个工具的实现。
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

    /// 从多个来源装配注册表。
    ///
    /// 按来源顺序合并；同名工具沿用 [`new`](Self::new) 的覆盖语义，
    /// 后到的来源胜出。顺序由装配方决定。
    pub fn from_sources(sources: &[Arc<dyn ToolSource>]) -> Self {
        let entries: Vec<ToolEntry> = sources
            .iter()
            .flat_map(|source| source.tools())
            .collect();
        Self::new(entries)
    }

    /// 在现有登记项之后追加另一张表，同名工具由后者覆盖。
    ///
    /// 用于装配阶段先建宿主工具表、再由扩展来源追加 `skill__*`；两表都
    /// 已解析完成，不需要重新提供 `ToolSource`。
    pub fn merged_with(&self, other: &ToolRegistry) -> Self {
        let mut entries = self.entries.clone();
        entries.extend(other.entries.iter().cloned());
        Self::new(entries)
    }

    /// 模型可见的全部规格。
    ///
    /// 直接返回克隆：调用方（rig 防腐层）需要把它们序列化进请求，
    /// 给出所有权可省去借用传递。
    pub fn specs(&self) -> Vec<ToolSpec> {
        self.entries.iter().map(|entry| entry.spec.clone()).collect()
    }

    /// 按名字查登记项。查不到说明模型指名了不存在的工具。
    pub fn find(&self, name: &str) -> Option<&ToolEntry> {
        self.entries.iter().find(|entry| entry.spec.name == name)
    }

    /// 统一执行入口：找到登记项就交给它执行，找不到返回拒绝观察。
    ///
    /// 分发由注册表负责，`AgentRunner` 的循环不需要关心名字背后的执行者。
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
        // 后注册的同名工具覆盖先注册的，装配代码可直接替换实现。
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

    /// 按来源装配：顺序保留，同名时后到的来源胜出。
    struct FixedSource {
        name: &'static str,
        tools: Vec<ToolEntry>,
    }

    impl ToolSource for FixedSource {
        fn name(&self) -> &str {
            self.name
        }

        fn tools(&self) -> Vec<ToolEntry> {
            self.tools.clone()
        }
    }

    fn entry(name: &str, reply: &'static str) -> ToolEntry {
        ToolEntry {
            spec: spec(name),
            invoker: Arc::new(RecordingInvoker::new(reply)),
        }
    }

    #[tokio::test]
    async fn from_sources_merges_in_order_and_later_sources_win() {
        let sources: Vec<Arc<dyn ToolSource>> = vec![
            Arc::new(FixedSource {
                name: "builtin",
                tools: vec![entry("read_file", "内置读"), entry("write_file", "内置写")],
            }),
            Arc::new(FixedSource {
                name: "remote",
                tools: vec![entry("write_file", "远端写"), entry("fetch", "远端取")],
            }),
        ];

        let registry = ToolRegistry::from_sources(&sources);

        // 顺序：先内置、后远端；同名 write_file 只剩远端那份。
        let names: Vec<String> = registry.specs().iter().map(|s| s.name.clone()).collect();
        assert_eq!(names, vec!["read_file", "write_file", "fetch"]);
        let hit = registry.invoke(&call("write_file")).await;
        assert_eq!(hit.content, "远端写", "同名工具应以后到的来源为准");
    }

    /// 证据台账的分类表必须与真实注册表一致。
    ///
    /// 两处都要校验：分类表里的名字拼错会让台账少记一项；真实工具
    /// 没进分类表、也没说明不记的理由，会从台账里消失，用户会误以为
    /// 任务没有访问过相关文件。
    ///
    /// 因此每个真实工具要么被分类，要么在"明确不记"名单里写明理由。
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
            // 台账的 subject() 取不到对象，只能退化成工具名。把
            // "apply_patch" 当文件名记进"写过的文件"是错的。解析补丁文本
            // 属于基础设施层；台账宁可少记一类，也不记错。
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

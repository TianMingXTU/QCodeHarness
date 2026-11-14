//! PreToolUse 阻断裁决：把外部命令挂点并入现有唯一 RunHooks 链。
//!
//! 现有 RunHooks 只有观察方法，裁决通过 `RunHooks::pre_tool_use` 默认方法
//! 承载（见 port/mod.rs），避免装配第二套 hooks。命令执行属于 infrastructure，
//! 应用层只持有 trait 与裁决类型。

use std::path::PathBuf;
use std::sync::Arc;

pub use crate::application::port::PreToolUseVerdict;
use crate::application::hooks_compat::{HookConfigReport, HookEventKind};
use crate::application::port::{BoxFuture, RunHooks};
use crate::domain::tool::ToolCall;
use crate::infrastructure::hooks_pretool::CommandPreToolUseHook;

/// 外部 PreToolUse 挂点。实现方负责命令执行、超时与失败显式报告。
pub trait PreToolUseHook: Send + Sync {
    fn decide<'a>(&'a self, call: &'a ToolCall) -> BoxFuture<'a, PreToolUseVerdict>;
}

/// 唯一的 RunHooks 链：先保留既有观察 Hook，再运行全部 PreToolUse 裁决。
///
/// 装配时把现有链（例如 `ExtensionHooks`）作为 `observer` 传入，本类型只
/// 在其基础上叠加外部命令挂点的否决权，不另起一套 `RunHooks`。
pub struct PreToolUseHooks {
    observer: Arc<dyn RunHooks>,
    hooks: Vec<Arc<dyn PreToolUseHook>>,
}

impl PreToolUseHooks {
    #[must_use]
    pub fn new(observer: Arc<dyn RunHooks>, hooks: Vec<Arc<dyn PreToolUseHook>>) -> Arc<Self> {
        Arc::new(Self { observer, hooks })
    }

    /// 依次收集裁决；第一个 Deny 胜出，其余挂点不再运行。
    pub async fn decide(&self, call: &ToolCall) -> PreToolUseVerdict {
        for hook in &self.hooks {
            let verdict = hook.decide(call).await;
            if matches!(verdict, PreToolUseVerdict::Deny(_)) {
                return verdict;
            }
        }
        PreToolUseVerdict::Allow
    }
}

impl RunHooks for PreToolUseHooks {
    // 观察只走既有链；否决裁决由 `pre_tool_use` 承载，避免装配第二套 hooks。
    fn before_tool_call<'a>(&'a self, call: &'a ToolCall) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            self.observer.before_tool_call(call).await;
        })
    }

    // 执行前裁决：先保留既有观察，再运行全部 PreToolUse 挂点。第一个
    // Deny 胜出。默认实现（port/mod.rs）只观察并放行，因此不接入
    // 外部命令挂点的既有链行为完全不变。
    fn pre_tool_use<'a>(&'a self, call: &'a ToolCall) -> BoxFuture<'a, PreToolUseVerdict> {
        Box::pin(async move {
            self.observer.before_tool_call(call).await;
            self.decide(call).await
        })
    }
}

impl PreToolUseHooks {
    /// Debug 输出只暴露挂点数量：观察链与挂点都是 trait 对象，逐字段打印
    /// 会把命令行或工具参数带进日志。
    fn hook_count(&self) -> usize {
        self.hooks.len()
    }
}

impl std::fmt::Debug for PreToolUseHooks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreToolUseHooks")
            .field("hook_count", &self.hook_count())
            .finish()
    }
}

/// 兼容报告到唯一 RunHooks 链的装配入口。
///
/// 只接受带可执行命令的映射；`unmapped` 非空时整体失败，绝不把未映射配置
/// 静默降级为放行。`working_directory` 传给全部命令挂点，`None` 表示继承
/// 进程当前目录。
///
/// # Errors
/// 报告含未映射项、或映射列表为空（没有可装配内容也是配置错误）时返回
/// 带原因的错误文本。
pub fn command_hooks_from_report(
    report: &HookConfigReport,
    observer: Arc<dyn RunHooks>,
    working_directory: Option<PathBuf>,
) -> Result<Arc<PreToolUseHooks>, String> {
    if !report.unmapped.is_empty() {
        let detail = report
            .unmapped
            .iter()
            .map(|item| format!("{}（{}）", item.path, item.reason))
            .collect::<Vec<_>>()
            .join("；");
        return Err(format!("hooks 配置存在未映射项，拒绝启用：{detail}"));
    }
    let mut hooks = Vec::new();
    for mapping in &report.mappings {
        match mapping.event {
            // PostToolUse 只有观察语义，当前 RunHooks 链没有可承载它的
            // 裁决点；显式拒绝而不是丢弃。
            HookEventKind::PostToolUse => {
                return Err(format!(
                    "hooks 配置存在未映射项，拒绝启用：{}（PostToolUse 尚无内部执行点）",
                    mapping.source
                ));
            }
            HookEventKind::PreToolUse => {
                let argv = shell_words_split(&mapping.command)?;
                let mut hook = CommandPreToolUseHook::new(argv);
                if let Some(directory) = &working_directory {
                    hook = hook.with_working_directory(directory.clone());
                }
                hooks.push(Arc::new(hook) as Arc<dyn PreToolUseHook>);
            }
        }
    }
    if hooks.is_empty() {
        return Err("hooks 配置没有可装配的 PreToolUse 命令，拒绝启用空链".to_owned());
    }
    Ok(PreToolUseHooks::new(observer, hooks))
}

/// 把 hook 命令字符串拆成 argv。
///
/// 兼容层拿到的命令是配置里的一个字符串；Windows 与 Unix 的引号规则不同，
/// 这里采用最小公共子集：空白分隔，双引号成段保留内部空白，不支持转义。
/// 解析歧义宁可报错也不猜，猜测会改变将要执行的命令。
fn shell_words_split(command: &str) -> Result<Vec<String>, String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut has_word = false;
    for ch in command.chars() {
        match ch {
            '"' => {
                in_quotes = !in_quotes;
                has_word = true;
            }
            c if c.is_whitespace() && !in_quotes => {
                if has_word {
                    words.push(std::mem::take(&mut current));
                    has_word = false;
                }
            }
            c => {
                current.push(c);
                has_word = true;
            }
        }
    }
    if in_quotes {
        return Err(format!("hooks 命令引号不闭合，拒绝装配：{command}"));
    }
    if has_word {
        words.push(current);
    }
    Ok(words)
}

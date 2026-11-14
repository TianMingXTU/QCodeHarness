//! PreToolUse 外部命令挂点：exit code 2 阻断，其余结果显式裁决。
//!
//! 命令执行只能发生在 infrastructure。启动/超时/信号失败采用 fail-closed，
//! 拒绝原因写入证据账本，不静默降级为放行。

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use crate::application::hooks::{PreToolUseHook, PreToolUseVerdict};
use crate::application::port::BoxFuture;
use crate::domain::tool::ToolCall;
use tokio::process::Command;

/// 一次 PreToolUse 命令。argv[0] 是可执行文件，argv[1..] 是固定参数。
#[derive(Clone, Debug)]
pub struct CommandPreToolUseHook {
    argv: Vec<String>,
    timeout: Duration,
    working_directory: Option<PathBuf>,
}

impl CommandPreToolUseHook {
    #[must_use]
    pub fn new(argv: Vec<String>) -> Self {
        Self {
            argv,
            timeout: Duration::from_secs(10),
            working_directory: None,
        }
    }

    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    #[must_use]
    pub fn with_working_directory(mut self, directory: PathBuf) -> Self {
        self.working_directory = Some(directory);
        self
    }

    /// 工具信息通过环境变量传递，避免把任意 JSON 写进可能已关闭的 stdin。
    fn environment(&self, call: &ToolCall) -> BTreeMap<&'static str, String> {
        let mut values = BTreeMap::new();
        values.insert("QCH_TOOL_NAME", call.name.clone());
        values.insert("QCH_TOOL_ARGUMENTS", call.arguments.to_string());
        values.insert("QCH_HOOK_EVENT", "PreToolUse".to_owned());
        values
    }
}

impl PreToolUseHook for CommandPreToolUseHook {
    fn decide<'a>(&'a self, call: &'a ToolCall) -> BoxFuture<'a, PreToolUseVerdict> {
        Box::pin(async move {
            let (program, args) = match self.argv.split_first() {
                Some(parts) => parts,
                None => {
                    return PreToolUseVerdict::Deny("PreToolUse 命令未配置可执行文件".to_owned())
                }
            };
            let mut command = Command::new(program);
            command.args(args);
            command.kill_on_drop(true);
            for (key, value) in self.environment(call) {
                command.env(key, value);
            }
            if let Some(directory) = &self.working_directory {
                command.current_dir(directory);
            }
            let child = command.output();
            let output = match tokio::time::timeout(self.timeout, child).await {
                Ok(Ok(output)) => output,
                Ok(Err(error)) => {
                    return PreToolUseVerdict::Deny(format!("PreToolUse 命令启动失败：{error}"))
                }
                Err(_) => {
                    return PreToolUseVerdict::Deny("PreToolUse 命令执行超时".to_owned())
                }
            };
            if output.status.code() == Some(2) {
                PreToolUseVerdict::Deny("命令以退出码 2 拒绝工具调用".to_owned())
            } else if output.status.success() {
                PreToolUseVerdict::Allow
            } else {
                let code = output
                    .status
                    .code()
                    .map_or_else(|| "信号终止".to_owned(), |code| code.to_string());
                PreToolUseVerdict::Deny(format!("PreToolUse 命令异常退出：{code}"))
            }
        })
    }
}

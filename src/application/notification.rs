//! OS 通知的应用层契约。
//!
//! 通知只描述「发生了什么」与「是否允许发送」，不碰进程、文件或终端。
//! 默认策略关闭，避免一次普通任务在用户未配置时启动外部程序。

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// 一次模型调用生命周期里值得打断用户注意力的两类事件。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NotificationEvent {
    /// 任务形成最终答复。
    TaskCompleted { summary: String },
    /// 运行停在审批闸门，等待用户决定。
    ApprovalWaiting { tool: String },
}

impl NotificationEvent {
    pub fn task_completed(summary: impl Into<String>) -> Self {
        Self::TaskCompleted { summary: summary.into() }
    }

    pub fn approval_waiting(tool: impl Into<String>) -> Self {
        Self::ApprovalWaiting { tool: tool.into() }
    }

    /// 发给外部命令的简短摘要。摘要不携带凭据或工具参数。
    pub fn summary(&self) -> String {
        match self {
            Self::TaskCompleted { summary } => format!("任务完成：{summary}"),
            Self::ApprovalWaiting { tool } => format!("等待审批：{tool}"),
        }
    }

    /// argv 中的机器可读事件名。命令按它决定本地通知样式。
    pub fn kind(&self) -> &'static str {
        match self {
            Self::TaskCompleted { .. } => "task_completed",
            Self::ApprovalWaiting { .. } => "approval_waiting",
        }
    }
}

/// 外部通知命令。program 与 args 分别保存，运行时绝不拼成 shell 字符串。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotificationConfig {
    pub program: String,
    pub args: Vec<String>,
    /// 单条通知命令的硬超时；超时后的进程树必须被终止。
    #[serde(with = "duration_millis")]
    pub timeout: Duration,
}

impl Default for NotificationConfig {
    fn default() -> Self {
        Self {
            program: String::new(),
            args: Vec::new(),
            timeout: Duration::from_secs(5),
        }
    }
}

/// 通知总开关。默认关闭是安全默认值：外部命令只在用户显式启用后运行。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotificationPolicy {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub command: NotificationConfig,
}

impl Default for NotificationPolicy {
    fn default() -> Self {
        Self { enabled: false, command: NotificationConfig::default() }
    }
}

/// 通知发送端口。错误以文本返回，调用方决定告警还是降级，不静默吞异常。
pub trait NotificationSink: Send + Sync {
    fn notify<'a>(&'a self, event: NotificationEvent) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>>;
}

/// 默认实现：装配层未启用通知时使用，保证零外部副作用。
pub struct NoopNotificationSink;

impl NotificationSink for NoopNotificationSink {
    fn notify<'a>(&'a self, _event: NotificationEvent) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async { Ok(()) })
    }
}

/// TUI 的终端写入端口。OSC 9 只是转义序列，具体输出目标由装配层注入。
pub trait TerminalWriter: Send + Sync {
    fn write_sequence(&self, sequence: &str) -> Result<(), String>;
}

/// 策略判定是纯函数：同一份配置对所有实现保持相同语义。
pub fn should_notify(policy: &NotificationPolicy, event: NotificationEvent) -> bool {
    policy.enabled && !policy.command.program.is_empty() && !event.summary().is_empty()
}

/// 事件转成独立 argv 片段。参数不拼接，避免 shell 元字符改变命令含义。
pub fn notification_argv(policy: &NotificationPolicy, event: NotificationEvent) -> Vec<String> {
    let mut argv = Vec::with_capacity(policy.command.args.len() + 2);
    argv.extend(policy.command.args.iter().cloned());
    argv.push(event.kind().to_owned());
    argv.push(event.summary());
    argv
}

/// 组合多个通知汇：主代理装配时可以同时登记外部命令与 TUI 序列。
pub struct CompositeNotificationSink {
    sinks: Vec<Arc<dyn NotificationSink>>,
}

impl CompositeNotificationSink {
    pub fn new(sinks: Vec<Arc<dyn NotificationSink>>) -> Self {
        Self { sinks }
    }
}

impl NotificationSink for CompositeNotificationSink {
    fn notify<'a>(&'a self, event: NotificationEvent) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async move {
            let mut errors = Vec::new();
            for sink in &self.sinks {
                if let Err(error) = sink.notify(event.clone()).await {
                    errors.push(error);
                }
            }
            if errors.is_empty() { Ok(()) } else { Err(errors.join("；")) }
        })
    }
}

mod duration_millis {
    use std::time::Duration;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(value: &Duration, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u64(value.as_millis() as u64)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Duration, D::Error> {
        Ok(Duration::from_millis(u64::deserialize(deserializer)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_policy_never_commands() {
        let event = NotificationEvent::task_completed("完成");
        assert!(!should_notify(&NotificationPolicy::default(), event));
    }

    #[test]
    fn argv_keeps_event_and_summary_separate() {
        let policy = NotificationPolicy {
            enabled: true,
            command: NotificationConfig {
                program: "notify".to_owned(),
                args: vec!["--app".to_owned(), "qch".to_owned()],
                timeout: Duration::from_secs(5),
            },
        };
        let argv = notification_argv(&policy, NotificationEvent::approval_waiting("run_command"));
        assert_eq!(argv, vec!["--app", "qch", "approval_waiting", "等待审批：run_command"]);
    }
}

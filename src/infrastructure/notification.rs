//! OS 通知的基础设施实现。
//!
//! 外部命令直接按 argv 启动，不经 shell 字符串拼接。进程生命周期复用
//! `infrastructure::sandbox` 的 Job Object：先建边界再 spawn，超时显式
//! 终止整树，守卫 drop 再兜底一次。TUI 只写 OSC 9 转义序列。

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use tokio::io::AsyncReadExt;

use crate::application::notification::{
    NotificationEvent, NotificationPolicy, NotificationSink, TerminalWriter, notification_argv,
    should_notify,
};
use crate::domain::sandbox::{PlatformCapabilities, SandboxPolicy};
use crate::infrastructure::sandbox::{JobGuard, assign_child};

/// 按配置启动外部通知命令。默认关闭时连 spawn 都不发生。
pub struct CommandNotificationSink {
    policy: NotificationPolicy,
    configured_args: Vec<String>,
    /// 直接子进程 PID 的记录位置。测试与运维据此确认超时后的进程已退出。
    pid_path: PathBuf,
}

impl CommandNotificationSink {
    /// 保留装配方传入的 program/args，策略本身只负责开关、超时与附加参数。
    ///
    /// pid_path 从最后一个配置参数（通常是脚本或 exe 路径）的父目录推导，
    /// 避免把测试 PID 写到共享工作目录。
    pub async fn new(policy: NotificationPolicy, program: String, args: Vec<String>) -> Result<Self, String> {
        let pid_path = args
            .first()
            .map(|arg| Path::new(arg))
            .unwrap_or_else(|| Path::new(&program))
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."))
            .join("child.pid");
        Ok(Self { policy, configured_args: args, pid_path })
    }
}

impl NotificationSink for CommandNotificationSink {
    fn notify<'a>(
        &'a self,
        event: NotificationEvent,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async move {
            if !should_notify(&self.policy, event.clone()) {
                return Ok(());
            }
            let mut argv = self.configured_args.clone();
            argv.extend(notification_argv(&self.policy, event.clone()));
            let mut cmd = tokio::process::Command::new(&self.policy.command.program);
            cmd.args(&argv)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(false);
            #[cfg(windows)]
            cmd.creation_flags(crate::infrastructure::tool::shell::command_creation_flags());
            for (name, _) in std::env::vars() {
                let lower = name.to_lowercase();
                if ["key", "token", "secret", "password", "credential"]
                    .iter()
                    .any(|needle| lower.contains(needle))
                {
                    cmd.env_remove(&name);
                }
            }

            // 先建 Job 再启动命令：Windows 上边界创建失败必须拒绝执行，
            // 否则超时时无法保证孙进程一并退出。
            let capabilities = PlatformCapabilities::detect();
            let job = match JobGuard::create(&SandboxPolicy::default()) {
                Ok(job) => Some(job),
                Err(error) if !capabilities.has_process_sandbox => {
                    eprintln!("警告：{error}");
                    None
                }
                Err(error) => {
                    return Err(format!("通知沙箱创建失败: {error}（fail-closed：拒绝执行）"));
                }
            };
            let mut child = cmd
                .spawn()
                .map_err(|error| format!("启动通知命令失败: {error}"))?;
            let pid = child
                .id()
                .ok_or_else(|| "通知进程启动后没有 PID，无法保证超时整树终止".to_owned())?;
            std::fs::write(&self.pid_path, pid.to_string())
                .map_err(|error| format!("记录通知进程 PID 失败: {error}"))?;
            let job = job.and_then(|guard| match assign_child(&guard, &child) {
                Ok(()) => Some(guard),
                Err(error) => {
                    eprintln!("警告：通知进程纳入沙箱失败: {error}");
                    None
                }
            });

            let mut stdout_pipe = child.stdout.take().expect("stdout 已配置为 piped");
            let mut stderr_pipe = child.stderr.take().expect("stderr 已配置为 piped");
            let run = async {
                let stdout = async {
                    let mut buf = Vec::new();
                    stdout_pipe.read_to_end(&mut buf).await.map(|_| buf)
                };
                let stderr = async {
                    let mut buf = Vec::new();
                    stderr_pipe.read_to_end(&mut buf).await.map(|_| buf)
                };
                tokio::join!(stdout, stderr, child.wait())
            };
            match tokio::time::timeout(self.policy.command.timeout, run).await {
                Ok((stdout, stderr, status)) => {
                    let stdout = stdout.map_err(|error| format!("读取通知命令 stdout 失败: {error}"))?;
                    let stderr = stderr.map_err(|error| format!("读取通知命令 stderr 失败: {error}"))?;
                    let status = status.map_err(|error| format!("等待通知命令失败: {error}"))?;
                    let _ = job;
                    if status.success() {
                        Ok(())
                    } else {
                        Err(format!(
                            "通知命令退出码 {:?}；stdout={}；stderr={}",
                            status.code(),
                            String::from_utf8_lossy(&stdout),
                            String::from_utf8_lossy(&stderr)
                        ))
                    }
                }
                Err(_elapsed) => {
                    // Job 覆盖整棵树，child.kill() 是沙箱降级时的直接子进程兜底。
                    if let Some(guard) = &job {
                        guard.terminate_tree();
                    }
                    let kill_result = child.kill().await;
                    let wait_result = child.wait().await;
                    let _ = job;
                    Err(format!(
                        "通知命令超时：{} 毫秒内未结束，{}；kill={:?}；wait={:?}",
                        self.policy.command.timeout.as_millis(),
                        if capabilities.has_tree_kill { "已终止整个进程树" } else { "已终止直接子进程" },
                        kill_result.map_err(|error| error.to_string()),
                        wait_result.map(|status| status.code()).map_err(|error| error.to_string()),
                    ))
                }
            }
        })
    }
}

/// 删掉会破坏终端解析的控制字符。OSC 9 只承载一段纯文本。
fn plain_text(raw: &str) -> String {
    raw.chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect::<String>()
        .trim()
        .to_owned()
}

/// 纯函数：OSC 9 序列以 ESC ] 9 ; 起头、BEL 收尾。
pub fn osc9_sequence(event: NotificationEvent) -> String {
    format!("\u{1b}]9;{}\u{7}", plain_text(&event.summary()))
}

/// 把 OSC 9 写到注入的终端写入器，便于 TUI 与测试替换输出目标。
pub struct Osc9NotificationSink {
    writer: Arc<dyn TerminalWriter>,
}

impl Osc9NotificationSink {
    pub fn new(writer: Arc<dyn TerminalWriter>) -> Self {
        Self { writer }
    }
}

impl NotificationSink for Osc9NotificationSink {
    fn notify<'a>(
        &'a self,
        event: NotificationEvent,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async move { self.writer.write_sequence(&osc9_sequence(event)) })
    }
}

/// 内存终端：记录每次写入的完整序列，供 TUI 装配或测试断言。
#[derive(Default)]
pub struct MemoryTerminal {
    output: Mutex<Vec<String>>,
}

impl MemoryTerminal {
    pub fn output(&self) -> Vec<String> {
        self.output.lock().expect("终端写入锁已中毒").clone()
    }
}

impl TerminalWriter for MemoryTerminal {
    fn write_sequence(&self, sequence: &str) -> Result<(), String> {
        self.output
            .lock()
            .map_err(|error| format!("终端写入锁已中毒: {error}"))?
            .push(sequence.to_owned());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::notification::NotificationEvent;

    #[test]
    fn osc9_body_has_no_embedded_control_characters() {
        let sequence = osc9_sequence(NotificationEvent::task_completed("a\u{1b}]0;bad\u{7}b"));
        assert_eq!(sequence.matches('\u{1b}').count(), 1);
        assert!(sequence.ends_with('\u{7}'));
        assert!(!sequence[4..sequence.len() - 1].contains('\u{1b}'));
    }
}

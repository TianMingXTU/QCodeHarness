//! 领域标识符：会话 ID、工具调用 ID。
//!
//! newtype 包住字符串，防止任意字符串冒充 ID。工具调用 ID 尤其重要：
//! 它把模型的调用意图与 Harness 的观察结果配对，混淆会产生错配的观察，
//! 在协议层只表现为模型输出异常。
//!
//! 不引入 `uuid` 等库：领域层不关心 ID 如何生成，只关心唯一且可比较。
//! 生成策略由调用方实现（见 `application::agent_run::new_session_id`）。

use std::fmt;

use serde::{Deserialize, Serialize};

/// 会话标识。一次 `AgentRunner::run()` 对应一个会话。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionId(String);

impl SessionId {
    /// 用给定文本构造。调用方负责保证其唯一性。
    pub fn new(raw: impl Into<String>) -> Self {
        Self(raw.into())
    }

    /// 只读访问内部文本。
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// 工具调用标识，由模型在提出调用时给出。
///
/// Harness 执行完毕后凭此 ID 把观察结果送回模型，是调用与观察配对的锚点。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CallId(String);

impl CallId {
    /// 用模型给出的原始文本构造。
    pub fn new(raw: impl Into<String>) -> Self {
        Self(raw.into())
    }

    /// 只读访问内部文本。
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for CallId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// 校验可安全用作目录名或工具名后缀的标识符。
///
/// 规则面向文件系统：只允许小写 ASCII 字母、数字、下划线与连字符，
/// 必须以字母或数字开头，并排除 Windows 保留设备名。`kind` 只用于
/// 错误信息，让调用方知道是哪类标识符不合法。
pub fn validate_safe_identifier(kind: &str, id: &str, max_bytes: usize) -> Result<(), String> {
    if id.is_empty()
        || id.len() > max_bytes
        || !id
            .bytes()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == b'_' || ch == b'-')
        || !id.as_bytes()[0].is_ascii_alphanumeric()
    {
        return Err(format!(
            "{kind} `{id}` 仅允许小写字母、数字、下划线与连字符，以字母或数字开头，最长 {max_bytes} 字节"
        ));
    }
    // Windows 的设备名即使带扩展名也不能作为普通文件使用。
    let upper = id.to_ascii_uppercase();
    let device = matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ["COM", "LPT"].iter().any(|prefix| {
            upper
                .strip_prefix(prefix)
                .is_some_and(|tail| tail.len() == 1 && matches!(tail.as_bytes()[0], b'1'..=b'9'))
        });
    if device {
        return Err(format!("{kind} `{id}` 是保留设备名"));
    }
    Ok(())
}

/// 工程任务标识。一次编排（可能横跨多个会话）对应一个任务。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TaskId(String);

impl TaskId {
    /// 用给定文本构造。调用方负责保证其唯一性。
    pub fn new(raw: impl Into<String>) -> Self {
        Self(raw.into())
    }

    /// 只读访问内部文本。
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for TaskId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

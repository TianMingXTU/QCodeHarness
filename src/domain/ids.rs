//! 领域标识符：会话 ID、工具调用 ID。
//!
//! 用 newtype 包住字符串，是为了让"任意字符串"不能冒充 ID 被传来传去——
//! 尤其是工具调用 ID，它要把模型的调用意图和 Harness 的观察结果绑在一起，
//! 一旦混淆就会出现"张冠李戴"的观察，而这种错误在协议层只会表现成模型胡言乱语。
//!
//! 这里刻意不引入 `uuid` 之类的库：领域层不关心 ID 怎么生成，
//! 只关心它唯一且可比较。生成策略属于基础设施（见 `infrastructure`）。

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
/// Harness 执行完毕后凭此 ID 把观察结果送回模型，因此它是配对关系的锚点。
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

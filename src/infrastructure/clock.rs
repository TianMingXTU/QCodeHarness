//! 时钟基础设施：系统时钟与 tokio 计时器。

use std::time::Duration;

use crate::application::port::{BoxFuture, Clock, Elapsed, TimeLimiter, Timestamp};
use crate::domain::tool::ToolOutcome;

/// 读系统时间的时钟。
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Timestamp {
        Timestamp::now()
    }
}

/// 用 tokio 计时器给动作加时限。
///
/// 只在 Tokio 运行时内可用；应用层因此只依赖 [`TimeLimiter`] 端口。
#[derive(Debug, Clone, Copy)]
pub struct TokioTimeLimiter {
    limit: Duration,
}

impl TokioTimeLimiter {
    /// 时限必须大于零；零会让每次调用立刻超时。
    ///
    /// # Errors
    ///
    /// `limit` 为零时返回说明文字。
    pub fn new(limit: Duration) -> Result<Self, String> {
        if limit.is_zero() {
            return Err("动作时限必须大于零".to_owned());
        }
        Ok(Self { limit })
    }
}

impl TimeLimiter for TokioTimeLimiter {
    fn limit<'a>(
        &'a self,
        action: BoxFuture<'a, ToolOutcome>,
    ) -> BoxFuture<'a, Result<ToolOutcome, Elapsed>> {
        Box::pin(async move { tokio::time::timeout(self.limit, action).await.map_err(|_| Elapsed) })
    }
}

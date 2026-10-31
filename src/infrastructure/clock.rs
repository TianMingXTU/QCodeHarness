//! 时钟基础设施：系统时钟。

use crate::application::port::{Clock, Timestamp};

/// 读系统时间的时钟。
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Timestamp {
        Timestamp::now()
    }
}

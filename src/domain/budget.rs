//! 预算抽象：轮数与墙钟上限。
//!
//! 预算在应用层检查，不依赖外部库。独立成模块而不作为 `AgentSpec` 的
//! 字段：预算是每次运行可调的外部输入（命令行、配置），规格描述 agent
//! 本身，两者变化频率不同。

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// 一次运行的上限。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Budget {
    /// 最大 ReAct 轮数，即最大模型调用次数，用于限制模型无休止调用工具。
    ///
    /// 开启历史摘要
    /// （[`crate::domain::context::ContextBudget::with_summarize`]）后，摘要
    /// 调用也计入 `turns`，但它发生在轮内补摘要的挂点，不在本闸门的检查点
    /// 之间。实际调用数最多可能比 `max_turns` 多 1 次（该轮摘要加正式调用）。
    /// 墙钟预算不受影响。
    pub max_turns: u32,
    /// 整体墙钟上限。`None` 表示不限时。
    ///
    /// 轮数限制无法约束每轮执行 30 秒命令的任务，10 轮也要几分钟。墙钟是
    /// 兜底维度：超时即停，正在执行的轮次允许跑完。
    pub max_wall_clock: Option<Duration>,
}

impl Budget {
    /// 用给定轮数上限构造，不限时。
    pub fn new(max_turns: u32) -> Self {
        Self {
            max_turns,
            max_wall_clock: None,
        }
    }

    /// 附带整体墙钟上限。
    pub fn with_wall_clock(mut self, limit: Duration) -> Self {
        self.max_wall_clock = Some(limit);
        self
    }

    /// 预算是否已用尽（任一维度达到上限即用尽）。
    pub fn exhausted(&self, spent: &Consumption) -> bool {
        spent.turns >= self.max_turns
            || self
                .max_wall_clock
                .is_some_and(|limit| spent.elapsed >= limit)
    }
}

/// 已消耗的量。
///
/// `elapsed` 只保存测量结果。domain 不读取时间，测量由调用方通过
/// `Clock` 端口完成（见 `application::port::Clock::elapsed_since`）。
///
/// 可序列化：恢复后需要接着计算轮数与墙钟预算，不能从零开始。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Consumption {
    /// 已经发生的模型调用次数。
    pub turns: u32,
    /// 本次运行已经过的时间。
    pub elapsed: Duration,
}

impl Consumption {
    /// 记录一次模型调用。
    pub fn record_turn(&mut self) {
        self.turns += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn turn_budget_exhausts_at_the_limit_alone() {
        let budget = Budget::new(3);
        assert!(!budget.exhausted(&Consumption {
            turns: 2,
            elapsed: Duration::from_secs(999),
        }));
        assert!(budget.exhausted(&Consumption {
            turns: 3,
            elapsed: Duration::ZERO,
        }));
    }

    #[test]
    fn wall_clock_exhausts_independently_of_turns() {
        let budget = Budget::new(1_000).with_wall_clock(Duration::from_secs(30));
        assert!(!budget.exhausted(&Consumption {
            turns: 0,
            elapsed: Duration::from_secs(29),
        }));
        assert!(budget.exhausted(&Consumption {
            turns: 0,
            elapsed: Duration::from_secs(30),
        }));
    }

    #[test]
    fn no_wall_clock_ignores_elapsed_time() {
        // 回归保护：不设时限时耗时不参与判断。
        let budget = Budget::new(3);
        assert!(!budget.exhausted(&Consumption {
            turns: 0,
            elapsed: Duration::from_secs(3_600),
        }));
    }
}

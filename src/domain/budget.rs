//! 预算抽象：轮数与 token 的上限。
//!
//! 预算在应用层检查，不依赖外部库。之所以单独成模块而不是塞进 `AgentSpec`
//! 的一个字段：预算是"每次运行可调的外部输入"（命令行、配置），
//! 规格是"这个 agent 是什么"，两者变化频率不同。

/// 一次运行的上限。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Budget {
    /// 最大 ReAct 轮数，即最大模型调用次数。防止模型无限调用工具。
    pub max_turns: u32,
}

impl Budget {
    /// 用给定轮数上限构造。
    pub fn new(max_turns: u32) -> Self {
        Self { max_turns }
    }

    /// 预算是否已用尽。
    pub fn exhausted(&self, spent: &Consumption) -> bool {
        spent.turns >= self.max_turns
    }
}

/// 已消耗的量。
///
/// P0 只统计轮数。token 用量由模型响应带回，但"用超了就怎样"涉及
/// 截断历史等策略，是 P2 的事；先把计数字段的位置留出来。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Consumption {
    /// 已经发生的模型调用次数。
    pub turns: u32,
}

impl Consumption {
    /// 记录一次模型调用。
    pub fn record_turn(&mut self) {
        self.turns += 1;
    }
}

//! Plan mode 的纯编排边界。
//!
//! 规划阶段先产出计划项，再执行任务步骤。计划项与任务快照步骤按下标绑定，
//! 恢复时按这份绑定算出边界，已完成步骤不会因计划状态落后而重跑。
//! 本模块不读文件、不调用模型，也不依赖 Todo 工具的具体实现；R1 的
//! `TodoItem` 可在装配层映射为 [`PlanMode::from_todo_items`] 的三元组。

use serde::{Deserialize, Serialize};

use crate::application::snapshot::TaskSnapshot;

/// 计划项状态。只接受 Todo 工具最终约定的三种值。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PlanStepStatus {
    Pending,
    InProgress,
    Completed,
}

impl PlanStepStatus {
    /// 解析 Todo 状态字符串。未知值原样写进错误，便于定位脏数据。
    pub fn parse(raw: &str) -> Result<Self, String> {
        match raw {
            "Pending" => Ok(Self::Pending),
            "InProgress" => Ok(Self::InProgress),
            "Completed" => Ok(Self::Completed),
            other => Err(format!("未知计划项状态: {other}")),
        }
    }
}

/// 单条结构化计划，绑定任务快照的一个步骤下标。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanStep {
    pub id: String,
    pub title: String,
    pub status: PlanStepStatus,
    pub snapshot_step_index: usize,
}

/// 规划阶段的完整计划。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanMode {
    steps: Vec<PlanStep>,
    binding_count: usize,
    completed_prefix: usize,
}

impl PlanMode {
    /// 从 Todo 风格的三元组构造计划，按出现顺序绑定快照步骤下标。
    ///
    /// R1 Todo 工具提供 `id/content/status` 后，装配层只需把 `TodoItem`
    /// 与 `TodoStatus` 转成这里的三元组，避免纯编排层依赖工具实现。
    pub fn from_todo_items(
        items: Vec<(impl Into<String>, impl Into<String>, impl std::fmt::Display)>,
    ) -> Result<Self, String> {
        let mut steps = Vec::with_capacity(items.len());
        for (index, (id, title, status)) in items.into_iter().enumerate() {
            let raw = status.to_string();
            let status = PlanStepStatus::parse(&raw)?;
            steps.push(PlanStep {
                id: id.into(),
                title: title.into(),
                status,
                snapshot_step_index: index,
            });
        }
        let binding_count = steps.len();
        Ok(Self {
            steps,
            binding_count,
            completed_prefix: 0,
        })
    }

    pub fn steps(&self) -> &[PlanStep] {
        &self.steps
    }

    /// 标明快照已经完成的步骤前缀；恢复时这些步骤一律不重跑。
    ///
    /// 计划项状态可能滞后于任务快照，例如任务在第 3 步中断时 Todo 仍停在
    /// `InProgress`。任务快照优先，才不会把已完成的副作用再做一遍。
    pub fn with_completed_prefix(mut self, count: usize) -> Self {
        self.completed_prefix = count;
        self
    }

    /// 覆盖计划项到快照步骤的绑定数量，用于验证错位计划。
    pub fn with_binding_count(mut self, count: usize) -> Self {
        self.binding_count = count;
        self
    }

    /// 核对计划项数、绑定下标与快照步骤数。
    pub fn verify(&self, snapshot: &TaskSnapshot) -> Result<(), String> {
        let expected = snapshot.task.steps().len();
        if self.steps.len() != expected || self.binding_count != expected {
            return Err(format!(
                "计划项 {} 条、绑定 {} 条，与任务快照 {} 个步骤不一致",
                self.steps.len(),
                self.binding_count,
                expected
            ));
        }
        for (index, step) in self.steps.iter().enumerate() {
            if step.snapshot_step_index != index {
                return Err(format!(
                    "计划项 {} 绑定步骤 {}，按下标应绑定 {}",
                    step.id, step.snapshot_step_index, index
                ));
            }
        }
        if self.completed_prefix > expected {
            return Err(format!(
                "快照完成前缀 {} 超过 {} 个步骤",
                self.completed_prefix, expected
            ));
        }
        Ok(())
    }

    /// 算恢复边界：先取快照完成前缀，再取第一个尚未完成的计划项。
    ///
    /// 任务失败于中途时，`current_index` 与 `failure.step` 都指向未完成
    /// 的那一步；边界只向后合并，不把 Completed 计划项重新排队。
    pub fn resume_boundary(&self) -> Result<PlanBoundary, String> {
        let mut completed = (0..self.completed_prefix).collect::<Vec<_>>();
        let mut next = None;
        for step in &self.steps {
            match step.status {
                PlanStepStatus::Completed => {
                    if !completed.contains(&step.snapshot_step_index) {
                        completed.push(step.snapshot_step_index);
                    }
                }
                PlanStepStatus::Pending | PlanStepStatus::InProgress => {
                    if next.is_none()
                        && step.snapshot_step_index >= self.completed_prefix
                        && !completed.contains(&step.snapshot_step_index)
                    {
                        next = Some(step.snapshot_step_index);
                    }
                }
            }
        }
        completed.sort_unstable();
        let Some(next_step_index) = next.or_else(|| {
            (self.steps.len() > self.completed_prefix && self.completed_prefix < self.steps.len())
                .then_some(self.completed_prefix)
        }) else {
            return Err("计划项全部完成，没有可恢复的步骤".to_owned());
        };
        Ok(PlanBoundary {
            next_step_index,
            completed_step_indexes: completed,
        })
    }
}

/// 恢复时刻的计划边界。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanBoundary {
    pub next_step_index: usize,
    pub completed_step_indexes: Vec<usize>,
}

/// 一次恢复执行的结果，供调用方写回计划状态或证据。
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct StepExecutionOutcome {
    pub completed_step_indexes: Vec<usize>,
}

/// 纯编排用的步骤执行替身。
///
/// 真实执行由 Orchestrator 完成；这里只验证恢复边界是否让已完成步骤
/// 在重复调用时保持幂等，不引入文件或模型调用。
#[derive(Debug, Default)]
pub struct StepExecutor {
    executed: Vec<usize>,
}

impl StepExecutor {
    /// 从边界开始执行给定步骤，跳过边界已完成项和本执行器已跑过的项。
    pub fn execute_from(
        &mut self,
        boundary: &PlanBoundary,
        steps: &[usize],
    ) -> (Vec<usize>, StepExecutionOutcome) {
        let mut run_now = Vec::new();
        for step in steps {
            if *step < boundary.next_step_index
                || boundary.completed_step_indexes.contains(step)
                || self.executed.contains(step)
            {
                continue;
            }
            run_now.push(*step);
            self.executed.push(*step);
        }
        (
            run_now.clone(),
            StepExecutionOutcome {
                completed_step_indexes: run_now,
            },
        )
    }
}

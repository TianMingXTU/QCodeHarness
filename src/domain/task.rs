//! 工程任务的领域模型：类型、步骤与状态机。
//!
//! `Task` 表达"一个要做的工程任务被拆成有序步骤"，状态只能经领域方法
//! 转移——编排器（`application::orchestrator`）驱动它，但不能直接改它。
//! 进度只有一个真相来源：`TaskState::Running { step }`。规格草图里的
//! 独立 `current` 字段与它重复，两处同存必然漂移，这里砍掉。

use serde::{Deserialize, Serialize};

use crate::domain::agent::AgentRole;
use crate::domain::ids::TaskId;

/// 任务类型。决定默认走哪个模板。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaskKind {
    /// 修 Bug：复现 → 定位 → 修改 → 测试 → 汇总。
    BugFix,
    /// 加功能：理解需求 → 拆解 → 实现 → 补测试 → 验收。
    Feature,
    /// 代码评审：读 diff → 分析上下文 → 识别风险 → 给建议。
    Review,
    /// 重构：定基线 → 分批改 → 回归 → 对比。
    Refactor,
}

impl TaskKind {
    /// 从 CLI 的 `--kind` 文本解析。既接受模板名（`bug-fix`）也接受
    /// 紧凑写法（`bugfix`）——用户不会去记两种命名哪边才是"对的"。
    pub fn parse_name(text: &str) -> Option<Self> {
        match text.to_ascii_lowercase().as_str() {
            "bug-fix" | "bugfix" => Some(Self::BugFix),
            "feature" => Some(Self::Feature),
            "review" => Some(Self::Review),
            "refactor" => Some(Self::Refactor),
            _ => None,
        }
    }

    /// 模板名，与 [`crate::domain::workflow`] 里的内置模板一一对应。
    pub fn name(&self) -> &'static str {
        match self {
            Self::BugFix => "bug-fix",
            Self::Feature => "feature",
            Self::Review => "review",
            Self::Refactor => "refactor",
        }
    }
}

/// 任务的当前状态。只能经 [`Task`] 的领域方法转移。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskState {
    /// 已创建，未开始。
    Pending,
    /// 正在执行第 `step` 步（0-based）。这是进度的唯一真相来源。
    Running { step: usize },
    /// 所有步骤完成。
    Completed,
    /// 中途失败。失败详情在 [`Task::failure`]。
    Failed,
}

/// 一步任务：做什么、谁来做、做到什么程度算完。
#[derive(Debug, Clone, PartialEq)]
pub struct TaskStep {
    /// 步骤说明，给人看也给模型看。必须自包含——每步是独立会话，
    /// 模型看不到上一步说了什么，"继续"这种说明毫无意义。
    pub description: String,
    /// 这一步期望 Agent 扮演的角色。P4 只占位（统一 `Coder`），
    /// P5 多 Agent 时用于选 Agent。
    pub role: AgentRole,
    /// 这一步的完成判据，拼进步骤 prompt 让模型自检。
    pub done_when: String,
}

/// 失败的记录：停在哪一步、为什么。
#[derive(Debug, Clone, PartialEq)]
pub struct TaskFailure {
    /// 失败发生在第几步（0-based）。
    pub step: usize,
    /// 失败原因，给调用方展示。
    pub reason: String,
}

/// 一次工程任务。
#[derive(Debug, Clone)]
pub struct Task {
    id: TaskId,
    kind: TaskKind,
    goal: String,
    steps: Vec<TaskStep>,
    state: TaskState,
    /// 失败详情。`state == Failed` 时必有值。
    failure: Option<TaskFailure>,
}

/// 任务状态机的非法转移。
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum TaskError {
    /// `start()` 只对 `Pending` 合法。
    #[error("任务已开始，不能再次 start")]
    AlreadyStarted,
    /// `advance()` 只对 `Running` 合法；已完成或已失败的任务不能再推进。
    #[error("任务不在运行态，不能再推进")]
    NotRunning,
    /// 没有步骤的任务不能开始。空步骤任务一旦放行，会"零调用完成"——
    /// 编排器跑完空循环就把任务标成 `Completed`，比报错难发现得多。
    #[error("任务没有任何步骤，无法开始（模板可能缺失或未找到）")]
    NoSteps,
}

impl Task {
    /// 创建任务，初始 [`TaskState::Pending`]。步骤来自模板展开（见
    /// [`crate::domain::workflow`]），也可以手工构造。
    pub fn new(
        id: TaskId,
        kind: TaskKind,
        goal: impl Into<String>,
        steps: Vec<TaskStep>,
    ) -> Self {
        Self {
            id,
            kind,
            goal: goal.into(),
            steps,
            state: TaskState::Pending,
            failure: None,
        }
    }

    /// 开始执行：`Pending → Running { step: 0 }`。
    ///
    /// # Errors
    ///
    /// 任务不是 `Pending` 时返回 [`TaskError::AlreadyStarted`]；
    /// 没有任何步骤时返回 [`TaskError::NoSteps`]。
    pub fn start(&mut self) -> Result<(), TaskError> {
        if self.state != TaskState::Pending {
            return Err(TaskError::AlreadyStarted);
        }
        if self.steps.is_empty() {
            return Err(TaskError::NoSteps);
        }
        self.state = TaskState::Running { step: 0 };
        Ok(())
    }

    /// 当前一步完成，推进一步；最后一步之后转 `Completed`。
    ///
    /// # Errors
    ///
    /// 任务不在 `Running` 态时返回 [`TaskError::NotRunning`]。
    pub fn advance(&mut self) -> Result<(), TaskError> {
        let TaskState::Running { step } = self.state else {
            return Err(TaskError::NotRunning);
        };
        self.state = if step + 1 < self.steps.len() {
            TaskState::Running { step: step + 1 }
        } else {
            TaskState::Completed
        };
        Ok(())
    }

    /// 标记失败。任何状态都可以转 `Failed`（终态）：规划阶段就能发现
    /// 任务做不下去，不必先 `start()`。
    pub fn fail(&mut self, reason: impl Into<String>) {
        let step = match self.state {
            TaskState::Running { step } => step,
            _ => 0,
        };
        self.failure = Some(TaskFailure {
            step,
            reason: reason.into(),
        });
        self.state = TaskState::Failed;
    }

    /// 当前步骤。不在 `Running` 态时没有"当前"可言。
    pub fn current_step(&self) -> Option<&TaskStep> {
        let TaskState::Running { step } = self.state else {
            return None;
        };
        self.steps.get(step)
    }

    /// 当前进度：`(已完成步数, 总步数)`。失败停在半步时，那一步不算完成。
    pub fn progress(&self) -> (usize, usize) {
        let done = match self.state {
            TaskState::Pending => 0,
            TaskState::Running { step } => step,
            TaskState::Completed => self.steps.len(),
            TaskState::Failed => self.failure.as_ref().map_or(0, |f| f.step),
        };
        (done, self.steps.len())
    }

    /// 任务 ID。
    pub fn id(&self) -> &TaskId {
        &self.id
    }

    /// 任务类型。
    pub fn kind(&self) -> TaskKind {
        self.kind
    }

    /// 任务目标。
    pub fn goal(&self) -> &str {
        &self.goal
    }

    /// 当前状态。
    pub fn state(&self) -> TaskState {
        self.state
    }

    /// 全部步骤。
    pub fn steps(&self) -> &[TaskStep] {
        &self.steps
    }

    /// 失败详情。
    pub fn failure(&self) -> Option<&TaskFailure> {
        self.failure.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 一个三步任务，状态机测试的公共底座。
    fn three_step_task() -> Task {
        let step = |text: &str| TaskStep {
            description: text.to_owned(),
            role: AgentRole::Coder,
            done_when: "有产出".to_owned(),
        };
        Task::new(
            TaskId::new("t1"),
            TaskKind::BugFix,
            "修一个 bug",
            vec![step("第一步"), step("第二步"), step("第三步")],
        )
    }

    #[test]
    fn start_then_advance_reaches_completed() {
        let mut task = three_step_task();
        task.start().unwrap();
        assert_eq!(task.state(), TaskState::Running { step: 0 });

        task.advance().unwrap();
        assert_eq!(task.state(), TaskState::Running { step: 1 });
        assert_eq!(task.progress(), (1, 3));

        task.advance().unwrap();
        task.advance().unwrap();
        assert_eq!(task.state(), TaskState::Completed);
        assert_eq!(task.progress(), (3, 3));
    }

    #[test]
    fn double_start_is_rejected() {
        let mut task = three_step_task();
        task.start().unwrap();
        assert!(matches!(task.start(), Err(TaskError::AlreadyStarted)));
    }

    #[test]
    fn advance_beyond_completion_is_rejected() {
        let mut task = three_step_task();
        // 还没 start 就推进。
        assert!(matches!(task.advance(), Err(TaskError::NotRunning)));

        task.start().unwrap();
        for _ in 0..3 {
            task.advance().unwrap();
        }
        // 三步全部推完后没有第四步。
        assert!(matches!(task.advance(), Err(TaskError::NotRunning)));
    }

    #[test]
    fn failed_task_records_step_and_cannot_advance() {
        let mut task = three_step_task();
        task.start().unwrap();
        task.advance().unwrap();
        task.fail("模型调用失败");

        assert_eq!(task.state(), TaskState::Failed);
        let failure = task.failure().unwrap();
        assert_eq!(failure.step, 1, "失败应记录停在哪一步");
        assert_eq!(failure.reason, "模型调用失败");
        assert_eq!(task.progress(), (1, 3), "失败的那一步不算完成");
        assert!(matches!(task.advance(), Err(TaskError::NotRunning)));
        assert!(task.current_step().is_none(), "失败后没有当前步骤");
    }

    #[test]
    fn current_step_follows_the_state() {
        let mut task = three_step_task();
        assert!(task.current_step().is_none(), "Pending 没有当前步骤");

        task.start().unwrap();
        assert_eq!(task.current_step().unwrap().description, "第一步");

        task.advance().unwrap();
        assert_eq!(task.current_step().unwrap().description, "第二步");
    }

    #[test]
    fn empty_step_task_cannot_start() {
        let mut task = Task::new(TaskId::new("t0"), TaskKind::Review, "空任务", vec![]);
        assert!(matches!(task.start(), Err(TaskError::NoSteps)));
        assert_eq!(task.state(), TaskState::Pending, "失败后仍留在 Pending");
    }

    #[test]
    fn task_kind_round_trips_through_cli_names() {
        for (text, kind) in [
            ("bug-fix", TaskKind::BugFix),
            ("bugfix", TaskKind::BugFix),
            ("feature", TaskKind::Feature),
            ("review", TaskKind::Review),
            ("refactor", TaskKind::Refactor),
        ] {
            assert_eq!(TaskKind::parse_name(text), Some(kind));
            assert_eq!(kind.name(), TaskKind::parse_name(kind.name()).unwrap().name());
        }
        assert_eq!(TaskKind::parse_name("deploy"), None, "未知类型必须拒绝");
    }
}

//! 工程任务的领域模型：类型、步骤与状态机。
//!
//! `Task` 表示一个拆成有序步骤的工程任务。状态只能通过领域方法转移；
//! 编排器（`application::orchestrator`）驱动任务，不能直接修改状态。
//! 进度的唯一来源是 `TaskState::Running { step }`。规格草图中的独立
//! `current` 字段与此重复，两处并存必然漂移，因此未采用。

use serde::{Deserialize, Serialize};

use crate::domain::agent::AgentRole;
use crate::domain::ids::TaskId;

/// 任务类型。决定默认走哪个模板（步骤见 [`crate::domain::workflow`]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaskKind {
    /// 修 Bug：定位 → 修改 → 测试 → 汇总。
    BugFix,
    /// 加功能：理解需求 → 规划 → 实现 → 验证。
    Feature,
    /// 代码评审：读 diff → 分析上下文 → 识别风险 → 给建议。
    Review,
    /// 重构：定基线 → 分批改 → 回归 → 对比。
    Refactor,
}

impl TaskKind {
    /// 从 CLI 的 `--kind` 文本解析。同时接受模板名（`bug-fix`）与紧凑
    /// 写法（`bugfix`），用户无需记住两种命名的区别。
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaskState {
    /// 已创建，未开始。
    Pending,
    /// 正在执行第 `step` 步（0-based）。这是进度的唯一来源。
    Running { step: usize },
    /// 所有步骤完成。
    Completed,
    /// 中途失败。失败详情在 [`Task::failure`]。
    Failed,
}

/// 一步任务：做什么、谁来做、做到什么程度算完。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskStep {
    /// 步骤说明，供人和模型阅读。必须自包含。步骤边界是一条新的用户
    /// 消息，模型据此知道当前是第几步。历史中的前几步只是过程记录，
    /// 不是本步指令，"继续"没有意义。
    pub description: String,
    /// 这一步期望 Agent 扮演的角色，编排器按它选 Agent 装配。
    pub role: AgentRole,
    /// 这一步的完成判据，拼进步骤 prompt 让模型自检。
    pub done_when: String,
}

/// 失败的记录：停在哪一步、为什么。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskFailure {
    /// 失败发生在第几步（0-based）。
    pub step: usize,
    /// 失败原因，给调用方展示。
    pub reason: String,
}

/// 一步的产出：该步骤完成了什么。
///
/// 任务级快照需要保存每一步的结论，而不仅是执行进度。恢复后下一步的
/// 模型必须看到前面步骤的结论，否则只能从头推断。
///
/// 放在 domain 而非快照模块：它是任务数据（第 N 步的结论），快照只是
/// 载体。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StepOutput {
    /// 步骤号（0-based，与 [`TaskState::Running`] 的 `step` 同口径）。
    pub step: usize,
    /// 这一步执行者的角色。
    pub role: AgentRole,
    /// 这一步的结论：模型的最终答案。
    pub answer: String,
}

impl StepOutput {
    /// 将本步结论交给模型时使用的用户消息。
    ///
    /// 使用用户消息而非普通轮次：裁剪时会保留全部用户消息，丢弃轮次
    /// （见 `domain::context::compact`）。结论需要比历史保留更久，恢复后
    /// 的下一步依赖它了解第 1 步的结果。
    ///
    /// 文本完全由 [`StepOutput`] 决定。恢复时据此判断结论是否已交接，
    /// 重复恢复同一快照不会重复注入。
    pub fn handoff_message(&self) -> String {
        format!(
            "{HANDOFF_PREFIX}{} 步·{}] 结论：{}",
            self.step + 1,
            self.role.name(),
            self.answer
        )
    }
}

/// 交接消息的前缀标记。
///
/// 用途有二：恢复时识别出这是交接的结论而非步骤说明，不计入步骤说明
/// 条数；重复恢复时识别结论已交接，避免重复注入。
pub const HANDOFF_PREFIX: &str = "[已完成第 ";

/// 判断用户消息是否为结论交接。
pub fn is_handoff_message(text: &str) -> bool {
    text.starts_with(HANDOFF_PREFIX)
}

/// 一次工程任务。
///
/// `Deserialize` 为手写实现：派生版本会接受进度下标越界这类构造路径
/// 无法产生的状态。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Task {
    id: TaskId,
    kind: TaskKind,
    goal: String,
    steps: Vec<TaskStep>,
    state: TaskState,
    /// 失败详情。`state == Failed` 时必有值。
    failure: Option<TaskFailure>,
}

/// `Task` 的反序列化：逐条校验状态机不变量。
///
/// 手写而非派生：派生的 `Deserialize` 会接受任意组合，快照中可能出现
/// "停在第 99 步、总共只有 3 步"的任务，直到编排循环访问越界下标才失败。
/// 反序列化是外部数据进入领域的唯一入口，不变量必须在此拦截。
///
/// 校验四条，与构造路径对应：
/// - `Running { step }` 的 `step` 必须小于 `steps.len()`。`start()` 只置 0，
///   `advance()` 仅在 `step + 1 < len` 时前进，不会越界。
/// - `Completed` 任务必须有步骤。`start()` 拒绝空步骤任务。
/// - `Failed` 必须有失败详情。`fail()` 总是同时写入状态与详情。
/// - 失败详情的步号必须小于 `steps.len()`。`retry()` 直接用它恢复运行态，
///   越界会让任务从不存在的一步开始。`retry()` 不清除失败详情，所以状态
///   为 `Running` 时也可能带着它。
#[derive(Deserialize)]
struct TaskWire {
    id: TaskId,
    kind: TaskKind,
    goal: String,
    steps: Vec<TaskStep>,
    state: TaskState,
    #[serde(default)]
    failure: Option<TaskFailure>,
}

impl<'de> Deserialize<'de> for Task {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let wire = TaskWire::deserialize(deserializer)?;
        if let TaskState::Running { step } = wire.state {
            if step >= wire.steps.len() {
                return Err(serde::de::Error::custom(format!(
                    "任务状态是 Running {{ step: {step} }}，但只有 {} 个步骤——进度下标越界",
                    wire.steps.len()
                )));
            }
        }
        if wire.state == TaskState::Completed && wire.steps.is_empty() {
            return Err(serde::de::Error::custom(
                "任务状态是 Completed，但它没有任何步骤——空任务不可能走到完成",
            ));
        }
        if wire.state == TaskState::Failed && wire.failure.is_none() {
            return Err(serde::de::Error::custom(
                "任务状态是 Failed，却没有失败详情——失败原因会丢",
            ));
        }
        if let Some(failure) = &wire.failure {
            if failure.step >= wire.steps.len() {
                return Err(serde::de::Error::custom(format!(
                    "失败步号 {} 超出 {} 个步骤——恢复会从不存在的一步开始",
                    failure.step,
                    wire.steps.len()
                )));
            }
        }
        Ok(Self {
            id: wire.id,
            kind: wire.kind,
            goal: wire.goal,
            steps: wire.steps,
            state: wire.state,
            failure: wire.failure,
        })
    }
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
    /// 没有步骤的任务不能开始。空步骤任务会零调用完成：编排器执行空循环
    /// 后直接标记为 `Completed`，比报错更难发现。
    #[error("任务没有任何步骤，无法开始（模板可能缺失或未找到）")]
    NoSteps,
    /// 只有失败的任务能重新进入运行态。
    ///
    /// 与 [`TaskError::NotRunning`] 分开：前者表示未在运行，后者表示从未
    /// 失败。对未失败的任务调用 `retry()` 会使进度倒退或跳步。
    #[error("任务不是失败态，不能重试")]
    NotFailed,
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

    /// 标记失败。任何状态都可转为 `Failed` 终态；规划阶段发现任务无法
    /// 继续时不必先调用 `start()`。
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

    /// 将失败的任务放回运行态，从失败的那一步继续。
    ///
    /// 任务级恢复的入口。任务因预算耗尽、墙钟超时或取消而中断后，恢复
    /// 路径回到 `Running { step }`，不从头开始。
    ///
    /// 进度不倒退：停在哪一步就从该步开始。已完成的步骤不会重做，其结论
    /// 保存在快照的 `completed_outputs` 中，恢复时交回模型。
    ///
    /// # Errors
    ///
    /// 任务不是 `Failed` 时返回 [`TaskError::NotFailed`]。未失败的任务
    /// 无需重试，调用此方法通常表示调用方逻辑有误。
    pub fn retry(&mut self) -> Result<(), TaskError> {
        let TaskState::Failed = self.state else {
            return Err(TaskError::NotFailed);
        };
        let step = self.failure.as_ref().map_or(0, |failure| failure.step);
        self.state = TaskState::Running { step };
        Ok(())
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

    /// 当前进行到第几步（0-based）。
    ///
    /// 返回 [`Task::progress`] 的第一个分量。`Pending` 为 0，`Completed`
    /// 为总步数，`Failed` 为失败的那一步。
    ///
    /// 任务级恢复依据此值决定从哪一步继续，因此每种状态都必须有确定值。
    pub fn current_index(&self) -> usize {
        self.progress().0
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

    // ── 序列化与状态机不变量 ──────────────────────────────────────────

    /// 任务在各状态下都能无损往返序列化。
    ///
    /// 快照是跨进程、跨版本的持久数据。任一状态无法反序列化，对应路径上
    /// 的任务就无法恢复。因此测试覆盖四种状态，失败态同时验证失败详情。
    #[test]
    fn task_round_trips_through_serde_in_every_state() {
        let mut running = three_step_task();
        running.start().unwrap();

        let mut failed = three_step_task();
        failed.start().unwrap();
        failed.advance().unwrap();
        failed.fail("模型调用失败");

        let mut completed = three_step_task();
        completed.start().unwrap();
        completed.advance().unwrap();
        completed.advance().unwrap();
        completed.advance().unwrap();

        for task in [three_step_task(), running, failed, completed] {
            let text = serde_json::to_string(&task).expect("任务应可序列化");
            let restored: Task = serde_json::from_str(&text).expect("任务应可反序列化");
            assert_eq!(restored.state(), task.state());
            assert_eq!(restored.id(), task.id());
            assert_eq!(restored.kind(), task.kind());
            assert_eq!(restored.goal(), task.goal());
            assert_eq!(restored.steps(), task.steps());
            assert_eq!(restored.failure(), task.failure());
        }
    }

    /// 恢复时的进度下标必须落在步骤范围内。
    ///
    /// 这是任务级恢复的首要约束：`Running { step }` 是进度的唯一来源，
    /// `step >= steps.len()` 的任务会在编排循环中越界。派生的
    /// `Deserialize` 会接受这种组合，手写反序列化才能拦截。
    #[test]
    fn a_running_step_beyond_the_step_list_is_rejected() {
        let mut task = three_step_task();
        task.start().unwrap();
        let mut wire = serde_json::to_value(&task).unwrap();
        wire["state"] = serde_json::json!({"Running": {"step": 3}});

        let error = serde_json::from_value::<Task>(wire).unwrap_err();

        assert!(
            error.to_string().contains("进度下标越界"),
            "越界的进度下标必须被明确拒绝: {error}"
        );
    }

    /// 状态与详情矛盾的组合一律拒绝。
    ///
    /// `Failed` 缺少失败详情会导致恢复时丢失失败原因；`Completed` 没有
    /// 步骤则不可能实际完成。
    #[test]
    fn contradictory_state_and_details_are_rejected() {
        let mut failed = three_step_task();
        failed.fail("坏了");
        let mut wire = serde_json::to_value(&failed).unwrap();
        wire["failure"] = serde_json::Value::Null;
        let error = serde_json::from_value::<Task>(wire).unwrap_err();
        assert!(error.to_string().contains("没有失败详情"), "{error}");

        let empty: Task = Task::new(TaskId::new("t0"), TaskKind::Review, "空任务", vec![]);
        let mut wire = serde_json::to_value(&empty).unwrap();
        wire["state"] = serde_json::json!("Completed");
        let error = serde_json::from_value::<Task>(wire).unwrap_err();
        assert!(error.to_string().contains("没有任何步骤"), "{error}");
    }

    /// 失败步号同样必须落在步骤范围内。
    ///
    /// `retry()` 直接用失败步号恢复运行态。越界时编排循环一步也不跑，
    /// 任务却会以成功收尾。
    #[test]
    fn a_failure_step_beyond_the_step_list_is_rejected() {
        let mut task = three_step_task();
        task.fail("坏了");
        let mut wire = serde_json::to_value(&task).unwrap();
        wire["failure"]["step"] = serde_json::json!(3);

        let error = serde_json::from_value::<Task>(wire).unwrap_err();

        assert!(
            error.to_string().contains("失败步号"),
            "越界的失败步号必须被明确拒绝: {error}"
        );
    }
}

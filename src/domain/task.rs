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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskStep {
    /// 步骤说明，给人看也给模型看。必须自包含——步骤边界是一条新的
    /// 用户消息，模型是从它知道"现在做第几步"的；历史里虽有前面几步
    /// 的轮次，但那些是过程记录，不是这一步的指令，"继续"毫无意义。
    pub description: String,
    /// 这一步期望 Agent 扮演的角色。P4 只占位（统一 `Coder`），
    /// P5 多 Agent 时用于选 Agent。
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

/// 一步的产出：这一步做完了什么。
///
/// 任务级快照要记住"每一步的结论"，而不只是"跑到了第几步"：恢复后
/// 下一步的模型必须看得见前面几步得出了什么，否则它只能从零重新推。
///
/// 放在 domain 而不是快照模块：它是任务数据（"第 N 步的结论"），
/// 快照只是它的载体。
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
    /// 把这一步的结论交给模型时用的用户消息。
    ///
    /// 用**用户消息**而不是普通轮次：裁剪时保留的是全部用户消息、
    /// 丢弃的是轮次（见 `domain::context::compact`）。结论要活得比
    /// 历史更久——恢复后的下一步靠它知道"第 1 步查出了什么"。
    ///
    /// 文本由 [`StepOutput`] 唯一决定：恢复时用它判断"这条结论是不是
    /// 已经交接过了"，同一份快照重复恢复不会把同一句话推两遍。
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
/// 它的用处有两个：恢复时靠它认出"这条用户消息是交接来的结论，不是
/// 步骤交代"，从而不把它算进步骤交代的条数；重复恢复时靠它认出
/// "这条结论已经交接过了"，不重复推第二遍。
pub const HANDOFF_PREFIX: &str = "[已完成第 ";

/// 这条用户消息是不是一条结论交接。
pub fn is_handoff_message(text: &str) -> bool {
    text.starts_with(HANDOFF_PREFIX)
}

/// 一次工程任务。
///
/// `Deserialize` 手写（见本文件下方的实现）：派生版本会接受"进度下标
/// 越界"这类构造路径产生不出的状态。
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

/// `Task` 的反序列化：逐条校验状态机的不变量。
///
/// 为什么手写而不派生：派生出来的 `Deserialize` 会接受任何组合，
/// 于是快照里可以躺着一个"停在第 99 步、总共只有 3 个步骤"的任务——
/// 它进了编排循环才在越界下标处崩，崩在一个看不懂的地方。反序列化
/// 是外部数据进入领域的唯一入口，不变量必须在这里拦住。
///
/// 校验的三条与构造路径一一对应：
/// - `Running { step }` 的 `step` 必须 `< steps.len()`：`start()` 只置 0，
///   `advance()` 只在 `step + 1 < len` 时前进，从不越界；
/// - `Completed` 的任务必有步骤：`start()` 拒绝空步骤任务；
/// - `Failed` 必有失败详情：`fail()` 总是同时写状态与详情。
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
    /// 没有步骤的任务不能开始。空步骤任务一旦放行，会"零调用完成"——
    /// 编排器跑完空循环就把任务标成 `Completed`，比报错难发现得多。
    #[error("任务没有任何步骤，无法开始（模板可能缺失或未找到）")]
    NoSteps,
    /// 只有失败的任务能重新进入运行态。
    ///
    /// 与 [`TaskError::NotRunning`] 分开：一个是"没在跑"，一个是"没失败过"。
    /// 两种误用的后果不同——对没失败过的任务调用 `retry()` 会让进度凭空
    /// 倒退或跳步。
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

    /// 把失败的任务放回运行态，从失败的那一步接着走。
    ///
    /// P10 任务级恢复的入口：任务被中断（预算耗尽、墙钟超时、被取消）
    /// 之后，恢复的路径要能回到 `Running { step }` 而不是从头再来。
    ///
    /// 进度**不倒退**：失败时停在哪一步，恢复就从哪一步开始。
    /// 前面已经做完的步骤不会重做——它们的结论在快照的
    /// `completed_outputs` 里，恢复时会交回给模型。
    ///
    /// # Errors
    ///
    /// 任务不是 `Failed` 态时返回 [`TaskError::NotFailed`]。没失败过的
    /// 任务不需要"重试"，对它调用这个方法多半是调用方的逻辑错了。
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
    /// 与 [`Task::progress`] 的第一个分量同口径，但语义不同：进度说的是
    /// "做完了几步"，这里说的是"现在在第几步"——`Pending` 是 0，
    /// `Completed` 是总步数（全都做完了），`Failed` 是失败的那一步
    /// （那一步还没做完）。
    ///
    /// 任务级恢复要靠它决定"从哪一步接着走"，所以它必须对每种状态都
    /// 有确定的值，不能只在 `Running` 态下有意义。
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

    // ── P10 切片 1a：任务可序列化，且重建时守住状态机不变量 ──────────

    /// 任务在各个状态下都要能无损往返。
    ///
    /// 快照是跨进程、跨版本的持久数据：任何一个状态反序列化不过，
    /// 那条路径上的任务就永远恢复不了。所以四种状态全测，
    /// 失败态还要带上失败详情一起走。
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
    /// 这是任务级恢复的第一条不变量：`Running { step }` 是进度的唯一
    /// 真相来源，而 `step >= steps.len()` 的任务进了编排循环就越界。
    /// 派生 `Deserialize` 会照单全收这种组合——手写的重建路径才拦得住。
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

    /// 状态与详情自相矛盾的组合一律拒绝。
    ///
    /// `Failed` 却没失败详情，意味着"为什么停在这里"会在恢复中丢失；
    /// `Completed` 却一个步骤都没有，意味着这个任务不可能真的跑完。
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
}

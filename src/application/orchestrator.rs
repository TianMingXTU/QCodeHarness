//! 任务编排器：按模板把任务拆成步骤，逐步驱动 `AgentRunner`。
//!
//! P4 的编排策略刻意简单可预测：每步一个全新 runner、独立会话，
//! 跑完一步 `advance()` 一步，任一步失败立即停。多 Agent 调度（按
//! `role` 选 Agent）留给 P5。

use std::sync::Arc;

use crate::application::agent_run::{new_session_id, AgentDependencies, AgentRunner};
use crate::application::error::ApplicationError;
use crate::application::port::EvidenceSink;
use crate::domain::agent::AgentSpec;
use crate::domain::budget::Budget;
use crate::domain::ids::{SessionId, TaskId};
use crate::domain::task::{Task, TaskError, TaskKind, TaskState};
use crate::domain::workflow::WorkflowCatalog;

/// 每步证据汇的工厂：编排器为每步开独立会话与证据文件，怎么造汇由
/// 装配方（main）决定——应用层不依赖具体实现。步骤的会话 ID 在
/// `run` 内部生成后传给工厂，证据文件因此与步骤会话同名可互查。
pub type EvidenceFactory =
    Arc<dyn Fn(&SessionId) -> std::io::Result<Arc<dyn EvidenceSink>> + Send + Sync>;

/// 一次编排的结果。
#[derive(Debug, Clone, PartialEq)]
pub struct TaskOutcome {
    /// 最终答案（最后一步的输出）。
    pub answer: String,
    /// 本次运行完成的步数，与 [`TaskOutcome::step_sessions`] 一一对应。
    /// 从 `Running { step }` 续跑时它小于任务总步数——任务级进度看
    /// [`Task::progress`]。
    pub steps_completed: usize,
    /// 每步的会话 ID，供事后按 ID 翻各自的证据文件。
    pub step_sessions: Vec<SessionId>,
}

/// 任务编排器。
pub struct Orchestrator {
    deps: AgentDependencies,
    budget: Budget,
    /// 每步 runner 的规格骨架。角色由步骤覆盖，系统提示词沿用装配方给的。
    spec: AgentSpec,
    templates: WorkflowCatalog,
    /// 每步的证据汇工厂。
    evidence_factory: EvidenceFactory,
}

impl Orchestrator {
    /// 组装编排器。`budget` 与 `spec` 是编排级配置：每个步骤的 runner
    /// 都用同一个预算与系统提示词。
    pub fn new(
        deps: AgentDependencies,
        budget: Budget,
        spec: AgentSpec,
        evidence_factory: EvidenceFactory,
    ) -> Self {
        Self {
            deps,
            budget,
            spec,
            templates: WorkflowCatalog::builtin(),
            evidence_factory,
        }
    }

    /// 按任务类型从内置模板创建任务：模板步骤展开，`{goal}` 替换为目标。
    pub fn create_task(&self, id: TaskId, kind: TaskKind, goal: impl Into<String>) -> Task {
        let goal = goal.into();
        let steps = self
            .templates
            .find(kind)
            .map(|template| template.expand(&goal))
            .unwrap_or_default();
        Task::new(id, kind, goal, steps)
    }

    /// 按模板执行一个任务：每步一个 AgentRunner，跑完一步推进一步。
    ///
    /// 步骤历史不跨步传递（每步独立会话）——这是刻意的简化：跨步传递
    /// 会让上下文无限增长，而 `ContextBudget` 的裁剪按"轮"算，
    /// 步骤边界才是自然的压缩点。代价是步骤 prompt 必须自包含。
    ///
    /// 从 `Running { step }` 接着跑：进度只有一个真相来源，将来
    /// "编排中断恢复"就是从这里续。
    ///
    /// # Errors
    ///
    /// 任一步失败：任务标记 [`TaskState::Failed`]（记录步号与原因），
    /// 返回 [`ApplicationError::StepFailed`]，不继续后续步骤。
    /// 任务不在可执行状态时返回 [`ApplicationError::Task`]。
    pub async fn run(&mut self, task: &mut Task) -> Result<TaskOutcome, ApplicationError> {
        if matches!(task.state(), TaskState::Pending) {
            task.start()?;
        }
        let start = match task.state() {
            TaskState::Running { step } => step,
            _ => return Err(ApplicationError::Task(TaskError::NotRunning)),
        };
        let total = task.steps().len();
        let mut step_sessions = Vec::new();
        let mut last_answer = String::new();

        for index in start..total {
            match self.run_step(task, index).await {
                Ok((session_id, answer)) => {
                    step_sessions.push(session_id);
                    task.advance()?;
                    last_answer = answer;
                }
                Err(error) => {
                    let reason = format!("第 {} 步失败: {error}", index + 1);
                    task.fail(reason.clone());
                    return Err(ApplicationError::StepFailed {
                        step: index + 1,
                        message: reason,
                        // 已完成步骤的会话 ID 随错误带出：失败时刻就能
                        // 直接按 ID 翻证据，不用再去 --list-sessions 找。
                        step_sessions: step_sessions.clone(),
                    });
                }
            }
        }

        Ok(TaskOutcome {
            answer: last_answer,
            steps_completed: step_sessions.len(),
            step_sessions,
        })
    }

    /// 执行第 `index` 步（0-based），返回该步的会话 ID 与最终答案。
    ///
    /// # Errors
    ///
    /// 证据初始化、模型调用或预算耗尽时上抛；步骤失败的处理（标记任务、
    /// 停止后续）由 [`Self::run`] 统一做。
    async fn run_step(
        &self,
        task: &Task,
        index: usize,
    ) -> Result<(SessionId, String), ApplicationError> {
        // 循环边界保证 index < total；空步骤任务已被 start() 拒绝
        // （TaskError::NoSteps），到这里越界只剩程序错误，直接下标访问。
        let step = &task.steps()[index];
        let total = task.steps().len();
        let prompt = format!(
            "任务：{goal}\n当前步骤（第 {now}/{total} 步）：{description}\n完成判据：{done_when}\n达成判据后停止调用工具，直接输出最终答案。",
            goal = task.goal(),
            now = index + 1,
            total = total,
            description = step.description,
            done_when = step.done_when,
        );
        let spec = AgentSpec {
            role: step.role,
            system_prompt: self.spec.system_prompt.clone(),
        };
        let session_id = new_session_id(&*self.deps.clock);
        let evidence = (self.evidence_factory)(&session_id)
            .map_err(|error| ApplicationError::EvidenceInit(error.to_string()))?;
        let mut deps = self.deps.clone();
        deps.evidence = evidence;
        let mut runner = AgentRunner::new(spec, session_id, prompt, deps);
        let outcome = runner.run(&self.budget).await?;
        Ok((runner.session_id().clone(), outcome.answer))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::application::port::{ApprovalGate, EvidenceSink};
    use crate::application::tool_registry::ToolRegistry;
    use crate::domain::agent::AgentRole;
    use crate::domain::policy::PolicyEngine;
    use crate::infrastructure::approval::auto::AutoApproveGate;
    use crate::infrastructure::clock::SystemClock;
    use crate::infrastructure::evidence::memory::MemoryEvidenceSink;
    use crate::infrastructure::model::rig_chat::RigChatGateway;

    use rig::completion::Message;
    use rig::message::UserContent;
    use rig::test_utils::{MockCompletionModel, MockTurn};

    fn spec() -> AgentSpec {
        AgentSpec {
            role: AgentRole::Coder,
            system_prompt: "编排测试".to_owned(),
        }
    }

    fn deps(mock: &MockCompletionModel) -> AgentDependencies {
        AgentDependencies {
            model: Arc::new(RigChatGateway::new(mock.clone().erase())),
            tools: ToolRegistry::new(vec![]),
            approvals: Arc::new(AutoApproveGate) as Arc<dyn ApprovalGate>,
            evidence: Arc::new(MemoryEvidenceSink::new()) as Arc<dyn EvidenceSink>,
            clock: Arc::new(SystemClock),
            policy: PolicyEngine::new(),
            context_budget: Default::default(),
            session_store: None,
        }
    }

    /// 测试用证据工厂：每步一个内存汇，不落盘。
    fn test_factory() -> EvidenceFactory {
        Arc::new(|_session_id: &SessionId| {
            Ok(Arc::new(MemoryEvidenceSink::new()) as Arc<dyn EvidenceSink>)
        })
    }

    /// 请求历史里是否带着工具观察——步骤独立性测试的判据。
    fn contains_tool_result(message: &Message) -> bool {
        let Message::User { content } = message else {
            return false;
        };
        content
            .iter()
            .any(|c| matches!(c, UserContent::ToolResult(_)))
    }

    #[tokio::test]
    async fn orchestrated_task_completes_all_steps_independently() {
        let mock = MockCompletionModel::from_turns(vec![
            MockTurn::text("第1步产出"),
            MockTurn::text("第2步产出"),
            MockTurn::text("第3步产出"),
            MockTurn::text("第4步产出"),
        ]);
        let mut orchestrator =
            Orchestrator::new(deps(&mock), Budget::new(20), spec(), test_factory());
        let mut task = orchestrator.create_task(TaskId::new("t1"), TaskKind::BugFix, "修好它");

        // bug-fix 模板展开成 4 步，目标已替换进步骤说明。
        assert_eq!(task.steps().len(), 4);
        assert!(task.steps()[0].description.contains("修好它"));

        let outcome = orchestrator.run(&mut task).await.unwrap();

        assert_eq!(task.state(), TaskState::Completed);
        assert_eq!(outcome.steps_completed, 4);
        assert_eq!(outcome.answer, "第4步产出");
        assert_eq!(outcome.step_sessions.len(), 4, "每步一个会话");
        // 每步恰好问了一次模型，且请求历史里没有任何工具观察——
        // 步骤之间不共享历史。
        assert_eq!(mock.requests().len(), 4);
        for request in &mock.requests() {
            for message in &request.chat_history {
                assert!(
                    !contains_tool_result(message.as_ref()),
                    "步骤历史应独立，实际包含观察: {message:?}"
                );
            }
        }
        // 每步的 prompt 都自包含：总目标 + 步骤号都要在用户消息里。
        let prompts: Vec<String> = mock
            .requests()
            .iter()
            .map(|request| {
                request
                    .chat_history
                    .iter()
                    .find_map(|message| match message.as_ref() {
                        Message::User { content } => content.iter().find_map(|c| match c {
                            UserContent::Text(text) => Some(text.text.clone()),
                            _ => None,
                        }),
                        _ => None,
                    })
                    .expect("每步请求里应有本步的用户消息")
            })
            .collect();
        assert!(prompts[0].contains("任务：修好它"));
        assert!(prompts[0].contains("第 1/4 步"));
        assert!(prompts[3].contains("第 4/4 步"));
    }

    #[tokio::test]
    async fn step_failure_fails_the_task_and_stops_later_steps() {
        // 第 2 步两次拿到空回复（原始 + 重试）→ EmptyResponse → 该步失败。
        // 第 3 步的脚本不应被消费。
        let mock = MockCompletionModel::from_turns(vec![
            MockTurn::text("第1步产出"),
            MockTurn::text(""),
            MockTurn::text(""),
            MockTurn::text("第3步产出"),
        ]);
        let mut orchestrator =
            Orchestrator::new(deps(&mock), Budget::new(20), spec(), test_factory());
        let mut task = orchestrator.create_task(TaskId::new("t2"), TaskKind::Review, "评审一下");

        let error = orchestrator.run(&mut task).await.unwrap_err();

        assert!(
            matches!(&error, ApplicationError::StepFailed { step: 2, .. }),
            "应报第 2 步失败，实际: {error}"
        );
        assert!(
            error.to_string().contains("空回复"),
            "错误信息应带上原错误，实际: {error}"
        );
        assert_eq!(task.state(), TaskState::Failed);
        let failure = task.failure().unwrap();
        assert_eq!(failure.step, 1, "失败详情记录 0-based 步号");
        assert!(failure.reason.contains("第 2 步失败"));
        // 已完成步骤的会话 ID 随错误带出，失败时刻就能按 ID 翻证据。
        assert!(
            matches!(&error, ApplicationError::StepFailed { step_sessions, .. } if step_sessions.len() == 1),
            "错误应带上已完成步骤的会话 ID: {error}"
        );
        // 第 1 步问过 1 次，第 2 步问了原始 + 重试共 2 次；第 3 步的脚本
        // 原封不动——后续步骤没被执行。
        assert_eq!(mock.requests().len(), 3);
    }

    #[tokio::test]
    async fn empty_step_task_cannot_run() {
        // 空步骤任务（模板缺失的产物）必须报错，不能零调用"完成"。
        let mock = MockCompletionModel::from_turns(vec![]);
        let mut orchestrator =
            Orchestrator::new(deps(&mock), Budget::new(20), spec(), test_factory());
        let mut task = Task::new(TaskId::new("t4"), TaskKind::Review, "空任务", vec![]);

        let error = orchestrator.run(&mut task).await.unwrap_err();

        assert!(
            matches!(error, ApplicationError::Task(TaskError::NoSteps)),
            "应报 NoSteps，实际: {error}"
        );
        assert_eq!(mock.requests().len(), 0, "空任务不应触发任何模型调用");
    }

    #[tokio::test]
    async fn already_finished_task_is_rejected() {
        let mock = MockCompletionModel::from_turns(vec![MockTurn::text("完成")]);
        let mut orchestrator =
            Orchestrator::new(deps(&mock), Budget::new(20), spec(), test_factory());
        let mut task = orchestrator.create_task(TaskId::new("t3"), TaskKind::Feature, "加个功能");

        // 已失败的任务是终态：直接拒绝，不做任何模型调用。
        task.fail("预先失败");
        let error = orchestrator.run(&mut task).await.unwrap_err();
        assert!(
            matches!(error, ApplicationError::Task(TaskError::NotRunning)),
            "{error}"
        );
        assert_eq!(mock.requests().len(), 0, "终态任务不应触发任何模型调用");
    }
}

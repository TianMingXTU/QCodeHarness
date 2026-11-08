//! 任务编排器：按模板把任务拆成步骤，逐步驱动 `AgentRunner`。
//!
//! 编排策略简单可预测：任一步失败立即停。runner 按步骤的角色装配，
//! 提示词与工具白名单都来自角色档案（`AgentRoster`）。多 Agent 并行
//! 调度留给后续阶段。
//!
//! 一个任务只开一个会话。步骤之间不另起 runner，直接在同一会话里推进：
//! 换角色、交代步骤、把上一步的结论写进历史。由此得到三点：
//! - 历史跨步延续，下一步看得见上一步读过的文件；
//! - 轮数与墙钟按任务累计，不再每步各发一份预算；
//! - 缓存路由键跨步稳定，provider 才可能读回前缀缓存。
//!
//! 代价是会话历史变长，由 `ContextBudget` 在每次模型请求前裁掉（见
//! `domain::context`）。

use std::sync::Arc;

use crate::application::agent_roster::AgentRoster;
use crate::application::agent_run::{new_session_id, AgentDependencies, AgentRunner};
use crate::application::error::ApplicationError;
use crate::application::port::EvidenceSink;
use crate::application::snapshot::{TaskSnapshot, TaskStore, TASK_FORMAT};
use crate::application::task_resume::{self, ResumeCheck, WorktreeChangePolicy};
use crate::application::worktree::WorktreeScanner;
use crate::domain::agent::AgentSpec;
use crate::domain::budget::Budget;
use crate::domain::evidence::AuditEvent;
use crate::domain::ids::{SessionId, TaskId};
use crate::domain::task::{StepOutput, Task, TaskError, TaskKind, TaskState};
use crate::domain::workflow::WorkflowCatalog;

/// 每任务证据汇的工厂：为整个任务唯一的会话建一份证据。
///
/// 具体实现由装配方（main）决定，应用层不依赖它。会话 ID 在 `run` 内部
/// 生成后传给工厂，证据文件因此与会话同名，可互相查。
pub type EvidenceFactory =
    Arc<dyn Fn(&SessionId) -> std::io::Result<Arc<dyn EvidenceSink>> + Send + Sync>;

/// 一次编排的结果。
#[derive(Debug, Clone, PartialEq)]
pub struct TaskOutcome {
    /// 最终答案，即最后一步的输出。
    pub answer: String,
    /// 本次运行完成的步数。从 `Running { step }` 续跑时小于任务总步数；
    /// 任务级进度看 [`Task::progress`]。
    pub steps_completed: usize,
    /// 本次任务的会话 ID。一个任务只有一个会话，证据文件、快照与
    /// `--resume` 都用它。
    pub session_id: SessionId,
}

/// 任务编排器。
pub struct Orchestrator {
    deps: AgentDependencies,
    budget: Budget,
    templates: WorkflowCatalog,
    /// 任务证据汇工厂。`run` 与 `resume` 各调用一次，为该次运行的会话
    /// 建证据；先跑后恢复的同一任务会调用两次，证据按会话 ID 追加到
    /// 同一份文件。
    evidence_factory: EvidenceFactory,
    /// 任务快照存储。`None` 表示不落任务快照。
    ///
    /// 与 `deps.session_store` 分开：前者存"任务做到哪"，后者存
    /// "runner 停在哪"。恢复粒度不同：任务级能从步骤边界继续，
    /// 会话级只能从半轮继续。
    task_store: Option<Arc<dyn TaskStore>>,
    /// 工作区扫描器：存快照时记一份清单，恢复时拿它比对。
    ///
    /// `None` 表示不做一致性检查。快照里的清单为空，恢复时没有可比的
    /// "当时的样子"，检查降级为"查不了"。
    worktree_scanner: Option<Arc<dyn WorktreeScanner>>,
    /// 工作区被改动时的处置策略。
    worktree_policy: WorktreeChangePolicy,
}

impl Orchestrator {
    /// 组装编排器，使用内置花名册。`budget` 是编排级配置，每个步骤的
    /// runner 共用。
    pub fn new(deps: AgentDependencies, budget: Budget, evidence_factory: EvidenceFactory) -> Self {
        Self::with_roster(deps, budget, evidence_factory, AgentRoster::builtin())
    }

    /// 用给定花名册组装，供测试与自定义配置使用。
    ///
    /// 花名册是必要输入：某步的角色查不到档案时该步显式报错
    /// （[`ApplicationError::ProfileMissing`]），不退回默认角色。
    ///
    /// 花名册只存一份，写进 `deps.roster`。拼系统提示词与调用层角色白名单
    /// 都用它。若另存一份，两者可能取自不同的花名册：提示词说"你是审阅者"，
    /// 白名单却按另一个角色放行。
    pub fn with_roster(
        mut deps: AgentDependencies,
        budget: Budget,
        evidence_factory: EvidenceFactory,
        roster: AgentRoster,
    ) -> Self {
        deps.roster = roster;
        Self {
            deps,
            budget,
            templates: WorkflowCatalog::builtin(),
            evidence_factory,
            task_store: None,
            worktree_scanner: None,
            worktree_policy: WorktreeChangePolicy::default(),
        }
    }

    /// 接上任务快照存储。
    ///
    /// 接上后每完成一步落一份任务快照，中断时能查到任务做到哪。没接时
    /// 任务照跑，只是不能任务级恢复。
    pub fn with_task_store(mut self, store: Arc<dyn TaskStore>) -> Self {
        self.task_store = Some(store);
        self
    }

    /// 接上工作区扫描器。
    ///
    /// 接上后任务快照带一份工作区清单，恢复时与当前清单比对，判断工作区
    /// 是否被改动。检测到改动时拒绝静默恢复。
    pub fn with_worktree_scanner(mut self, scanner: Arc<dyn WorktreeScanner>) -> Self {
        self.worktree_scanner = Some(scanner);
        self
    }

    /// 设定工作区被改动时的处置策略。
    ///
    /// 默认 [`WorktreeChangePolicy::Refuse`]，检测到改动就拒绝恢复。
    /// 用户查看差异并确认后，用 [`WorktreeChangePolicy::Confirmed`] 放行。
    pub fn with_worktree_policy(mut self, policy: WorktreeChangePolicy) -> Self {
        self.worktree_policy = policy;
        self
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

    /// 按模板执行一个任务：所有步骤在同一个会话里跑完。
    ///
    /// 步骤之间不另起 runner。`begin_step` 换角色、交代步骤、把上一步的
    /// 结论写进历史，会话 ID、轮数、墙钟与缓存路由键全部延续。
    ///
    /// 从 `Running { step }` 接着跑。进度只以任务状态为准。
    ///
    /// # Errors
    ///
    /// 任一步失败时任务标记 [`TaskState::Failed`]（记录步号与原因），
    /// 返回 [`ApplicationError::StepFailed`]，后续步骤不再执行。
    /// 任务不在可执行状态时返回 [`ApplicationError::Task`]。
    pub async fn run(&mut self, task: &mut Task) -> Result<TaskOutcome, ApplicationError> {
        let budget = self.budget;
        self.run_with_budget(task, &budget).await
    }

    /// 用调用方给的预算跑一个任务，其余与 [`Self::run`] 相同。
    ///
    /// 长任务基准的第一段需要故意设小的轮数上限来逼出中断；构造器里的
    /// 预算是全局配置，这个方法允许单次覆盖，不让全局配置为单次实验
    /// 让路。
    ///
    /// # Errors
    ///
    /// 与 [`Self::run`] 相同。
    pub async fn run_with_budget(
        &mut self,
        task: &mut Task,
        budget: &Budget,
    ) -> Result<TaskOutcome, ApplicationError> {
        if matches!(task.state(), TaskState::Pending) {
            task.start()?;
        }
        let start = match task.state() {
            TaskState::Running { step } => step,
            _ => return Err(ApplicationError::Task(TaskError::NotRunning)),
        };
        // 会话与证据在进入步骤循环前定好，每一步都往同一个会话里推。
        // 缓存路由键取会话 ID，跨步命中依赖这一点。
        let session_id = new_session_id(&*self.deps.clock);
        let evidence = (self.evidence_factory)(&session_id)
            .map_err(|error| ApplicationError::EvidenceInit(error.to_string()))?;
        let mut runner: Option<AgentRunner> = None;
        let mut completed = 0usize;
        let mut last_answer = String::new();
        // 每一步的结论。任务级恢复用它把前面几步查到的内容交回模型
        // （见 `application::task_resume`）。
        let mut completed_outputs: Vec<StepOutput> = Vec::new();

        for index in start..task.steps().len() {
            let step_role = task.steps()[index].role;
            match self
                .run_step(task, index, budget, &session_id, &evidence, &mut runner)
                .await
            {
                Ok(answer) => {
                    task.advance()?;
                    completed += 1;
                    last_answer = answer.clone();
                    completed_outputs.push(StepOutput {
                        step: index,
                        role: step_role,
                        answer,
                    });
                    // 每完成一步落一份任务快照，中断时才能查到任务做到哪。
                    self.save_task_snapshot(task, runner.as_ref(), &completed_outputs);
                }
                Err(error) => {
                    let reason = format!("第 {} 步失败: {error}", index + 1);
                    task.fail(reason.clone());
                    // 失败时同样落快照，记录失败步号与已完成的步骤。
                    self.save_task_snapshot(task, runner.as_ref(), &completed_outputs);
                    return Err(ApplicationError::StepFailed {
                        step: index + 1,
                        message: reason,
                        // 失败步骤的观察也写在这个证据文件里。带出 ID，失败时
                        // 可以直接翻证据，不必再查 --list-sessions。
                        session_id,
                        steps_completed: completed,
                    });
                }
            }
        }

        Ok(TaskOutcome {
            answer: last_answer,
            steps_completed: completed,
            session_id,
        })
    }

    /// 把当前任务状态写成一份任务快照。
    ///
    /// 落盘失败只输出 stderr 警告。任务快照服务于恢复，一次写盘失败不应
    /// 让已跑了几十轮的任务失败（与 `AgentRunner::save_snapshot` 一致）。
    ///
    /// 未接存储时不写快照，单元测试不受影响。
    fn save_task_snapshot(
        &self,
        task: &Task,
        runner: Option<&AgentRunner>,
        completed_outputs: &[StepOutput],
    ) {
        let Some(store) = self.task_store.as_ref() else {
            return;
        };
        let Some(runner) = runner else {
            // 还没开跑，没有运行状态可存。第一步完成后 runner 一定存在。
            return;
        };
        let snapshot = TaskSnapshot {
            format_version: TASK_FORMAT,
            task: task.clone(),
            run: runner.snapshot(),
            completed_outputs: completed_outputs.to_vec(),
            worktree: self.scan_worktree(),
        };
        if let Err(error) = store.save(&snapshot) {
            eprintln!("警告：任务快照保存失败（任务继续）: {error}");
        }
    }

    /// 扫描工作区，为快照记录当时的状态。
    ///
    /// 扫描失败时返回空清单，表示当时的状态未知。恢复时工作区检查会
    /// 降级为"查不了"，不能当作没有改动。
    fn scan_worktree(&self) -> crate::domain::worktree::WorktreeManifest {
        match self.worktree_scanner.as_ref() {
            Some(scanner) => scanner.scan().unwrap_or_default(),
            None => crate::domain::worktree::WorktreeManifest::default(),
        }
    }

    /// 恢复前核对工作区，被改动时按策略处理。
    ///
    /// 三个分支：
    ///
    /// - 没动过：放行。
    /// - 动过且策略是 `Refuse`：返回 [`ApplicationError::WorktreeChanged`]，
    ///   差异清单随错误带出。
    /// - 扫不出来：打一条警告后放行。读不出来不代表没改动，但硬失败会让
    ///   本可恢复的任务卡住，而这类失败通常是环境问题。
    ///
    /// 快照里没有清单时（老版本快照，或扫描未留下基准）跳过检查，
    /// 因为没有可比基准。跳过时输出警告，让用户知道检查没有发生。
    fn check_worktree_unchanged(&self, snapshot: &TaskSnapshot) -> Result<(), ApplicationError> {
        if snapshot.worktree.is_empty() {
            eprintln!("警告：快照里没有工作区清单，跳过工作区一致性检查");
            return Ok(());
        }
        let Some(scanner) = self.worktree_scanner.as_ref() else {
            eprintln!("警告：没有装配工作区扫描器，跳过工作区一致性检查");
            return Ok(());
        };
        match task_resume::check_worktree(&snapshot.worktree, scanner.as_ref()) {
            ResumeCheck::Clean => Ok(()),
            ResumeCheck::WorktreeChanged { changes } => {
                if self.worktree_policy == WorktreeChangePolicy::Confirmed {
                    return Ok(());
                }
                Err(ApplicationError::WorktreeChanged {
                    changes: changes.iter().map(|change| change.describe()).collect(),
                })
            }
            ResumeCheck::WorktreeUnreadable { reason } => {
                eprintln!("警告：无法读取工作区（{reason}），跳过工作区一致性检查");
                Ok(())
            }
        }
    }

    /// 从任务快照恢复任务，把剩余步骤跑完。
    ///
    /// 与 [`Self::run`] 的分工：`run` 拒绝 `Failed` 状态的任务，
    /// `resume` 接手中途断掉的任务，从停下的一步继续，已完成的步骤
    /// 不重做。
    ///
    /// 恢复前的核对：
    ///
    /// 1. 快照版本与步骤交代一致（[`task_resume::check_resumable`]），
    ///    不符即拒绝恢复。
    /// 2. 工作区一致性检查：有差异且策略为 `Refuse` 时返回
    ///    [`task_resume::ResumeCheck::WorktreeChanged`]，由调用方要求
    ///    用户确认；清单缺失、扫描器未装配或工作区读不出来时打警告
    ///    放行，检查降级但不阻塞恢复。
    ///
    /// 恢复后已完成的结论会作为用户消息交回模型（见
    /// [`task_resume::hand_off_completed_outputs`]），后续步骤因此能看见
    /// 前面几步的结果。
    ///
    /// # Errors
    ///
    /// 核对不通过时返回 [`ApplicationError::Snapshot`]；工作区被改过时
    /// 返回 [`ApplicationError::WorktreeChanged`]。
    pub async fn resume(
        &mut self,
        snapshot: TaskSnapshot,
    ) -> Result<TaskOutcome, ApplicationError> {
        // 用快照记录的清单与当前工作区比对，改动过就按策略处理，默认拒绝。
        self.check_worktree_unchanged(&snapshot)?;
        // 任务快照只记录步骤边界。进程若在某一步中途退出（等审批、
        // 执行工具），实际现场在会话快照里。合并进来，恢复才不会重复
        // 已发生的副作用（见 [`task_resume::merge_session_state`]）。
        let snapshot = self.merge_session_state(snapshot);
        task_resume::check_resumable(&snapshot, None)?;
        let session_id = snapshot.run.session_id.clone();
        let evidence = (self.evidence_factory)(&session_id)
            .map_err(|error| ApplicationError::EvidenceInit(error.to_string()))?;
        // 恢复沿用编排器的预算，轮数上限按整个任务计算。
        let budget = &self.budget;

        let mut task = snapshot.task.clone();
        // 任务被中断时会停在 Failed。放回运行态，从失败的那一步继续。
        if matches!(task.state(), TaskState::Failed) {
            task.retry()?;
        }
        let start = match task.state() {
            TaskState::Running { step } => step,
            TaskState::Completed => {
                return Ok(TaskOutcome {
                    answer: snapshot
                        .completed_outputs
                        .last()
                        .map(|output| output.answer.clone())
                        .unwrap_or_default(),
                    steps_completed: task.steps().len(),
                    session_id,
                });
            }
            _ => return Err(ApplicationError::Task(TaskError::NotRunning)),
        };

        // runner 从快照里的运行状态重建，会话历史、轮数、墙钟、缓存
        // 路由键全部延续。
        let mut deps = self.deps.clone();
        deps.evidence = evidence.clone();
        let mut runner = AgentRunner::resume(snapshot.run.clone(), deps)?;
        // 把已完成步骤的结论交回模型，否则下一步只能从零重推。
        task_resume::hand_off_completed_outputs(&mut runner, &snapshot.completed_outputs);

        let mut completed = 0usize;
        let mut last_answer = String::new();
        let mut completed_outputs = snapshot.completed_outputs.clone();
        // runner 放在循环外，每一步都在同一个 runner 上推进，状态跨步延续。
        let mut runner = Some(runner);

        for index in start..task.steps().len() {
            let step_role = task.steps()[index].role;
            match self
                .run_step(&task, index, budget, &session_id, &evidence, &mut runner)
                .await
            {
                Ok(answer) => {
                    task.advance()?;
                    completed += 1;
                    last_answer = answer.clone();
                    completed_outputs.push(StepOutput {
                        step: index,
                        role: step_role,
                        answer,
                    });
                    self.save_task_snapshot(&task, runner.as_ref(), &completed_outputs);
                }
                Err(error) => {
                    let reason = format!("第 {} 步失败: {error}", index + 1);
                    task.fail(reason.clone());
                    self.save_task_snapshot(&task, runner.as_ref(), &completed_outputs);
                    return Err(ApplicationError::StepFailed {
                        step: index + 1,
                        message: reason,
                        session_id,
                        steps_completed: completed,
                    });
                }
            }
        }

        Ok(TaskOutcome {
            answer: last_answer,
            steps_completed: completed,
            session_id,
        })
    }

    /// 把会话快照里的半轮状态合并进任务快照。
    ///
    /// 读盘在这里完成，合并逻辑在 [`task_resume::merge_session_state`]
    /// （纯函数，可单独测试）。会话快照读不出来或损坏时按"没有现场"处理：
    /// 退回步骤边界恢复只会重做那一步，用损坏的现场继续则可能出错。
    fn merge_session_state(&self, snapshot: TaskSnapshot) -> TaskSnapshot {
        let Some(store) = self.deps.session_store.as_ref() else {
            return snapshot;
        };
        let session = match store.load(&snapshot.run.session_id) {
            Ok(Some(session)) => Some(session),
            Ok(None) => None,
            Err(error) => {
                eprintln!("警告：会话快照读取失败（按步骤边界恢复）: {error}");
                None
            }
        };
        task_resume::merge_session_state(snapshot, session)
    }

    /// 执行第 `index` 步（0-based），返回该步的最终答案。
    ///
    /// 按步骤的角色装配。系统提示词来自角色档案，工具白名单由 runner 按
    /// 同一角色在调用层校验（见 [`AgentRunner::begin_step`]）。第一步
    /// 新建 runner，后续步骤在同一个 runner 上推进。
    ///
    /// # Errors
    ///
    /// 角色缺档案、证据初始化失败、模型调用失败或预算耗尽时上抛。
    /// 步骤失败的处理（标记任务、停止后续步骤）由 [`Self::run`] 统一完成。
    async fn run_step(
        &self,
        task: &Task,
        index: usize,
        budget: &Budget,
        session_id: &SessionId,
        evidence: &Arc<dyn EvidenceSink>,
        runner: &mut Option<AgentRunner>,
    ) -> Result<String, ApplicationError> {
        // 循环保证 index < total；空步骤任务已被 start() 以 NoSteps 拒绝，
        // 越界只可能是程序错误，直接用下标访问。
        let step = &task.steps()[index];
        let total = task.steps().len();
        let profile = self
            .deps
            .roster
            .find(step.role)
            .ok_or_else(|| ApplicationError::ProfileMissing {
                step: index + 1,
                role: step.role.name().to_owned(),
            })?;
        let spec = AgentSpec {
            role: step.role,
            system_prompt: profile.system_prompt.clone(),
            // 角色专属的模型与档位：档案有配置就用，否则用运行时默认值。
            // 规划需要强推理，测试只需跑命令读输出，两者对能力的要求不同。
            model: profile.model.clone(),
            effort: profile.effort,
            verbosity: profile.verbosity,
        };
        let prompt = crate::application::step_prompt::step_prompt(
            task.goal(),
            step,
            index,
            total,
        );

        let mut active = match runner.take() {
            Some(mut existing) => {
                existing.begin_step(spec, prompt)?;
                existing
            }
            None => {
                let mut deps = self.deps.clone();
                deps.evidence = evidence.clone();
                AgentRunner::new(spec, session_id.clone(), prompt, deps)
            }
        };
        // 记录步骤开始时的角色与轮次，事后查证据不必再对照快照。
        evidence.record(
            self.deps.clock.now(),
            AuditEvent::StepStarted {
                turn: active.session().rounds() + 1,
                role: step.role,
            },
        );
        // 这一步失败时仍要把 runner 放回去。它持有本轮的历史、轮数与
        // 墙钟，恢复所需的状态都在这里。丢弃后任务快照里的会话会停在上一步
        // 结束处，这一步的交代与观察都会丢失。
        let outcome = active.run(budget).await;
        *runner = Some(active);
        Ok(outcome?.answer)
    }
}
#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use super::*;
    use crate::application::context_policy::DefaultContextPolicy;
    use crate::application::port::{ApprovalGate, EvidenceSink, NoopHooks};
    use crate::application::tool_registry::ToolRegistry;
    use crate::domain::agent::AgentRole;
    use crate::domain::context::ContextBudget;
    use crate::domain::policy::PolicyEngine;
    use crate::domain::workspace::WorkspaceRoot;
    use crate::infrastructure::approval::auto::AutoApproveGate;
    use crate::infrastructure::clock::SystemClock;
    use crate::application::memory_evidence::MemoryEvidenceSink;
    use crate::infrastructure::model::rig_chat::RigChatGateway;
    use crate::infrastructure::tool::all_tools;

    use rig::completion::Message;
    use rig::message::UserContent;
    use rig::test_utils::{MockCompletionModel, MockTurn};
    use serde_json::json;

    fn deps(mock: &MockCompletionModel) -> AgentDependencies {
        AgentDependencies {
            model: Arc::new(RigChatGateway::new(mock.clone().erase())),
            tools: ToolRegistry::new(vec![]),
            approvals: Arc::new(AutoApproveGate) as Arc<dyn ApprovalGate>,
            evidence: Arc::new(MemoryEvidenceSink::new()) as Arc<dyn EvidenceSink>,
            clock: Arc::new(SystemClock),
            policy: PolicyEngine::new(),
            roster: AgentRoster::builtin(),
            context_budget: Default::default(),
            session_store: None,
            hooks: Arc::new(NoopHooks),
            context_policy: Arc::new(DefaultContextPolicy),
            additional_guards: Vec::new(),
        }
    }

    /// 测试用证据工厂：每个任务一个内存汇，不落盘。
    fn test_factory() -> EvidenceFactory {
        Arc::new(|_session_id: &SessionId| {
            Ok(Arc::new(MemoryEvidenceSink::new()) as Arc<dyn EvidenceSink>)
        })
    }

    /// 共享一个内存汇的工厂：编排器只调用一次，拿到的是整个任务的证据。
    /// 统计"工具被调了几次"只能从这里数。请求历史会重放此前的调用，
    /// 按历史计数会重复。
    fn shared_factory() -> (EvidenceFactory, Arc<MemoryEvidenceSink>) {
        let sink = Arc::new(MemoryEvidenceSink::new());
        let cloned = sink.clone();
        let factory: EvidenceFactory =
            Arc::new(move |_session_id: &SessionId| Ok(cloned.clone() as Arc<dyn EvidenceSink>));
        (factory, sink)
    }

    /// 全量 10 个内置工具的注册表，使用真实规格，供角色边界测试使用。
    /// 工具会被真实执行，白名单测试依赖文件是否被改动来判断。
    fn full_registry() -> ToolRegistry {
        let root = std::env::temp_dir().join(format!("qch-orch-tools-{}", std::process::id()));
        ToolRegistry::new(all_tools(WorkspaceRoot::new(root)))
    }

    /// 请求里模型可见的工具名（按注册顺序）。
    fn offered_tools(mock: &MockCompletionModel, index: usize) -> Vec<String> {
        mock.requests()[index]
            .tools
            .iter()
            .map(|tool| tool.name.as_str().to_owned())
            .collect()
    }

    /// 请求里工具定义的序列化形式，用于断言逐字相同。
    ///
    /// 只比较名字不够：描述或参数 schema 变化同样会破坏前缀缓存，
    /// 这种变化在名字列表上看不出来。
    fn tools_json(mock: &MockCompletionModel, index: usize) -> String {
        serde_json::to_string(&mock.requests()[index].tools).expect("工具定义应可序列化")
    }

    /// 请求里发给模型的系统提示词。
    fn system_prompt_of(mock: &MockCompletionModel, index: usize) -> String {
        mock.requests()[index]
            .system_instructions()
            .expect("每步请求都应带系统提示词")
            .to_owned()
    }

    /// 请求历史里出现的全部用户消息文本。
    fn user_messages(mock: &MockCompletionModel, index: usize) -> Vec<String> {
        mock.requests()[index]
            .chat_history
            .iter()
            .filter_map(|message| match message.as_ref() {
                Message::User { content } => content
                    .iter()
                    .filter_map(|part| match part {
                        UserContent::Text(text) => Some(text.text.clone()),
                        UserContent::ToolResult(_) => None,
                        _ => None,
                    })
                    .reduce(|mut all, text| {
                        all.push('\n');
                        all.push_str(&text);
                        all
                    }),
                _ => None,
            })
            .collect()
    }

    /// 请求历史的序列化形式，用于断言上一步的结论是否进入下一步。
    fn history_text(mock: &MockCompletionModel, index: usize) -> String {
        serde_json::to_string(&mock.requests()[index].chat_history).expect("历史应可序列化")
    }

    #[tokio::test]
    async fn orchestrated_task_completes_all_steps_in_one_session() {
        let mock = MockCompletionModel::from_turns(vec![
            MockTurn::text("第1步产出"),
            MockTurn::text("第2步产出"),
            MockTurn::text("第3步产出"),
            MockTurn::text("第4步产出"),
        ]);
        let mut orchestrator = Orchestrator::new(deps(&mock), Budget::new(20), test_factory());
        let mut task = orchestrator.create_task(TaskId::new("t1"), TaskKind::BugFix, "修好它");

        // bug-fix 模板展开成 4 步，目标已替换进步骤说明。
        assert_eq!(task.steps().len(), 4);
        assert!(task.steps()[0].description.contains("修好它"));

        let outcome = orchestrator.run(&mut task).await.unwrap();

        assert_eq!(task.state(), TaskState::Completed);
        assert_eq!(outcome.steps_completed, 4);
        assert_eq!(outcome.answer, "第4步产出");
        assert_eq!(mock.requests().len(), 4, "一步一问");
        // 一个任务一个会话：四次请求必须用同一个缓存路由键。换键会让
        // provider 作废此前的前缀缓存，这是单会话的主要收益。
        for request in &mock.requests() {
            assert_eq!(
                request.options.cache_key.as_deref(),
                Some(outcome.session_id.as_str()),
                "每一步都必须用同一个缓存路由键"
            );
        }
        // 历史跨步延续：第 4 步看得见前三步各自的结论。
        let last = history_text(&mock, 3);
        for answer in ["第1步产出", "第2步产出", "第3步产出"] {
            assert!(last.contains(answer), "第 4 步应看见 {answer}: {last}");
        }
        // 每一步的用户消息都自包含：总目标 + 步骤号。
        for (index, expected) in [(0, "第 1/4 步"), (3, "第 4/4 步")] {
            let prompts = user_messages(&mock, index);
            assert!(
                prompts.iter().any(|text| text.contains("任务：修好它")),
                "第 {index} 步应带总目标: {prompts:?}"
            );
            assert!(
                prompts.iter().any(|text| text.contains(expected)),
                "第 {index} 步应标明步骤号: {prompts:?}"
            );
        }
    }

    #[tokio::test]
    async fn cross_step_history_means_the_same_file_is_read_once() {
        // 跨步骤不重读：规划步读一次 notes.txt，实施步的请求里应已带有
        // 该内容。
        //
        // harness 不做读取去重。两次读之间文件可能已被写入，旧观察可能
        // 过期，硬性拦截会迫使模型使用陈旧内容。不重读依赖历史可见性。
        // 因此判别断言落在历史文本上：若回归成每步一个会话，第 2 步的
        // 请求只剩步骤指令，断言会失败。mock 脚本第 2 步不再发 read_file，
        // 模拟的是模型已经看到内容的行为。
        let workspace = std::env::temp_dir().join(format!("qch-orch-reread-{}", std::process::id()));
        std::fs::create_dir_all(&workspace).expect("建临时工作区");
        std::fs::write(workspace.join("notes.txt"), "根因在解析层").expect("写 fixture 文件");
        let registry = ToolRegistry::new(all_tools(WorkspaceRoot::new(workspace.clone())));

        let mock = MockCompletionModel::from_turns(vec![
            // 第 1 步（Planner，只读）：读文件 → 给出根因。
            MockTurn::tool_call("call_r1", "read_file", json!({"path": "notes.txt"})),
            MockTurn::text("根因在解析层，notes.txt 已说明"),
            // 第 2~4 步不再读文件，只引用 notes.txt 中说明的内容。
            // 脚本可以这样写，前提是该观察已在历史里。
            MockTurn::text("按 notes.txt 的根因改好了"),
            MockTurn::text("测试通过"),
            MockTurn::text("汇总：按 notes.txt 定位的根因修复"),
        ]);
        let deps = AgentDependencies {
            tools: registry,
            ..deps(&mock)
        };
        let (factory, evidence) = shared_factory();
        let mut orchestrator = Orchestrator::new(deps, Budget::new(20), factory);
        let mut task = orchestrator.create_task(TaskId::new("t-reread"), TaskKind::BugFix, "读 notes.txt 定位并修复");

        let outcome = orchestrator.run(&mut task).await.unwrap();

        assert_eq!(task.state(), TaskState::Completed);
        // 判别断言：第 2 步的请求里带有第 1 步读到的内容。每步一个会话
        // 的旧实现下历史为空，这条断言会失败。
        let second = history_text(&mock, 2);
        assert!(second.contains("根因在解析层"), "第 2 步应已拿到文件内容: {second}");
        // 内容在后续步骤中仍然可见：第 3、4 步也能看到。两处都断言，
        // 才能确认单会话编排的连续性。
        for index in [3, 4] {
            let text = history_text(&mock, index);
            assert!(text.contains("根因在解析层"), "第 {index} 步应仍带着第 1 步的内容: {text}");
        }
        // 脚本里模型只发出过一次 read_file。
        let reads = evidence
            .events()
            .iter()
            .filter(|entry| {
                matches!(&entry.event, AuditEvent::ToolObserved { call, .. } if call.name == "read_file")
            })
            .count();
        assert_eq!(reads, 1, "mock 脚本全程只读了一次");
        assert!(outcome.answer.contains("notes.txt"));

        let _ = std::fs::remove_dir_all(&workspace);
    }

    #[tokio::test]
    async fn tool_definitions_are_identical_across_steps() {
        // 每一步发给模型的工具定义必须逐字相同。否则每步的工具数组都
        // 不同，provider 的前缀缓存无法命中，单会话节省的历史传递会被
        // 工具定义的抖动抵消。
        let mock = MockCompletionModel::from_turns(vec![
            MockTurn::text("规划产出"),
            MockTurn::text("实现产出"),
            MockTurn::text("验证产出"),
            MockTurn::text("汇总产出"),
        ]);
        let deps = AgentDependencies {
            tools: full_registry(),
            ..deps(&mock)
        };
        let mut orchestrator = Orchestrator::new(deps, Budget::new(20), test_factory());
        let mut task = orchestrator.create_task(TaskId::new("t5"), TaskKind::BugFix, "修好它");

        orchestrator.run(&mut task).await.unwrap();

        assert_eq!(mock.requests().len(), 4);
        // 全量注册表，不按角色裁剪模型可见的工具。
        // 10 = read_file / read_files / list_dir / write_file / search_text /
        //      edit_file / apply_patch / run_command / git_status / git_diff。
        assert_eq!(offered_tools(&mock, 0).len(), 10);
        let baseline = tools_json(&mock, 0);
        for index in 1..4 {
            assert_eq!(
                tools_json(&mock, index),
                baseline,
                "第 {index} 步的工具定义必须与第 1 步逐字相同"
            );
        }
    }

    #[tokio::test]
    async fn each_step_keeps_its_own_prompt_but_shares_one_tool_set() {
        // 提示词按角色不同（规划者/实施者/测试者），工具集完全一样。
        let mock = MockCompletionModel::from_turns(vec![
            MockTurn::text("规划产出"),
            MockTurn::text("实现产出"),
            MockTurn::text("验证产出"),
            MockTurn::text("汇总产出"),
        ]);
        let deps = AgentDependencies {
            tools: full_registry(),
            ..deps(&mock)
        };
        let mut orchestrator = Orchestrator::new(deps, Budget::new(20), test_factory());
        let mut task = orchestrator.create_task(TaskId::new("t5b"), TaskKind::BugFix, "修好它");

        orchestrator.run(&mut task).await.unwrap();

        let prompts: Vec<String> = (0..4).map(|index| system_prompt_of(&mock, index)).collect();
        assert!(prompts[0].contains("规划者"), "第 1 步应是规划者提示词");
        assert!(prompts[1].contains("实施者"), "第 2 步应是编码者提示词");
        assert!(prompts[2].contains("测试者"), "第 3 步应是测试者提示词");
        for (a, b) in prompts.iter().zip(prompts.iter().skip(1)) {
            assert_ne!(a, b, "相邻步骤的提示词应不同");
        }
        // 第 2、4 步同为 Coder，提示词必须相同：同一个 runner 只换 spec，
        // 同一角色的提示词不应变化。
        assert_eq!(prompts[1], prompts[3]);
    }

    #[tokio::test]
    async fn role_whitelist_is_enforced_at_the_call_layer() {
        // 角色白名单在调用层生效。Reviewer 尝试写文件时，工具定义里能
        // 看到 write_file（对所有步骤统一），但调用在分发前被角色边界
        // 拒绝，文件没有改动。
        let workspace =
            std::env::temp_dir().join(format!("qch-orch-whitelist-{}", std::process::id()));
        std::fs::create_dir_all(&workspace).expect("建临时工作区");
        let target = workspace.join("keep.txt");
        std::fs::write(&target, "原文").expect("写初始内容");
        let registry = ToolRegistry::new(all_tools(WorkspaceRoot::new(workspace.clone())));

        let mock = MockCompletionModel::from_turns(vec![
            MockTurn::tool_call(
                "call_w1",
                "write_file",
                json!({"path": "keep.txt", "content": "被改"}),
            ),
            MockTurn::text("第 1 步评审结论"),
            // 第 2 步换一种越权：执行命令（Tester 有权限，Reviewer 没有）。
            MockTurn::tool_call("call_c1", "run_command", json!({"command": "echo hi"})),
            MockTurn::text("第 2 步评审结论"),
            MockTurn::text("第 3 步评审结论"),
            MockTurn::text("第 4 步评审结论"),
        ]);
        let deps = AgentDependencies {
            tools: registry,
            ..deps(&mock)
        };
        let mut orchestrator = Orchestrator::new(deps, Budget::new(20), test_factory());
        let mut task = orchestrator.create_task(TaskId::new("t6"), TaskKind::Review, "评审一下");

        let outcome = orchestrator.run(&mut task).await.unwrap();

        assert_eq!(task.state(), TaskState::Completed);
        assert_eq!(outcome.answer, "第 4 步评审结论");
        // 越权调用不允许执行，不存在"审一下再放行"。
        assert_eq!(
            std::fs::read_to_string(&target).expect("读回文件"),
            "原文",
            "越权写入必须连执行都到不了"
        );
        // 拒绝观察回到模型，并说明这是角色权限问题，审批无法解决。
        let after_write_refusal = history_text(&mock, 1);
        assert!(
            after_write_refusal.contains("角色 `reviewer` 无权调用工具 `write_file`"),
            "{after_write_refusal}"
        );
        assert!(
            after_write_refusal.contains("不是审批问题"),
            "拒绝说明必须让模型知道换参数或求审批都没用: {after_write_refusal}"
        );
        // 工具定义里包含 write_file，边界在调用层，不在可见性层。
        assert!(offered_tools(&mock, 0).contains(&"write_file".to_owned()));
        // run_command 同样越权。
        let after_command_refusal = history_text(&mock, 3);
        assert!(
            after_command_refusal.contains("无权调用工具 `run_command`"),
            "{after_command_refusal}"
        );
        // 第 1、2 步各两次请求（越权调用与拒绝后的结论），第 3、4 步各一次。
        assert_eq!(mock.requests().len(), 6);

        let _ = std::fs::remove_dir_all(&workspace);
    }

    #[tokio::test]
    async fn unrestricted_role_still_gets_every_tool() {
        // 白名单不限制 Coder：`None` 表示不限，Coder 可以调用全部工具。
        let mock = MockCompletionModel::from_turns(vec![
            MockTurn::text("规划产出"),
            MockTurn::tool_call(
                "call_w1",
                "write_file",
                json!({"path": "notes.txt", "content": "改好了"}),
            ),
            MockTurn::text("测试通过"),
            MockTurn::text("汇总改动"),
            MockTurn::text("遗留风险：无"),
        ]);
        let workspace = std::env::temp_dir().join(format!("qch-orch-coder-{}", std::process::id()));
        std::fs::create_dir_all(&workspace).expect("建临时工作区");
        std::fs::write(workspace.join("notes.txt"), "旧").expect("写 fixture 文件");
        let registry = ToolRegistry::new(all_tools(WorkspaceRoot::new(workspace.clone())));
        let deps = AgentDependencies {
            tools: registry,
            ..deps(&mock)
        };
        let mut orchestrator = Orchestrator::new(deps, Budget::new(20), test_factory());
        let mut task = orchestrator.create_task(TaskId::new("t7"), TaskKind::BugFix, "改 notes.txt");

        orchestrator.run(&mut task).await.unwrap();

        assert_eq!(
            std::fs::read_to_string(workspace.join("notes.txt")).expect("读回文件"),
            "改好了",
            "Coder 不限工具，写入必须真的发生"
        );

        let _ = std::fs::remove_dir_all(&workspace);
    }

    #[tokio::test]
    async fn step_failure_fails_the_task_and_stops_later_steps() {
        // 第 2 步连续两次空回复（原始与重试）触发 EmptyResponse，该步失败。
        // 第 3 步的脚本不应被消费。
        let mock = MockCompletionModel::from_turns(vec![
            MockTurn::text("第1步产出"),
            MockTurn::text(""),
            MockTurn::text(""),
            MockTurn::text("第3步产出"),
        ]);
        let mut orchestrator = Orchestrator::new(deps(&mock), Budget::new(20), test_factory());
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
        // 一个任务一个会话，失败步骤的观察也在同一个证据文件里。因此
        // 会话 ID 总能带出，包括第 1 步就失败的场景。
        let ApplicationError::StepFailed {
            session_id,
            steps_completed,
            ..
        } = &error
        else {
            panic!("应是 StepFailed");
        };
        assert_eq!(*steps_completed, 1, "只完成了第 1 步");
        assert!(
            !session_id.as_str().is_empty(),
            "会话 ID 必须带出，失败时刻就能翻证据"
        );
        // 第 1 步 1 次请求，第 2 步 2 次（原始加重试），第 3 步的脚本未被消费。
        assert_eq!(mock.requests().len(), 3);
    }

    #[tokio::test]
    async fn empty_step_task_cannot_run() {
        // 空步骤任务必须报错，不能零次调用就标为完成。
        let mock = MockCompletionModel::from_turns(vec![]);
        let mut orchestrator = Orchestrator::new(deps(&mock), Budget::new(20), test_factory());
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
        let mut orchestrator = Orchestrator::new(deps(&mock), Budget::new(20), test_factory());
        let mut task = orchestrator.create_task(TaskId::new("t3"), TaskKind::Feature, "加个功能");

        // 已失败的任务是终态：直接拒绝，不调用模型。
        task.fail("预先失败");
        let error = orchestrator.run(&mut task).await.unwrap_err();
        assert!(
            matches!(error, ApplicationError::Task(TaskError::NotRunning)),
            "{error}"
        );
        assert_eq!(mock.requests().len(), 0, "终态任务不应触发任何模型调用");
    }

    #[tokio::test]
    async fn missing_profile_fails_the_step_explicitly() {
        // 空花名册：任何角色都没有档案。该步显式失败并标记任务失败，
        // 不 panic，也不退回默认角色。
        let mock = MockCompletionModel::from_turns(vec![]);
        let mut orchestrator = Orchestrator::with_roster(
            deps(&mock),
            Budget::new(20),
            test_factory(),
            AgentRoster::new(vec![]),
        );
        let mut task = orchestrator.create_task(TaskId::new("t7"), TaskKind::BugFix, "修好它");

        let error = orchestrator.run(&mut task).await.unwrap_err();

        assert!(
            matches!(&error, ApplicationError::StepFailed { step: 1, .. }),
            "应报第 1 步失败，实际: {error}"
        );
        assert!(error.to_string().contains("没有档案"), "{error}");
        assert!(error.to_string().contains("planner"), "{error}");
        assert_eq!(task.state(), TaskState::Failed);
        assert_eq!(mock.requests().len(), 0, "装配失败不应触发模型调用");
    }

    // ── 项目指令到达模型 ───────────────────────────────────────────────

    /// 端到端验证注入的项目指令出现在模型收到的系统提示词里。
    ///
    /// roster 测试只验证字符串拼接，这里验证拼好的字符串确实发出。任一
    /// 环节漏传（快照用了旧 spec、编排器未使用注入的花名册）都会在此暴露。
    #[tokio::test]
    async fn project_instructions_reach_the_model() {
        let mock = MockCompletionModel::from_turns(vec![
            MockTurn::text("规划完了"),
            MockTurn::text("改完了"),
            MockTurn::text("测完了"),
            MockTurn::text("审完了"),
        ]);
        let roster = AgentRoster::builtin().with_project_instructions("提交信息必须用中文");
        let mut orchestrator =
            Orchestrator::with_roster(deps(&mock), Budget::new(20), test_factory(), roster);
        let mut task = orchestrator.create_task(TaskId::new("t8"), TaskKind::BugFix, "修好它");

        orchestrator.run(&mut task).await.expect("四步都应跑完");

        assert_eq!(mock.requests().len(), 4, "一步一次模型调用");
        for index in 0..4 {
            assert!(
                system_prompt_of(&mock, index).contains("提交信息必须用中文"),
                "第 {index} 步的系统提示词应含项目指令"
            );
        }
    }

    /// 未注入项目指令时，模型收到内置档案原文，不加任何内容。
    ///
    /// 兼容性承诺：workspace 中没有 QCODEHARNESS.md 时，行为与加入该
    /// 特性之前一致。
    #[tokio::test]
    async fn without_project_instructions_the_prompt_is_unchanged() {
        let mock = MockCompletionModel::from_turns(vec![
            MockTurn::text("规划完了"),
            MockTurn::text("改完了"),
            MockTurn::text("测完了"),
            MockTurn::text("审完了"),
        ]);
        let roster = AgentRoster::builtin();
        let expected = roster
            .find(AgentRole::Planner)
            .expect("应有档案")
            .system_prompt
            .clone();
        let mut orchestrator =
            Orchestrator::with_roster(deps(&mock), Budget::new(20), test_factory(), roster);
        let mut task = orchestrator.create_task(TaskId::new("t9"), TaskKind::BugFix, "修好它");

        orchestrator.run(&mut task).await.expect("应跑完");

        assert_eq!(system_prompt_of(&mock, 0), expected);
    }

    // ── 按角色分配模型与档位 ───────────────────────────────────────────

    /// 四步任务中每一步都使用自身角色的模型。
    ///
    /// 编排器按步骤查档案，把模型名带进 `AgentSpec`，网关再写入请求的
    /// `model` 字段。任一环节漏传，模型名会退回 rig 的默认值，测试失败。
    ///
    /// 使用 `Refactor` 模板：四步覆盖三个角色（测试者/编码者/评审者），
    /// `BugFix` 的四步是规划者/编码者/测试者/编码者，不含评审者。
    #[tokio::test]
    async fn each_step_uses_the_model_of_its_own_role() {
        let mock = MockCompletionModel::from_turns(vec![
            MockTurn::text("基线跑完了"),
            MockTurn::text("改完了"),
            MockTurn::text("回归跑完了"),
            MockTurn::text("审完了"),
        ]);
        let models = HashMap::from([
            (AgentRole::Planner, "planner-model".to_owned()),
            (AgentRole::Coder, "coder-model".to_owned()),
            (AgentRole::Tester, "tester-model".to_owned()),
            (AgentRole::Reviewer, "reviewer-model".to_owned()),
        ]);
        let roster = AgentRoster::builtin().with_role_models(&models, &HashMap::new(), &HashMap::new());
        let mut orchestrator =
            Orchestrator::with_roster(deps(&mock), Budget::new(20), test_factory(), roster);
        let mut task =
            orchestrator.create_task(TaskId::new("t-role-model"), TaskKind::Refactor, "重构它");

        orchestrator.run(&mut task).await.expect("四步都应跑完");

        let requests = mock.requests();
        assert_eq!(requests.len(), 4, "一步一次模型调用");
        // Refactor 模板：测试者 → 编码者 → 测试者 → 评审者。
        let expected = [
            "tester-model",
            "coder-model",
            "tester-model",
            "reviewer-model",
        ];
        for (index, model) in expected.iter().enumerate() {
            assert_eq!(
                requests[index].model.as_deref(),
                Some(*model),
                "第 {index} 步应用它自己角色的模型"
            );
        }
    }

    /// 未配置角色模型时，请求里的模型名由 rig 填入默认值，我们不覆盖。
    ///
    /// rig 的 operation 层在发送前会把空的 `request.model` 补成 wire 的
    /// 模型名（`operation/completion.rs` 的 `request.model.take()...or_else(
    /// || Some(target.model()))`）。生产路径上"未配置"表现为请求带有
    /// provider 默认模型，不会出现空模型名。
    ///
    /// 这条断言检查反方向：未配置时不能自行写入模型名，否则会覆盖用户
    /// 部署的默认模型。请求中确实没有我们设置的模型名，由 `rig_chat` 的
    /// `without_a_configured_model_the_request_carries_none` 在
    /// build_request 层验证，那里能看到 rig 填充之前的原始请求。
    #[tokio::test]
    async fn without_role_models_the_request_keeps_the_default_model() {
        let mock = MockCompletionModel::from_turns(vec![
            MockTurn::text("规划完了"),
            MockTurn::text("改完了"),
            MockTurn::text("测完了"),
            MockTurn::text("汇总完了"),
        ]);
        let mut orchestrator = Orchestrator::new(deps(&mock), Budget::new(20), test_factory());
        let mut task = orchestrator.create_task(TaskId::new("t-no-model"), TaskKind::BugFix, "修好它");

        orchestrator.run(&mut task).await.expect("四步都应跑完");

        for (index, request) in mock.requests().iter().enumerate() {
            assert_eq!(
                request.model.as_deref(),
                Some("mock-model"),
                "第 {index} 步不该被我们覆盖成别的模型"
            );
        }
    }

    // ── 单会话编排的预算与裁剪 ─────────────────────────────────────────

    #[tokio::test]
    async fn one_session_means_one_task_level_turn_budget() {
        // 预算按任务累计，四步共用 max_turns。第 1、2 步各花 2 轮用光，
        // 第 3 步一开始即被拦下。
        let mock = MockCompletionModel::from_turns(vec![
            MockTurn::tool_call("call_1", "read_file", json!({"path": "a.txt"})),
            MockTurn::text("第1步产出"),
            MockTurn::tool_call("call_2", "read_file", json!({"path": "a.txt"})),
            MockTurn::text("第2步产出"),
            MockTurn::text("第3步产出"),
        ]);
        let mut orchestrator = Orchestrator::new(deps(&mock), Budget::new(4), test_factory());
        let mut task = orchestrator.create_task(TaskId::new("t10"), TaskKind::Review, "评审一下");

        let error = orchestrator.run(&mut task).await.unwrap_err();

        assert!(
            matches!(&error, ApplicationError::StepFailed { step: 3, .. }),
            "第 3 步一进来就该撞上累计轮数上限，实际: {error}"
        );
        assert!(
            error.to_string().contains("已达最大轮数"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn long_multi_step_history_still_compacts_at_step_boundaries() {
        // 单会话使历史变长，裁剪更常发生。裁剪只删轮次，步骤交代
        // （"现在在第几步"）必须全部保留。
        let mut turns = Vec::new();
        // bug-fix 四步，前两步各 3 轮工具调用 + 1 轮结论，后两步各 1 轮结论。
        for step in 1..=2u32 {
            for index in 1..=3 {
                turns.push(MockTurn::tool_call(
                    format!("call_{step}_{index}"),
                    "read_file",
                    json!({"path": format!("r{step}-{index}.txt")}),
                ));
            }
            turns.push(MockTurn::text(format!("第{step}步产出")));
        }
        turns.push(MockTurn::text("第3步产出"));
        turns.push(MockTurn::text("第4步产出"));
        let mock = MockCompletionModel::from_turns(turns);

        let workspace = std::env::temp_dir().join(format!("qch-orch-compact-{}", std::process::id()));
        std::fs::create_dir_all(&workspace).expect("建临时工作区");
        for name in ["r1-1.txt", "r1-2.txt", "r1-3.txt", "r2-1.txt", "r2-2.txt", "r2-3.txt"] {
            std::fs::write(workspace.join(name), format!("{name} 的内容")).expect("写 fixture 文件");
        }
        let registry = ToolRegistry::new(all_tools(WorkspaceRoot::new(workspace.clone())));
        let deps = AgentDependencies {
            tools: registry,
            ..deps(&mock)
        };
        let budget_context = ContextBudget {
            max_tokens: Some(1),
            min_recent_rounds: 2,
            keep_task: true,
            summarize: None,
        };
        let mut deps = deps;
        deps.context_budget = budget_context;
        let mut orchestrator = Orchestrator::new(deps, Budget::new(20), test_factory());
        let mut task = orchestrator.create_task(TaskId::new("t11"), TaskKind::BugFix, "读六个文件再汇总");

        let outcome = orchestrator.run(&mut task).await.unwrap();

        assert_eq!(task.state(), TaskState::Completed);
        assert_eq!(outcome.answer, "第4步产出");
        // 最后一次请求（第 4 步）只保留 2 轮，四条步骤交代都在。
        let last = user_messages(&mock, mock.requests().len() - 1);
        for expected in ["任务：读六个文件再汇总", "第 2/4 步", "第 3/4 步", "第 4/4 步"] {
            assert!(
                last.iter().any(|text| text.contains(expected)),
                "裁剪后仍应看到 {expected}: {last:?}"
            );
        }
        // 确认第 1 步的观察已被裁掉。
        let joined = last.join("\n");
        assert!(
            !joined.contains("r1-1.txt 的内容"),
            "第 1 轮的观察应已被裁掉: {joined}"
        );

        let _ = std::fs::remove_dir_all(&workspace);
    }
}

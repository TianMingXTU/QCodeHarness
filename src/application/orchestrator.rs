//! 任务编排器：按模板把任务拆成步骤，逐步驱动 `AgentRunner`。
//!
//! 编排策略刻意简单可预测：任一步失败立即停；runner 按步骤的角色装配，
//! 提示词与工具白名单都来自角色档案（`AgentRoster`）。多 Agent 并行
//! 调度留给后续阶段。
//!
//! **一个任务只开一个会话**：步骤之间不另起 runner，而是在同一个会话里
//! 推进一步——换角色、交代步骤、把上一步的结论落进历史。换来三件事：
//! 历史跨步延续（下一步看得见上一步读过的文件，不必重读）、轮数与墙钟
//! 按任务累计（不再每步各发一份预算）、缓存路由键跨步稳定（provider
//! 才可能把前面攒下的前缀缓存读回来）。代价是会话历史会变长，由
//! `ContextBudget` 在步骤边界自然裁掉（见 `domain::context`）。

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

/// 每任务证据汇的工厂：编排器为任务开一个会话与一份证据文件，怎么造汇
/// 由装配方（main）决定——应用层不依赖具体实现。会话 ID 在 `run` 内部
/// 生成后传给工厂，证据文件因此与会话同名可互查。
pub type EvidenceFactory =
    Arc<dyn Fn(&SessionId) -> std::io::Result<Arc<dyn EvidenceSink>> + Send + Sync>;

/// 一次编排的结果。
#[derive(Debug, Clone, PartialEq)]
pub struct TaskOutcome {
    /// 最终答案（最后一步的输出）。
    pub answer: String,
    /// 本次运行完成的步数。从 `Running { step }` 续跑时它小于任务总步数
    /// ——任务级进度看 [`Task::progress`]。
    pub steps_completed: usize,
    /// 本次任务的会话 ID。一个任务只有一个会话：证据文件、快照、
    /// `--resume` 都用它。
    pub session_id: SessionId,
}

/// 任务编排器。
pub struct Orchestrator {
    deps: AgentDependencies,
    budget: Budget,
    templates: WorkflowCatalog,
    /// 每个任务的证据汇工厂：一个任务只调用一次，为整个任务的唯一
    /// 会话建一份证据。
    evidence_factory: EvidenceFactory,
    /// 任务快照存储。`None` 表示不落任务快照。
    ///
    /// 与 `deps.session_store` 分开：前者存"这个任务做到哪"，
    /// 后者存"这个 runner 停在哪"。两者回答的问题不同，恢复的粒度
    /// 也不同——任务级恢复能从步骤边界继续，会话级恢复只能从半轮继续。
    task_store: Option<Arc<dyn TaskStore>>,
    /// 工作区扫描器：存快照时记一份清单，恢复时拿它比对。
    ///
    /// `None` 表示不做工作区一致性检查。快照里的清单因此是空的，
    /// 恢复时也拿不到可比的"当时的样子"——检查降级为"查不了"。
    worktree_scanner: Option<Arc<dyn WorktreeScanner>>,
    /// 工作区被改动时的处置策略。
    worktree_policy: WorktreeChangePolicy,
}

impl Orchestrator {
    /// 组装编排器，用内置花名册。`budget` 是编排级配置：每个步骤的
    /// runner 都用同一个预算。
    pub fn new(deps: AgentDependencies, budget: Budget, evidence_factory: EvidenceFactory) -> Self {
        Self::with_roster(deps, budget, evidence_factory, AgentRoster::builtin())
    }

    /// 用给定花名册组装（测试与将来的自定义配置用）。
    ///
    /// 花名册是装配的必要输入：某步的角色查不到档案时，该步显式报错
    /// （[`ApplicationError::ProfileMissing`]），绝不静默退回默认角色。
    ///
    /// 花名册只存一份：写进 `deps.roster`，任务里既用它拼系统提示词，
    /// 也用它做调用层的角色白名单。若编排器再另存一份，提示词与白名单
    /// 就可能从两份分叉的花名册上各自取值——提示词说"你是审阅者"，
    /// 白名单却按另一个角色放行，谁都没错却整条链路错了。
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
    /// 接上之后，任务每完成一步就落一份任务快照——中断时才有"这个任务
    /// 做到哪"的记录可查。没接的话任务照跑，只是不能任务级恢复。
    pub fn with_task_store(mut self, store: Arc<dyn TaskStore>) -> Self {
        self.task_store = Some(store);
        self
    }

    /// 接上工作区扫描器。
    ///
    /// 接上之后，任务快照里会带一份工作区清单；恢复时拿它与当时的清单
    /// 比对，得出"工作区有没有被人动过"。这是"不静默继续"的数据来源。
    pub fn with_worktree_scanner(mut self, scanner: Arc<dyn WorktreeScanner>) -> Self {
        self.worktree_scanner = Some(scanner);
        self
    }

    /// 设定工作区被改动时的处置策略。
    ///
    /// 默认 [`WorktreeChangePolicy::Refuse`]：检测到改动就拒绝恢复。
    /// 用户看过差异并确认之后，用
    /// [`WorktreeChangePolicy::Confirmed`] 放行——那是人的决定，
    /// 程序不替他做。
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

    /// 按模板执行一个任务：一个会话跑完所有步骤。
    ///
    /// 步骤之间不另起 runner：`begin_step` 换角色、交代步骤、把上一步的
    /// 结论落进历史，会话 ID、轮数、墙钟与缓存路由键全部延续。
    ///
    /// 从 `Running { step }` 接着跑：进度只有一个真相来源。
    ///
    /// # Errors
    ///
    /// 任一步失败：任务标记 [`TaskState::Failed`]（记录步号与原因），
    /// 返回 [`ApplicationError::StepFailed`]，不继续后续步骤。
    /// 任务不在可执行状态时返回 [`ApplicationError::Task`]。
    pub async fn run(&mut self, task: &mut Task) -> Result<TaskOutcome, ApplicationError> {
        let budget = self.budget;
        self.run_with_budget(task, &budget).await
    }

    /// 用调用方给的预算跑一个任务，其余与 [`Self::run`] 相同。
    ///
    /// 长任务基准的第一段要用一个**故意设小**的轮数上限逼出中断，
    /// 而编排器自带的预算是给正常运行用的——为这一个场景改装配里的
    /// 预算，等于让装配配置为实验条件让路。预算是"这一次运行"的参数，
    /// 不是编排器的身份，所以它跟在 run 后面而不是构造器里。
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
        // 会话与证据在进入步骤循环前一次定好：后面每一步都往同一个
        // 会话里推，而不是每步另开一个。这也是缓存能跨步命中的前提——
        // 路由键取会话 ID，每步换键等于每步换一台机器。
        let session_id = new_session_id(&*self.deps.clock);
        let evidence = (self.evidence_factory)(&session_id)
            .map_err(|error| ApplicationError::EvidenceInit(error.to_string()))?;
        let mut runner: Option<AgentRunner> = None;
        let mut completed = 0usize;
        let mut last_answer = String::new();
        // 每一步的结论。任务级恢复要靠它把"前面几步查出了什么"交回
        // 给模型（见 `application::task_resume`）。
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
                    // 每完成一步就落一份任务快照：中断时"做到哪"才查得到。
                    self.save_task_snapshot(task, runner.as_ref(), &completed_outputs);
                }
                Err(error) => {
                    let reason = format!("第 {} 步失败: {error}", index + 1);
                    task.fail(reason.clone());
                    // 失败这一刻也要落快照：任务失败在第几步、失败前
                    // 完成了哪些步骤，都在快照里。
                    self.save_task_snapshot(task, runner.as_ref(), &completed_outputs);
                    return Err(ApplicationError::StepFailed {
                        step: index + 1,
                        message: reason,
                        // 失败步骤的观察也在同一个证据文件里：带着 ID 出去，
                        // 失败时刻就能翻证据，不用再去 --list-sessions 找。
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

    /// 把当前的任务状态落成一份任务快照。
    ///
    /// 落盘失败降级为 stderr 警告：任务快照是恢复的便利，不是任务本身
    /// 的成败——一次写盘失败不该把跑了几十轮的任务带崩（同
    /// `AgentRunner::save_snapshot` 的立场）。
    ///
    /// 存不下就不存：没接存储的编排器（单测、基准）不受影响。
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
            // 还没开跑就没有运行状态可存。第一步完成时 runner 一定在。
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

    /// 扫一遍工作区，给快照留一份"当时的样子"。
    ///
    /// 扫不出来时给空清单：那意味着"当时是什么样"根本不知道，恢复时的
    /// 工作区检查会降级为"查不了"——而不是假装没改动过。
    fn scan_worktree(&self) -> crate::domain::worktree::WorktreeManifest {
        match self.worktree_scanner.as_ref() {
            Some(scanner) => scanner.scan().unwrap_or_default(),
            None => crate::domain::worktree::WorktreeManifest::default(),
        }
    }

    /// 恢复前核对工作区：被人动过就按策略处理。
    ///
    /// 三个分支：
    ///
    /// - **没动过**：放行。
    /// - **动过且策略是 `Refuse`**：返回 [`ApplicationError::WorktreeChanged`]，
    ///   差异清单带在错误里，用户看得见到底改了什么。
    /// - **扫不出来**：只打一条警告就放行。查不了不等于没改动，
    ///   但也拦不住——硬失败会让一个本来能恢复的任务彻底卡住，
    ///   而工作区读不出来通常是环境问题，不是任务问题。
    ///
    /// 快照里没有清单（老版本的任务快照，或扫描没留下基准）时跳过检查：
    /// 没有可比的基准，比出来的"没改动"是假的。跳过要出声——用户得知道
    /// 这一道检查没有发生，而不是被一条看不见的绿灯骗过去。
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

    /// 从任务快照恢复一个任务，接着把剩下的步骤跑完。
    ///
    /// 与 [`Self::run`] 的分工：`run` 从头跑一个任务；这里接手一个
    /// 中途断掉的任务，从它停下的那一步继续，前面做完的步骤**不重做**。
    ///
    /// 恢复要过的三道关（任一不过就不恢复）：
    ///
    /// 1. **快照版本**要对（[`task_resume::check_resumable`]）。
    /// 2. **任务与会话要对得上**：任务的步骤数与会话里开跑的步数必须
    ///    一致，`session_id` 也必须一致。对不上说明手里的两半不是同一个
    ///    任务，拿它恢复等于用 A 的进度续 B 的历史。
    /// 3. **工作区要没被人动过**：动过了就返回
    ///    [`task_resume::ResumeCheck::WorktreeChanged`]，由调用方要求
    ///    人确认——绝不静默继续。
    ///
    /// 恢复后每一步的结论会作为用户消息交回给模型（见
    /// [`task_resume::hand_off_completed_outputs`]），所以第 3 步看得见
    /// 第 1、2 步查出了什么。
    ///
    /// # Errors
    ///
    /// 核对不通过时返回 [`ApplicationError::Snapshot`]；工作区被改过时
    /// 返回 [`ApplicationError::WorktreeChanged`]。
    pub async fn resume(
        &mut self,
        snapshot: TaskSnapshot,
    ) -> Result<TaskOutcome, ApplicationError> {
        // 工作区检查：拿快照里存的"当时的样子"与"现在的样子"比对。
        // 改动过就按策略处理——默认拒绝，绝不静默继续。
        self.check_worktree_unchanged(&snapshot)?;
        // 任务快照是**步骤边界**的快照。若进程死在某一步的半路上（等人
        // 审批、正在执行工具），真正的现场在会话快照里——把它合并进来，
        // 恢复才谈得上"不静默重复已发生的副作用"（见
        // [`task_resume::merge_session_state`]）。
        let snapshot = self.merge_session_state(snapshot);
        task_resume::check_resumable(&snapshot, None)?;
        let session_id = snapshot.run.session_id.clone();
        let evidence = (self.evidence_factory)(&session_id)
            .map_err(|error| ApplicationError::EvidenceInit(error.to_string()))?;
        // 恢复沿用编排器的预算（轮数上限对整个任务计）。
        let budget = &self.budget;

        let mut task = snapshot.task.clone();
        // 任务停在 Failed 上（被中断时编排器会标记失败）：放回运行态，
        // 进度不倒退——从失败的那一步接着走。
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

        // runner 从快照里的运行状态重建：会话历史、轮数、墙钟、缓存
        // 路由键全部延续——换 runner 等于换了一个 agent 接手半截任务。
        let mut deps = self.deps.clone();
        deps.evidence = evidence.clone();
        let mut runner = AgentRunner::resume(snapshot.run.clone(), deps)?;
        // 把已完成步骤的结论交回给模型：恢复后的下一步必须看得见前面
        // 几步查出了什么，否则它只能从零重推。
        task_resume::hand_off_completed_outputs(&mut runner, &snapshot.completed_outputs);

        let mut completed = 0usize;
        let mut last_answer = String::new();
        let mut completed_outputs = snapshot.completed_outputs.clone();
        // runner 住在循环外：每一步都在同一个 runner 上推进一步，
        // 历史、轮数、墙钟与缓存路由键因此跨步延续。
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

    /// 把会话快照里的半轮现场合并进任务快照。
    ///
    /// 读盘在这里做，合并的判断在 [`task_resume::merge_session_state`]
    /// （纯函数，可单独测）。会话快照读不出来或读坏了都按"没有现场"处理：
    /// 退回到步骤边界恢复只是重做那一步，而拿一个坏现场去续会出事。
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
    /// 按步骤的角色装配：系统提示词来自角色档案，工具白名单由 runner 按
    /// 同一个角色在调用层校验（见 [`AgentRunner::begin_step`]）。
    /// 第一步新建 runner，后续步骤在同一个 runner 上推进一步。
    ///
    /// # Errors
    ///
    /// 角色缺档案、证据初始化、模型调用或预算耗尽时上抛；步骤失败的
    /// 处理（标记任务、停止后续）由 [`Self::run`] 统一做。
    async fn run_step(
        &self,
        task: &Task,
        index: usize,
        budget: &Budget,
        session_id: &SessionId,
        evidence: &Arc<dyn EvidenceSink>,
        runner: &mut Option<AgentRunner>,
    ) -> Result<String, ApplicationError> {
        // 循环边界保证 index < total；空步骤任务已被 start() 拒绝
        // （TaskError::NoSteps），到这里越界只剩程序错误，直接下标访问。
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
            // 角色专属的模型与档位：档案里配了就用，没配就用运行时的默认。
            // 这一步是"能力与延迟"的分水岭——规划要强推理，
            // 测试跑命令读输出就行。
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
        // 证据自述角色与起点：JSONL 里每一步开始处都写明是谁在跑、
        // 从第几轮接上，事后翻证据不必借助快照交叉对照。
        evidence.record(
            self.deps.clock.now(),
            AuditEvent::StepStarted {
                turn: active.session().rounds() + 1,
                role: step.role,
            },
        );
        // 这一步失败时 runner **要放回去**：它带着这轮的历史、轮数与
        // 墙钟，正是恢复需要的全部状态。丢掉它的话，任务快照存下来的
        // 会话停在"上一步结束"的地方，恢复时那一步的交代与观察全没了。
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
    use crate::application::port::{ApprovalGate, EvidenceSink, NoopHooks};
    use crate::application::tool_registry::ToolRegistry;
    use crate::domain::agent::AgentRole;
    use crate::domain::context::ContextBudget;
    use crate::domain::policy::PolicyEngine;
    use crate::domain::workspace::WorkspaceRoot;
    use crate::infrastructure::approval::auto::AutoApproveGate;
    use crate::infrastructure::clock::SystemClock;
    use crate::infrastructure::evidence::memory::MemoryEvidenceSink;
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
        }
    }

    /// 测试用证据工厂：一个任务一个内存汇，不落盘。
    fn test_factory() -> EvidenceFactory {
        Arc::new(|_session_id: &SessionId| {
            Ok(Arc::new(MemoryEvidenceSink::new()) as Arc<dyn EvidenceSink>)
        })
    }

    /// 共享一个内存汇的工厂：编排器只调用一次，所以拿到的就是整个任务
    /// 的证据。"这个工具真被调了几次"只能从这里数——请求历史里同一次
    /// 调用会在后续每一轮原样重放，数历史会把它算成好几次。
    fn shared_factory() -> (EvidenceFactory, Arc<MemoryEvidenceSink>) {
        let sink = Arc::new(MemoryEvidenceSink::new());
        let cloned = sink.clone();
        let factory: EvidenceFactory =
            Arc::new(move |_session_id: &SessionId| Ok(cloned.clone() as Arc<dyn EvidenceSink>));
        (factory, sink)
    }

    /// 全量 10 个内置工具的注册表（真实规格），角色边界测试的底座。
    /// 工具本身会被真的执行（白名单测试就要看文件有没有被改动）。
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

    /// 请求里工具定义的序列化形式，用来断言"逐字相同"。
    ///
    /// 只比名字不够：描述或参数 schema 变了同样会废掉前缀缓存，
    /// 而那种抖动在名字列表上看不出来。
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

    /// 请求历史的序列化形式，用来断言"上一步的结论有没有到下一步眼里"。
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
        // 一个任务一个会话：四次请求必须带同一个缓存路由键。换键等于换
        // 一台机器，provider 会把前面攒下的前缀缓存全部作废——这是
        // 单会话最主要的收益来源，这里钉住它。
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
        // 验收项：跨步骤不重读。规划步读一次 notes.txt，实施步的请求里
        // 必须已经带着那份内容——它没有理由再读一遍。
        //
        // 注意 harness 不做"读取去重"：两次读之间文件可能已被写工具改掉，
        // 旧观察是过期的，硬拦截会让模型拿着陈年内容干活。"不重读"靠的是
        // 历史可见性，不是拦截。所以本测试的判别性断言在历史文本上——
        // 一旦回归成"每步一个会话"，第 2 步的请求就只剩步骤指令，
        // 断言会红；而 mock 脚本里第 2 步根本没再发 read_file，是因为
        // 脚本模拟的正是"模型已经看见了内容"的行为。
        let workspace = std::env::temp_dir().join(format!("qch-orch-reread-{}", std::process::id()));
        std::fs::create_dir_all(&workspace).expect("建临时工作区");
        std::fs::write(workspace.join("notes.txt"), "根因在解析层").expect("写 fixture 文件");
        let registry = ToolRegistry::new(all_tools(WorkspaceRoot::new(workspace.clone())));

        let mock = MockCompletionModel::from_turns(vec![
            // 第 1 步（Planner，只读）：读文件 → 给出根因。
            MockTurn::tool_call("call_r1", "read_file", json!({"path": "notes.txt"})),
            MockTurn::text("根因在解析层，notes.txt 已说明"),
            // 第 2~4 步：不再读文件，只引用"notes.txt 已说明"的内容——
            // 脚本敢这么写，前提正是那份观察已经在历史里。
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
        // 判别性断言：第 2 步的请求里带着第 1 步读到的内容。
        // 每步一个会话的旧实现下，这一步的历史是空的，这条断言会红。
        let second = history_text(&mock, 2);
        assert!(second.contains("根因在解析层"), "第 2 步应已拿到文件内容: {second}");
        // 内容还要活过后续步骤：第 3、4 步同样看得到，而不是只在跨一步
        // 时碰巧在。两条都断，单会话编排的连续性才算是钉住了。
        for index in [3, 4] {
            let text = history_text(&mock, index);
            assert!(text.contains("根因在解析层"), "第 {index} 步应仍带着第 1 步的内容: {text}");
        }
        // 脚本里模型确实只发出过这一次 read_file——它没有理由再读。
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
        // 每一步发给模型的工具定义必须逐字相同。不同的话
        // 每步的工具数组都不一样，provider 的前缀缓存一步都命不中——
        // 单会话省下来的历史传递全被工具定义的抖动吃回去。
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
        // 全量注册表：不再按角色裁掉模型可见的工具。
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
        // 第 2、4 步同为 Coder，提示词必须一字相同——同一个 runner 换
        // spec，不该有"第二步的实施者提示词和第四步不一样"这种怪事。
        assert_eq!(prompts[1], prompts[3]);
    }

    #[tokio::test]
    async fn role_whitelist_is_enforced_at_the_call_layer() {
        // 验收项：角色白名单在调用层生效。评审妄想写文件——工具定义里
        // 看得见 write_file（对所有步骤统一），但调用在分发前就
        // 被角色边界拒下，文件一个字节都没动。
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
            // 第 2 步换一种越权：跑命令（Tester 有，Reviewer 没有）。
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
        // 边界不是"审一下再说"，是根本不许执行。
        assert_eq!(
            std::fs::read_to_string(&target).expect("读回文件"),
            "原文",
            "越权写入必须连执行都到不了"
        );
        // 拒绝观察回到了模型手里，并且点破这是角色权限而非审批问题。
        let after_write_refusal = history_text(&mock, 1);
        assert!(
            after_write_refusal.contains("角色 `reviewer` 无权调用工具 `write_file`"),
            "{after_write_refusal}"
        );
        assert!(
            after_write_refusal.contains("不是审批问题"),
            "拒绝说明必须让模型知道换参数或求审批都没用: {after_write_refusal}"
        );
        // 工具定义里确实有 write_file——边界在调用层，不在可见性层。
        assert!(offered_tools(&mock, 0).contains(&"write_file".to_owned()));
        // run_command 同样越权。
        let after_command_refusal = history_text(&mock, 3);
        assert!(
            after_command_refusal.contains("无权调用工具 `run_command`"),
            "{after_command_refusal}"
        );
        // 每步两问（妄想调用 + 拒绝后的结论），第 4 步一问。
        // 第 1、2 步各两问（妄想调用 + 拒绝后的结论），第 3、4 步各一问。
        assert_eq!(mock.requests().len(), 6);

        let _ = std::fs::remove_dir_all(&workspace);
    }

    #[tokio::test]
    async fn unrestricted_role_still_gets_every_tool() {
        // 白名单不是把 Coder 也关起来：`None` = 不限，Coder 一步都不能少。
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
        // 第 2 步两次拿到空回复（原始 + 重试）→ EmptyResponse → 该步失败。
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
        // 一个任务一个会话：失败步骤的观察也在同一个证据文件里，
        // 所以会话 ID 无条件带得出来——包括第 1 步就失败的场景。
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
        // 第 1 步问过 1 次，第 2 步问了原始 + 重试共 2 次；第 3 步的脚本
        // 原封不动——后续步骤没被执行。
        assert_eq!(mock.requests().len(), 3);
    }

    #[tokio::test]
    async fn empty_step_task_cannot_run() {
        // 空步骤任务（模板缺失的产物）必须报错，不能零调用"完成"。
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

        // 已失败的任务是终态：直接拒绝，不做任何模型调用。
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
        // 绝不 panic、也绝不静默退回某个默认角色。
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

    // ── 项目指令真的到达模型 ───────────────────────────────────────────

    /// 端到端验证：注入的项目指令必须出现在模型收到的系统提示词里。
    ///
    /// 前面 roster 的测试只证明"字符串拼对了"，这条证明"拼对的字符串
    /// 真的发出去了"——中间任何一环漏传（快照用了旧 spec、编排器没用
    /// 注入了的花名册）都会在这里露出来。
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

    /// 没注入项目指令时，模型收到的就是内置档案那一份，一字不多。
    ///
    /// 这是兼容性承诺：workspace 里没有 QCODEHARNESS.md 时，
    /// 行为与加这个特性之前完全一致。
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

    /// 四步任务里，每一步都用它自己角色的模型。
    ///
    /// 编排器按步骤查档案、把档案里的模型名带进 `AgentSpec`，网关再把
    /// 它写到请求的 `model` 字段上——这条断言把整条链路钉住：哪一环
    /// 漏传，模型名都会退回成 rig 填的默认值，测试立刻红。
    ///
    /// 用 `Refactor` 模板而不是 `BugFix`：它的四步刚好覆盖四个角色
    /// （测试者/编码者/测试者/评审者），`BugFix` 只有三步且不含评审者。
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

    /// 没配角色模型时，请求里的模型名是 rig 自己填的默认值，我们不覆盖它。
    ///
    /// rig 的 operation 层在发送前会把空的 `request.model` 补成 wire 自己的
    /// 模型名（`operation/completion.rs` 的 `request.model.take()...or_else(
    /// || Some(target.model()))`），所以生产路径上"没配"表现为**请求里
    /// 带着 provider 的默认模型**，而不是不带模型。
    ///
    /// 这条断言守的是反方向：没配时我们绝不能塞一个模型名进去。塞进去
    /// 就等于把用户部署的默认模型顶掉——那正是"没配却变了"。
    /// 请求里"确实没有我们设的模型名"这一条由
    /// `rig_chat` 的 `without_a_configured_model_the_request_carries_none`
    /// 在 build_request 层守住（那里能看见 rig 填充之前的原始请求）。
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
        // 预算按任务累计，不再每步各发一份：四步共用 max_turns。
        // 第 1、2 步各花 2 轮就把它用光，第 3 步一开始就被拦下。
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
        // 单会话让历史变长，裁剪因此更常发生。裁掉的必须是轮次，
        // 步骤交代（"现在在第几步"）一条都不能少。
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
        // 最后一次请求（第 4 步）只保留 2 轮，但四条步骤交代都在。
        let last = user_messages(&mock, mock.requests().len() - 1);
        for expected in ["任务：读六个文件再汇总", "第 2/4 步", "第 3/4 步", "第 4/4 步"] {
            assert!(
                last.iter().any(|text| text.contains(expected)),
                "裁剪后仍应看到 {expected}: {last:?}"
            );
        }
        // 第 1 步的观察确实被裁掉了（否则这条测试什么都没验证）。
        let joined = last.join("\n");
        assert!(
            !joined.contains("r1-1.txt 的内容"),
            "第 1 轮的观察应已被裁掉: {joined}"
        );

        let _ = std::fs::remove_dir_all(&workspace);
    }
}

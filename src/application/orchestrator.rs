//! 任务编排器：按模板把任务拆成步骤，逐步驱动 `AgentRunner`。
//!
//! P4 的编排策略刻意简单可预测：任一步失败立即停。P5 起按步骤的角色
//! 装配 runner：提示词与工具白名单都来自角色档案（`AgentRoster`），
//! 多 Agent 并行调度留给后续阶段。
//!
//! P9-3 起**一个任务只开一个会话**。步骤之间不再另起 runner，而是在
//! 同一个会话里推进一步：换角色、交代步骤、把上一步的结论落进历史。
//! 换来三件事——历史跨步延续（下一步看得见上一步读过的文件，不必重读）、
//! 轮数与墙钟按任务累计（不再每步各发一份预算）、缓存路由键跨步稳定
//! （provider 才可能把前面攒下的前缀缓存读回来）。代价是会话历史会变长，
//! 由 `ContextBudget` 在步骤边界自然裁掉（见 `domain::context`）。

use std::sync::Arc;

use crate::application::agent_roster::AgentRoster;
use crate::application::agent_run::{new_session_id, AgentDependencies, AgentRunner};
use crate::application::error::ApplicationError;
use crate::application::port::EvidenceSink;
use crate::domain::agent::AgentSpec;
use crate::domain::budget::Budget;
use crate::domain::evidence::AuditEvent;
use crate::domain::ids::{SessionId, TaskId};
use crate::domain::task::{Task, TaskError, TaskKind, TaskState};
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
    /// 本次任务的会话 ID。P9-3 起一个任务只有一个会话：证据文件、快照、
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

    /// 按模板执行一个任务：一个会话跑完所有步骤。
    ///
    /// 步骤之间不另起 runner：`begin_step` 换角色、交代步骤、把上一步的
    /// 结论落进历史，会话 ID、轮数、墙钟与缓存路由键全部延续。
    ///
    /// 从 `Running { step }` 接着跑：进度只有一个真相来源，将来
    /// "编排中断恢复"就是从这里续。注意续跑会开一个**新**会话——
    /// 快照恢复恢复的是单步 runner，不是整个任务（见 `--resume`）。
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
        // 会话与证据在进入步骤循环前一次定好：后面每一步都往同一个
        // 会话里推，而不是每步另开一个。这也是缓存能跨步命中的前提——
        // 路由键取会话 ID，每步换键等于每步换一台机器。
        let session_id = new_session_id(&*self.deps.clock);
        let evidence = (self.evidence_factory)(&session_id)
            .map_err(|error| ApplicationError::EvidenceInit(error.to_string()))?;
        let mut runner: Option<AgentRunner> = None;
        let mut completed = 0usize;
        let mut last_answer = String::new();

        for index in start..task.steps().len() {
            match self
                .run_step(task, index, &session_id, &evidence, &mut runner)
                .await
            {
                Ok(answer) => {
                    task.advance()?;
                    completed += 1;
                    last_answer = answer;
                }
                Err(error) => {
                    let reason = format!("第 {} 步失败: {error}", index + 1);
                    task.fail(reason.clone());
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
        };
        let prompt = format!(
            "任务：{goal}\n当前步骤（第 {now}/{total} 步）：{description}\n完成判据：{done_when}\n达成判据后停止调用工具，直接输出最终答案。",
            goal = task.goal(),
            now = index + 1,
            total = total,
            description = step.description,
            done_when = step.done_when,
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
        let outcome = active.run(&self.budget).await?;
        *runner = Some(active);
        Ok(outcome.answer)
    }
}
#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::application::port::{ApprovalGate, EvidenceSink};
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

    /// 全量 9 个内置工具的注册表（真实规格），角色边界测试的底座。
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
        // 一台机器，provider 会把前面攒下的前缀缓存全部作废——那是 P9-3
        // 最主要的收益来源，这里钉住它。
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
        // P9-3 的前提：每一步发给模型的工具定义必须逐字相同。不同的话
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
        // 9 = read_file / read_files / list_dir / write_file / search_text /
        //     edit_file / run_command / git_status / git_diff。
        assert_eq!(offered_tools(&mock, 0).len(), 9);
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
        // 看得见 write_file（P9-3 起对所有步骤统一），但调用在分发前就
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

    // ── P8-4：项目指令真的到达模型 ─────────────────────────────────────

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
    /// 这是 P8-4 的兼容性承诺：workspace 里没有 QCODEHARNESS.md 时，
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

    // ── P9-3：单会话编排的预算与裁剪 ─────────────────────────────────

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
            max_rounds: 2,
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

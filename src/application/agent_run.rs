//! AgentRunner：ReAct 循环的核心编排器。
//!
//! `AgentRunner` 是应用层的核心，它不直接依赖 rig 或 tokio，
//! 而是通过端口（trait）与外部世界交互，保证可测试性。
//!
//! 行为与重构前的 `main.rs` 循环完全一致（先搬后改）：
//! 每轮重建请求、无工具调用即给出最终答案、未注册工具生成拒绝观察、
//! 达到 `max_turns` 报错、空回复报错。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::application::error::ApplicationError;
use crate::application::port::{
    ApprovalGate, ApprovalRequest, Clock, EvidenceSink, ModelGateway, ModelReply,
    ModelRequest,
};
use crate::application::agent_roster::AgentRoster;
use crate::application::snapshot::{RunSnapshot, RUN_FORMAT, SessionStore, StoreError};
use crate::application::summarizer::{render_slices, SummarizeInput, Summarizer};
use crate::application::tool_registry::ToolRegistry;
use crate::domain::agent::{AgentSpec, AgentTurn};
use crate::domain::budget::{Budget, Consumption};
use crate::domain::context::{compact, ContextBudget};
use crate::domain::evidence::AuditEvent;
use crate::domain::ids::SessionId;
use crate::domain::policy::{PolicyDecision, PolicyEngine, PolicyVerdict};
use crate::domain::sandbox::write_escape_suspect;
use serde_json::Value;
use crate::domain::session::{ContextSlice, Session};
use crate::domain::tool::{ToolAnnotations, ToolCall, ToolOutcome, ToolSpec};

/// 摘要连续失败的上限。给两次机会（瞬态故障实测存在），之后本次运行
/// 不再重试：持续失败的摘要调用每轮都白花一次模型调用。
const SUMMARY_FAILURE_LIMIT: u32 = 2;

/// 重新询问审批时的统一说法。
///
/// 措辞要点破"之前问过、那次没答复"：人被重新问一次时，最自然的反应
/// 是"怎么又问我"，而不是"上次那个还没答复"。不点破这一层，人可能
/// 以为程序出了故障，或者——更糟——顺手按了 y。
pub const APPROVAL_REASK_REASON: &str =
    "这个审批请求之前就问过，那时没答复；恢复后需要你重新决定一次。";

/// 重放提示里给出的原因。
pub const REPLAY_REASON: &str = "中断发生在这次调用执行期间，副作用可能已经发生，结果未知";

/// 运行状态机。
///
/// 每一步都是可中断的挂点：超时在 `run` 外层包 `tokio::time::timeout`；
/// 审批停在 [`RunState::AwaitingApproval`]，把状态序列化后等外部回复；
/// 恢复就是反序列化状态继续 `step`。
///
/// 可序列化：P4 起快照要把它原样存盘，恢复时从同一个位置继续。
/// 变体名与语义自 P0 未变，只加了 serde。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum RunState {
    /// 准备问模型。
    AwaitingModel,
    /// 模型给出了调用，正在逐条判断要不要审批。
    Evaluating(Vec<ToolCall>),
    /// 停在审批闸门前，等一个结论。
    AwaitingApproval(ApprovalRequest),
    /// 正在逐条执行工具。
    Dispatching(Vec<ToolCall>),
    /// 模型给出了最终答案。
    Completed(String),
}

/// 一次运行的结果。
#[derive(Debug, Clone, PartialEq)]
pub struct AgentOutcome {
    /// 模型的最终答案。
    pub answer: String,
    /// 实际发生的模型调用次数。
    pub turns_used: u32,
}

/// 已交给模型、正在等观察的一轮。
///
/// `outcomes` 的长度就是已处理的调用数，因此不需要单独的下标游标。
///
/// 公开且可序列化：快照要能表达"执行到半轮"的状态，恢复后
/// `step()` 才能从下一条调用继续，已跑完的观察不会重跑。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PendingRound {
    /// 模型这一轮的完整回复。
    pub turn: AgentTurn,
    /// 与 `turn.calls` 同下标一一对应。
    pub outcomes: Vec<ToolOutcome>,
}

/// 取消开关：`run()` 在每一轮问模型之前检查它。
///
/// 通过 [`AgentRunner::cancel_handle`] 克隆给外部。取消是协作式的——
/// 不打断执行中的工具（run_command 有自己的超时），在下一个安全停点
/// （下一轮问模型之前）生效。
#[derive(Debug, Clone, Default)]
pub struct CancelHandle {
    cancelled: Arc<AtomicBool>,
}

impl CancelHandle {
    /// 请求取消当前运行。
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }

    /// 是否已被请求取消。
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }
}

/// `AgentRunner` 的全部外部依赖。
///
/// 收敛成一个结构体而不是一串位置参数：位置参数到第七八个就没人分得清
/// 哪个是哪个，加一个依赖还要惊动所有调用点；结构体带字段名，构造点
/// 自解释。没有 `Default`——没有合理的默认模型，缺依赖就该在构造点写明。
///
/// 可克隆：编排器（P4-2）每步要克隆一份给新的 runner，`Arc` 字段
/// 克隆只是引用计数 +1。
#[derive(Clone)]
pub struct AgentDependencies {
    /// 模型网关。
    pub model: Arc<dyn ModelGateway>,
    /// 工具注册表：规格与执行入口绑在一起，加工具只在一处发生。
    pub tools: ToolRegistry,
    /// 审批闸门：副作用操作执行前的拦截点。
    pub approvals: Arc<dyn ApprovalGate>,
    /// 证据汇：审计事件的落点。
    pub evidence: Arc<dyn EvidenceSink>,
    /// 时钟：时间戳与耗时的唯一来源。
    pub clock: Arc<dyn Clock>,
    /// 策略引擎：决定每个调用是放行、要审批、还是直接拒绝。
    ///
    /// 它是领域类型而不是端口 trait——它无 IO、无外部依赖，
    /// 直接持有比再开一个端口更简单。
    pub policy: PolicyEngine,
    /// 角色花名册：按 `spec.role` 查出这个角色能调哪些工具。
    ///
    /// P9-3 起工具定义对所有步骤统一（模型看到的永远是全量注册表），
    /// 角色边界从"模型看不见"下沉到"调不动"——花名册就是那道边界的
    /// 唯一真相来源，runner 自己来查，不靠调用方预先过滤。放在依赖里
    /// 而不是构造参数里：`resume()` 同样要按快照里的角色重算一遍。
    pub roster: AgentRoster,
    /// 上下文预算：发给模型前按它裁剪历史。
    pub context_budget: ContextBudget,
    /// 快照存储。`None` 表示不做自动保存（单测、不需要恢复的场景）。
    ///
    /// 放在依赖里而不是 `run` 的参数里：保存时机是循环内部的事
    /// （每轮提交后、进审批前），调用方没有更合适的插桩点。
    pub session_store: Option<Arc<dyn SessionStore>>,
}

/// ReAct 循环的编排器。
pub struct AgentRunner {
    model: Arc<dyn ModelGateway>,
    /// 工具注册表：规格与执行入口绑在一起，加工具只在一处发生。
    tools: ToolRegistry,
    /// 注册表里全部规格的缓存，问模型时塞进请求。
    /// 注册表构造后不变，缓存一次省得每轮克隆整张表。
    visible_specs: Vec<ToolSpec>,
    approvals: Arc<dyn ApprovalGate>,
    evidence: Arc<dyn EvidenceSink>,
    clock: Arc<dyn Clock>,
    /// 策略引擎：决定每个调用是放行、要审批、还是直接拒绝。
    ///
    /// 它是领域类型而不是端口 trait——它无 IO、无外部依赖，
    /// 直接持有比再开一个端口更简单。
    policy: PolicyEngine,
    /// 角色花名册：角色边界的真相来源（见 [`AgentDependencies::roster`]）。
    roster: AgentRoster,
    /// 当前角色可调用的工具名；`None` 表示不限。
    ///
    /// 换角色时重算（构造、恢复、`begin_step` 各一次），之后每条调用
    /// 都查它。缓存而不是每次现查：`AgentRoster::allowed_tools` 要遍历
    /// 档案，而它在每条工具调用上都会被问到。
    allowed_tools: Option<Vec<String>>,
    /// 上下文预算：发给模型前按它裁剪历史。
    ///
    /// 裁剪只改"发给模型时怎么看"，不改 `session` 本身——
    /// 证据、审计与将来的恢复都要完整历史。
    context_budget: ContextBudget,
    /// 快照存储的克隆。`None` 表示不自动保存。
    session_store: Option<Arc<dyn SessionStore>>,
    /// 取消开关，克隆给外部。
    cancel: CancelHandle,
    spec: AgentSpec,
    session: Session,
    state: RunState,
    consumption: Consumption,
    pending: Option<PendingRound>,
    /// 墙钟测量锚点：本次 `run()` 的开始时刻。`run()` 入口重置，
    /// 手动逐 `step()` 驱动时退化为"构造时刻"。
    run_start: SystemTime,
    /// 墙钟基数：进入本次 `run()` 前已累计的耗时（恢复时来自快照）。
    elapsed_base: Duration,
    /// 连续摘要失败次数。达到上限后本次运行不再重试摘要：
    /// 持续失败的摘要调用每轮都白花一次模型调用，截断它。
    summary_failures: u32,
    /// 恢复时的重放提示：中断时正在执行的那一次调用。
    ///
    /// `Some` 表示这个调用在中断前已发起、可能已经产生副作用，而结果
    /// 未知。恢复时**不自动重跑**——重跑一次就是把同一次副作用做两遍；
    /// 也不装作没发生过——把这件事告诉模型，由它决定要不要再来一次
    /// （见 [`AgentRunner::replay_interrupted_call`]）。
    replay_pending: Option<ToolCall>,
    /// 恢复时的重新审批标记：这次审批之前问过、没答复。
    ///
    /// 恢复后要**再问一次**闸门，绝不凭快照放行（见
    /// [`AgentRunner::apply_resume_semantics`]）。
    reask_approval: bool,
}

impl AgentRunner {
    /// 组装一个 runner。
    ///
    /// 全部外部依赖装进 [`AgentDependencies`]：字段名自解释，
    /// 加新依赖不惊动既有调用点。
    ///
    /// `session_id` 由调用方传入而不是内部生成：证据文件要按会话 ID 命名，
    /// 而文件必须在 runner 开工前就打开。内部生成会让"证据文件名"和
    /// "runner 自报的会话 ID"变成两个值。
    pub fn new(
        spec: AgentSpec,
        session_id: SessionId,
        task: impl Into<String>,
        deps: AgentDependencies,
    ) -> Self {
        let AgentDependencies {
            model,
            tools,
            approvals,
            evidence,
            clock,
            policy,
            roster,
            context_budget,
            session_store,
        } = deps;
        let visible_specs = tools.specs();
        let session = Session::new(session_id, task);
        let run_start = clock.now();
        let mut runner = Self {
            model,
            tools,
            visible_specs,
            approvals,
            evidence,
            clock,
            policy,
            roster,
            allowed_tools: None,
            context_budget,
            session_store,
            cancel: CancelHandle::default(),
            spec,
            session,
            state: RunState::AwaitingModel,
            consumption: Consumption::default(),
            pending: None,
            run_start,
            elapsed_base: Duration::ZERO,
            summary_failures: 0,
            replay_pending: None,
            reask_approval: false,
        };
        runner.refresh_role_boundary();
        runner
    }

    /// 从快照恢复一个 runner，接着往下跑。
    ///
    /// 与 `new()` 的分工：`new()` 从零开始，`resume()` 接手一个已有的
    /// 会话与状态机位置，两者共享同一套依赖注入方式。
    /// 取消状态不跨恢复传递——恢复本身是一次新的"继续"意图，
    /// 旧运行里的取消请求不该惩罚新运行。
    ///
    /// 恢复成功后立即把快照再存一次：刚恢复就又崩了的场景下，
    /// 不能连恢复前的那份都没有（快照点还在旧位置）。
    ///
    /// # Errors
    ///
    /// 版本不匹配、历史损坏、或状态机位置与待配平轮次不配平（半轮缺
    /// pending、正常点带 pending、调用清单错位）时返回
    /// [`ApplicationError::Snapshot`]；历史配对错误经
    /// [`ApplicationError::Session`] 上抛。
    pub fn resume(snapshot: RunSnapshot, deps: AgentDependencies) -> Result<Self, ApplicationError> {
        snapshot.check_version()?;
        // 半轮状态全靠 pending 续跑（step() 从 outcomes.len() 推下一条调用），
        // 缺了它 step() 只能报 NoPendingRound——把这种矛盾在入口就拦下。
        let mid_round = matches!(
            snapshot.state,
            RunState::Evaluating(_) | RunState::AwaitingApproval(_) | RunState::Dispatching(_)
        );
        if mid_round && snapshot.pending.is_none() {
            return Err(StoreError::Corrupt(
                "快照停在半轮状态，但缺少待配平的轮次（pending）".to_owned(),
            )
            .into());
        }
        // 反向同样要配平：停在正常点（等待模型/已完成）却带着 pending，
        // 恢复后 step() 会先去配一轮幽灵观察，把历史配错。
        if !mid_round && snapshot.pending.is_some() {
            return Err(StoreError::Corrupt(
                "快照停在正常点，却带有未配平的轮次（pending）".to_owned(),
            )
            .into());
        }
        // 状态机里记的调用清单必须与待配平轮次一致：step() 按 state 里
        // 的清单取下一个调用，按 pending 里的清单收观察，两边不一致
        // 会让调用与观察错位配对。
        if let (
            Some(pending),
            RunState::Evaluating(calls) | RunState::Dispatching(calls),
        ) = (&snapshot.pending, &snapshot.state)
        {
            if calls != &pending.turn.calls {
                return Err(StoreError::Corrupt(
                    "状态机记录的调用清单与待配平轮次不一致".to_owned(),
                )
                .into());
            }
        }
        // 观察数就是"这一轮已经跑完几条调用"。它不能超过调用数（观察比
        // 调用还多是自相矛盾），而在"正在执行"的状态里它必须更少——
        // 全部配平意味着这一轮已经结束，那该停在"等模型回复"而不是
        // "执行中"。这两种形态构造路径产生不出来，在入口就拦下。
        if let Some(pending) = &snapshot.pending {
            if pending.outcomes.len() > pending.turn.calls.len() {
                return Err(StoreError::Corrupt(
                    "快照的观察数超过了调用数".to_owned(),
                )
                .into());
            }
            if matches!(snapshot.state, RunState::Dispatching(_))
                && pending.outcomes.len() == pending.turn.calls.len()
            {
                return Err(StoreError::Corrupt(
                    "快照停在执行中，但这一轮的调用已经全部配平".to_owned(),
                )
                .into());
            }
        }
        let session = Session::from_snapshot(snapshot.session_id.clone(), snapshot.session)?;
        let AgentDependencies {
            model,
            tools,
            approvals,
            evidence,
            clock,
            policy,
            roster,
            context_budget,
            session_store,
        } = deps;
        let visible_specs = tools.specs();
        let clock_now = clock.now();
        // 墙钟基数要在 consumption 被移进 runner 之前取出。
        let elapsed_base = snapshot.consumption.elapsed;
        let mut runner = Self {
            model,
            tools,
            visible_specs,
            approvals,
            evidence,
            clock,
            policy,
            roster,
            allowed_tools: None,
            context_budget,
            session_store,
            spec: snapshot.spec,
            session,
            state: snapshot.state,
            consumption: snapshot.consumption,
            pending: snapshot.pending,
            cancel: CancelHandle::default(),
            run_start: clock_now,
            elapsed_base,
            summary_failures: 0,
            replay_pending: None,
            reask_approval: false,
        };
        runner.refresh_role_boundary();
        // 半轮状态各有各的恢复语义，翻译成"接下来该怎么走"。
        runner.apply_resume_semantics();
        runner.save_snapshot();
        Ok(runner)
    }

    /// 恢复语义：把快照里的半轮状态翻译成"接下来该怎么走"。
    ///
    /// 三种半轮状态，三种语义：
    ///
    /// - **`Evaluating`**：调用还没执行，策略也还没裁决。恢复后照常走
    ///   评估流程——裁决用**当前**的 [`PolicyEngine`] 重算。快照里其实
    ///   没有存裁决结果（裁决只落在审计事件里），所以"策略可能变了"
    ///   是天然成立的：恢复时看到的是今天的策略，不是中断那天的。
    /// - **`AwaitingApproval`**：上一次已经问过人，人没答复。恢复后把
    ///   状态退回评估态，让当前策略重新裁决；若仍要审批，就**再问一次**
    ///   （带 `resumed` 标记），绝不凭快照放行。
    /// - **`Dispatching`**：调用已发起、可能已经产生副作用，结果未知。
    ///   恢复后**不自动重跑**，而是把重放提示交给模型，由它决定要不要
    ///   再跑一次。
    ///
    /// 退回评估态而不是就地续跑，是为了让三种半轮状态共用同一条"先裁决、
    /// 再决定要不要问人"的路径——恢复语义只有一处实现，不会出现"从
    /// `AwaitingApproval` 恢复走的是另一套规矩"的分支。
    fn apply_resume_semantics(&mut self) {
        let Some(pending) = self.pending.clone() else {
            return;
        };
        match self.state.clone() {
            RunState::AwaitingApproval(_) => {
                self.state = RunState::Evaluating(pending.turn.calls.clone());
                self.reask_approval = true;
            }
            RunState::Dispatching(_) => {
                self.state = RunState::Evaluating(pending.turn.calls.clone());
                // 在途的那一条 = 已配平的条数（观察数就是"跑完几条"）。
                self.replay_pending = pending
                    .turn
                    .calls
                    .get(pending.outcomes.len())
                    .cloned();
            }
            _ => {}
        }
    }

    /// 会话 ID。
    pub fn session_id(&self) -> &SessionId {
        self.session.id()
    }

    /// 完整会话历史。裁剪不影响这里：`session` 始终是全量真相。
    pub fn session(&self) -> &Session {
        &self.session
    }

    /// 取消把手，克隆给外部。外部在任何时刻调用 `cancel()`，
    /// `run()` 都会在下一轮问模型之前停下。
    pub fn cancel_handle(&self) -> CancelHandle {
        self.cancel.clone()
    }

    /// 进入下一步：换角色、交代步骤、把上一步的结论落进历史。
    ///
    /// P9-3 单会话编排的接缝。一个任务只开一个会话，步骤之间不再另起
    /// runner——历史、轮数、墙钟与缓存路由键全部延续，下一步的模型因此
    /// 看得见上一步读过的文件和得出的结论，不必把同样的文件再读一遍。
    ///
    /// 只在 [`RunState::Completed`] 之后调用：那意味着上一步正常收尾。
    /// 半轮状态（有未配平的调用）下换步骤会丢掉 pending，历史从此错位。
    ///
    /// # Errors
    ///
    /// 把上一步的最终答案提交进历史时若配平被破坏，返回
    /// [`ApplicationError::Session`]。正常流程不可能——无调用轮次的观察
    /// 必然是空的；真出现说明会话状态已被破坏，带着错历史继续问模型
    /// 只会把错误放大。
    pub fn begin_step(
        &mut self,
        spec: AgentSpec,
        prompt: String,
    ) -> Result<(), ApplicationError> {
        // 恢复路径：runner 停在这一步的**半轮**上（评估中 / 等人审批 /
        // 正在执行），而这一步的交代已经在历史末尾。这种情况下"进入
        // 下一步"其实是"继续这一步"——把状态机重置成"等模型回复"会把
        // pending 那一轮整个丢掉，中断时的执行现场（可能已经发生的
        // 副作用、还没答复的审批）就此消失，恢复语义也就无从谈起。
        let resuming_same_step = matches!(
            self.state,
            RunState::Evaluating(_) | RunState::AwaitingApproval(_) | RunState::Dispatching(_)
        ) && self.session.ends_with_user_message(&prompt);
        if resuming_same_step {
            // 只换角色与白名单：这一步的现场（状态机位置与 pending）原样保留。
            self.spec = spec;
            self.refresh_role_boundary();
            self.save_snapshot();
            return Ok(());
        }
        // 上一步的最终答案必须先落进历史。`step()` 的 Completed 分支
        // 刻意不提交轮次（最终答案没有观察可配），而单会话编排的全部
        // 价值就在于下一步看得见上一步的结论——不提交的话，第二步的
        // 模型只能看到第二步自己的东西，"跨步骤不重读"无从谈起。
        if let RunState::Completed(answer) = &self.state {
            let answer = answer.clone();
            self.session
                .commit_round(AgentTurn { text: answer, calls: Vec::new() }, Vec::new())?;
        }
        // 恢复路径上这一步的交代**已经在历史里**了：中断就发生在这一步
        // 当中，交接是快照的一部分。再推一遍就成了同一句交代出现两次，
        // 模型看到两条一模一样的指令，"会话开了几步"也会因此算错。
        //
        // 只看历史末尾不够：预算耗尽时末条是观察（那一轮已提交），交代
        // 在更早的位置。查全历史——这条 prompt 由 step_prompt 唯一生成，
        // 全历史只该有一条。
        if !self.session.has_user_message(&prompt) {
            self.session.push_user(prompt);
        }
        self.spec = spec;
        self.refresh_role_boundary();
        self.state = RunState::AwaitingModel;
        self.pending = None;
        // 立刻存一次：快照若还停在 Completed，恢复的人会以为任务已经
        // 结束，而实际上还有步骤没跑。
        self.save_snapshot();
        Ok(())
    }

    /// 追加一条用户消息到会话历史。
    ///
    /// P10 任务级恢复用：把已完成步骤的结论交回给模型，让恢复后的
    /// 下一步看得见前面几步查出了什么（见 `application::task_resume`）。
    ///
    /// 与 [`Self::begin_step`] 的分工：`begin_step` 换角色、交代步骤、
    /// 并把上一步的最终答案提交成一轮；这条只推一条消息，不动状态机。
    /// 恢复路径上 runner 是从快照里重建的，上一步的答案已经在历史里
    /// （它随会话快照一起存着），不需要再提交一次。
    ///
    /// 不存快照：这条消息本身就会随下一次保存点一起落盘，而恢复刚
    /// 发生时立刻存一份会把"刚恢复"与"刚交接"两个状态混在一个文件里，
    /// 反而更难看出恢复到了哪。
    pub fn push_user_message(&mut self, message: impl Into<String>) {
        self.session.push_user(message);
    }

    /// 把一条用户消息插到当前步骤交代之前。
    ///
    /// 结论交接用：结论是对前几步的总结，落在"现在做第几步"这条交代
    /// 之前才是对的顺序——放后面就成了倒着说。
    pub fn insert_user_message_before_step(&mut self, message: impl Into<String>) {
        let position = self.session.step_prompt_position();
        self.session.insert_user_at(position, message);
    }

    /// 按当前 `spec.role` 重算角色可调用的工具名。
    ///
    /// 查不到档案时是 `None`（不限）：编排路径已在调用
    /// [`AgentRoster::find`] 时把"缺档案"变成显式错误，走到这里说明
    /// 角色一定有档案，`None` 只可能是档案本身没配白名单。
    fn refresh_role_boundary(&mut self) {
        self.allowed_tools = self
            .roster
            .allowed_tools(self.spec.role)
            .map(<[String]>::to_vec);
    }

    /// 这条调用是否越过当前角色的工具白名单。
    ///
    /// 返回 `Some(拒绝说明)` 表示越权。与注册表查找、策略裁决、审批闸门
    /// 相互独立：注册表管"这个工具存不存在"，策略管"该不该审"，闸门管
    /// "人点不点头"，这道管"这个角色配不配提这个调用"。少任何一道都有
    /// 一条真实的越权路径能通到底。
    ///
    /// 拒绝说明必须点破"这是权限问题不是审批问题"：模型最常见的反应是
    /// 换个参数重试同一次调用，或者跑去请求审批——两条路都是死路，
    /// 说清边界它才会改用这个角色真能用的工具。
    fn role_refusal(&self, call: &ToolCall) -> Option<String> {
        let allowed = self.allowed_tools.as_ref()?;
        if allowed.iter().any(|name| name == &call.name) {
            return None;
        }
        Some(format!(
            "角色 `{}` 无权调用工具 `{}`（该角色可用: {}）。这是角色权限边界，\
             不是审批问题——请改用该角色可用的工具，或把这件事留给后续步骤的角色。",
            self.spec.role.name(),
            call.name,
            allowed.join(", "),
        ))
    }

    /// 角色边界检查，越权就记一条拒绝观察并推进。
    ///
    /// 返回 `Ok(None)` 表示没越权，调用方照常往下走。
    ///
    /// 检查逻辑只有这一份，调用点有两处（`Evaluating` 与 `Dispatching`）：
    /// 状态机的形状决定的——恢复可以从 `Dispatching` 直接进入，那条路
    /// 上 `Evaluating` 的检查被整段跳过。只在 `Evaluating` 查一次的后果
    /// 是：一个停在 `Dispatching(write_file)` 的受限角色快照，恢复后
    /// 就能把写工具放行。
    ///
    /// `in_flight` 表示这次调用正是中断时正在执行的那一条。越权时要多
    /// 说一句"它可能已经执行过"：角色边界管的是**现在**能不能调这个
    /// 工具，而那次调用可能在中断前就已经跑掉了——只说"不许调"会让
    /// 模型以为它压根没跑过，于是照原样再提一次。
    async fn refuse_out_of_role(
        &mut self,
        call: &ToolCall,
        in_flight: bool,
    ) -> Result<Option<RunState>, ApplicationError> {
        let Some(reason) = self.role_refusal(call) else {
            return Ok(None);
        };
        let reason = if in_flight {
            format!(
                "{reason}（另注：这次调用在中断前已经发起，可能已经执行过；\
                 上面这条拒绝只说明现在不能执行它。）"
            )
        } else {
            reason
        };
        self.record_and_advance(call.clone(), ToolOutcome::refused(reason))
            .await
            .map(Some)
    }

    /// 这次调用是不是"中断时正在执行"的那一条。
    ///
    /// 是的话，它可能已经产生了副作用——恢复时不自动重跑，而是把决定权
    /// 交回模型（见 [`AgentRunner::replay_interrupted_call`]）。
    fn is_in_flight(&self, call: &ToolCall) -> bool {
        self.replay_pending
            .as_ref()
            .is_some_and(|pending| pending.id == call.id)
    }

    /// 造一份审批请求并停在闸门前。
    ///
    /// `resumed` 标出"这是恢复后的重新询问"：闸门据此不让会话信任免掉
    /// 这一问，人据此知道上一次问过、没答复。
    fn park_at_approval(
        &mut self,
        call: ToolCall,
        annotations: ToolAnnotations,
        reason: Option<String>,
        resumed: bool,
    ) -> RunState {
        let request = ApprovalRequest {
            turn: self.session.rounds() + 1,
            call,
            annotations,
            reason,
            resumed,
        };
        if resumed {
            // 恢复路径上那一次"问过"发生在上一个进程里，当前证据文件里
            // 没有它——补一条，否则事后翻 JSONL 会以为人从头到尾只被问过
            // 一次。
            self.record(AuditEvent::ApprovalReasked {
                turn: request.turn,
                tool: request.call.name.clone(),
                reason: APPROVAL_REASK_REASON.to_owned(),
            });
        }
        self.state = RunState::AwaitingApproval(request.clone());
        // 最重要的保存点：等人审批时人可能走开、进程可能被杀，
        // 快照必须先落盘，恢复后才能把同一个请求重新递给闸门。
        self.save_snapshot();
        RunState::AwaitingApproval(request)
    }

    /// 恢复时处理"中断时正在执行"的那一次调用。
    ///
    /// **不执行它。** 中断发生在调用发起之后、观察记下之前——它可能已经
    /// 产生了副作用（文件已写、命令已跑），而我们不知道结果。重跑一次
    /// 就是把同一次副作用做两遍；不重跑又不能装作没发生过。于是：
    ///
    /// 1. 留一条 [`AuditEvent::ToolCallReplayed`] 证据；
    /// 2. 给模型回一条观察，说明"这个调用之前跑过、结果是什么、要不要
    ///    再跑由你决定"；
    /// 3. 这一轮照常配平，模型下一轮就能看到这条提示并自己决定。
    async fn replay_interrupted_call(
        &mut self,
        call: ToolCall,
    ) -> Result<RunState, ApplicationError> {
        let turn = self.session.rounds() + 1;
        let previous = self.previous_outcome_of(&call);
        self.record(AuditEvent::ToolCallReplayed {
            turn,
            tool: call.name.clone(),
            reason: REPLAY_REASON.to_owned(),
            previous_outcome: previous.clone(),
        });
        // 只对这一次调用生效：重放提示说过就作废。
        self.replay_pending = None;
        let content = replay_notice(&call, previous.as_ref());
        self.record_and_advance(call, ToolOutcome::refused(content)).await
    }

    /// 这个调用在本次会话里之前跑过没有；跑过就把那次的结果找出来。
    ///
    /// 判据是"工具名与参数完全一致"：同一路径的同一写入、同一参数的
    /// 同一命令。参数不同就不是同一件事——拿别的结果当"上次的结果"，
    /// 会把模型引向错误的结论（它会以为文件已经是新内容了）。
    ///
    /// 先看本轮已完成的调用（它们还没进历史，但确实跑过），再看历史里
    /// 更早的轮次。
    fn previous_outcome_of(&self, call: &ToolCall) -> Option<ToolOutcome> {
        if let Some(pending) = &self.pending {
            let index = pending.turn.calls.iter().position(|earlier| {
                earlier.name == call.name && earlier.arguments == call.arguments
            });
            if let Some(outcome) = index.and_then(|index| pending.outcomes.get(index)) {
                return Some(outcome.clone());
            }
        }
        let mut turns = self.session.turns().iter();
        while let Some(turn) = turns.next() {
            let crate::domain::session::Turn::Agent(agent) = turn else {
                continue;
            };
            // 观察紧跟在带调用的那一轮之后（`commit_round` 保证配对）。
            let Some(crate::domain::session::Turn::Observation(outcomes)) = turns.next() else {
                continue;
            };
            let index = agent.calls.iter().position(|earlier| {
                earlier.name == call.name && earlier.arguments == call.arguments
            });
            if let Some(outcome) = index.and_then(|index| outcomes.get(index)) {
                return Some(outcome.clone());
            }
        }
        None
    }

    /// 推进一步。调用方决定何时停——这是超时/取消/恢复的挂点。
    ///
    /// # Errors
    ///
    /// 模型调用失败、会话配对被破坏、或模型给出空回复时返回错误。
    /// 出错后 runner 回到 [`RunState::AwaitingModel`]，已提交的轮次仍在
    /// 历史里，重新 `run` 会接着往下走。
    pub async fn step(&mut self) -> Result<RunState, ApplicationError> {
        // 取出状态再匹配，否则整个 match 期间都借用着 self.state，
        // 各分支就没法改写它。每个分支都会重新赋值，取出的默认值不会漏出去。
        match std::mem::replace(&mut self.state, RunState::AwaitingModel) {
            RunState::AwaitingModel => {
                let reply = self.ask_model().await?;
                if reply.turn.is_final_answer() {
                    self.state = RunState::Completed(reply.turn.text.clone());
                    // 终态也存一次快照：任务完成的那一刻，"最后状态"对
                    // --list-sessions 和事后排查都有价值。
                    self.save_snapshot();
                    return Ok(RunState::Completed(reply.turn.text));
                }

                let turn_number = self.session.rounds() + 1;
                for call in &reply.turn.calls {
                    self.record(AuditEvent::ToolRequested {
                        turn: turn_number,
                        call: call.clone(),
                    });
                }
                let calls = reply.turn.calls.clone();
                self.pending = Some(PendingRound {
                    turn: reply.turn,
                    outcomes: Vec::with_capacity(calls.len()),
                });
                self.state = RunState::Evaluating(calls.clone());
                Ok(RunState::Evaluating(calls))
            }

            RunState::Evaluating(calls) => {
                let call = self.next_call(&calls)?.clone();
                // 这次调用是不是中断时正在执行的那一条。
                let in_flight = self.is_in_flight(&call);
                // 角色边界先于一切：未注册检查、策略裁决、人工审批管的都是
                // "这个调用能不能发生"，这里管"这个角色配不配提它"。放在
                // 最前面，越权调用连策略和闸门都不值得惊动——为人省下
                // 一次注定被拒的审批，也让审计里不会出现一条误导性的
                // "策略已放行"。
                if let Some(state) = self.refuse_out_of_role(&call, in_flight).await? {
                    // 越权调用就此终结：重放提示对它不再适用。
                    if in_flight {
                        self.replay_pending = None;
                    }
                    return Ok(state);
                }
                // 恢复路径：这次调用在中断前已发起，可能已经产生副作用。
                // 不自动重跑——把"跑过没有、结果是什么"告诉模型，由它决定
                // 要不要再来一次。
                if in_flight {
                    return self.replay_interrupted_call(call).await;
                }
                // 未注册的工具不进审批也不执行，直接拒绝。
                let Some(entry) = self.tools.find(&call.name) else {
                    let outcome = ToolOutcome::refused(unregistered_tool_message(
                        &call.name,
                        &self.visible_specs,
                    ));
                    return self.record_and_advance(call, outcome).await;
                };
                let spec = &entry.spec;

                // 策略裁决优先于注解：注解只是默认策略的输入之一，
                // 配置一条规则就能覆盖它。
                let decision = self.policy.evaluate(&call, spec);
                self.record(AuditEvent::PolicyEvaluated {
                    turn: self.session.rounds() + 1,
                    tool: call.name.clone(),
                    verdict: decision.verdict,
                    rule_index: decision.rule_index,
                    reason: decision.reason.clone(),
                });

                // P8-3：策略说放行，但命令看着像把写操作伸到 workspace 外时，
                // 升级成要审批。审计里记的仍是策略的原判——策略没改，改的是
                // 这次调用要不要人过目，两件事不能混在一条事件里。
                let (decision, escalation) = escalate_out_of_workspace_write(decision, &call);

                // 恢复时的"重新审批"标记只作用于这一次调用：裁决完就清掉。
                // 它必须在这里取走而不是留在字段里——否则策略放行时它会
                // 一直挂着，蹭到下一次审批上去。
                let reask = std::mem::replace(&mut self.reask_approval, false);

                match decision.verdict {
                    // 拒绝：连闸门都不问，直接给模型一条拒绝观察。
                    // 内容里带上命中规则的理由，模型才知道该换哪条路。
                    PolicyVerdict::Deny => {
                        let outcome = ToolOutcome::refused(denied_by_policy_message(
                            &call.name,
                            &decision,
                        ));
                        self.record_and_advance(call, outcome).await
                    }
                    // 要审批：停在闸门前，策略理由一并带上，
                    // 让人（或未来的自动闸门）知道是哪条规则要他点头。
                    PolicyVerdict::RequireApproval => {
                        Ok(self.park_at_approval(call, spec.annotations, escalation, reask))
                    }
                    // 放行：不过闸门，直接进执行。
                    PolicyVerdict::Allow => {
                        // 恢复路径上的例外：这个请求在中断前已经被判为
                        // "需要人过目"。策略后来放宽了，也不能替人把这个
                        // 判断抹掉——人要再点一次，否则"人没答复"会被
                        // 悄悄读成"人点过了"。
                        if reask {
                            return Ok(
                                self.park_at_approval(call, spec.annotations, escalation, true)
                            );
                        }
                        self.state = RunState::Dispatching(calls.clone());
                        Ok(RunState::Dispatching(calls))
                    }
                }
            }

            RunState::AwaitingApproval(request) => {
                let decision = self.approvals.request(&request).await;
                self.record(AuditEvent::ApprovalDecided {
                    turn: request.turn,
                    tool: request.call.name.clone(),
                    allowed: decision.is_ok(),
                    reason: decision.as_ref().err().cloned(),
                });
                match decision {
                    // 否决不是崩溃：生成一条拒绝观察，让模型换条路。
                    Err(reason) => {
                        self.record_and_advance(request.call, ToolOutcome::refused(reason))
                            .await
                    }
                    Ok(()) => {
                        let calls = self.current_calls()?;
                        self.state = RunState::Dispatching(calls.clone());
                        Ok(RunState::Dispatching(calls))
                    }
                }
            }

            RunState::Dispatching(calls) => {
                let call = self.next_call(&calls)?.clone();
                let in_flight = self.is_in_flight(&call);
                // 恢复可能从 Dispatching 直接进入（快照停在半轮执行），
                // 那时 Evaluating 的检查被跳过——所以这里必须再查一次。
                if let Some(state) = self.refuse_out_of_role(&call, in_flight).await? {
                    if in_flight {
                        self.replay_pending = None;
                    }
                    return Ok(state);
                }
                // 在途的调用同样不自动重跑（见 `replay_interrupted_call`）。
                if in_flight {
                    return self.replay_interrupted_call(call).await;
                }
                // **执行前落盘**：这是"副作用可能已经发生"这个事实唯一
                // 能被记录下来的时刻——调用一旦发起，进程就可能死在它
                // 里面，那时磁盘上必须还留着"这一次正要执行"的痕迹，
                // 否则恢复时无从知道该不该重跑。
                //
                // 状态机在 `step()` 开头被换成了 `AwaitingModel`，所以这里
                // 要显式写回 `Dispatching`：落盘的状态必须是真实的位置，
                // 否则快照说"在等模型"，恢复就会当成什么都没发生。
                self.state = RunState::Dispatching(calls.clone());
                self.save_snapshot();
                let outcome = self.tools.invoke(&call).await;
                self.record_and_advance(call, outcome).await
            }

            RunState::Completed(answer) => Ok(RunState::Completed(answer)),
        }
    }

    /// 跑到终局。行为与重构前的 `for turn in 1..=max_turns` 一致。
    ///
    /// # Errors
    ///
    /// 见 [`Self::step`]；另外达到 `max_turns` 仍无最终答案时返回
    /// [`ApplicationError::BudgetExhausted`]，整体耗时超过
    /// `Budget::max_wall_clock` 时返回 [`ApplicationError::WallClockExhausted`]，
    /// 外部取消时返回 [`ApplicationError::Cancelled`]——三者可区分，
    /// 调用方能分辨"跑超了"还是"被叫停了"。
    pub async fn run(&mut self, budget: &Budget) -> Result<AgentOutcome, ApplicationError> {
        // 每次进入 run() 都重置测量锚点：基数是此前累计的耗时——恢复的
        // 运行来自快照，接着算墙钟；全新运行的基数是零，行为不变。
        self.run_start = self.clock.now();
        self.elapsed_base = self.consumption.elapsed;
        loop {
            // 耗时每个循环都重测：Consumption 只是"已测量结果"的容器，
            // 测量必须走 Clock 端口，domain 不取时间。
            self.refresh_elapsed();
            // 取消、超时与轮数预算都只在"准备问模型"之前查：正在分发的
            // 轮次要允许跑完，已花出去的 token 不废在半途。
            if matches!(self.state, RunState::AwaitingModel) {
                if self.cancel.is_cancelled() {
                    self.save_snapshot();
                    return Err(ApplicationError::Cancelled);
                }
                if budget.exhausted(&self.consumption) {
                    // exhausted 命中后先分清维度：两种超限的对策不同，
                    // 错误要能指导调用方动作。
                    if let Some(limit) = budget.max_wall_clock {
                        if self.consumption.elapsed >= limit {
                            self.save_snapshot();
                            return Err(ApplicationError::WallClockExhausted {
                                elapsed: self.consumption.elapsed,
                                limit,
                            });
                        }
                    }
                    self.save_snapshot();
                    return Err(ApplicationError::BudgetExhausted {
                        max_turns: budget.max_turns,
                    });
                }
            }
            match self.step().await {
                Ok(RunState::Completed(answer)) => {
                    return Ok(AgentOutcome {
                        answer,
                        turns_used: self.consumption.turns,
                    });
                }
                Ok(_) => {}
                // 出错也存一次：任务没完成，下次从这里接。
                Err(error) => {
                    self.save_snapshot();
                    return Err(error);
                }
            }
        }
    }

    /// 问一次模型，并把"思考"记进证据。
    async fn ask_model(&mut self) -> Result<ModelReply, ApplicationError> {
        // 先克隆 Arc：否则请求借着 self 的字段，而它们紧接着就要被改写。
        let model = self.model.clone();
        // 先探测一次裁剪：被裁轮次值得摘要且现有摘要没覆盖时，
        // 先补摘要再正式裁——摘要是挂点，补完后下面的 compact 会带上它。
        let (_, probe) = compact(&self.session, &self.context_budget);
        if !probe.dropped.is_empty() && self.should_summarize(&probe.dropped) {
            self.refresh_summary(&probe.dropped).await;
        }
        // 裁剪是发给模型前的视图变换：请求里只放模型真正看到的东西。
        // 丢了历史就留一条证据，事后翻 JSONL 能对出"第 N 轮模型看到了什么"。
        let (history, report) = compact(&self.session, &self.context_budget);
        if !report.dropped.is_empty() {
            self.record(AuditEvent::ContextCompacted {
                turn: self.session.rounds() + 1,
                rounds_before: report.rounds_before,
                rounds_after: report.rounds_after,
                dropped: report.dropped,
            });
            // token 账单独记一条：轮数是手段，token 才是花掉的钱。
            self.record(AuditEvent::ContextBudgeted {
                turn: self.session.rounds() + 1,
                tokens_before: report.tokens_before as u64,
                tokens_after: report.tokens_after as u64,
                dropped_tokens: report.dropped_tokens as u64,
            });
        }
        let request = ModelRequest {
            history,
            spec: &self.spec,
            tools: &self.visible_specs,
            // 会话 ID 作路由键：同一会话的每一轮都必须是同一个键，
            // 否则 provider 把它们路由到不同机器，前缀缓存一次都读不回来。
            cache_key: Some(self.session.id().as_str()),
        };
        let reply = model.complete(request.clone()).await?;
        // 空回复是模型端的瞬态故障（实测：偶发"计费了几十个输出 token，
        // 却抽不出任何文本或调用"），不是任务的终点——重试一次同一请求，
        // 仍为空才报错。重试的这一问还没被记为消耗轮，不计入预算。
        let reply = if reply.turn.text.trim().is_empty() && reply.turn.calls.is_empty() {
            eprintln!("警告：模型返回空回复，重试一次");
            model.complete(request).await?
        } else {
            reply
        };
        self.consumption.record_turn();
        self.record(AuditEvent::Thought {
            turn: self.session.rounds() + 1,
            text: reply.turn.text.clone(),
            usage: reply.usage,
        });
        if reply.turn.text.trim().is_empty() && reply.turn.calls.is_empty() {
            return Err(ApplicationError::EmptyResponse);
        }
        Ok(reply)
    }

    /// 是否值得为这次裁剪生成/更新摘要。
    ///
    /// 三个闸门都过才生成：连续失败没有达到上限（持续失败时每轮重试
    /// 只是白花模型调用，截断后本次运行退回占位说明）、被裁轮次达到
    /// 策略阈值（小额裁剪不值得多花一次调用），且已有摘要没有覆盖
    /// 全部被裁轮次（覆盖了就是白花钱）。
    fn should_summarize(&self, dropped: &[u32]) -> bool {
        if self.summary_failures >= SUMMARY_FAILURE_LIMIT {
            return false;
        }
        let Some(policy) = self.context_budget.summarize else {
            return false;
        };
        if dropped.len() < policy.min_dropped_to_summarize {
            return false;
        }
        match self.session.summary() {
            Some(summary) => !summary.covers_all(dropped),
            None => true,
        }
    }

    /// 被裁轮次涉及的步骤号（1-based，升序去重）。
    ///
    /// 轮次号到步号的映射是**结构性的**：会话历史里每条非交接的用户
    /// 消息就是一步的交代，数一遍用户消息就知道每轮属于第几步。
    fn steps_of(&self, dropped: &[u32]) -> Vec<u32> {
        let per_round = self.session.round_steps();
        let mut steps: Vec<u32> = dropped
            .iter()
            .filter_map(|number| {
                // 轮次从 1 开始，数组下标从 0 开始。
                per_round.get((*number as usize).saturating_sub(1)).copied()
            })
            .collect();
        steps.dedup();
        steps
    }

    /// 生成并挂载摘要。摘要只是优化：失败打警告后继续用占位说明，
    /// 绝不把错误抛给任务。
    ///
    /// 摘要调用计入 `consumption`（它是一次真实的模型调用，预算账要
    /// 如实），并留 [`AuditEvent::SummaryGenerated`] 供事后对账。
    async fn refresh_summary(&mut self, dropped: &[u32]) {
        let turn = self.session.rounds() + 1;
        // 借用 session 的同步部分先渲染成 owned 文本——`ContextSlice`
        // 借着 session，不能跨过下面的 await。
        let slices: Vec<ContextSlice<'_>> = self
            .session
            .slices()
            .into_iter()
            .filter(|slice| {
                matches!(slice, ContextSlice::Round { number, .. } if dropped.contains(number))
            })
            .collect();
        let transcript = render_slices(&slices);
        let existing = self.session.summary().cloned();
        let covers = (
            dropped.first().copied().unwrap_or(1),
            dropped.last().copied().unwrap_or(1),
        );
        // 摘要覆盖哪几步由代码算：历史里每条非交接的用户消息就是一步的
        // 交代，数一遍就知道每轮属于第几步。模型可能漏写、可能写错编号，
        // 而"这份摘要讲的是哪几步"必须准确——下一步的模型要靠它对上
        // 自己的进度。
        let steps = self.steps_of(dropped);
        let summarizer = Summarizer::new(self.model.clone());
        match summarizer
            .summarize(SummarizeInput {
                dropped_transcript: transcript,
                existing,
                covers,
                steps,
            })
            .await
        {
            Ok((summary, usage)) => {
                self.session.set_summary(summary.clone());
                self.consumption.record_turn();
                self.record(AuditEvent::SummaryGenerated {
                    turn,
                    covers: summary.covers,
                    usage,
                });
            }
            Err(error) => {
                self.summary_failures += 1;
                eprintln!("警告：历史摘要生成失败（继续用占位说明）: {error}");
                self.record(AuditEvent::SummaryFailed {
                    turn,
                    reason: error.to_string(),
                });
            }
        }
    }

    /// 取本轮下一个待处理的调用。
    fn next_call<'a>(&self, calls: &'a [ToolCall]) -> Result<&'a ToolCall, ApplicationError> {
        let processed = self.pending_outcome_count()?;
        calls.get(processed).ok_or(ApplicationError::NoPendingRound)
    }

    /// 当前轮的调用列表。
    fn current_calls(&self) -> Result<Vec<ToolCall>, ApplicationError> {
        Ok(self.pending_round()?.turn.calls.clone())
    }

    /// 已处理的观察数，即下一个待处理调用的下标。
    fn pending_outcome_count(&self) -> Result<usize, ApplicationError> {
        Ok(self.pending_round()?.outcomes.len())
    }

    /// 待配平的轮次。Evaluating/Dispatching/AwaitingApproval 三个状态下必然存在。
    fn pending_round(&self) -> Result<&PendingRound, ApplicationError> {
        self.pending.as_ref().ok_or(ApplicationError::NoPendingRound)
    }

    /// 记下一条观察，然后推进：还有调用没处理就回到 `Evaluating`，
    /// 处理完了就原子提交本轮，回到 `AwaitingModel`。
    async fn record_and_advance(
        &mut self,
        call: ToolCall,
        outcome: ToolOutcome,
    ) -> Result<RunState, ApplicationError> {
        let turn = self.session.rounds() + 1;
        self.record(AuditEvent::ToolObserved {
            turn,
            call,
            outcome: outcome.clone(),
        });

        // 先在一个块里把 pending 改完并取出决策依据，避免整个函数
        // 都借着他、后面没法动 self.state。
        let (calls, finished) = {
            let pending = self
                .pending
                .as_mut()
                .ok_or(ApplicationError::NoPendingRound)?;
            pending.outcomes.push(outcome);
            (
                pending.turn.calls.clone(),
                pending.outcomes.len() >= pending.turn.calls.len(),
            )
        };

        if !finished {
            self.state = RunState::Evaluating(calls.clone());
            return Ok(RunState::Evaluating(calls));
        }

        // 观察齐了才提交：助手消息与它的观察在历史里永远相邻。
        let pending = self
            .pending
            .take()
            .ok_or(ApplicationError::NoPendingRound)?;
        self.session.commit_round(pending.turn, pending.outcomes)?;
        self.state = RunState::AwaitingModel;
        // 历史增长的点：恢复从这里开始最干净（无半轮状态）。
        self.save_snapshot();
        Ok(RunState::AwaitingModel)
    }

    /// 记一条证据，时间戳由时钟取。
    fn record(&self, event: AuditEvent) {
        self.evidence.record(self.clock.now(), event);
    }

    /// 把墙钟重测进 `consumption`。所有保存点都先调它：`step()` 内部的
    /// 保存点在 `run()` 的循环计时之外，不重测的话快照里的已耗时停在
    /// 上一轮循环顶部的旧值，恢复后墙钟预算被低估。
    fn refresh_elapsed(&mut self) {
        self.consumption.elapsed = self.elapsed_base + self.clock.elapsed_since(self.run_start);
    }

    /// 捕获当前状态的可序列化快照。
    pub fn snapshot(&self) -> RunSnapshot {
        RunSnapshot {
            format_version: RUN_FORMAT,
            session_id: self.session.id().clone(),
            spec: self.spec.clone(),
            session: self.session.snapshot(),
            state: self.state.clone(),
            consumption: self.consumption,
            pending: self.pending.clone(),
        }
    }

    /// 把快照存进依赖里给的存储，失败降级为 stderr 警告。
    ///
    /// 快照是"锦上添花"：任务本身是主线，不能让一次写盘失败
    /// （磁盘满、目录被删）把跑了几十轮的任务带崩。
    fn save_snapshot(&mut self) {
        // 先克隆再刷新耗时：refresh_elapsed 要 &mut self，不能带着
        // 对 session_store 的借用调它。
        let Some(store) = self.session_store.clone() else {
            return;
        };
        // 存之前重测一次墙钟：保存点可能落在 run() 循环计时之外。
        self.refresh_elapsed();
        let snapshot = self.snapshot();
        if let Err(error) = store.save(&snapshot) {
            eprintln!("警告：快照保存失败（任务继续）: {error}");
        }
    }
}

/// 生成会话 ID。
///
/// 不引 `uuid`：P0 只需要"一次运行内唯一、日志里能认出来"。
/// 进程号加纳秒时间戳已经够用，且不给依赖树添新东西。
///
/// 公开是因为 `main.rs` 要先知道 ID 才能给证据文件命名——
/// 否则 runner 内部生成一个、外部算另一个，文件名和内容就对不上了。
pub fn new_session_id(clock: &dyn Clock) -> SessionId {
    let nanos = clock
        .now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    SessionId::new(format!("session-{}-{nanos}", std::process::id()))
}

/// 未注册工具的拒绝说明，列出可用工具让模型自己挑。
fn unregistered_tool_message(name: &str, specs: &[ToolSpec]) -> String {
    let available: Vec<&str> = specs.iter().map(|spec| spec.name.as_str()).collect();
    format!("未注册的工具 `{name}`，可用工具: {}", available.join(", "))
}

/// 策略拒绝的说明。
///
/// 必须带上规则理由：只说"被禁止"的话，模型无法判断是换参数、换工具、
/// 还是这条路根本走不通，只会盲目重试同一个调用。
fn denied_by_policy_message(name: &str, decision: &PolicyDecision) -> String {
    match decision.rule_index {
        Some(index) => format!(
            "工具 `{name}` 被策略规则 #{index} 拒绝：{}。请换一种方式完成，不要重试同一调用。",
            decision.reason
        ),
        None => format!("工具 `{name}` 被默认策略拒绝：{}", decision.reason),
    }
}

/// 恢复时给模型的重放提示。
///
/// 措辞要回答模型真正会问的三个问题：这个调用跑过没有、结果是什么、
/// 现在该怎么办。少答一个它就会自己猜——而它猜的方向通常是"再跑一遍"，
/// 那正是我们要避免的重复副作用。
fn replay_notice(call: &ToolCall, previous: Option<&ToolOutcome>) -> String {
    let known = match previous {
        Some(outcome) => format!("同一调用此前执行过的结果是：{}。", outcome.content),
        None => "这次执行的结果未知（中断就发生在它执行期间）。".to_owned(),
    };
    format!(
        "【中断恢复】工具 `{}` 在中断前已经发起执行，可能已经产生了副作用，\
         因此**没有自动再跑一次**。{known}\
         要不要再执行一次由你判断：需要就重新发起同一个调用，\
         不需要就用现有信息继续。",
        call.name
    )
}

/// 策略放行但调用疑似越界写入时，把裁决升级为要求审批。
///
/// 返回升级后的裁决，以及要写进 [`ApprovalRequest::reason`] 的理由；
/// 没有升级时理由为 `None`。
///
/// 补的是策略的一个真实缺口：策略按"工具名 + 参数文本"放行 `run_command`
/// 之后，命令内容就不受任何约束了——模型敲 `echo x > C:\Windows\temp\y`，
/// 策略看到的是"run_command，已放行"。这道检查让这种调用至少有人过目。
///
/// 只升级 `Allow`：本来就要审的（`RequireApproval`）不必再贴一遍理由，
/// 被拒的（`Deny`）连闸门都到不了。`rule_index` 置回 `None`——放行它的
/// 规则并没有改主意，改主意的是这次调用本身。
fn escalate_out_of_workspace_write(
    decision: PolicyDecision,
    call: &ToolCall,
) -> (PolicyDecision, Option<String>) {
    if decision.verdict != PolicyVerdict::Allow {
        return (decision, None);
    }
    // 只有带 command 参数的调用（即 run_command）走得进这个启发式，
    // 其他工具没有命令文本可看。
    let Some(command) = call.arguments.get("command").and_then(Value::as_str) else {
        return (decision, None);
    };
    let Some(suspect) = write_escape_suspect(command) else {
        return (decision, None);
    };
    (
        PolicyDecision {
            verdict: PolicyVerdict::RequireApproval,
            reason: format!("策略已放行，但疑似越界写入：{suspect}"),
            rule_index: None,
        },
        Some(suspect),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::port::{BoxFuture, Decision, ToolInvoker};
    use rig::completion::Message;
    use crate::domain::agent::AgentRole;
    use crate::domain::evidence::{AuditEvent, TokenUsage};
    use crate::domain::ids::CallId;
    use crate::domain::tool::{OutcomeStatus, ToolAnnotations};
    use crate::infrastructure::evidence::memory::MemoryEvidenceSink;
    use crate::infrastructure::model::rig_chat::RigChatGateway;
    use rig::message::{AssistantContent, ToolName, UserContent};
    use rig::test_utils::{MockCompletionModel, MockTurn};
    use serde_json::json;

    fn spec_with() -> AgentSpec {
        AgentSpec {
            role: AgentRole::Coder,
            system_prompt: "系统提示".to_owned(),
            model: None,
            effort: None,
            verbosity: None,
        }
    }

    /// 把一组规格包成注册表，全部指向同一个执行器。
    ///
    /// 测试里大多数场景只关心"模型调了什么"，不关心谁执行。
    fn registry_with(invoker: Arc<dyn ToolInvoker>, specs: Vec<ToolSpec>) -> ToolRegistry {
        ToolRegistry::new(
            specs
                .into_iter()
                .map(|spec| crate::application::tool_registry::ToolEntry { spec, invoker: invoker.clone() })
                .collect(),
        )
    }

    /// 只读工具规格。
    fn read_file_spec() -> ToolSpec {
        ToolSpec {
            name: "read_file".to_owned(),
            description: "读文件".to_owned(),
            parameters: json!({"type": "object"}),
            annotations: ToolAnnotations::new(true, false, true, false),
        }
    }

    /// 破坏性工具规格，用来验证审批接缝。
    fn write_file_spec() -> ToolSpec {
        ToolSpec {
            name: "write_file".to_owned(),
            description: "写文件".to_owned(),
            parameters: json!({"type": "object"}),
            annotations: ToolAnnotations::new(false, true, false, false),
        }
    }

    /// 记录调用但不真做事的执行器。
    #[derive(Default)]
    struct RecordingInvoker {
        calls: std::sync::Mutex<Vec<String>>,
    }

    impl ToolInvoker for RecordingInvoker {
        fn invoke<'a>(&'a self, call: &'a ToolCall) -> BoxFuture<'a, ToolOutcome> {
            Box::pin(async move {
                self.calls.lock().unwrap().push(call.name.clone());
                ToolOutcome::success("文件内容")
            })
        }
    }

    /// 数一数闸门被问了多少次、问的是什么。
    #[derive(Default)]
    struct CountingGate {
        requested: std::sync::Mutex<Vec<String>>,
        /// 收到的完整请求，按顺序。P8-3 的升级测试要断言 `reason`。
        seen: std::sync::Mutex<Vec<ApprovalRequest>>,
    }

    impl ApprovalGate for CountingGate {
        fn request<'a>(&'a self, request: &'a ApprovalRequest) -> BoxFuture<'a, Decision> {
            Box::pin(async move {
                self.requested
                    .lock()
                    .unwrap()
                    .push(request.call.name.clone());
                self.seen.lock().unwrap().push(request.clone());
                Ok(())
            })
        }
    }

    impl CountingGate {
        fn requested_tools(&self) -> Vec<String> {
            self.requested.lock().unwrap().clone()
        }

        /// 第一次收到的请求；没被问过就 panic。
        fn first_request(&self) -> ApprovalRequest {
            self.seen
                .lock()
                .unwrap()
                .first()
                .cloned()
                .expect("闸门应被问过")
        }
    }

    /// 恒否决的闸门，用来验证否决会变成拒绝观察而不是崩溃。
    struct DenyAllGate;

    impl ApprovalGate for DenyAllGate {
        fn request<'a>(&'a self, _request: &'a ApprovalRequest) -> BoxFuture<'a, Decision> {
            Box::pin(async { Err("不允许".to_owned()) })
        }
    }

    /// 一套组装好的 runner 加可断言的把手。
    struct Harness {
        runner: AgentRunner,
        /// mock 与 gateway 内部共享脚本和请求记录，所以能事后断言。
        mock: MockCompletionModel,
        evidence: Arc<MemoryEvidenceSink>,
        approvals: Arc<CountingGate>,
    }

    /// 用脚本化的模型回合组装 runner：不需要 API key，也不走网络。
    fn harness(turns: Vec<MockTurn>, tools: Vec<ToolSpec>) -> Harness {
        harness_with_policy(turns, tools, PolicyEngine::new())
    }

    /// 同上，但可指定策略引擎。
    fn harness_with_policy(
        turns: Vec<MockTurn>,
        tools: Vec<ToolSpec>,
        policy: PolicyEngine,
    ) -> Harness {
        let mock = MockCompletionModel::from_turns(turns);
        let evidence = Arc::new(MemoryEvidenceSink::new());
        let approvals = Arc::new(CountingGate::default());
        // 全部规格共用一个 RecordingInvoker：这些测试只关心循环行为，
        // 不关心具体工具做什么。
        let registry = registry_with(Arc::new(RecordingInvoker::default()), tools);
        let runner = AgentRunner::new(
            spec_with(),
            SessionId::new("test-session"),
            "读一下 Cargo.toml",
            AgentDependencies {
                model: Arc::new(RigChatGateway::new(mock.clone().erase())),
                tools: registry,
                approvals: approvals.clone(),
                evidence: evidence.clone(),
                clock: Arc::new(crate::infrastructure::clock::SystemClock),
                policy,
                roster: AgentRoster::builtin(),
                context_budget: ContextBudget::default(),
                session_store: None,
            },
        );
        Harness {
            runner,
            mock,
            evidence,
            approvals,
        }
    }

    /// 第 `index` 次请求的历史消息。
    fn history(mock: &MockCompletionModel, index: usize) -> Vec<rig::completion::Message> {
        mock.requests()[index]
            .chat_history
            .iter()
            .map(|message| (**message).clone())
            .collect()
    }

    /// 拼一个助手工具调用块，供"一轮多个调用"的测试用。
    ///
    /// `MockTurn::tool_call` 一次只能塞一个调用；多个调用要走
    /// `from_contents`，那就得自己拼 `AssistantContent`。
    fn call_content(id: &str, name: &str, arguments: serde_json::Value) -> AssistantContent {
        AssistantContent::tool_call(
            id,
            ToolName::new(name).expect("测试里的工具名应合法"),
            arguments,
        )
    }

    // ── 用例 1：能连续调用工具 ──────────────────────────────────────────

    #[tokio::test]
    async fn tool_call_then_final_answer_completes_in_two_requests() {
        let mut harness = harness(
            vec![
                MockTurn::tool_call("call_1", "read_file", json!({"path": "Cargo.toml"})),
                MockTurn::text("项目用了 rig、serde、tokio。"),
            ],
            vec![read_file_spec()],
        );

        let outcome = harness.runner.run(&Budget::new(20)).await.unwrap();

        assert!(outcome.answer.contains("rig"));
        assert_eq!(outcome.turns_used, 2);
        assert_eq!(harness.mock.request_count(), 2);
    }

    // ── 用例 2：达到最大轮数要停，且错误里带轮数 ────────────────────────

    #[tokio::test]
    async fn max_turns_stops_the_run_and_names_the_limit() {
        let mut harness = harness(
            vec![
                MockTurn::tool_call("call_1", "read_file", json!({"path": "a"})),
                MockTurn::tool_call("call_2", "read_file", json!({"path": "b"})),
                MockTurn::tool_call("call_3", "read_file", json!({"path": "c"})),
                MockTurn::text("完"),
            ],
            vec![read_file_spec()],
        );

        let error = harness.runner.run(&Budget::new(3)).await.unwrap_err();

        assert!(matches!(
            error,
            ApplicationError::BudgetExhausted { max_turns: 3 }
        ));
        assert!(error.to_string().contains('3'));
        // 三轮都问过了，第四回合的文本没有被消费。
        assert_eq!(harness.mock.request_count(), 3);
    }

    // ── 用例 3：错误观察能回填，模型可恢复 ──────────────────────────────

    #[tokio::test]
    async fn unregistered_tool_yields_error_observation() {
        let mut harness = harness(
            vec![
                MockTurn::tool_call("call_1", "rm_rf", json!({})),
                MockTurn::text("工具不存在，我换个法子"),
            ],
            vec![read_file_spec()],
        );

        harness.runner.run(&Budget::new(20)).await.unwrap();

        // 第二次请求的历史里，最后一条是带 is_error 的 tool 结果。
        let messages = history(&harness.mock, 1);
        let Message::User { content } = messages.last().unwrap() else {
            panic!("观察结果应当是 user 消息");
        };
        let Some(UserContent::ToolResult(result)) = content.first() else {
            panic!("应当是 tool 结果内容");
        };
        assert!(result.is_error);
        assert!(result.content[0].as_text().unwrap().contains("rm_rf"));
    }

    // ── 用例 4：不需要工具时不多余调用 ──────────────────────────────────

    #[tokio::test]
    async fn plain_answer_makes_exactly_one_request() {
        let mut harness = harness(
            vec![MockTurn::text("不需要工具，直接回答。")],
            vec![read_file_spec()],
        );

        let outcome = harness.runner.run(&Budget::new(20)).await.unwrap();

        assert_eq!(outcome.answer, "不需要工具，直接回答。");
        assert_eq!(harness.mock.request_count(), 1);
    }

    // ── 用例 5：消息协议配对 ───────────────────────────────────────────

    #[tokio::test]
    async fn second_request_history_is_properly_paired() {
        let mut harness = harness(
            vec![
                MockTurn::tool_call("call_1", "read_file", json!({"path": "Cargo.toml"})),
                MockTurn::text("看完了"),
            ],
            vec![read_file_spec()],
        );

        harness.runner.run(&Budget::new(20)).await.unwrap();

        let messages = history(&harness.mock, 1);
        assert_eq!(messages.len(), 4);
        assert!(
            matches!(&messages[0], Message::System { content } if content == "系统提示"),
            "首条应当是系统提示，实际是 {:?}",
            messages[0]
        );
        assert!(
            matches!(&messages[1], Message::User { .. }),
            "第二条应当是用户任务"
        );
        match &messages[2] {
            Message::Assistant(assistant) => {
                assert_eq!(
                    assistant.tool_calls().count(),
                    1,
                    "助手消息应含一次工具调用"
                );
            }
            other => panic!("第三条应当是助手消息，实际是 {other:?}"),
        }
        match &messages[3] {
            Message::User { content } => {
                assert!(
                    matches!(content.first(), Some(UserContent::ToolResult(_))),
                    "第四条应当是 tool 结果"
                );
            }
            other => panic!("第四条应当是 tool 结果，实际是 {other:?}"),
        }
    }

    // ── 用例 6：审批接缝存在 ───────────────────────────────────────────

    #[tokio::test]
    async fn destructive_tool_goes_through_the_approval_gate() {
        let mut harness = harness(
            vec![
                MockTurn::tool_call(
                    "call_1",
                    "write_file",
                    json!({"path": "out.txt", "content": "hi"}),
                ),
                MockTurn::text("写好了"),
            ],
            vec![read_file_spec(), write_file_spec()],
        );

        harness.runner.run(&Budget::new(20)).await.unwrap();

        assert_eq!(harness.approvals.requested_tools(), vec!["write_file"]);
    }

    #[tokio::test]
    async fn read_only_tool_skips_the_approval_gate() {
        let mut harness = harness(
            vec![
                MockTurn::tool_call("call_1", "read_file", json!({"path": "Cargo.toml"})),
                MockTurn::text("读完了"),
            ],
            vec![read_file_spec()],
        );

        harness.runner.run(&Budget::new(20)).await.unwrap();

        assert!(harness.approvals.requested_tools().is_empty());
    }

    // ── 用例 7：证据接缝存在 ───────────────────────────────────────────

    #[tokio::test]
    async fn every_tool_call_leaves_an_audit_trail() {
        let mut harness = harness(
            vec![
                MockTurn::tool_call("call_1", "read_file", json!({"path": "a"})),
                MockTurn::tool_call("call_2", "read_file", json!({"path": "b"})),
                MockTurn::text("两行都读完了"),
            ],
            vec![read_file_spec()],
        );

        harness.runner.run(&Budget::new(20)).await.unwrap();

        let events = harness.evidence.events();
        let requested = events
            .iter()
            .filter(|event| matches!(event.event, AuditEvent::ToolRequested { .. }))
            .count();
        let observed = events
            .iter()
            .filter(|event| matches!(event.event, AuditEvent::ToolObserved { .. }))
            .count();
        assert_eq!(requested, 2, "每次调用都应有一条 ToolRequested");
        assert_eq!(observed, 2, "每次调用都应有一条 ToolObserved");
        // 序号严格递增，从 1 开始。
        assert_eq!(events[0].sequence, 1);
        assert!(
            events
                .windows(2)
                .all(|pair| pair[1].sequence > pair[0].sequence)
        );
    }

    // ── 一轮多个调用（P9-4 parallel_tool_calls）────────────────────────

    /// 一次回复里带多个工具调用时，全部都要执行并各回一条观察。
    ///
    /// P9-4 起请求显式带 `parallel_tool_calls(true)`（见
    /// `infrastructure::model::rig_chat::generation_options`），模型于是会
    /// 在一轮里给多个调用。这里守住三件事：一条不漏、顺序不乱、各配各的
    /// 观察——错位配对会让模型把 A 的结果当成 B 的，后面整个任务都建在
    /// 错配上，而表面上一切正常。
    #[tokio::test]
    async fn multiple_calls_in_one_reply_are_all_observed_in_order() {
        let mut harness = harness(
            vec![
                MockTurn::from_contents([
                    call_content("call_1", "read_file", json!({"path": "Cargo.toml"})),
                    call_content(
                        "call_2",
                        "write_file",
                        json!({"path": "out.txt", "content": "x"}),
                    ),
                    call_content("call_3", "read_file", json!({"path": "b.txt"})),
                ]),
                MockTurn::text("三个都处理完了"),
            ],
            vec![read_file_spec(), write_file_spec()],
        );

        let outcome = harness.runner.run(&Budget::new(20)).await.unwrap();

        assert!(outcome.answer.contains("三个都处理完了"));
        // 三条调用只花一次模型往返：轮数不随调用数上涨，这是 P9-4 要的账。
        assert_eq!(outcome.turns_used, 2);
        assert_eq!(harness.mock.request_count(), 2);

        let requested: Vec<String> = harness
            .evidence
            .events()
            .iter()
            .filter_map(|event| match &event.event {
                AuditEvent::ToolRequested { call, .. } => Some(call.name.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            requested,
            vec!["read_file", "write_file", "read_file"],
            "三条调用都要留下请求记录，且顺序与回复一致"
        );

        let observed: Vec<String> = harness
            .evidence
            .events()
            .iter()
            .filter_map(|event| match &event.event {
                AuditEvent::ToolObserved { call, .. } => Some(call.name.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            observed, requested,
            "每条调用都要配到自己的观察，不能错位"
        );

        // 审批只问写工具：两个只读调用照常跳过闸门。
        assert_eq!(harness.approvals.requested_tools(), vec!["write_file"]);
    }

    /// 一轮多个调用时，历史里仍是"一条助手消息 + 一组 tool 结果"。
    ///
    /// OpenAI 兼容端点要求同一轮的所有 tool 结果跟在同一条助手消息之后；
    /// 拆成多组会让请求直接 400。这条守住的是翻译层的配对形状。
    #[tokio::test]
    async fn one_reply_with_many_calls_produces_one_paired_history_block() {
        let mut harness = harness(
            vec![
                MockTurn::from_contents([
                    call_content("call_1", "read_file", json!({"path": "Cargo.toml"})),
                    call_content("call_2", "read_file", json!({"path": "b.txt"})),
                ]),
                MockTurn::text("两行都读完了"),
            ],
            vec![read_file_spec()],
        );

        harness.runner.run(&Budget::new(20)).await.unwrap();

        let messages = history(&harness.mock, 1);
        assert_eq!(messages.len(), 4, "系统 + 任务 + 助手 + 一组观察");
        let Message::Assistant(assistant) = &messages[2] else {
            panic!("第三条应当是助手消息，实际是 {:?}", messages[2]);
        };
        assert_eq!(assistant.tool_calls().count(), 2, "助手消息应含两次调用");
        let Message::User { content } = &messages[3] else {
            panic!("第四条应当是 tool 结果，实际是 {:?}", messages[3]);
        };
        assert_eq!(content.len(), 2, "两条结果应在同一组里");
    }

    // ── 审批否决不是崩溃 ───────────────────────────────────────────────

    #[tokio::test]
    async fn denied_tool_becomes_a_refused_observation() {
        let mock = MockCompletionModel::from_turns(vec![
            MockTurn::tool_call(
                "call_1",
                "write_file",
                json!({"path": "out.txt", "content": "hi"}),
            ),
            MockTurn::text("不让写，那我只汇报"),
        ]);
        let mut runner = AgentRunner::new(
            spec_with(),
            SessionId::new("test-session"),
            "写个文件",
            AgentDependencies {
                model: Arc::new(RigChatGateway::new(mock.clone().erase())),
                tools: registry_with(
                    Arc::new(RecordingInvoker::default()),
                    vec![read_file_spec(), write_file_spec()],
                ),
                approvals: Arc::new(DenyAllGate),
                evidence: Arc::new(MemoryEvidenceSink::new()),
                clock: Arc::new(crate::infrastructure::clock::SystemClock),
                policy: PolicyEngine::new(),
                roster: AgentRoster::builtin(),
                context_budget: ContextBudget::default(),
                session_store: None,
            },
        );

        let outcome = runner.run(&Budget::new(20)).await.unwrap();

        assert_eq!(outcome.answer, "不让写，那我只汇报");
        let messages = history(&mock, 1);
        let Message::User { content } = messages.last().unwrap() else {
            panic!("观察结果应当是 user 消息");
        };
        let Some(UserContent::ToolResult(result)) = content.first() else {
            panic!("应当是 tool 结果内容");
        };
        assert!(result.is_error);
        assert!(result.content[0].as_text().unwrap().contains("不允许"));
    }

    // ── 空回复要报错，不能当成成功 ─────────────────────────────────────

    #[tokio::test]
    async fn empty_reply_is_retried_once_and_recovers() {
        // 第一次空回复是瞬态故障：重试同一请求应拿到后面的脚本位。
        let mut harness = harness(
            vec![
                MockTurn::from_contents(Vec::<AssistantContent>::new()),
                MockTurn::text("重试后想清楚了"),
            ],
            vec![read_file_spec()],
        );

        let outcome = harness.runner.run(&Budget::new(20)).await.unwrap();

        assert_eq!(outcome.answer, "重试后想清楚了");
        assert_eq!(harness.mock.request_count(), 2, "空回复触发了一次重试");
        // 重试前的空回复不算消耗轮。
        assert_eq!(outcome.turns_used, 1);
    }

    #[tokio::test]
    async fn empty_reply_twice_is_an_error() {
        let mut harness = harness(
            vec![
                MockTurn::from_contents(Vec::<AssistantContent>::new()),
                MockTurn::from_contents(Vec::<AssistantContent>::new()),
            ],
            vec![read_file_spec()],
        );

        let error = harness.runner.run(&Budget::new(20)).await.unwrap_err();

        assert!(matches!(error, ApplicationError::EmptyResponse));
    }

    // ── 会话与状态机可观测 ─────────────────────────────────────────────

    #[tokio::test]
    async fn session_history_pairs_every_round() {
        let mut harness = harness(
            vec![
                MockTurn::tool_call("call_1", "read_file", json!({"path": "a"})),
                MockTurn::tool_call("call_2", "read_file", json!({"path": "b"})),
                MockTurn::text("读完了"),
            ],
            vec![read_file_spec()],
        );

        let outcome = harness.runner.run(&Budget::new(20)).await.unwrap();
        let outcome_turns = outcome.turns_used;

        assert_eq!(outcome_turns, 3);
        // 会话 ID 由调用方给定，runner 原样使用、不改写。
        assert_eq!(harness.runner.session_id().as_str(), "test-session");

        // 协议层验证配对：最后一个请求的历史里，每个带 tool_calls 的
        // 助手消息后面必须紧跟一条 tool 结果消息，否则 OpenAI 兼容端点会 400。
        let messages = history(&harness.mock, harness.mock.request_count() - 1);
        let assistant_with_calls = messages
            .iter()
            .filter(|message| match message {
                Message::Assistant(assistant) => assistant.tool_calls().count() > 0,
                _ => false,
            })
            .count();
        assert_eq!(assistant_with_calls, 2, "两轮各应有一条带调用的助手消息");
        for (index, message) in messages.iter().enumerate() {
            let has_calls = match message {
                Message::Assistant(assistant) => assistant.tool_calls().count() > 0,
                _ => false,
            };
            if has_calls {
                assert!(
                    matches!(
                        messages.get(index + 1),
                        Some(Message::User { content })
                            if matches!(content.first(), Some(UserContent::ToolResult(_)))
                    ),
                    "第 {index} 条的助手消息没有紧跟 tool 结果"
                );
            }
        }
    }

    #[tokio::test]
    async fn state_machine_walks_through_every_phase() {
        let mut harness = harness(
            vec![
                MockTurn::tool_call("call_1", "read_file", json!({"path": "a"})),
                MockTurn::text("读完了"),
            ],
            vec![read_file_spec()],
        );

        assert!(matches!(
            harness.runner.step().await.unwrap(),
            RunState::Evaluating(_)
        ));
        assert!(matches!(
            harness.runner.step().await.unwrap(),
            RunState::Dispatching(_)
        ));
        assert!(matches!(
            harness.runner.step().await.unwrap(),
            RunState::AwaitingModel
        ));
        assert!(matches!(
            harness.runner.step().await.unwrap(),
            RunState::Completed(_)
        ));
    }

    // ── 以下是不依赖 mock 模型的单元测试 ───────────────────────────────

    #[test]
    fn unregistered_tool_message_lists_available_tools() {
        let message = unregistered_tool_message("rm_rf", &[read_file_spec()]);
        assert!(message.contains("rm_rf"));
        assert!(message.contains("read_file"));
    }

    #[test]
    fn registry_finds_spec_by_name() {
        let registry = registry_with(
            Arc::new(RecordingInvoker::default()),
            vec![read_file_spec(), write_file_spec()],
        );
        assert!(registry.find("read_file").is_some());
        assert!(registry.find("list_dir").is_none());
        assert_eq!(registry.specs().len(), 2);
    }

    #[test]
    fn read_only_tools_need_no_approval() {
        assert!(!read_file_spec().annotations.requires_approval());
        assert!(write_file_spec().annotations.requires_approval());
    }

    #[test]
    fn refused_outcome_is_an_error_for_the_model() {
        let outcome = ToolOutcome::refused("不行");
        assert_eq!(outcome.status, OutcomeStatus::Refused);
        assert!(outcome.is_error());
    }

    #[tokio::test]
    async fn invoker_receives_the_call() {
        let invoker = Arc::new(RecordingInvoker::default());
        let call = ToolCall {
            id: CallId::new("call_1"),
            name: "read_file".to_owned(),
            arguments: json!({"path": "Cargo.toml"}),
        };
        let outcome = invoker.invoke(&call).await;

        assert_eq!(outcome.status, OutcomeStatus::Success);
        assert_eq!(invoker.calls.lock().unwrap().as_slice(), ["read_file"]);
    }

    #[tokio::test]
    async fn deny_all_gate_denies_everything() {
        let gate = DenyAllGate;
        let request = ApprovalRequest {
            turn: 1,
            call: ToolCall {
                id: CallId::new("call_1"),
                name: "write_file".to_owned(),
                arguments: json!({}),
            },
            annotations: write_file_spec().annotations,
            reason: None,
            resumed: false,
        };

        assert_eq!(gate.request(&request).await, Err("不允许".to_owned()));
    }

    #[tokio::test]
    async fn auto_approve_gate_allows_everything() {
        let gate = crate::infrastructure::approval::auto::AutoApproveGate;
        let request = ApprovalRequest {
            turn: 1,
            call: ToolCall {
                id: CallId::new("call_1"),
                name: "write_file".to_owned(),
                arguments: json!({}),
            },
            annotations: write_file_spec().annotations,
            reason: None,
            resumed: false,
        };

        assert_eq!(gate.request(&request).await, Ok(()));
    }

    #[test]
    fn session_id_is_unique_within_a_process() {
        let clock = crate::infrastructure::clock::SystemClock;
        // 同一进程内连取两个 ID：纳秒精度足够时不会相同。
        assert_ne!(
            new_session_id(&clock).as_str(),
            new_session_id(&clock).as_str()
        );
    }

    #[test]
    fn token_usage_defaults_to_zero() {
        assert_eq!(TokenUsage::default().input_tokens, 0);
        assert_eq!(TokenUsage::default().output_tokens, 0);
    }

    // ── P1：策略引擎接入 ────────────────────────────────────────────────

    /// 拒绝观察的内容。
    fn refusal_text(mock: &MockCompletionModel, index: usize) -> String {
        let messages = history(mock, index);
        let Message::User { content } = messages.last().unwrap() else {
            panic!("观察结果应当是 user 消息");
        };
        let Some(UserContent::ToolResult(result)) = content.first() else {
            panic!("应当是 tool 结果内容");
        };
        result.content[0].as_text().unwrap().to_owned()
    }

    #[tokio::test]
    async fn default_policy_still_gates_write_file() {
        // P0 行为不能变：默认策略下 write_file 仍要过闸门。
        let mut harness = harness(
            vec![
                MockTurn::tool_call("call_1", "write_file", json!({"path": "a.txt"})),
                MockTurn::text("写完了"),
            ],
            vec![read_file_spec(), write_file_spec()],
        );

        harness.runner.run(&Budget::new(20)).await.unwrap();

        assert_eq!(harness.approvals.requested_tools(), vec!["write_file"]);
    }

    /// 构造只含一条规则的引擎，测试里配策略用。
    fn policy_with(tool: &str, verdict: PolicyVerdict, description: &str) -> PolicyEngine {
        PolicyEngine::with_rules(vec![crate::domain::policy::PolicyRule {
            tool: Some(tool.to_owned()),
            scope: None,
            verdict,
            description: description.to_owned(),
        }])
    }

    #[tokio::test]
    async fn allow_rule_skips_the_gate_entirely() {
        let policy = policy_with("write_file", PolicyVerdict::Allow, "测试信任目录，免审");
        let mut harness = harness_with_policy(
            vec![
                MockTurn::tool_call("call_1", "write_file", json!({"path": "a.txt"})),
                MockTurn::text("写完了"),
            ],
            vec![read_file_spec(), write_file_spec()],
            policy,
        );

        harness.runner.run(&Budget::new(20)).await.unwrap();

        assert!(
            harness.approvals.requested_tools().is_empty(),
            "Allow 规则不应问闸门"
        );
    }

    #[tokio::test]
    async fn default_policy_gates_run_command() {
        // P2 验收点：新加的最危险工具 run_command（open_world）没有在
        // 策略引擎里登记任何规则，默认策略按注解自动把它送进闸门——
        // "加高危工具不需要改策略引擎"正是 P1 接缝设计的验收点。
        //
        // 命令刻意避开 P9-5 默认基线放行的那几条（cargo test / build、
        // git status / diff）：验收点是"基线之外的命令仍要人点头"，
        // 拿 `cargo test` 当例子会让它测不到任何东西。
        let run_command_spec = ToolSpec {
            name: "run_command".to_owned(),
            description: "跑命令".to_owned(),
            parameters: json!({"type": "object"}),
            annotations: ToolAnnotations::new(false, true, false, true),
        };
        let mut harness = harness(
            vec![
                MockTurn::tool_call("call_1", "run_command", json!({"command": "rm -rf target"})),
                MockTurn::text("跑完了"),
            ],
            vec![read_file_spec(), run_command_spec],
        );

        harness.runner.run(&Budget::new(20)).await.unwrap();

        assert_eq!(
            harness.approvals.requested_tools(),
            vec!["run_command"],
            "默认策略下 run_command 必须过审批闸门"
        );
    }

    /// 默认基线放行的命令确实不问人——这是 P9-5 省下的那次点击。
    ///
    /// 与上一条配对：一条守住"高危仍要审"，这一条守住"无害的别再审"。
    /// 只测前者的话，将来有人把基线删了，测试全绿而等待成本悄悄回去。
    #[tokio::test]
    async fn baseline_commands_skip_the_approval_gate() {
        let run_command_spec = ToolSpec {
            name: "run_command".to_owned(),
            description: "跑命令".to_owned(),
            parameters: json!({"type": "object"}),
            annotations: ToolAnnotations::new(false, true, false, true),
        };
        let mut harness = harness(
            vec![
                MockTurn::tool_call("call_1", "run_command", json!({"command": "cargo test"})),
                MockTurn::text("跑完了"),
            ],
            vec![read_file_spec(), run_command_spec],
        );

        harness.runner.run(&Budget::new(20)).await.unwrap();

        assert!(
            harness.approvals.requested_tools().is_empty(),
            "cargo test 在默认基线里，不该再问人"
        );
    }

    #[tokio::test]
    async fn deny_rule_refuses_without_asking_the_gate() {
        let policy = policy_with("write_file", PolicyVerdict::Deny, "P1 禁止写文件");
        let mut harness = harness_with_policy(
            vec![
                MockTurn::tool_call("call_1", "write_file", json!({"path": "a.txt"})),
                MockTurn::text("不让写，那我只汇报"),
            ],
            vec![read_file_spec(), write_file_spec()],
            policy,
        );

        let outcome = harness.runner.run(&Budget::new(20)).await.unwrap();

        assert_eq!(outcome.answer, "不让写，那我只汇报");
        assert!(
            harness.approvals.requested_tools().is_empty(),
            "Deny 规则不该问闸门"
        );
        // 拒绝观察要说明是哪条规则、为什么，模型才知道换路。
        let text = refusal_text(&harness.mock, 1);
        assert!(text.contains("write_file"), "应点名工具: {text}");
        assert!(text.contains("策略规则 #0"), "应指明规则序号: {text}");
        assert!(text.contains("P1 禁止写文件"), "应带上规则理由: {text}");
    }

    #[tokio::test]
    async fn require_approval_rule_gates_a_read_only_tool() {
        // 反过来也成立：只读工具也能被规则要求审批。
        let policy = policy_with("read_file", PolicyVerdict::RequireApproval, "敏感目录要审");
        let mut harness = harness_with_policy(
            vec![
                MockTurn::tool_call("call_1", "read_file", json!({"path": "secret"})),
                MockTurn::text("看完了"),
            ],
            vec![read_file_spec()],
            policy,
        );

        harness.runner.run(&Budget::new(20)).await.unwrap();

        assert_eq!(harness.approvals.requested_tools(), vec!["read_file"]);
    }

    #[tokio::test]
    async fn every_policy_verdict_leaves_an_audit_event() {
        let policy = policy_with("read_file", PolicyVerdict::Deny, "禁读");
        let mut harness = harness_with_policy(
            vec![
                MockTurn::tool_call("call_1", "read_file", json!({"path": "a"})),
                MockTurn::text("完"),
            ],
            vec![read_file_spec()],
            policy,
        );

        harness.runner.run(&Budget::new(20)).await.unwrap();

        let evaluated: Vec<_> = harness
            .evidence
            .events()
            .into_iter()
            .filter(|e| matches!(e.event, AuditEvent::PolicyEvaluated { .. }))
            .collect();
        assert_eq!(evaluated.len(), 1, "每次策略裁决都该留证");
    }

    // ── P8-3：越界写入升级 ─────────────────────────────────────────────

    /// 策略放行了 `run_command`，但命令把输出写到 workspace 外时，
    /// 必须升级成要审批。
    ///
    /// 这是 P8-3 的核心承诺：策略按"工具名 + 参数文本"放行之后，
    /// 命令内容就不受任何约束了，补的正是这个缺口。
    #[tokio::test]
    async fn allow_rule_still_gates_an_out_of_workspace_write() {
        let policy = policy_with("run_command", PolicyVerdict::Allow, "shell 免审");
        let run_command_spec = ToolSpec {
            name: "run_command".to_owned(),
            description: "跑命令".to_owned(),
            parameters: json!({"type": "object"}),
            annotations: ToolAnnotations::new(false, true, false, true),
        };
        let mut harness = harness_with_policy(
            vec![
                MockTurn::tool_call(
                    "call_1",
                    "run_command",
                    json!({"command": r"echo hi > C:\Windows\temp\x"}),
                ),
                MockTurn::text("跑完了"),
            ],
            vec![run_command_spec],
            policy,
        );

        harness.runner.run(&Budget::new(20)).await.unwrap();

        assert_eq!(
            harness.approvals.requested_tools(),
            vec!["run_command"],
            "越界写入必须被升级为要审批"
        );
        // 升级理由要递到闸门：用户得知道为什么要他看这一眼。
        let request = harness.approvals.first_request();
        let reason = request.reason.expect("升级请求必须带 reason");
        assert!(reason.contains(r"C:\Windows\temp\x"), "理由应点名目标: {reason}");
    }

    /// 同样的 Allow 规则下，落在 workspace 内的命令不被升级。
    ///
    /// 没有这一条，"升级"就会退化成"所有 run_command 都要审"——
    /// 用户被无谓地打扰几次之后就会一律放行，升级也就白做了。
    #[tokio::test]
    async fn allow_rule_does_not_gate_an_in_workspace_command() {
        let policy = policy_with("run_command", PolicyVerdict::Allow, "shell 免审");
        let run_command_spec = ToolSpec {
            name: "run_command".to_owned(),
            description: "跑命令".to_owned(),
            parameters: json!({"type": "object"}),
            annotations: ToolAnnotations::new(false, true, false, true),
        };
        let mut harness = harness_with_policy(
            vec![
                MockTurn::tool_call("call_1", "run_command", json!({"command": "cargo test"})),
                MockTurn::text("跑完了"),
            ],
            vec![run_command_spec],
            policy,
        );

        harness.runner.run(&Budget::new(20)).await.unwrap();

        assert!(
            harness.approvals.requested_tools().is_empty(),
            "workspace 内的命令不该被升级"
        );
    }

    /// 默认策略（P9-5 的内置基线）放行 `git diff *` 一族，但这条命令
    /// 用 git 自己的 `--output` 选项把 workspace 的内容写到外面。
    ///
    /// 它既没有写入型动词也没有 `>`，按 P8-3 原来的两个判据整个漏掉；
    /// 而基线又刚刚把它放行，两道防线同时失守。本机实测
    /// `git diff --output=D:\x` 会真的落盘，所以这不是理论风险。
    #[tokio::test]
    async fn default_baseline_still_gates_a_diff_that_writes_outside() {
        let run_command_spec = ToolSpec {
            name: "run_command".to_owned(),
            description: "跑命令".to_owned(),
            parameters: json!({"type": "object"}),
            annotations: ToolAnnotations::new(false, true, false, true),
        };
        let mut harness = harness_with_policy(
            vec![
                MockTurn::tool_call(
                    "call_1",
                    "run_command",
                    json!({"command": r"git diff --output=C:\Windows\temp\x"}),
                ),
                MockTurn::text("跑完了"),
            ],
            vec![run_command_spec],
            PolicyEngine::new(),
        );

        harness.runner.run(&Budget::new(20)).await.unwrap();

        assert_eq!(
            harness.approvals.requested_tools(),
            vec!["run_command"],
            "基线放行的命令族也不能把内容写到 workspace 外而无人过目"
        );
        let reason = harness
            .approvals
            .first_request()
            .reason
            .expect("升级请求必须带 reason");
        assert!(
            reason.contains(r"C:\Windows\temp\x"),
            "理由应点名越界目标: {reason}"
        );
    }

    /// 审计里记的仍是策略的原判（Allow），不是升级后的裁决。
    ///
    /// 策略没改，改的是"这次调用要不要人过目"。两件事混在一条
    /// `PolicyEvaluated` 里，事后就分不清是配置放宽了还是启发式报警了。
    #[tokio::test]
    async fn escalated_call_still_records_the_original_policy_verdict() {
        let policy = policy_with("run_command", PolicyVerdict::Allow, "shell 免审");
        let run_command_spec = ToolSpec {
            name: "run_command".to_owned(),
            description: "跑命令".to_owned(),
            parameters: json!({"type": "object"}),
            annotations: ToolAnnotations::new(false, true, false, true),
        };
        let mut harness = harness_with_policy(
            vec![
                MockTurn::tool_call(
                    "call_1",
                    "run_command",
                    json!({"command": r"del ..\outside"}),
                ),
                MockTurn::text("跑完了"),
            ],
            vec![run_command_spec],
            policy,
        );

        harness.runner.run(&Budget::new(20)).await.unwrap();

        let evaluated: Vec<_> = harness
            .evidence
            .events()
            .into_iter()
            .filter_map(|e| match e.event {
                AuditEvent::PolicyEvaluated { verdict, .. } => Some(verdict),
                _ => None,
            })
            .collect();
        assert_eq!(evaluated, vec![PolicyVerdict::Allow], "证据应记策略原判");
    }

    // ── P8-1：受保护路径经真实工具层的端到端验证 ─────────────────────

    /// 策略放行 `write_file` 也写不进 `.git/config`。
    ///
    /// 单独一条工具层测试只能证明"工具自己会拒"；这一条把策略引擎、runner
    /// 与真实文件工具串起来，证明保护是**结构性的**——不管配置怎么写，
    /// 调用都到不了文件系统。这是 P8-1"保护不留给配置去放宽"那句话的
    /// 直接对应物。
    #[tokio::test]
    async fn allow_policy_cannot_write_into_a_protected_path() {
        // 造一个真的 .git/config：拒绝必须发生在文件系统之前，
        // 不能让"文件不存在"成为一种假通过。
        let dir = std::env::temp_dir().join(format!("qch-e2e-protected-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".git")).expect("建 .git");
        std::fs::write(dir.join(".git/config"), "[core]\n").expect("写 .git/config");

        let policy = policy_with("write_file", PolicyVerdict::Allow, "写入一律免审");
        let mock = MockCompletionModel::from_turns(vec![
            MockTurn::tool_call(
                "call_1",
                "write_file",
                json!({"path": ".git/config", "content": "恶意覆盖"}),
            ),
            MockTurn::text("写完了"),
        ]);
        let approvals = Arc::new(CountingGate::default());
        let evidence = Arc::new(MemoryEvidenceSink::new());
        let mut runner = AgentRunner::new(
            spec_with(),
            SessionId::new("test-session"),
            "改一下 git 配置",
            AgentDependencies {
                model: Arc::new(RigChatGateway::new(mock.clone().erase())),
                tools: ToolRegistry::new(crate::infrastructure::tool::all_tools(
                    crate::domain::workspace::WorkspaceRoot::new(&dir),
                )),
                approvals: approvals.clone(),
                evidence: evidence.clone(),
                clock: Arc::new(crate::infrastructure::clock::SystemClock),
                policy,
                roster: AgentRoster::builtin(),
                context_budget: ContextBudget::default(),
                session_store: None,
            },
        );

        // 断言全在证据、文件内容与闸门上；答案本身与保护无关，
        // 但 run() 必须正常收尾（没被保护拒绝带崩）才算通过。
        let _outcome = runner.run(&Budget::new(20)).await.unwrap();

        // 模型看到的是一条拒绝观察，不是成功——它必须知道自己被拦了，
        // 否则它会基于"已写入"继续往下推理。
        let observed: Vec<String> = evidence
            .events()
            .into_iter()
            .filter_map(|entry| match entry.event {
                AuditEvent::ToolObserved { outcome, .. } => Some(outcome.content),
                _ => None,
            })
            .collect();
        assert!(
            observed.iter().any(|text| text.contains("受保护")),
            "模型应看到受保护拒绝: {observed:?}"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join(".git/config")).unwrap(),
            "[core]\n",
            "受保护文件必须保持原样"
        );
        // 免审策略下闸门本来就不会被问；这里断言的是"没有退化成要审批"——
        // 保护是拒绝，不是升级。
        assert!(
            approvals.requested_tools().is_empty(),
            "受保护路径应直接拒绝，不该绕道审批"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── P3：上下文预算与历史裁剪 ────────────────────────────────────────

    /// 观察内容随调用参数变化的执行器：裁剪测试要能区分"第几轮的观察"。
    struct EchoInvoker;

    impl ToolInvoker for EchoInvoker {
        fn invoke<'a>(&'a self, call: &'a ToolCall) -> BoxFuture<'a, ToolOutcome> {
            Box::pin(async move {
                let path = call
                    .arguments
                    .get("path")
                    .and_then(|value| value.as_str())
                    .unwrap_or("?");
                ToolOutcome::success(format!("OBSERVATION_FOR_{path}"))
            })
        }
    }

    #[tokio::test]
    async fn context_budget_drops_old_observations_from_requests_but_not_from_session() {
        let mut turns = Vec::new();
        for index in 1..=6 {
            turns.push(MockTurn::tool_call(
                format!("call_{index}"),
                "read_file",
                json!({"path": format!("r{index}.txt")}),
            ));
        }
        turns.push(MockTurn::text("六轮都看完了"));
        let mock = MockCompletionModel::from_turns(turns);
        let evidence = Arc::new(MemoryEvidenceSink::new());
        let mut runner = AgentRunner::new(
            spec_with(),
            SessionId::new("test-session"),
            "读六个文件",
            AgentDependencies {
                model: Arc::new(RigChatGateway::new(mock.clone().erase())),
                tools: registry_with(Arc::new(EchoInvoker), vec![read_file_spec()]),
                approvals: Arc::new(CountingGate::default()),
                evidence: evidence.clone(),
                clock: Arc::new(crate::infrastructure::clock::SystemClock),
                policy: PolicyEngine::new(),
                roster: AgentRoster::builtin(),
                context_budget: ContextBudget {
                    max_tokens: Some(1),
                    min_recent_rounds: 2,
                    keep_task: true,
                    summarize: None,
                },
                session_store: None,
            },
        );

        runner.run(&Budget::new(20)).await.unwrap();

        // 最后一次请求：任务在、占位说明在，只看得到第 5、6 轮的观察。
        let rendered = serde_json::to_string(&history(&mock, mock.request_count() - 1)).unwrap();
        assert!(rendered.contains("读六个文件"), "任务消息应保留: {rendered}");
        assert!(rendered.contains("已省略"), "应有占位说明: {rendered}");
        assert!(rendered.contains("OBSERVATION_FOR_r5.txt"), "{rendered}");
        assert!(rendered.contains("OBSERVATION_FOR_r6.txt"), "{rendered}");
        assert!(
            !rendered.contains("OBSERVATION_FOR_r1.txt"),
            "第 1 轮的观察不应出现在请求里: {rendered}"
        );
        assert!(
            !rendered.contains("OBSERVATION_FOR_r4.txt"),
            "第 4 轮的观察也不应出现: {rendered}"
        );

        // Session 是全量真相：6 轮工具调用与观察一个不少
        // （最终答案走 RunState::Completed，不进历史——P0 起的行为）。
        assert_eq!(runner.session().rounds(), 6);
        let observations = runner
            .session()
            .turns()
            .iter()
            .filter(|turn| matches!(turn, crate::domain::session::Turn::Observation(_)))
            .count();
        assert_eq!(observations, 6);

        // 裁剪要留痕：证据里能对出每轮丢了哪些历史。
        let compacted: Vec<_> = evidence
            .events()
            .into_iter()
            .filter(|event| {
                matches!(
                    &event.event,
                    AuditEvent::ContextCompacted { dropped, .. } if dropped.as_slice() == [1, 2, 3, 4]
                )
            })
            .collect();
        assert_eq!(compacted.len(), 1, "超预算的请求各留一条裁剪证据");
    }

    // ── P4-3：历史摘要 ──────────────────────────────────────────────────

    /// 摘要测试的公共装配：3 轮工具调用 + 最终答案，预算只留 2 轮。
    fn summary_test_runner(
        mock: &MockCompletionModel,
        context_budget: ContextBudget,
    ) -> (AgentRunner, Arc<MemoryEvidenceSink>) {
        let evidence = Arc::new(MemoryEvidenceSink::new());
        let runner = AgentRunner::new(
            spec_with(),
            SessionId::new("summary-session"),
            "读三个文件",
            AgentDependencies {
                model: Arc::new(RigChatGateway::new(mock.clone().erase())),
                tools: registry_with(Arc::new(EchoInvoker), vec![read_file_spec()]),
                approvals: Arc::new(CountingGate::default()),
                evidence: evidence.clone(),
                clock: Arc::new(crate::infrastructure::clock::SystemClock),
                policy: PolicyEngine::new(),
                roster: AgentRoster::builtin(),
                context_budget,
                session_store: None,
            },
        );
        (runner, evidence)
    }

    #[tokio::test]
    async fn summary_is_generated_once_and_replaces_the_placeholder() {
        // 脚本：3 轮工具调用后历史 3 > 预算 2，第 4 问触发裁剪（丢第 1 轮）
        // → 摘要调用① → r4 工具调用；第 5 问丢第 1-2 轮，现有摘要（第 1 轮）
        // 没盖全 → 摘要调用②（增量合并）→ 最终答案。
        let mock = MockCompletionModel::from_turns(vec![
            MockTurn::tool_call("call_1", "read_file", json!({"path": "r1.txt"})),
            MockTurn::tool_call("call_2", "read_file", json!({"path": "r2.txt"})),
            MockTurn::tool_call("call_3", "read_file", json!({"path": "r3.txt"})),
            MockTurn::text("摘要：第 1 轮读了 r1"),
            MockTurn::tool_call("call_4", "read_file", json!({"path": "r4.txt"})),
            MockTurn::text("摘要：第 1-2 轮读了 r1 和 r2"),
            MockTurn::text("四个文件都看完了"),
        ]);
        let (mut runner, evidence) = summary_test_runner(
            &mock,
            ContextBudget {
                max_tokens: Some(1),
                min_recent_rounds: 2,
                keep_task: true,
                summarize: None,
            }
            .with_summarize(1),
        );

        runner.run(&Budget::new(20)).await.unwrap();

        // 5 次真实请求 + 2 次摘要调用。
        assert_eq!(mock.request_count(), 7, "每次超出覆盖范围的裁剪各补一次摘要");

        // 摘要调用①：独立的单消息请求，只带被裁片段，不带任务与保留轮次。
        let first_summary = serde_json::to_string(&history(&mock, 3)).unwrap();
        assert!(
            first_summary.contains("对话历史片段"),
            "摘要请求应带摘要指令: {first_summary}"
        );
        assert!(
            first_summary.contains("OBSERVATION_FOR_r1.txt"),
            "摘要应看到第 1 轮的观察"
        );
        assert!(
            !first_summary.contains("OBSERVATION_FOR_r2.txt"),
            "第一次摘要不应包含未被裁掉的轮次: {first_summary}"
        );

        // 摘要调用②是增量的：旧摘要在提示里，新片段拼在后面。
        let second_summary = serde_json::to_string(&history(&mock, 5)).unwrap();
        assert!(second_summary.contains("此前已有摘要"), "应带上旧摘要做增量合并");
        assert!(
            second_summary.contains("OBSERVATION_FOR_r2.txt"),
            "第二次摘要应包含新被裁的第 2 轮"
        );

        // 最后一问：摘要顶替了占位说明，被裁轮次的信息以摘要形式回到上下文。
        let final_request = serde_json::to_string(&history(&mock, 6)).unwrap();
        assert!(
            final_request.contains("摘要：第 1-2 轮读了 r1 和 r2"),
            "最终请求应带最新摘要: {final_request}"
        );
        assert!(
            !final_request.contains("已省略"),
            "摘要覆盖全部被裁轮次时不应再出现占位说明: {final_request}"
        );

        // 摘要挂到会话上（快照随之带走），并留了证据。
        let summary = runner.session().summary().unwrap();
        assert_eq!(summary.covers, (1, 2));
        let summary_events: Vec<_> = evidence
            .events()
            .into_iter()
            .filter(|event| matches!(event.event, AuditEvent::SummaryGenerated { .. }))
            .collect();
        assert_eq!(summary_events.len(), 2, "每次摘要调用各留一条证据");
    }

    #[tokio::test]
    async fn failed_summary_degrades_to_the_placeholder_and_the_task_survives() {
        // 每次摘要尝试都拿到空文本（重试一次仍为空）→ 全部失败 →
        // 每次都降级占位说明，任务照常跑完。
        // 4 轮工具调用 + 2 次失败的摘要尝试（各含 1 次重试）+ 最终答案。
        let mock = MockCompletionModel::from_turns(vec![
            MockTurn::tool_call("call_1", "read_file", json!({"path": "r1.txt"})),
            MockTurn::tool_call("call_2", "read_file", json!({"path": "r2.txt"})),
            MockTurn::tool_call("call_3", "read_file", json!({"path": "r3.txt"})),
            MockTurn::text(""),
            MockTurn::text(""),
            MockTurn::tool_call("call_4", "read_file", json!({"path": "r4.txt"})),
            MockTurn::text(""),
            MockTurn::text(""),
            MockTurn::text("降级后也跑完了"),
        ]);
        let (mut runner, _evidence) =
            summary_test_runner(&mock, ContextBudget::default().with_summarize(1));
        // 把 token 上限压到 1 才会触发裁剪；Default 是 16000。
        // 直接改字段：测试装配的便利优先于不可变性。
        runner.context_budget.max_tokens = Some(1);

        let outcome = runner.run(&Budget::new(20)).await.unwrap();

        assert_eq!(outcome.answer, "降级后也跑完了");
        assert!(runner.session().summary().is_none(), "失败的摘要不应被挂载");
        assert_eq!(mock.request_count(), 9);
        let final_request = serde_json::to_string(&history(&mock, 8)).unwrap();
        assert!(
            final_request.contains("已省略"),
            "降级后应退回占位说明: {final_request}"
        );
        assert!(!final_request.contains("摘要："), "{final_request}");
    }

    #[tokio::test]
    async fn persistently_failing_summary_stops_retrying_after_the_limit() {
        // 连续失败的摘要达到上限后不再重试：否则每次裁剪都白花两次
        // 注定失败的调用。前两次摘要失败（各含 1 次重试），第三个
        // 可摘要的裁剪点直接跳过摘要、退回占位说明。
        let mock = MockCompletionModel::from_turns(vec![
            MockTurn::tool_call("call_1", "read_file", json!({"path": "r1.txt"})),
            MockTurn::tool_call("call_2", "read_file", json!({"path": "r2.txt"})),
            MockTurn::tool_call("call_3", "read_file", json!({"path": "r3.txt"})),
            MockTurn::text(""),
            MockTurn::text(""),
            MockTurn::tool_call("call_4", "read_file", json!({"path": "r4.txt"})),
            MockTurn::text(""),
            MockTurn::text(""),
            MockTurn::tool_call("call_5", "read_file", json!({"path": "r5.txt"})),
            MockTurn::text("跑完了"),
        ]);
        let (mut runner, evidence) =
            summary_test_runner(&mock, ContextBudget::default().with_summarize(1));
        runner.context_budget.max_tokens = Some(1);

        let outcome = runner.run(&Budget::new(20)).await.unwrap();

        assert_eq!(outcome.answer, "跑完了");
        // 6 次真实请求 + 2 次摘要尝试（各含 1 次重试）= 10；没有上限
        // 第三个裁剪点还会再花 2 次，共 12。
        assert_eq!(mock.request_count(), 10);
        // 失败的摘要也要留痕：事后翻 JSONL 能对上"这几轮为什么没有摘要"。
        let failures: Vec<_> = evidence
            .events()
            .into_iter()
            .filter(|event| matches!(event.event, AuditEvent::SummaryFailed { .. }))
            .collect();
        assert_eq!(failures.len(), 2, "每次失败的摘要各留一条证据");
    }

    #[tokio::test]
    async fn summarize_disabled_keeps_the_p3_behaviour() {
        // 不开摘要：同样的裁剪场景，多出来的摘要调用一次都不该有。
        let mock = MockCompletionModel::from_turns(vec![
            MockTurn::tool_call("call_1", "read_file", json!({"path": "r1.txt"})),
            MockTurn::tool_call("call_2", "read_file", json!({"path": "r2.txt"})),
            MockTurn::tool_call("call_3", "read_file", json!({"path": "r3.txt"})),
            MockTurn::tool_call("call_4", "read_file", json!({"path": "r4.txt"})),
            MockTurn::text("完成"),
        ]);
        let (mut runner, _evidence) = summary_test_runner(
            &mock,
            ContextBudget {
                max_tokens: Some(1),
                min_recent_rounds: 2,
                keep_task: true,
                summarize: None,
            },
        );

        runner.run(&Budget::new(20)).await.unwrap();

        assert_eq!(mock.request_count(), 5, "没有摘要调用");
        assert!(runner.session().summary().is_none());
        for index in 0..mock.request_count() {
            let rendered = serde_json::to_string(&history(&mock, index)).unwrap();
            assert!(
                !rendered.contains("对话历史片段"),
                "不开摘要就不该有摘要请求: {rendered}"
            );
        }
        let final_request = serde_json::to_string(&history(&mock, 4)).unwrap();
        assert!(final_request.contains("已省略"), "占位说明照旧: {final_request}");
    }

    // ── P3：整体超时与取消 ──────────────────────────────────────────────

    /// 可编排耗时的时钟：`elapsed_since` 返回共享计数器里的值。
    /// 测试用它让"时间"在工具执行时跳跃，而不真 `sleep`——这正是
    /// 时钟走端口而不是 domain 直接取时间换来的能力。
    #[derive(Default)]
    struct TimeWarpClock {
        elapsed_ms: Arc<std::sync::atomic::AtomicU64>,
    }

    impl TimeWarpClock {
        fn advance_ms(&self, millis: u64) {
            self.elapsed_ms
                .fetch_add(millis, std::sync::atomic::Ordering::Relaxed);
        }
    }

    impl Clock for TimeWarpClock {
        fn now(&self) -> crate::application::port::Timestamp {
            std::time::SystemTime::UNIX_EPOCH
        }

        fn elapsed_since(
            &self,
            _start: crate::application::port::Timestamp,
        ) -> std::time::Duration {
            std::time::Duration::from_millis(
                self.elapsed_ms.load(std::sync::atomic::Ordering::Relaxed),
            )
        }
    }

    #[tokio::test]
    async fn wall_clock_budget_stops_the_run_with_a_distinguishable_error() {
        let mock = MockCompletionModel::from_turns(vec![
            MockTurn::tool_call("call_1", "read_file", json!({"path": "a"})),
            MockTurn::text("不该被消费"),
        ]);
        let clock = Arc::new(TimeWarpClock::default());
        // 每次工具执行"消耗"60 秒模拟时间：第一轮跑完就已越过 30 秒上限。
        struct SlowInvoker {
            clock: Arc<TimeWarpClock>,
        }
        impl ToolInvoker for SlowInvoker {
            fn invoke<'a>(&'a self, _call: &'a ToolCall) -> BoxFuture<'a, ToolOutcome> {
                self.clock.advance_ms(60_000);
                Box::pin(async { ToolOutcome::success("文件内容") })
            }
        }
        let mut runner = AgentRunner::new(
            spec_with(),
            SessionId::new("test-session"),
            "任务",
            AgentDependencies {
                model: Arc::new(RigChatGateway::new(mock.clone().erase())),
                tools: registry_with(
                    Arc::new(SlowInvoker { clock: clock.clone() }),
                    vec![read_file_spec()],
                ),
                approvals: Arc::new(CountingGate::default()),
                evidence: Arc::new(MemoryEvidenceSink::new()),
                clock: clock.clone(),
                policy: PolicyEngine::new(),
                roster: AgentRoster::builtin(),
                context_budget: ContextBudget::default(),
                session_store: None,
            },
        );

        let error = runner
            .run(&Budget::new(20).with_wall_clock(std::time::Duration::from_secs(30)))
            .await
            .unwrap_err();

        let ApplicationError::WallClockExhausted { elapsed, limit } = &error else {
            panic!("应当是墙钟超时错误，实际 {error:?}");
        };
        assert!(*elapsed >= std::time::Duration::from_secs(30), "{elapsed:?}");
        assert_eq!(*limit, std::time::Duration::from_secs(30));
        assert!(error.to_string().contains("上限"));
        // 正在执行的轮次允许跑完：第一轮已提交，历史完整；
        // 第二次问模型没有发生。
        assert_eq!(runner.session().rounds(), 1);
        assert_eq!(mock.request_count(), 1);
    }

    #[tokio::test]
    async fn cancel_stops_before_the_next_model_call_and_keeps_history() {
        let mock = MockCompletionModel::from_turns(vec![
            MockTurn::tool_call("call_1", "read_file", json!({"path": "a"})),
            MockTurn::text("不该被消费"),
        ]);
        // 把手归 runner 所有（构造后才能取），而 invoker 在构造 runner 之前
        // 就得存在——用共享槽位交接，与生产代码"先建 runner、再分发把手"
        // 的顺序一致。
        let handle_slot = Arc::new(std::sync::OnceLock::new());
        struct CancellingInvoker {
            handle_slot: Arc<std::sync::OnceLock<CancelHandle>>,
        }
        impl ToolInvoker for CancellingInvoker {
            fn invoke<'a>(&'a self, _call: &'a ToolCall) -> BoxFuture<'a, ToolOutcome> {
                if let Some(handle) = self.handle_slot.get() {
                    handle.cancel();
                }
                Box::pin(async { ToolOutcome::success("文件内容") })
            }
        }
        let mut runner = AgentRunner::new(
            spec_with(),
            SessionId::new("test-session"),
            "任务",
            AgentDependencies {
                model: Arc::new(RigChatGateway::new(mock.clone().erase())),
                tools: registry_with(
                    Arc::new(CancellingInvoker { handle_slot: handle_slot.clone() }),
                    vec![read_file_spec()],
                ),
                approvals: Arc::new(CountingGate::default()),
                evidence: Arc::new(MemoryEvidenceSink::new()),
                clock: Arc::new(crate::infrastructure::clock::SystemClock),
                policy: PolicyEngine::new(),
                roster: AgentRoster::builtin(),
                context_budget: ContextBudget::default(),
                session_store: None,
            },
        );
        handle_slot
            .set(runner.cancel_handle())
            .expect("取消把手只交接一次");

        let error = runner.run(&Budget::new(20)).await.unwrap_err();

        // 错误种类可区分：是"被叫停"，不是跑失败也不是超预算。
        assert!(matches!(error, ApplicationError::Cancelled), "{error:?}");
        assert!(error.to_string().contains("取消"));
        // 取消是优雅的：执行中的轮次跑完并提交，历史完整；
        // 取消之后不再问模型。
        assert_eq!(runner.session().rounds(), 1);
        assert_eq!(mock.request_count(), 1);
    }

    #[tokio::test]
    async fn no_wall_clock_means_elapsed_time_never_stops_the_run() {
        // 回归保护：时钟报"已过 1 小时"，但预算没设时限——
        // max_wall_clock = None 的行为必须与 P2 完全一致。
        let clock = Arc::new(TimeWarpClock::default());
        clock.advance_ms(3_600_000);
        let mock = MockCompletionModel::from_turns(vec![
            MockTurn::tool_call("call_1", "read_file", json!({"path": "a"})),
            MockTurn::text("读完了"),
        ]);
        let mut runner = AgentRunner::new(
            spec_with(),
            SessionId::new("test-session"),
            "任务",
            AgentDependencies {
                model: Arc::new(RigChatGateway::new(mock.clone().erase())),
                tools: registry_with(Arc::new(RecordingInvoker::default()), vec![read_file_spec()]),
                approvals: Arc::new(CountingGate::default()),
                evidence: Arc::new(MemoryEvidenceSink::new()),
                clock,
                policy: PolicyEngine::new(),
                roster: AgentRoster::builtin(),
                context_budget: ContextBudget::default(),
                session_store: None,
            },
        );
        let outcome = runner.run(&Budget::new(20)).await.unwrap();

        assert_eq!(outcome.answer, "读完了");
        assert_eq!(outcome.turns_used, 2);
    }

    // ── P10-2：有界恢复 ────────────────────────────────────────────────
    //
    // 恢复的三种半轮状态，三种语义：
    //
    // - `Evaluating`：还没执行 → 用**当前**策略重新裁决；
    // - `AwaitingApproval`：问过人但没答复 → **再问一次**，人要再点一次；
    // - `Dispatching`：正在执行 → 副作用可能已经发生，**不自动重跑**，
    //   把决定权交回模型。

    /// 一个待执行的写调用。
    fn write_call(id: &str) -> ToolCall {
        ToolCall {
            id: CallId::new(id),
            name: "write_file".to_owned(),
            arguments: json!({"path": "out.txt", "content": "x"}),
        }
    }

    /// 恢复测试用的依赖装配。
    fn resume_deps(
        mock: &MockCompletionModel,
        invoker: Arc<dyn ToolInvoker>,
        evidence: Arc<MemoryEvidenceSink>,
        approvals: Arc<CountingGate>,
        policy: PolicyEngine,
    ) -> AgentDependencies {
        AgentDependencies {
            model: Arc::new(RigChatGateway::new(mock.clone().erase())),
            tools: registry_with(invoker, vec![read_file_spec(), write_file_spec()]),
            approvals,
            evidence,
            clock: Arc::new(crate::infrastructure::clock::SystemClock),
            policy,
            roster: AgentRoster::builtin(),
            context_budget: ContextBudget::default(),
            session_store: None,
        }
    }

    /// 停在"正在执行工具"这一刻的快照。
    ///
    /// 中断就发生在调用发起之后、观察记下之前——副作用可能已经出去，
    /// 而我们不知道结果。这是最危险的那种中断。
    fn dispatching_snapshot(call: ToolCall) -> RunSnapshot {
        let turn = AgentTurn {
            text: "写一下".to_owned(),
            calls: vec![call.clone()],
        };
        RunSnapshot {
            format_version: RUN_FORMAT,
            session_id: SessionId::new("resume-session"),
            spec: spec_with(),
            session: Session::new(SessionId::new("resume-session"), "任务").snapshot(),
            state: RunState::Dispatching(vec![call.clone()]),
            consumption: Consumption {
                turns: 1,
                elapsed: Duration::from_secs(1),
            },
            pending: Some(PendingRound {
                turn,
                outcomes: Vec::new(),
            }),
        }
    }

    /// 停在"评估调用、还没裁决"这一刻的快照。
    fn evaluating_snapshot(call: ToolCall) -> RunSnapshot {
        let turn = AgentTurn {
            text: "写一下".to_owned(),
            calls: vec![call.clone()],
        };
        RunSnapshot {
            format_version: RUN_FORMAT,
            session_id: SessionId::new("resume-session"),
            spec: spec_with(),
            session: Session::new(SessionId::new("resume-session"), "任务").snapshot(),
            state: RunState::Evaluating(vec![call.clone()]),
            consumption: Consumption {
                turns: 1,
                elapsed: Duration::from_secs(1),
            },
            pending: Some(PendingRound {
                turn,
                outcomes: Vec::new(),
            }),
        }
    }

    /// 停在"等人审批"这一刻的快照。
    ///
    /// 这正是"上一次问过、人没答复"的状态：进程停在闸门前，人没有
    /// 回答就被中断了。
    fn awaiting_approval_snapshot(call: ToolCall) -> RunSnapshot {
        let turn = AgentTurn {
            text: "写一下".to_owned(),
            calls: vec![call.clone()],
        };
        let request = ApprovalRequest {
            turn: 1,
            call: call.clone(),
            annotations: write_file_spec().annotations,
            reason: None,
            resumed: false,
        };
        RunSnapshot {
            format_version: RUN_FORMAT,
            session_id: SessionId::new("resume-session"),
            spec: spec_with(),
            session: Session::new(SessionId::new("resume-session"), "任务").snapshot(),
            state: RunState::AwaitingApproval(request),
            consumption: Consumption {
                turns: 1,
                elapsed: Duration::from_secs(1),
            },
            pending: Some(PendingRound {
                turn,
                outcomes: Vec::new(),
            }),
        }
    }

    /// 请求历史里最后一条工具结果的内容。
    fn last_tool_result_text(mock: &MockCompletionModel, index: usize) -> String {
        let messages = history(mock, index);
        let Message::User { content } = messages.last().unwrap() else {
            panic!("观察结果应当是 user 消息");
        };
        let Some(UserContent::ToolResult(result)) = content.first() else {
            panic!("应当是 tool 结果内容");
        };
        result.content[0].as_text().unwrap().to_owned()
    }

    /// 恢复时，中断时正在执行的那个调用**不自动重跑**。
    ///
    /// 切片 2 最要紧的一条：副作用可能已经发生，重跑一次就是把同一次
    /// 副作用做两遍。恢复必须把决定权交回模型，而不是替模型决定。
    #[tokio::test]
    async fn resuming_from_dispatching_does_not_rerun_the_interrupted_call() {
        let call = write_call("call_1");
        let invoker = Arc::new(RecordingInvoker::default());
        let evidence = Arc::new(MemoryEvidenceSink::new());
        let mock = MockCompletionModel::from_turns(vec![MockTurn::text("我不重跑了")]);
        let mut runner = AgentRunner::resume(
            dispatching_snapshot(call.clone()),
            resume_deps(
                &mock,
                invoker.clone(),
                evidence.clone(),
                Arc::new(CountingGate::default()),
                PolicyEngine::new(),
            ),
        )
        .expect("快照应可恢复");

        let state = runner.step().await.unwrap();

        assert!(
            matches!(state, RunState::AwaitingModel),
            "重放提示之后应回到等模型回复，实际 {state:?}"
        );
        assert!(
            invoker.calls.lock().unwrap().is_empty(),
            "中断时正在执行的调用不得被自动重跑"
        );
        // 证据里要有一条"可能已执行"的记录：事后翻 JSONL 才知道这里
        // 差点发生过一次重复副作用。
        let replays: Vec<_> = evidence
            .events()
            .into_iter()
            .filter(|entry| matches!(entry.event, AuditEvent::ToolCallReplayed { .. }))
            .collect();
        assert_eq!(replays.len(), 1, "应留下一条重放证据");
    }

    /// 模型看得见"这个调用之前跑过、要不要再跑由你决定"。
    ///
    /// 提示要给出"跑过没有"的事实：结果未知也要说清楚，不能让模型
    /// 以为调用没跑过、于是照原样再来一遍。
    #[tokio::test]
    async fn the_model_is_told_the_interrupted_call_may_already_have_run() {
        let call = write_call("call_1");
        let invoker = Arc::new(RecordingInvoker::default());
        let evidence = Arc::new(MemoryEvidenceSink::new());
        let mock = MockCompletionModel::from_turns(vec![MockTurn::text("我不重跑了")]);
        let mut runner = AgentRunner::resume(
            dispatching_snapshot(call),
            resume_deps(
                &mock,
                invoker,
                evidence,
                Arc::new(CountingGate::default()),
                PolicyEngine::new(),
            ),
        )
        .unwrap();

        runner.step().await.unwrap(); // 重放提示
        runner.step().await.unwrap(); // 问模型

        let text = last_tool_result_text(&mock, 0);
        assert!(text.contains("中断恢复"), "模型应看见重放提示: {text}");
        assert!(text.contains("write_file"), "提示应点名工具: {text}");
        assert!(
            text.contains("没有自动再跑"),
            "提示应说明没有自动重跑: {text}"
        );
        assert!(
            text.contains("结果未知"),
            "提示应说明结果未知: {text}"
        );
        assert!(
            text.contains("由你判断"),
            "提示应把决定权交回模型: {text}"
        );
    }

    /// 恢复时，审批要**再问一次**，不能凭快照放行。
    ///
    /// 人被问过但没答复，那就不算同意——这是安全要求，不是体验问题。
    #[tokio::test]
    async fn resuming_from_awaiting_approval_asks_the_gate_again() {
        let call = write_call("call_1");
        let approvals = Arc::new(CountingGate::default());
        let invoker = Arc::new(RecordingInvoker::default());
        let evidence = Arc::new(MemoryEvidenceSink::new());
        let mock = MockCompletionModel::from_turns(vec![MockTurn::text("写完了")]);
        let mut runner = AgentRunner::resume(
            awaiting_approval_snapshot(call),
            resume_deps(
                &mock,
                invoker.clone(),
                evidence.clone(),
                approvals.clone(),
                PolicyEngine::new(),
            ),
        )
        .unwrap();

        // 第一步：按当前策略重新裁决，停在审批前。
        let state = runner.step().await.unwrap();
        assert!(
            matches!(state, RunState::AwaitingApproval(_)),
            "恢复后仍应停在审批前，实际 {state:?}"
        );
        assert!(
            invoker.calls.lock().unwrap().is_empty(),
            "没点头之前不该执行"
        );
        assert!(
            approvals.requested_tools().is_empty(),
            "裁决这一步还不该问闸门"
        );

        // 第二步：问闸门——人被问了一次（快照里那次在上一个进程里）。
        runner.step().await.unwrap();
        assert_eq!(approvals.requested_tools(), vec!["write_file"]);
        // 请求要标出"这是重新问"：人需要知道上一次问过、没答复。
        assert!(
            approvals.first_request().resumed,
            "重新询问必须带 resumed 标记"
        );
        // 证据里也要留一条：上一个进程里那次"问过"不在当前证据文件里。
        let reasks: Vec<_> = evidence
            .events()
            .into_iter()
            .filter(|entry| matches!(entry.event, AuditEvent::ApprovalReasked { .. }))
            .collect();
        assert_eq!(reasks.len(), 1, "重新询问要留一条证据");
    }

    /// 恢复时用**当前**的策略引擎重新裁决，而不是快照里的旧结论。
    ///
    /// 快照里其实没存裁决结果（裁决只在审计事件里），所以"策略变了"
    /// 是天然成立的：恢复时看到的是今天的策略。策略改成拒绝之后，
    /// 那个调用必须被拒——哪怕中断时它是放行的。
    #[tokio::test]
    async fn resuming_from_evaluating_uses_the_current_policy() {
        let call = write_call("call_1");
        let approvals = Arc::new(CountingGate::default());
        let invoker = Arc::new(RecordingInvoker::default());
        let evidence = Arc::new(MemoryEvidenceSink::new());
        let mock = MockCompletionModel::from_turns(vec![MockTurn::text("那就不写了")]);
        let mut runner = AgentRunner::resume(
            evaluating_snapshot(call),
            resume_deps(
                &mock,
                invoker.clone(),
                evidence,
                approvals.clone(),
                policy_with("write_file", PolicyVerdict::Deny, "恢复时的策略：禁止写文件"),
            ),
        )
        .unwrap();

        runner.step().await.unwrap(); // 裁决：拒绝
        runner.step().await.unwrap(); // 问模型

        assert!(
            invoker.calls.lock().unwrap().is_empty(),
            "被当前策略拒绝的调用不得执行"
        );
        assert!(
            approvals.requested_tools().is_empty(),
            "被拒绝的调用不该惊动闸门"
        );
        // 模型看到的是一条拒绝观察，理由来自**当前**策略。
        let text = last_tool_result_text(&mock, 0);
        assert!(text.contains("恢复时的策略"), "拒绝理由应来自当前策略: {text}");
    }

    /// 停在审批前时，当前策略改成拒绝 → 直接拒，不问闸门。
    ///
    /// 策略重算不是走过场：今天的策略说不行，今天就不行。
    #[tokio::test]
    async fn resuming_from_awaiting_approval_uses_the_current_policy_too() {
        let call = write_call("call_1");
        let approvals = Arc::new(CountingGate::default());
        let invoker = Arc::new(RecordingInvoker::default());
        let evidence = Arc::new(MemoryEvidenceSink::new());
        let mock = MockCompletionModel::from_turns(vec![MockTurn::text("那就不写了")]);
        let mut runner = AgentRunner::resume(
            awaiting_approval_snapshot(call),
            resume_deps(
                &mock,
                invoker.clone(),
                evidence,
                approvals.clone(),
                policy_with("write_file", PolicyVerdict::Deny, "恢复时的策略：禁止写文件"),
            ),
        )
        .unwrap();

        runner.step().await.unwrap();

        assert!(
            approvals.requested_tools().is_empty(),
            "当前策略拒绝时不该再问闸门"
        );
        assert!(
            invoker.calls.lock().unwrap().is_empty(),
            "被当前策略拒绝的调用不得执行"
        );
    }

    /// 当前策略放行时，恢复仍然要**再问一次**。
    ///
    /// 这个请求在中断前已经被判为"需要人过目"。策略后来放宽了，也不能
    /// 替人把这个判断抹掉——否则"人没答复"会被悄悄读成"人点过了"。
    #[tokio::test]
    async fn a_relaxed_policy_does_not_cancel_the_re_ask() {
        let call = write_call("call_1");
        let approvals = Arc::new(CountingGate::default());
        let invoker = Arc::new(RecordingInvoker::default());
        let evidence = Arc::new(MemoryEvidenceSink::new());
        let mock = MockCompletionModel::from_turns(vec![MockTurn::text("写完了")]);
        let mut runner = AgentRunner::resume(
            awaiting_approval_snapshot(call),
            resume_deps(
                &mock,
                invoker.clone(),
                evidence,
                approvals.clone(),
                policy_with("write_file", PolicyVerdict::Allow, "恢复时的策略：写文件免审"),
            ),
        )
        .unwrap();

        let state = runner.step().await.unwrap();

        assert!(
            matches!(state, RunState::AwaitingApproval(_)),
            "策略放宽也不该跳过重新询问，实际 {state:?}"
        );
        runner.step().await.unwrap();
        assert_eq!(
            approvals.requested_tools(),
            vec!["write_file"],
            "闸门必须被问一次"
        );
        assert!(approvals.first_request().resumed);
    }

    /// 恢复后累计轮数**不重置**：预算接着算。
    ///
    /// 快照里的 `consumption` 是恢复的账本起点。若恢复时把它清零，
    /// 一个长任务就能靠"中断-恢复"无限续命——轮数预算就成了摆设。
    #[tokio::test]
    async fn cumulative_turns_survive_a_resume() {
        // 第一次运行：2 轮工具调用，正好用光 2 轮的预算。
        let first_mock = MockCompletionModel::from_turns(vec![
            MockTurn::tool_call("call_1", "read_file", json!({"path": "a"})),
            MockTurn::tool_call("call_2", "read_file", json!({"path": "b"})),
        ]);
        let evidence = Arc::new(MemoryEvidenceSink::new());
        let mut first = AgentRunner::new(
            spec_with(),
            SessionId::new("resume-session"),
            "任务",
            resume_deps(
                &first_mock,
                Arc::new(RecordingInvoker::default()),
                evidence.clone(),
                Arc::new(CountingGate::default()),
                PolicyEngine::new(),
            ),
        );
        let error = first.run(&Budget::new(2)).await.unwrap_err();
        assert!(matches!(error, ApplicationError::BudgetExhausted { .. }));
        let snapshot = first.snapshot();
        assert_eq!(snapshot.consumption.turns, 2, "账本记着已用两轮");

        // 恢复：总预算 3 轮，已经用掉 2 轮，只剩 1 轮。
        let second_mock = MockCompletionModel::from_turns(vec![
            MockTurn::tool_call("call_3", "read_file", json!({"path": "c"})),
            MockTurn::text("第二轮才轮到的收尾"),
        ]);
        let mut resumed = AgentRunner::resume(
            snapshot,
            resume_deps(
                &second_mock,
                Arc::new(RecordingInvoker::default()),
                evidence,
                Arc::new(CountingGate::default()),
                PolicyEngine::new(),
            ),
        )
        .unwrap();
        let error = resumed.run(&Budget::new(3)).await.unwrap_err();

        assert!(
            matches!(error, ApplicationError::BudgetExhausted { .. }),
            "恢复后的预算应当接着算，实际 {error}"
        );
        assert_eq!(
            second_mock.request_count(),
            1,
            "只剩一轮可用：清零的话这里能跑两轮"
        );
        assert_eq!(resumed.snapshot().consumption.turns, 3);
    }
}

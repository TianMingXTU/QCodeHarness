//! AgentRunner：ReAct 循环的核心编排器。
//!
//! 应用层核心，不直接依赖 rig 或 tokio，通过端口 trait 与外部交互，
//! 因此可测试。
//!
//! 循环规则：每轮重建请求；无工具调用即返回最终答案；未注册工具
//! 生成拒绝观察；达到 `max_turns` 或收到空回复时报错。

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::application::agent_roster::AgentRoster;
use crate::application::context_policy::ContextPolicy;
use crate::application::control::ExecutionControl;
use crate::application::error::ApplicationError;
use crate::application::port::{
    ApprovalGate, ApprovalRequest, CancelHandle, Clock, EvidenceSink, ModelGateway, ModelReply,
    ModelRequest, RunHooks,
};
use crate::application::snapshot::{RUN_FORMAT, RunSnapshot, SessionStore, StoreError};
use crate::application::summarizer::{SummarizeInput, Summarizer, render_slices};
use crate::application::tool_guard::{
    BoundaryEscalationGuard, GuardContext, GuardVerdict, PolicyGuard, ToolGuard, run_guard_chain,
};
use crate::application::tool_registry::ToolRegistry;
use crate::domain::agent::{AgentSpec, AgentTurn};
use crate::domain::budget::{Budget, Consumption};
use crate::domain::context::{CompactionReport, ContextBudget};
use crate::domain::evidence::AuditEvent;
use crate::domain::ids::SessionId;
use crate::domain::policy::{PolicyDecision, PolicyEngine};
use crate::domain::session::{ContextSlice, Session};
use crate::domain::tool::{ToolAnnotations, ToolCall, ToolOutcome, ToolSpec};

/// 摘要生成失败的上限（运行内累计）。允许两次，瞬态故障确实出现过；达到
/// 后本次运行不再重试，避免每轮都付出一次注定失败的模型调用。
const SUMMARY_FAILURE_LIMIT: u32 = 2;

/// 恢复后重新询问审批时的提示语。
///
/// 必须说明"上次已问过但未答复"，否则用户可能以为程序出错。
pub const APPROVAL_REASK_REASON: &str =
    "这个审批请求之前就问过，那时没答复；恢复后需要你重新决定一次。";

/// 重放提示中给出的原因。
///
/// 快照在调用发起**之前**落盘，因此无法确认该调用到底有没有真正开始
/// 执行。措辞按这个不确定度写：既不能说"已执行"，也不能说"未执行"。
pub const REPLAY_REASON: &str =
    "中断发生在这次调用执行期间，无法确认它是否真的执行过，副作用可能已经发生，结果未知";

/// 运行状态机。
///
/// 任意时刻都可能被外部中断：墙钟超时由 `run` 内部在步骤边界检查
/// （`Budget::max_wall_clock`），单次模型调用与工具调用各有自己的时限；
/// 审批停在 [`RunState::AwaitingApproval`]，状态序列化后等待外部回复；
/// 恢复时反序列化并从原位置继续 `step`。
///
/// 可序列化，快照原样存盘，恢复时从同一位置继续。变体名是落盘契约的
/// 一部分，不能改名。
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
    /// 计入消耗的模型调用次数。空回复的自动重试与失败的摘要调用不计。
    pub turns_used: u32,
}

/// 已交给模型、等待观察的一轮。
///
/// `outcomes` 的长度即已处理的调用数，无需单独的下标游标。
///
/// 公开且可序列化，快照需要表达"执行到半轮"的状态。恢复后 `step()`
/// 从下一条调用继续，已完成的观察不会重跑。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct PendingRound {
    /// 模型这一轮的完整回复。
    pub turn: AgentTurn,
    /// 与 `turn.calls` 同下标一一对应。
    pub outcomes: Vec<ToolOutcome>,
}

/// `AgentRunner` 的全部外部依赖。
///
/// 用结构体而非位置参数：字段名让构造点自解释，新增依赖不影响既有
/// 调用点。没有 `Default`，因为没有合理的默认模型，缺依赖应在构造点
/// 显式写出。
///
/// 可克隆：编排器每步克隆一份给新的 runner，`Arc` 字段只增加引用计数。
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
    /// 策略引擎：决定每个调用是放行、需要审批还是直接拒绝。
    ///
    /// 领域类型，无 IO、无外部依赖，直接持有比另开端口更简单。
    pub policy: PolicyEngine,
    /// 追加到内置守卫链之后的工具守卫。
    ///
    /// 内置链由 `default_guard_chain` 构造，包含策略裁决与越界升级。追加项
    /// 按链序参与"最严格者胜"的合并，空表示只用内置链。策略守卫始终在链首，
    /// `PolicyEvaluated` 证据与它同源，不会被追加项替换。
    pub additional_guards: Vec<Arc<dyn ToolGuard>>,
    /// 角色花名册：按 `spec.role` 查出该角色可调用的工具。
    ///
    /// 工具定义对所有步骤统一，模型始终看到全量注册表；角色边界下沉到
    /// "调不动"，由 runner 查花名册，不依赖调用方预先过滤。放在依赖里而非
    /// 构造参数里，因为 `resume()` 也要按快照中的角色重算。
    pub roster: AgentRoster,
    /// 上下文预算：发给模型前按它裁剪历史。
    pub context_budget: ContextBudget,
    /// 上下文策略：决定发给模型的历史视图。
    ///
    /// 预算只给上限，裁剪方式由策略决定。装配方通常使用
    /// [`DefaultContextPolicy`](crate::application::context_policy::DefaultContextPolicy)，
    /// 行为与既有裁剪一致。
    pub context_policy: Arc<dyn ContextPolicy>,
    /// 快照存储。`None` 表示不自动保存，用于单测等不需要恢复的场景。
    ///
    /// 放在依赖里而非 `run` 的参数里：保存时机由循环内部决定（每轮提交后、
    /// 进入审批前），调用方没有更合适的插桩点。
    pub session_store: Option<Arc<dyn SessionStore>>,
    /// 运行挂点：模型调用与工具执行前后的观察者。
    ///
    /// 只观察，不干预；拦截由策略与审批闸门负责。不需要事件时填
    /// [`crate::application::port::NoopHooks`]，`resume()` 也带着它运行，
    /// 恢复路径与首次运行看到同一串事件。
    pub hooks: Arc<dyn RunHooks>,
}

/// ReAct 循环的编排器。
pub struct AgentRunner {
    model: Arc<dyn ModelGateway>,
    /// 工具注册表：规格与执行入口绑在一起，加工具只在一处发生。
    tools: ToolRegistry,
    /// 注册表中全部规格的缓存，随请求发给模型。
    /// 注册表构造后不变，缓存可避免每轮克隆整张表。
    visible_specs: Vec<ToolSpec>,
    approvals: Arc<dyn ApprovalGate>,
    evidence: Arc<dyn EvidenceSink>,
    clock: Arc<dyn Clock>,
    /// 工具守卫链：策略裁决与越界升级按序执行，最严格者胜。
    ///
    /// 内置链是 `PolicyGuard` + `BoundaryEscalationGuard`，与原内联逻辑
    /// 一致。守卫是纯函数，没有 IO。策略引擎从链上取，确保 `PolicyEvaluated`
    /// 证据与守卫使用同一份裁决。
    guards: Vec<Arc<dyn ToolGuard>>,
    /// 角色花名册：角色边界的事实来源（见 [`AgentDependencies::roster`]）。
    roster: AgentRoster,
    /// 当前角色可调用的工具名；`None` 表示不限。
    ///
    /// 换角色时重算（构造、恢复、`begin_step` 各一次），之后每条调用都查它。
    /// 缓存是因为 `AgentRoster::allowed_tools` 需要遍历档案，而它会在每条
    /// 工具调用上被查询。
    allowed_tools: Option<Vec<String>>,
    /// 上下文预算：发给模型前按它裁剪历史。
    ///
    /// 裁剪只影响发给模型的视图，不修改 `session`；证据、审计与恢复都需要
    /// 完整历史。
    context_budget: ContextBudget,
    /// 上下文策略：把预算变成实际视图（见 [`AgentDependencies::context_policy`]）。
    context_policy: Arc<dyn ContextPolicy>,
    /// 快照存储的克隆。`None` 表示不自动保存。
    session_store: Option<Arc<dyn SessionStore>>,
    /// 取消开关，克隆给外部。
    cancel: CancelHandle,
    spec: AgentSpec,
    session: Session,
    state: RunState,
    consumption: Consumption,
    pending: Option<PendingRound>,
    /// 墙钟测量锚点：本次 `run()` 的开始时刻。`run()` 入口重置；手动逐
    /// `step()` 驱动时保持构造时刻。
    run_start: SystemTime,
    /// 墙钟基数：进入本次 `run()` 前已累计的耗时（恢复时来自快照）。
    elapsed_base: Duration,
    /// 连续摘要失败次数。达到上限后本次运行不再重试，避免每轮都付出一次
    /// 注定失败的模型调用。
    summary_failures: u32,
    /// 恢复时的重放提示：中断时正在执行的那一次调用。
    ///
    /// `Some` 表示该调用在中断前已发起，可能产生副作用，结果未知。恢复时
    /// **不自动重跑**，否则同一次副作用会执行两遍；也不能当作没发生过。
    /// 应把事实告诉模型，由它决定是否重试（见
    /// [`AgentRunner::replay_interrupted_call`]）。
    replay_pending: Option<ToolCall>,
    /// 恢复时的重新审批标记：该审批之前问过但未答复。
    ///
    /// 恢复后必须**再问一次**闸门，不能凭快照放行（见
    /// [`AgentRunner::apply_resume_semantics`]）。
    reask_approval: bool,
    /// 运行挂点：来自依赖。只观察，不参与状态机。
    hooks: Arc<dyn RunHooks>,
    /// 宿主提供的执行控制：补充输入队列与运行状态检查。
    ///
    /// 输入只在完整轮次配平后应用；`check` 失败在下一个工具前停下。
    /// `None` 表示宿主不参与运行期控制，行为与旧版一致。
    execution_control: Option<Arc<dyn ExecutionControl>>,
}

impl AgentRunner {
    /// 组装一个 runner。
    ///
    /// 外部依赖全部收进 [`AgentDependencies`]，字段名自解释，新增依赖不影响
    /// 既有调用点。
    ///
    /// `session_id` 由调用方传入。证据文件按会话 ID 命名，且必须在 runner
    /// 启动前打开；内部生成会导致证据文件名与 runner 自报的会话 ID 不一致。
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
            additional_guards,
            roster,
            context_budget,
            context_policy,
            session_store,
            hooks,
        } = deps;
        let visible_specs = tools.specs();
        let session = Session::new(session_id, task);
        let run_start = clock.now();
        let mut guards = default_guard_chain(policy);
        guards.extend(additional_guards);
        let mut runner = Self {
            model,
            tools,
            visible_specs,
            approvals,
            evidence,
            clock,
            guards,
            roster,
            allowed_tools: None,
            context_budget,
            context_policy,
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
            hooks,
            execution_control: None,
        };
        runner.refresh_role_boundary();
        runner.bind_cancel_to_gate();
        runner
    }

    /// 从快照恢复一个 runner，继续执行。
    ///
    /// 与 `new()` 的分工：`new()` 从零开始，`resume()` 接手已有会话与状态机
    /// 位置，两者使用同一套依赖注入方式。取消状态不跨恢复传递，恢复是一次
    /// 新的"继续"意图，旧运行的取消请求不应影响新运行。
    ///
    /// 恢复成功后立即再保存一次快照，避免刚恢复又崩溃时只剩旧位置的快照。
    ///
    /// # Errors
    ///
    /// 版本不匹配，或状态机位置与待配平轮次不一致（半轮缺 pending、
    /// 正常点带 pending、调用清单错位）时返回 [`ApplicationError::Snapshot`]；
    /// 历史损坏或配对错误经 [`ApplicationError::Session`] 上抛。
    pub fn resume(
        snapshot: RunSnapshot,
        deps: AgentDependencies,
    ) -> Result<Self, ApplicationError> {
        snapshot.check_version()?;
        // 半轮状态依赖 pending 续跑（step() 从 outcomes.len() 推下一条调用）。
        // 缺了它 step() 只能报 NoPendingRound，因此在入口拦下这种矛盾。
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
        // 反向也要配平：停在正常点（等待模型/已完成）却带着 pending，恢复后
        // step() 会先配一轮不存在的观察，导致历史错位。
        if !mid_round && snapshot.pending.is_some() {
            return Err(StoreError::Corrupt(
                "快照停在正常点，却带有未配平的轮次（pending）".to_owned(),
            )
            .into());
        }
        // 状态机记录的调用清单必须与待配平轮次一致：step() 按 state 取下一个
        // 调用，按 pending 收观察，不一致会导致调用与观察错位配对。
        if let (Some(pending), RunState::Evaluating(calls) | RunState::Dispatching(calls)) =
            (&snapshot.pending, &snapshot.state)
            && calls != &pending.turn.calls {
                return Err(StoreError::Corrupt(
                    "状态机记录的调用清单与待配平轮次不一致".to_owned(),
                )
                .into());
            }
        // 观察数即本轮已完成的调用数。它不能超过调用数；在 Dispatching 状态下
        // 必须小于调用数，全部配平表示本轮已结束，应停在等待模型而非执行中。
        // 这两种情况正常构造路径产生不出来，在入口拦下。
        if let Some(pending) = &snapshot.pending {
            if pending.outcomes.len() > pending.turn.calls.len() {
                return Err(StoreError::Corrupt("快照的观察数超过了调用数".to_owned()).into());
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
            additional_guards,
            roster,
            context_budget,
            context_policy,
            session_store,
            hooks,
        } = deps;
        let visible_specs = tools.specs();
        let clock_now = clock.now();
        // 墙钟基数要在 consumption 被移进 runner 之前取出。
        let elapsed_base = snapshot.consumption.elapsed;
        let mut guards = default_guard_chain(policy);
        guards.extend(additional_guards);
        let mut runner = Self {
            model,
            tools,
            visible_specs,
            approvals,
            evidence,
            clock,
            guards,
            roster,
            allowed_tools: None,
            context_budget,
            context_policy,
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
            hooks,
            execution_control: None,
        };
        runner.refresh_role_boundary();
        runner.bind_cancel_to_gate();
        // 半轮状态各有各的恢复语义，翻译成"接下来该怎么走"。
        runner.apply_resume_semantics();
        runner.save_snapshot();
        Ok(runner)
    }

    /// 把取消通道交给审批闸门。
    ///
    /// 闸门要能表达"中止整个运行"，就得有请求取消的手段。runner 持有
    /// [`CancelHandle`]，闸门拿到的是它的副本：两边看到同一个开关。
    fn bind_cancel_to_gate(&self) {
        self.approvals.bind_cancel(self.cancel.clone());
    }

    /// 在完整轮次配平后应用宿主补充输入。
    ///
    /// 输入只在 [`RunState::AwaitingModel`] 边界取用：历史里"助手轮 + 观察"
    /// 的配对未被打散，新输入追加在末尾，下一轮模型才看得见。先落盘再通知
    /// 宿主 `input_applied`：生效以持久化为准，宿主确认的是"这条输入已写进
    /// 会话"，而不是"请求已收到"。宿主取输入或确认失败都归为
    /// [`ApplicationError::ExecutionControl`]，停止后续动作，不静默吞掉。
    fn apply_inputs(&mut self) -> Result<(), ApplicationError> {
        let Some(control) = self.execution_control.clone() else {
            return Ok(());
        };
        let inputs = control.take_inputs();
        if inputs.is_empty() {
            return Ok(());
        }
        for input in &inputs {
            // 宿主可在崩溃前保留尚未确认的输入；会话历史已含该 ID 时不再追加，
            // 但仍要确认，队列才不会反复投递同一条消息。
            if self.session.input_text(&input.id).is_none() {
                self.session
                    .push_input(input.id.clone(), input.text.clone());
            }
        }
        self.try_save_snapshot()?;
        for input in &inputs {
            control
                .input_applied(input)
                .map_err(|message| ApplicationError::ExecutionControl { message })?;
        }
        Ok(())
    }

    /// 工具执行前询问宿主运行状态是否可靠。
    ///
    /// 宿主的审计或运行检查失败时，返回 [`ApplicationError::ExecutionControl`]
    /// 在下一个工具前停下。放在 Dispatching 分支的调用前，不阻断已开始的调用。
    fn check_control(&self) -> Result<(), ApplicationError> {
        let Some(control) = &self.execution_control else {
            return Ok(());
        };
        control
            .check()
            .map_err(|message| ApplicationError::ExecutionControl { message })
    }

    /// 在已完成的会话上继续对话：把终态答案提交进历史，退回等待模型。
    ///
    /// 与 [`Self::begin_step`] 的分工：`begin_step` 换角色、交代下一步；本方法
    /// 不换角色，只把 `Completed` 的答案落进历史并回到 [`RunState::AwaitingModel`]。
    /// 供任务级"继续对话"使用（见 `orchestrator::continue_conversation`）。
    /// 已完成的最后一步答案只停在 `Completed` 态里、尚未入历史，不提交会让
    /// 继续对话时模型看不见它。历史与用量全部保留，不重放已完成步骤。
    ///
    /// # Errors
    ///
    /// 提交最终答案破坏配对时返回 [`ApplicationError::Session`]；正常路径不会发生。
    pub fn start_conversation(&mut self) -> Result<(), ApplicationError> {
        if let RunState::Completed(answer) = &self.state {
            let answer = answer.clone();
            self.session.commit_round(
                AgentTurn {
                    text: answer,
                    calls: Vec::new(),
                },
                Vec::new(),
            )?;
        }
        self.state = RunState::AwaitingModel;
        self.pending = None;
        self.save_snapshot();
        Ok(())
    }

    /// 把快照里的半轮状态转换为后续执行路径。
    ///
    /// 三种半轮状态，三种语义：
    ///
    /// - **`Evaluating`**：调用尚未执行，策略也未裁决。恢复后照常评估，
    ///   使用**当前**的 [`PolicyEngine`] 重算。快照未保存裁决结果（裁决只
    ///   落在审计事件里），因此恢复时看到的是当前策略。
    /// - **`AwaitingApproval`**：上次已询问用户，未获答复。恢复后回到评估态，
    ///   由当前策略重新裁决；即使策略放宽，也**再问一次**（带 `resumed`
    ///   标记），不能凭快照放行。
    /// - **`Dispatching`**：快照保存在调用发起之前，恢复时该调用可能已执行、
    ///   也可能没执行，结果未知。恢复后**不自动重跑**，把重放提示交给
    ///   模型，由它决定是否重试。
    ///
    /// 统一退回评估态，让三种状态共用同一条"先裁决、再决定是否询问用户"
    /// 的路径，恢复语义只有一处实现。
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
                // 在途调用 = 已配平条数，观察数即已跑完的条数。
                self.replay_pending = pending.turn.calls.get(pending.outcomes.len()).cloned();
            }
            _ => {}
        }
    }

    /// 会话 ID。
    pub fn session_id(&self) -> &SessionId {
        self.session.id()
    }

    /// 完整会话历史。裁剪不影响这里，`session` 始终保存全量数据。
    pub fn session(&self) -> &Session {
        &self.session
    }

    /// 取消把手，可克隆给外部。外部随时调用 `cancel()`，`run()` 会在下一轮
    /// 模型调用前停下。
    pub fn cancel_handle(&self) -> CancelHandle {
        self.cancel.clone()
    }

    /// 接入宿主执行控制：运行期补充输入与运行状态检查。
    ///
    /// 补充输入只在完整轮次配平后（回到 [`RunState::AwaitingModel`]）应用，
    /// 保证历史里"助手轮 + 观察"的配对不被打散；`check` 失败（审计或宿主
    /// 不可靠）在下一个工具前停下，用 [`ApplicationError::ExecutionControl`]
    /// 表达，不静默吞掉。取消开关与输入应用共用 `cancel`，宿主取消后在途
    /// 工具仍会配平，未开始的调用以拒绝观察回填。
    pub fn with_execution_control(
        mut self,
        control: Arc<dyn ExecutionControl>,
        cancel: CancelHandle,
    ) -> Self {
        self.execution_control = Some(control);
        self.cancel = cancel;
        // 构造时闸门绑定的是默认开关；换成宿主的取消通道后必须重新绑定，
        // 否则异步审批永远看不到宿主发出的取消。
        self.bind_cancel_to_gate();
        self
    }

    /// 进入下一步：换角色、交代步骤、把上一步结论写入历史。
    ///
    /// 单会话编排的接缝：一个任务只开一个会话，步骤间不新建 runner，历史、
    /// 轮数、墙钟与缓存路由键全部延续。下一步的模型因此能看到上一步读过的
    /// 文件和结论，无需重复读取。
    ///
    /// 只能在 [`RunState::Completed`] 之后调用，或经 [`Self::resume`] 走半轮
    /// 恢复路径（恢复时先配平 pending 再进入本方法）。带未配平调用时换步骤
    /// 会丢掉 pending，历史随之错位。
    ///
    /// # Errors
    ///
    /// 提交上一步最终答案时若破坏配平，返回 [`ApplicationError::Session`]。
    /// 正常流程不会发生：无调用轮次的观察必然为空。若真出现，说明会话状态
    /// 已损坏，带着错误历史继续询问模型只会放大错误。
    pub fn begin_step(&mut self, spec: AgentSpec, prompt: String) -> Result<(), ApplicationError> {
        // 恢复路径：runner 停在本步的半轮状态，且本步的交代已在历史末尾。
        // 此时"进入下一步"实际是"继续这一步"。若把状态机重置为等待模型，
        // pending 轮次会整体丢失，副作用与未答复的审批等执行现场也随之消失。
        let resuming_same_step = matches!(
            self.state,
            RunState::Evaluating(_) | RunState::AwaitingApproval(_) | RunState::Dispatching(_)
        ) && self.session.ends_with_user_message(&prompt);
        if resuming_same_step {
            // 只换角色与白名单，状态机位置与 pending 保持原样。
            self.spec = spec;
            self.refresh_role_boundary();
            self.save_snapshot();
            return Ok(());
        }
        // 上一步的最终答案必须先写入历史。`step()` 的 Completed 分支不提交
        // 轮次（最终答案没有观察可配），而单会话编排的价值在于下一步能看到
        // 上一步的结论；不提交则跨步骤复用上下文无法实现。
        if let RunState::Completed(answer) = &self.state {
            let answer = answer.clone();
            self.session.commit_round(
                AgentTurn {
                    text: answer,
                    calls: Vec::new(),
                },
                Vec::new(),
            )?;
        }
        // 恢复路径上本步的交代已在历史里：中断发生在步骤执行期间，交接
        // 内容属于快照。再推一次会让同一句指令出现两次，并导致步骤计数错误。
        //
        // 只查历史末尾不够：预算耗尽时末条是观察（该轮已提交），交代在更早
        // 的位置。prompt 由 step_prompt 唯一生成，全历史应只有一条。
        if !self.session.has_user_message(&prompt) {
            self.session.push_user(prompt);
        }
        self.spec = spec;
        self.refresh_role_boundary();
        self.state = RunState::AwaitingModel;
        self.pending = None;
        // 立即保存一次。若快照仍停在 Completed，恢复时会误以为任务已结束，
        // 而实际还有步骤未执行。
        self.save_snapshot();
        Ok(())
    }

    /// 追加一条用户消息到会话历史末尾。
    ///
    /// 与 [`Self::begin_step`] 的分工：`begin_step` 换角色、交代步骤并把上一步
    /// 最终答案提交成一轮；本方法只追加消息，不改变状态机。目前生产路径没有
    /// 调用点，`task_resume` 的结论交接走 [`Self::insert_user_message_before_step`]；
    /// 保留它是为了让测试能在不改状态机的前提下追加输入。
    ///
    /// 不保存快照：消息会随下一次保存点落盘。恢复刚发生时立刻保存，会把
    /// "刚恢复"与"刚交接"两个状态混在同一文件中，不易判断恢复位置。
    pub fn push_user_message(&mut self, message: impl Into<String>) {
        self.session.push_user(message);
    }

    /// 把一条用户消息插到当前步骤交代之前。
    ///
    /// 结论交接用：结论总结前几步，应位于"现在执行第几步"的交代之前。
    pub fn insert_user_message_before_step(&mut self, message: impl Into<String>) {
        let position = self.session.step_prompt_position();
        self.session.insert_user_at(position, message);
    }

    /// 按当前 `spec.role` 重算角色可调用的工具名。
    ///
    /// 查不到档案时返回 `None`（不限）。编排路径在调用 [`AgentRoster::find`]
    /// 时已把缺档案变成显式错误，因此这里角色必有档案，`None` 只表示档案
    /// 未配置白名单。
    fn refresh_role_boundary(&mut self) {
        self.allowed_tools = self
            .roster
            .allowed_tools(self.spec.role)
            .map(<[String]>::to_vec);
    }

    /// 检查调用是否越过当前角色的工具白名单。
    ///
    /// 返回 `Some(拒绝说明)` 表示越权。这项检查与注册表查找、策略裁决、审批
    /// 闸门相互独立：注册表判断工具是否存在，策略判断是否需审批，闸门决定
    /// 用户是否同意，这里判断角色是否有权提出该调用。缺少任一层都会留下
    /// 真实的越权路径。
    ///
    /// 拒绝说明必须指出这是权限问题而非审批问题。模型常会换参数重试同一
    /// 调用或请求审批，这两条路都走不通，说明边界后它才会改用可用工具。
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

    /// 检查角色边界，越权时记录拒绝观察并推进。
    ///
    /// 返回 `Ok(None)` 表示未越权，调用方继续。
    ///
    /// 检查逻辑只有一份，调用点有两处（`Evaluating` 与 `Dispatching`）。
    /// 恢复可以从 `Dispatching` 直接进入，跳过 `Evaluating` 的检查。若只在
    /// `Evaluating` 检查，停在 `Dispatching(write_file)` 的受限角色快照恢复后
    /// 会把写工具放行。
    ///
    /// `in_flight` 表示该调用为中断时正在执行的调用。越权时需额外说明
    /// "它是否执行过无法确认"：角色边界只约束**当前**能否调用，而该调用
    /// 可能在中断前已经执行过。只说"不允许调用"会让模型误以为从未执行，
    /// 从而原样重提。
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
                "{reason}（另注：这次调用在中断前可能已经发起，无法确认是否执行过；\
                 上面这条拒绝只说明现在不能执行它。）"
            )
        } else {
            reason
        };
        self.record_and_advance(call.clone(), ToolOutcome::refused(reason))
            .await
            .map(Some)
    }

    /// 判断调用是否为中断时正在执行的调用。
    ///
    /// 若是，它是否产生过副作用无法确认。恢复时不自动重跑，将决定权交回
    /// 模型（见 [`AgentRunner::replay_interrupted_call`]）。
    fn is_in_flight(&self, call: &ToolCall) -> bool {
        self.replay_pending
            .as_ref()
            .is_some_and(|pending| pending.id == call.id)
    }

    /// 构造审批请求并停在闸门前。
    ///
    /// `resumed` 表示恢复后的重新询问。闸门据此不让会话信任跳过本次询问，
    /// 用户据此知道上次问过但未答复。
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
            // 恢复路径上的那次询问发生在上一个进程，当前证据文件没有记录。
            // 补一条，否则查看 JSONL 会以为用户只被询问过一次。
            self.record(AuditEvent::ApprovalReasked {
                turn: request.turn,
                tool: request.call.name.clone(),
                reason: APPROVAL_REASK_REASON.to_owned(),
            });
        }
        self.state = RunState::AwaitingApproval(request.clone());
        // 关键保存点：等待审批时用户可能离开，进程也可能被杀。快照必须先
        // 落盘，恢复后才能把同一请求重新交给闸门。
        self.save_snapshot();
        RunState::AwaitingApproval(request)
    }

    /// 处理恢复时"中断时正在执行"的调用。
    ///
    /// **不执行它。** 快照保存在调用发起之前，恢复时无法判断该调用有没有
    /// 真正执行过：可能已产生副作用（文件已写、命令已跑），结果未知。重跑
    /// 会让同一次副作用执行两遍，当作没发生过则可能漏掉关键输出。处理方式：
    ///
    /// 1. 记录一条 [`AuditEvent::ToolCallReplayed`] 证据；
    /// 2. 向模型返回观察，说明该调用是否执行过无法确认、已知结果是什么、
    ///    是否重试由模型决定；
    /// 3. 本轮照常配平，模型下一轮能看到提示并自行决定。
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
        // 只对本次调用生效，提示完成后清除。
        self.replay_pending = None;
        let content = replay_notice(&call, previous.as_ref());
        self.record_and_advance(call, ToolOutcome::refused(content))
            .await
    }

    /// 查找调用在本次会话中此前的执行结果。
    ///
    /// 判据是工具名与参数完全一致：同一路径的同一写入、同一参数的同一命令。
    /// 参数不同则不是同一操作，用其他结果冒充会让模型误判（例如以为文件
    /// 已是新内容）。
    ///
    /// 先查本轮已完成的调用（尚未进入历史，但确实执行过），再查更早轮次。
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

    /// 推进一步。调用方决定何时停止，这里是超时、取消与恢复的挂点。
    ///
    /// # Errors
    ///
    /// 模型调用失败、会话配对被破坏或模型返回空回复时返回错误。出错后 runner
    /// 回到 [`RunState::AwaitingModel`]，已提交轮次保留在历史里，重新 `run`
    /// 会继续执行。
    pub async fn step(&mut self) -> Result<RunState, ApplicationError> {
        // 先取出状态再匹配，避免整个 match 期间借用 self.state。每个分支都会
        // 重新赋值，取出的默认值不会外泄。
        match std::mem::replace(&mut self.state, RunState::AwaitingModel) {
            RunState::AwaitingModel => {
                // 补充输入只在完整轮次配平后应用，历史里"助手轮 + 观察"的配对
                // 不被打散。应用在询问模型前，下一轮模型才看得见新输入。
                self.apply_inputs()?;
                let reply = self.ask_model().await?;
                if reply.turn.is_final_answer() {
                    self.state = RunState::Completed(reply.turn.text.clone());
                    // 终态也保存快照：完成时的最后状态对 --list-sessions 与
                    // 事后排查有价值。
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
                // 判断这次调用是否为中断时正在执行的那一条。
                let in_flight = self.is_in_flight(&call);
                // 角色边界先于其他检查：未注册检查、策略裁决与人工审批判断
                // "该调用能否发生"，这里判断"该角色是否有权提出"。放在最前面，
                // 越权调用不必进入策略与闸门，既省下一次注定被拒的审批，也
                // 避免审计中出现误导性的"策略已放行"。
                if let Some(state) = self.refuse_out_of_role(&call, in_flight).await? {
                    // 越权调用就此终止，重放提示不再适用。
                    if in_flight {
                        self.replay_pending = None;
                    }
                    return Ok(state);
                }
                // 恢复路径：该调用在中断前可能已发起，无法确认是否执行过。
                // 不自动重跑，把这个不确定度告诉模型，由它决定是否重试。
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

                // 守卫链：策略裁决在前，越界升级在后。审计仍记录策略原判：
                // 策略未变，变的只是本次调用是否需要用户过目，两者不能混在
                // 同一条事件里。
                let policy_guard = self
                    .guards
                    .iter()
                    .find_map(|guard| guard.as_policy_guard())
                    .expect("装配时必须提供策略守卫：PolicyEvaluated 证据与守卫一票同源");
                let decision = policy_guard.evaluate(&call, spec);
                self.record(AuditEvent::PolicyEvaluated {
                    turn: self.session.rounds() + 1,
                    tool: call.name.clone(),
                    verdict: decision.verdict,
                    rule_index: decision.rule_index,
                    reason: decision.reason.clone(),
                });

                let (verdict, strictest_index) =
                    run_guard_chain(&self.guards, &call, &GuardContext::new(spec));

                // 恢复时的重新审批标记只作用于本次调用，裁决后清除。必须在
                // 此时取走，否则策略放行时它会残留到下一次审批。
                let reask = std::mem::replace(&mut self.reask_approval, false);

                match verdict {
                    // 拒绝：不询问闸门，直接返回拒绝观察。理由需包含命中的
                    // 规则，模型才能选择其他路径。
                    GuardVerdict::Deny(reason) => {
                        let outcome = ToolOutcome::refused(denied_by_chain_message(
                            &call.name, &decision, &reason,
                        ));
                        self.record_and_advance(call, outcome).await
                    }
                    // 需要审批：策略自身的审批不带复核理由，只有策略之外的
                    // 守卫（越界升级）才带。闸门用 `reason` 的 Some/None 区分
                    // 复核与常规审批：复核不使用会话信任，每次都需用户确认；
                    // 常规审批允许 [a]/[p]。普通策略审批若带理由，会话信任会
                    // 整体失效。
                    GuardVerdict::RequireApproval(reason) => {
                        // 按守卫身份而非名字判断这一票是否来自策略守卫。名字
                        // 可能重复，不能承载"常规审批还是复核"的语义。
                        let from_policy = strictest_index
                            .and_then(|index| self.guards.get(index))
                            .map(|guard| guard.as_policy_guard().is_some())
                            .unwrap_or(false);
                        let escalation = (!from_policy).then_some(reason);
                        Ok(self.park_at_approval(call, spec.annotations, escalation, reask))
                    }
                    // 放行：不过闸门，直接进执行。
                    GuardVerdict::Pass => {
                        // 恢复路径的例外：该请求在中断前已被判为需要用户过目。
                        // 即使策略后来放宽，也不能替用户取消这一判断；用户
                        // 必须再确认一次，否则"未答复"会被误读为"已同意"。
                        if reask {
                            return Ok(self.park_at_approval(call, spec.annotations, None, true));
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
                    // 否决不是崩溃，生成拒绝观察，让模型换路径。
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
                // 恢复可能从 Dispatching 直接进入（快照停在半轮执行），跳过
                // Evaluating 的检查，因此这里必须再查一次。
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
                // 取消已请求且本调用尚未开始：不执行，以拒绝观察配平本轮，
                // 历史保持"助手轮 + 观察"完整，恢复时不会重发这次调用。
                if self.cancel.is_cancelled() {
                    let outcome = ToolOutcome::refused("运行已取消，该调用未执行".to_owned());
                    return self.record_and_advance(call, outcome).await;
                }
                // 宿主运行状态检查：审计不可靠时，在下一个工具前停下。已开始的
                // 调用不受影响（上面取消判断已放最前），只拦尚未发出的调用。
                self.check_control()?;
                // **执行前落盘**：调用一旦发起，进程可能在其执行期间退出。
                // 磁盘上必须留下"本次正要执行"的痕迹，否则恢复时无法判断
                // 是否应重跑。
                //
                // 状态机在 `step()` 开头已被换成 `AwaitingModel`，这里要显式
                // 写回 `Dispatching`。落盘状态必须是真实位置，否则快照会声称
                // 在等待模型，恢复时将当作什么都没发生。
                self.state = RunState::Dispatching(calls.clone());
                self.save_snapshot();
                self.hooks.before_tool_call(&call).await;
                let outcome = self.tools.invoke(&call).await;
                self.hooks.after_tool_result(&call, &outcome).await;
                self.record_and_advance(call, outcome).await
            }

            RunState::Completed(answer) => Ok(RunState::Completed(answer)),
        }
    }

    /// 运行到终局：循环调用 [`Self::step`]，直到给出最终答案，或遇到轮数、
    /// 墙钟、取消三种终点之一。
    ///
    /// # Errors
    ///
    /// 见 [`Self::step`]。达到 `max_turns` 仍无最终答案时返回
    /// [`ApplicationError::BudgetExhausted`]；总耗时超过
    /// `Budget::max_wall_clock` 时返回 [`ApplicationError::WallClockExhausted`]；
    /// 外部取消时返回 [`ApplicationError::Cancelled`]。三者可区分，调用方能
    /// 判断是超限还是被取消。
    pub async fn run(&mut self, budget: &Budget) -> Result<AgentOutcome, ApplicationError> {
        // 每次进入 run() 都重置测量锚点。基数取此前累计耗时：恢复运行来自
        // 快照，墙钟继续累计；全新运行基数为零，行为不变。
        self.run_start = self.clock.now();
        self.elapsed_base = self.consumption.elapsed;
        loop {
            // 每轮循环都重测耗时。Consumption 只是已测量结果的容器，测量
            // 必须走 Clock 端口，domain 不直接取时间。
            self.refresh_elapsed();
            // 取消、超时与轮数预算只在本轮准备询问模型前检查。正在分发的
            // 轮次允许跑完，避免已消耗的 token 半途作废。
            if matches!(self.state, RunState::AwaitingModel) {
                if self.cancel.is_cancelled() {
                    self.save_snapshot();
                    return Err(ApplicationError::Cancelled);
                }
                if budget.exhausted(&self.consumption) {
                    // exhausted 命中后先区分维度：两种超限的处理方式不同，
                    // 错误需要能指导调用方采取动作。
                    if let Some(limit) = budget.max_wall_clock
                        && self.consumption.elapsed >= limit {
                            self.save_snapshot();
                            return Err(ApplicationError::WallClockExhausted {
                                elapsed: self.consumption.elapsed,
                                limit,
                            });
                        }
                    self.save_snapshot();
                    return Err(ApplicationError::BudgetExhausted {
                        max_turns: budget.max_turns,
                    });
                }
            }
            match self.step().await {
                Ok(RunState::Completed(answer)) => {
                    // 取消可能发生在模型调用期间：回复抵达时闸门已经放行过，
                    // 循环顶部没有机会再检查。此时不能把取消后的回复报成成功。
                    if self.cancel.is_cancelled() {
                        self.save_snapshot();
                        return Err(ApplicationError::Cancelled);
                    }
                    return Ok(AgentOutcome {
                        answer,
                        turns_used: self.consumption.turns,
                    });
                }
                Ok(_) => {}
                // 出错也保存一次，任务未完成时下次可从这里继续。
                Err(error) => {
                    self.save_snapshot();
                    return Err(error);
                }
            }
        }
    }

    /// 问一次模型，并把"思考"记进证据。
    async fn ask_model(&mut self) -> Result<ModelReply, ApplicationError> {
        // 先克隆 Arc，否则 request 会借用 self 的字段，而这些字段随后要修改。
        let model = self.model.clone();
        // 先按策略探测视图。若被裁轮次值得摘要且现有摘要未覆盖，先补摘要再
        // 正式请求；摘要补完后，下面重新生成的视图会包含它。
        let probe_view =
            self.context_policy
                .view(&self.session, &self.context_budget, self.session.summary());
        let probe = CompactionReport::measure(&self.session, &probe_view);
        if !probe.dropped.is_empty() && self.should_summarize(&probe.dropped) {
            self.refresh_summary(&probe.dropped).await;
        }
        // 裁剪是发给模型前的视图变换，请求只包含模型实际看到的内容。历史
        // 被裁剪时记录证据，便于事后从 JSONL 查对"第 N 轮模型看到了什么"。
        // 报告按实际视图与完整历史现测，换策略也能如实反映。证据沿用既有
        // 口径：仅整轮被裁时记录，按片裁剪的策略不记事件。
        let history =
            self.context_policy
                .view(&self.session, &self.context_budget, self.session.summary());
        let report = CompactionReport::measure(&self.session, &history);
        if !report.dropped.is_empty() {
            self.record(AuditEvent::ContextCompacted {
                turn: self.session.rounds() + 1,
                rounds_before: report.rounds_before,
                rounds_after: report.rounds_after,
                dropped: report.dropped,
            });
            // token 单独记一条：轮数只是手段，token 才是实际成本。
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
            // 会话 ID 作路由键：同一会话每轮必须使用同一个键，否则 provider
            // 会把请求路由到不同机器，前缀缓存无法命中。
            cache_key: Some(self.session.id().as_str()),
        };
        self.hooks.before_model_call(&request).await;
        let reply = model.complete(request.clone()).await?;
        // 空回复是模型端的瞬态故障（实测出现过计费了数十个输出 token 却没有
        // 文本或调用的情况），不代表任务结束。重试同一请求一次，仍为空才报错。
        // 重试不计入消耗轮与预算。
        let reply = if reply.turn.text.trim().is_empty() && reply.turn.calls.is_empty() {
            eprintln!("警告：模型返回空回复，重试一次");
            let retried = model.complete(request).await?;
            self.hooks.after_model_reply(&retried, true).await;
            retried
        } else {
            self.hooks.after_model_reply(&reply, false).await;
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

    /// 判断本次裁剪是否值得生成或更新摘要。
    ///
    /// 三个条件都满足才生成：失败次数未达运行内累计上限（否则每轮重试都
    /// 会产生一次无效模型调用，达到上限后退回占位说明）；被裁轮次达到
    /// 策略阈值（小额裁剪不值得额外调用）；已有摘要未覆盖全部被裁轮次。
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
    /// 轮次到步号的映射取决于历史结构：每条非交接的用户消息代表一步的交代，
    /// 统计用户消息即可确定每轮所属步骤。
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

    /// 生成并挂载摘要。摘要是优化项：失败时打印警告并继续使用占位说明，
    /// 不把错误抛给任务。
    ///
    /// 摘要调用计入 `consumption`（这是一次真实的模型调用，预算需如实
    /// 记录），并留下 [`AuditEvent::SummaryGenerated`] 供事后对账。
    async fn refresh_summary(&mut self, dropped: &[u32]) {
        let turn = self.session.rounds() + 1;
        // 先同步渲染为 owned 文本。`ContextSlice` 借用 session，不能跨过
        // 下面的 await。
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
        // 摘要覆盖的步骤由代码计算：每条非交接的用户消息代表一步的交代，
        // 统计即可确定每轮所属步骤。模型可能漏写或写错编号，而后续模型
        // 要靠这个信息判断进度。
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
        self.pending
            .as_ref()
            .ok_or(ApplicationError::NoPendingRound)
    }

    /// 记录一条观察并推进：仍有调用未处理则回到 `Evaluating`；全部处理完
    /// 则原子提交本轮，回到 `AwaitingModel`。
    async fn record_and_advance(
        &mut self,
        call: ToolCall,
        outcome: ToolOutcome,
    ) -> Result<RunState, ApplicationError> {
        let turn = self.session.rounds() + 1;
        let mut outcome = outcome;
        // 执行器报告边界事实，应用层只负责转换为证据；观察文本不再作为判据。
        let boundary = outcome.execution.take();
        // MCP 故障事实同样只在进程内交接：取走后才记录 `ToolObserved`，
        // 观察与快照中不会保留第二份事实。
        let mcp_failure = outcome.mcp_failure.take();
        self.record(AuditEvent::ToolObserved {
            turn,
            call: call.clone(),
            outcome: outcome.clone(),
        });
        if let Some(failure) = mcp_failure {
            self.record(AuditEvent::McpCallFailed {
                turn,
                call_id: call.id.clone(),
                tool: call.name.clone(),
                failure,
            });
        }
        if let Some(boundary) = boundary {
            self.record(AuditEvent::SandboxApplied {
                turn,
                call,
                platform: boundary.platform.to_owned(),
                active: boundary.active,
                boundary_checked: boundary.boundary_checked,
            });
        }

        // 在独立块中修改 pending 并取出决策依据，避免整个函数持有借用后
        // 无法修改 self.state。
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

        // 观察齐全后才提交：助手消息与其观察在历史中始终相邻。
        let pending = self
            .pending
            .take()
            .ok_or(ApplicationError::NoPendingRound)?;
        self.session.commit_round(pending.turn, pending.outcomes)?;
        self.state = RunState::AwaitingModel;
        // 历史增长点。恢复从这里开始最干净，没有半轮状态。
        self.save_snapshot();
        Ok(RunState::AwaitingModel)
    }

    /// 记一条证据，时间戳由时钟取。
    fn record(&self, event: AuditEvent) {
        self.evidence.record(self.clock.now(), event);
    }

    /// 将墙钟重新测量写入 `consumption`。所有保存点都先调用它：`step()` 内部
    /// 的保存点位于 `run()` 循环计时之外，不重测会让快照中的已耗时停留在
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

    /// 普通运行沿用快照失败时警告的策略；补充输入确认需要严格保存。
    fn save_snapshot(&mut self) {
        if let Err(error) = self.try_save_snapshot() {
            eprintln!("警告：快照保存失败（任务继续）: {error}");
        }
    }

    fn try_save_snapshot(&mut self) -> Result<(), StoreError> {
        let Some(store) = self.session_store.clone() else {
            return Ok(());
        };
        self.refresh_elapsed();
        store.save(&self.snapshot())
    }
}

/// 生成会话 ID。
///
/// 不引入 `uuid`：只需要一次运行内唯一且日志可辨认。进程号加纳秒时间戳
/// 已足够，也不增加依赖。
///
/// 公开是因为 `main.rs` 需要先知道 ID 才能给证据文件命名，否则 runner
/// 内部生成一个、外部计算另一个，文件名与内容会不一致。
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

/// 构造拒绝说明。
///
/// 拒绝票来自策略守卫时沿用原模板，区分"规则 #N 拒绝"与"默认策略拒绝"；
/// 来自其他守卫时给出守卫自己的理由。只说"被禁止"会让模型无法判断该换
/// 参数、换工具还是放弃，只会盲目重试同一调用。
fn denied_by_chain_message(name: &str, decision: &PolicyDecision, guard_reason: &str) -> String {
    // 理由与策略原判一致，说明这一票来自策略守卫，按原模板处理。
    if guard_reason == decision.reason {
        return match decision.rule_index {
            Some(index) => format!(
                "工具 `{name}` 被策略规则 #{index} 拒绝：{}。请换一种方式完成，不要重试同一调用。",
                decision.reason
            ),
            None => format!("工具 `{name}` 被默认策略拒绝：{}", decision.reason),
        };
    }
    format!("工具 `{name}` 被拒绝：{guard_reason}。请换一种方式完成，不要重试同一调用。")
}

/// 构造恢复时给模型的重放提示。
///
/// 需要回答三个问题：该调用是否执行过、结果是什么、现在该怎么做。缺少
/// 任何一项，模型都会自行猜测，而通常猜成"再跑一遍"，导致重复副作用。
fn replay_notice(call: &ToolCall, previous: Option<&ToolOutcome>) -> String {
    let known = match previous {
        Some(outcome) => format!("同一调用此前执行过的结果是：{}。", outcome.content),
        None => "这次执行的结果未知（中断就发生在它执行期间）。".to_owned(),
    };
    format!(
        "【中断恢复】工具 `{}` 在中断前可能已经发起执行，是否真的执行过无法确认，\
         副作用也可能已经产生，因此**没有自动再跑一次**。{known}\
         要不要再执行一次由你判断：需要就重新发起同一个调用，\
         不需要就用现有信息继续。",
        call.name
    )
}

/// 内置守卫链：策略裁决在前，越界升级在后（升级只对放行的调用生效）。
///
/// `PolicyGuard` 持有引擎本体，Runner 不再单独保存一份。两处各存一份
/// `PolicyEngine` 虽然是值类型、互不干扰，但存在配置改一处漏一处的风险。
fn default_guard_chain(policy: PolicyEngine) -> Vec<Arc<dyn ToolGuard>> {
    vec![
        Arc::new(PolicyGuard::new(policy)),
        Arc::new(BoundaryEscalationGuard),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::context_policy::DefaultContextPolicy;
    use crate::application::memory_evidence::MemoryEvidenceSink;
    use crate::application::port::{BoxFuture, Decision, NoopHooks, ToolInvoker};
    use crate::domain::agent::AgentRole;
    use crate::domain::evidence::{AuditEvent, TokenUsage};
    use crate::domain::ids::CallId;
    use crate::domain::policy::PolicyVerdict;
    use crate::domain::tool::{OutcomeStatus, ToolAnnotations};
    use crate::infrastructure::model::rig_chat::RigChatGateway;
    use rig::completion::Message;
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

    /// 将一组规格包成注册表，全部指向同一个执行器。
    ///
    /// 大多数测试只关心模型调用了什么，不关心由谁执行。
    fn registry_with(invoker: Arc<dyn ToolInvoker>, specs: Vec<ToolSpec>) -> ToolRegistry {
        ToolRegistry::new(
            specs
                .into_iter()
                .map(|spec| crate::application::tool_registry::ToolEntry {
                    spec,
                    invoker: invoker.clone(),
                })
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

    /// 破坏性工具规格，用于验证审批接缝。
    fn write_file_spec() -> ToolSpec {
        ToolSpec {
            name: "write_file".to_owned(),
            description: "写文件".to_owned(),
            parameters: json!({"type": "object"}),
            annotations: ToolAnnotations::new(false, true, false, false),
        }
    }

    /// 记录调用但不实际执行的执行器。边界事实按需附加，模拟真实执行器。
    #[derive(Default)]
    struct RecordingInvoker {
        calls: std::sync::Mutex<Vec<String>>,
        boundary: Option<crate::domain::sandbox::ExecutionBoundary>,
        /// 附加到观察上的 MCP 故障事实，验证应用层只转述、不推断。
        mcp_failure: Option<crate::domain::mcp::McpCallFailure>,
    }

    impl RecordingInvoker {
        fn with_boundary(boundary: crate::domain::sandbox::ExecutionBoundary) -> Self {
            Self {
                calls: Default::default(),
                boundary: Some(boundary),
                mcp_failure: None,
            }
        }

        fn with_mcp_failure(
            boundary: crate::domain::sandbox::ExecutionBoundary,
            failure: crate::domain::mcp::McpCallFailure,
        ) -> Self {
            Self {
                calls: Default::default(),
                boundary: Some(boundary),
                mcp_failure: Some(failure),
            }
        }
    }

    impl ToolInvoker for RecordingInvoker {
        fn invoke<'a>(&'a self, call: &'a ToolCall) -> BoxFuture<'a, ToolOutcome> {
            Box::pin(async move {
                self.calls.lock().unwrap().push(call.name.clone());
                let content = match call.name.as_str() {
                    "empty_command" => "command 不能为空".to_owned(),
                    "git_status" => "不是 git 仓库根".to_owned(),
                    _ => "文件内容".to_owned(),
                };
                let status = match call.name.as_str() {
                    "empty_command" | "git_status" => OutcomeStatus::Failed,
                    _ => OutcomeStatus::Success,
                };
                let mut outcome = ToolOutcome {
                    content,
                    status,
                    execution: None,
                    mcp_failure: None,
                };
                if let Some(boundary) = self.boundary {
                    outcome = outcome.with_execution(boundary);
                }
                if let Some(failure) = self.mcp_failure.clone() {
                    outcome = outcome.with_mcp_failure(failure);
                }
                outcome
            })
        }
    }

    /// 记录闸门被询问的次数与内容。
    #[derive(Default)]
    struct CountingGate {
        requested: std::sync::Mutex<Vec<String>>,
        /// 按顺序保存收到的完整请求。越界写入升级测试需要断言 `reason`。
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

    /// 始终否决的闸门，验证否决会变成拒绝观察而非崩溃。
    struct DenyAllGate;

    impl ApprovalGate for DenyAllGate {
        fn request<'a>(&'a self, _request: &'a ApprovalRequest) -> BoxFuture<'a, Decision> {
            Box::pin(async { Err("不允许".to_owned()) })
        }
    }

    /// 组装好的 runner 及可断言的把手。
    struct Harness {
        runner: AgentRunner,
        /// mock 与 gateway 共享脚本和请求记录，可在事后断言。
        mock: MockCompletionModel,
        evidence: Arc<MemoryEvidenceSink>,
        approvals: Arc<CountingGate>,
    }

    /// 用脚本化的模型回合组装 runner，无需 API key，也不走网络。
    fn harness(turns: Vec<MockTurn>, tools: Vec<ToolSpec>) -> Harness {
        harness_with_policy(turns, tools, PolicyEngine::new())
    }

    /// 同上，但可指定策略引擎。
    fn harness_with_policy(
        turns: Vec<MockTurn>,
        tools: Vec<ToolSpec>,
        policy: PolicyEngine,
    ) -> Harness {
        harness_with_invoker(turns, tools, policy, Arc::new(RecordingInvoker::default()))
    }

    /// 同上，但可指定执行器返回的边界事实。
    fn harness_with_invoker(
        turns: Vec<MockTurn>,
        tools: Vec<ToolSpec>,
        policy: PolicyEngine,
        invoker: Arc<dyn ToolInvoker>,
    ) -> Harness {
        harness_full(
            turns,
            tools,
            policy,
            invoker,
            Arc::new(NoopHooks),
            Vec::new(),
        )
    }

    /// 同上，但可指定运行挂点与追加守卫。Hook 与守卫用例从这里进入。
    fn harness_full(
        turns: Vec<MockTurn>,
        tools: Vec<ToolSpec>,
        policy: PolicyEngine,
        invoker: Arc<dyn ToolInvoker>,
        hooks: Arc<dyn RunHooks>,
        additional_guards: Vec<Arc<dyn ToolGuard>>,
    ) -> Harness {
        let mock = MockCompletionModel::from_turns(turns);
        let evidence = Arc::new(MemoryEvidenceSink::new());
        let approvals = Arc::new(CountingGate::default());
        let registry = registry_with(invoker, tools);
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
                context_policy: Arc::new(DefaultContextPolicy),
                additional_guards,
                session_store: None,
                hooks,
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

    /// 构造一个助手工具调用块，供"一轮多个调用"的测试使用。
    ///
    /// `MockTurn::tool_call` 一次只能包含一个调用，多个调用需要走
    /// `from_contents`，因此要自行构造 `AssistantContent`。
    fn call_content(id: &str, name: &str, arguments: serde_json::Value) -> AssistantContent {
        AssistantContent::tool_call(
            id,
            ToolName::new(name).expect("测试里的工具名应合法"),
            arguments,
        )
    }

    /// 按事件顺序记录运行挂点收到的调用，用于断言时序。
    #[derive(Default)]
    struct RecordingHooks {
        events: std::sync::Mutex<Vec<String>>,
    }

    impl RecordingHooks {
        fn recorded(&self) -> Vec<String> {
            self.events.lock().unwrap().clone()
        }
    }

    impl RunHooks for RecordingHooks {
        fn before_model_call<'a>(&'a self, request: &'a ModelRequest<'a>) -> BoxFuture<'a, ()> {
            Box::pin(async move {
                self.events
                    .lock()
                    .unwrap()
                    .push(format!("before_model_call:{}条历史", request.history.len()));
            })
        }

        fn after_model_reply<'a>(
            &'a self,
            reply: &'a ModelReply,
            retried: bool,
        ) -> BoxFuture<'a, ()> {
            Box::pin(async move {
                self.events.lock().unwrap().push(format!(
                    "after_model_reply:重试={retried},调用数={}",
                    reply.turn.calls.len()
                ));
            })
        }

        fn before_tool_call<'a>(&'a self, call: &'a ToolCall) -> BoxFuture<'a, ()> {
            Box::pin(async move {
                self.events
                    .lock()
                    .unwrap()
                    .push(format!("before_tool_call:{}", call.name));
            })
        }

        fn after_tool_result<'a>(
            &'a self,
            call: &'a ToolCall,
            outcome: &'a ToolOutcome,
        ) -> BoxFuture<'a, ()> {
            let status = match outcome.status {
                OutcomeStatus::Success => "success",
                OutcomeStatus::Failed => "failed",
                OutcomeStatus::Refused => "refused",
            };
            Box::pin(async move {
                self.events
                    .lock()
                    .unwrap()
                    .push(format!("after_tool_result:{}:{status}", call.name));
            })
        }
    }

    // ── 运行挂点：观察点齐全且不打扰状态机 ──────────────────────────────

    #[tokio::test]
    async fn hooks_observe_every_model_call_and_tool_execution() {
        let hooks = Arc::new(RecordingHooks::default());
        let mut harness = harness_full(
            vec![
                MockTurn::tool_call("call_1", "read_file", json!({"path": "Cargo.toml"})),
                MockTurn::text("读完了。"),
            ],
            vec![read_file_spec()],
            PolicyEngine::new(),
            Arc::new(RecordingInvoker::default()),
            hooks.clone(),
            Vec::new(),
        );

        let outcome = harness.runner.run(&Budget::new(20)).await.unwrap();

        assert_eq!(outcome.turns_used, 2);
        assert_eq!(
            hooks.recorded(),
            vec![
                "before_model_call:1条历史",
                "after_model_reply:重试=false,调用数=1",
                "before_tool_call:read_file",
                "after_tool_result:read_file:success",
                // 第二次往返带任务、上一轮与它的观察。
                "before_model_call:2条历史",
                "after_model_reply:重试=false,调用数=0",
            ],
            "挂点应按真实顺序覆盖两次模型往返与一次工具执行"
        );
    }

    // ── 守卫链：装配期追加的守卫真的参与裁决 ──────────────────────────

    #[tokio::test]
    async fn assembly_can_append_a_custom_guard_that_changes_the_verdict() {
        struct DenyReads;

        impl ToolGuard for DenyReads {
            fn name(&self) -> &str {
                "deny-reads"
            }

            fn inspect(&self, call: &ToolCall, _ctx: &GuardContext<'_>) -> GuardVerdict {
                GuardVerdict::Deny(format!("测试守卫拒绝 {}", call.name))
            }
        }

        let invoker = Arc::new(RecordingInvoker::default());
        let mut harness = harness_full(
            vec![
                MockTurn::tool_call("call_1", "read_file", json!({"path": "Cargo.toml"})),
                MockTurn::text("被拒了，换条路。"),
            ],
            vec![read_file_spec()],
            PolicyEngine::new(),
            invoker.clone(),
            Arc::new(NoopHooks),
            vec![Arc::new(DenyReads) as Arc<dyn ToolGuard>],
        );

        let outcome = harness.runner.run(&Budget::new(20)).await.unwrap();

        assert_eq!(outcome.turns_used, 2, "拒绝观察应回填给模型再走一轮");
        assert!(
            invoker.calls.lock().unwrap().is_empty(),
            "被守卫拒绝的调用不得落到执行器"
        );
        assert!(
            harness.approvals.requested_tools().is_empty(),
            "直接拒绝不该去问审批闸门"
        );
    }

    /// 守卫名可以重复，但不能改变审批语义。冒充 `policy` 的自定义守卫投出的
    /// `RequireApproval` 仍按复核处理，理由写入 ApprovalRequest。
    #[tokio::test]
    async fn a_guard_named_policy_does_not_get_policy_semantics() {
        struct Impostor;

        impl ToolGuard for Impostor {
            fn name(&self) -> &str {
                "policy"
            }

            fn inspect(&self, _call: &ToolCall, _ctx: &GuardContext<'_>) -> GuardVerdict {
                GuardVerdict::RequireApproval("冒充策略的复核".to_owned())
            }
        }

        let mut harness = harness_full(
            vec![
                MockTurn::tool_call("call_1", "read_file", json!({"path": "Cargo.toml"})),
                MockTurn::text("等审批。"),
            ],
            vec![read_file_spec()],
            PolicyEngine::new(),
            Arc::new(RecordingInvoker::default()),
            Arc::new(NoopHooks),
            vec![Arc::new(Impostor) as Arc<dyn ToolGuard>],
        );

        harness.runner.run(&Budget::new(20)).await.unwrap();

        assert_eq!(
            harness.approvals.first_request().reason.as_deref(),
            Some("冒充策略的复核"),
            "名字叫 policy 不等于它是策略守卫"
        );
    }

    // ── 上下文策略：注入的策略真的被 Runner 使用 ──────────────────────

    #[tokio::test]
    async fn a_custom_context_policy_is_actually_used_by_the_runner() {
        struct KeepEverything {
            calls: std::sync::atomic::AtomicUsize,
        }

        impl ContextPolicy for KeepEverything {
            fn view<'a>(
                &self,
                session: &'a Session,
                _budget: &ContextBudget,
                _summary: Option<&crate::domain::context::Summary>,
            ) -> Vec<ContextSlice<'a>> {
                self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                session.slices()
            }
        }

        let policy = Arc::new(KeepEverything {
            calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let mock = MockCompletionModel::from_turns(vec![
            MockTurn::tool_call("call_1", "read_file", json!({"path": "Cargo.toml"})),
            MockTurn::text("读完了。"),
        ]);
        let evidence = Arc::new(MemoryEvidenceSink::new());
        let mut runner = AgentRunner::new(
            spec_with(),
            SessionId::new("test-session"),
            "读一下 Cargo.toml",
            AgentDependencies {
                model: Arc::new(RigChatGateway::new(mock.clone().erase())),
                tools: registry_with(
                    Arc::new(RecordingInvoker::default()),
                    vec![read_file_spec()],
                ),
                approvals: Arc::new(CountingGate::default()),
                evidence: evidence.clone(),
                clock: Arc::new(crate::infrastructure::clock::SystemClock),
                policy: PolicyEngine::new(),
                additional_guards: Vec::new(),
                roster: AgentRoster::builtin(),
                // 紧预算：默认策略会裁掉整轮，自定义策略全量保留。
                context_budget: ContextBudget {
                    max_tokens: Some(1),
                    min_recent_rounds: 0,
                    keep_task: true,
                    summarize: None,
                },
                context_policy: policy.clone(),
                session_store: None,
                hooks: Arc::new(NoopHooks),
            },
        );

        let outcome = runner.run(&Budget::new(20)).await.unwrap();

        assert_eq!(outcome.turns_used, 2);
        assert_eq!(
            policy.calls.load(std::sync::atomic::Ordering::SeqCst),
            4,
            "两次模型往返各调用一次探针视图与一次终稿视图"
        );
        let compacted = evidence
            .events()
            .into_iter()
            .any(|entry| matches!(entry.event, AuditEvent::ContextCompacted { .. }));
        assert!(
            !compacted,
            "自定义策略没裁整轮，不该产生 ContextCompacted 证据"
        );
    }

    #[tokio::test]
    async fn noop_hooks_run_a_normal_task_untouched() {
        // 缺省挂点不改变行为：与"能连续调用工具"同一剧本，只是不传入自定义 hooks。
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

    // ── 一轮多个调用 ────────────────────────────────────────────────────

    /// 一次回复包含多个工具调用时，全部执行并各返回一条观察。
    ///
    /// 请求显式设置 `parallel_tool_calls(true)`（见
    /// `infrastructure::model::rig_chat::generation_options`），模型因此会在
    /// 一轮中给出多个调用。本测试锁定三点：一条不漏、顺序不乱、各自配对
    /// 观察。错位配对会让模型把 A 的结果当成 B 的，后续任务建立在错误前提上
    /// 却表面正常。
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
        // 三条调用只消耗一次模型往返，轮数不随调用数增长，这是本测试的核心。
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
        assert_eq!(observed, requested, "每条调用都要配到自己的观察，不能错位");

        // 审批只问写工具：两个只读调用照常跳过闸门。
        assert_eq!(harness.approvals.requested_tools(), vec!["write_file"]);
    }

    /// 一轮多个调用时，历史仍是一条助手消息加一组 tool 结果。
    ///
    /// OpenAI 兼容端点要求同一轮的所有 tool 结果跟在同一条助手消息之后，
    /// 拆成多组会导致请求返回 400。本测试锁定翻译层的配对形状。
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
                hooks: Arc::new(NoopHooks),
                context_policy: Arc::new(DefaultContextPolicy),
                additional_guards: Vec::new(),
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
        // 第一次空回复视为瞬态故障：重试同一请求应消费后面的脚本回合。
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

        // 协议层验证配对：最后一个请求中，每个带 tool_calls 的助手消息后必须
        // 紧跟一条 tool 结果消息，否则 OpenAI 兼容端点会返回 400。
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

    // ── 策略引擎接入 ────────────────────────────────────────────────────

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
        // 基线行为：默认策略下 write_file 仍要过闸门。
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
        // run_command（open_world）未在策略引擎中登记规则，默认策略按注解把它
        // 送入闸门。本测试锁定"新增高危工具无需修改策略引擎"这一接缝。
        //
        // 命令刻意避开默认基线放行的 cargo test / build、git status / diff：
        // 要验证的是基线之外的命令仍需用户确认，用 `cargo test` 测不到这一点。
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

    /// 默认基线放行的命令不询问用户，节省等待成本。
    ///
    /// 与上一条配对：一条锁定"高危仍需审批"，一条锁定"无害命令不再询问"。
    /// 只测前者时，若有人删除基线，测试仍会全绿，等待成本会回升。
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
        let policy = policy_with("write_file", PolicyVerdict::Deny, "测试规则：禁止写文件");
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
        // 拒绝观察需说明命中的规则和理由，模型才能选择其他路径。
        let text = refusal_text(&harness.mock, 1);
        assert!(text.contains("write_file"), "应点名工具: {text}");
        assert!(text.contains("策略规则 #0"), "应指明规则序号: {text}");
        assert!(
            text.contains("测试规则：禁止写文件"),
            "应带上规则理由: {text}"
        );
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

    /// 策略自身的审批不带复核理由。
    ///
    /// `ApprovalRequest.reason` 的 Some/None 是闸门区分复核与常规审批的唯一
    /// 信号：Some 会禁用会话信任（[a]/[p]）。普通策略审批若带理由，用户就
    /// 无法通过一次 a/p 减少后续询问。
    #[tokio::test]
    async fn policy_approval_carries_no_review_reason() {
        let policy = policy_with("write_file", PolicyVerdict::RequireApproval, "写文件要审");
        let mut harness = harness_with_policy(
            vec![
                MockTurn::tool_call("call_1", "write_file", json!({"path": "a.txt"})),
                MockTurn::text("写完了"),
            ],
            vec![read_file_spec(), write_file_spec()],
            policy,
        );

        harness.runner.run(&Budget::new(20)).await.unwrap();

        assert_eq!(harness.approvals.requested_tools(), vec!["write_file"]);
        let request = harness.approvals.first_request();
        assert!(
            request.reason.is_none(),
            "常规策略审批不得带复核理由（会禁用会话信任），实际: {:?}",
            request.reason
        );
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

    // ── 越界写入升级 ────────────────────────────────────────────────────

    /// 策略放行 `run_command`，但命令将输出写到 workspace 外时，必须升级为
    /// 需要审批。
    ///
    /// 这是升级检查的核心承诺：策略按工具名与参数文本放行后，命令内容不再受
    /// 约束，升级检查补上这个缺口。
    #[tokio::test]
    async fn allow_rule_still_gates_an_out_of_workspace_write() {
        let policy = policy_with("run_command", PolicyVerdict::Allow, "shell 免审");
        let run_command_spec = ToolSpec {
            name: "run_command".to_owned(),
            description: "跑命令".to_owned(),
            parameters: json!({"type": "object"}),
            annotations: ToolAnnotations::new(false, true, false, true),
        };
        let command = r"echo hi > C:\Windows\temp\x";
        let mut harness = harness_with_policy(
            vec![
                MockTurn::tool_call("call_1", "run_command", json!({"command": command})),
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
        // 升级理由要传到闸门，用户需要知道为什么复核。
        // 理由必须与嫌疑文本完全一致：`Some` 本身就是"这是复核"的信号，添加
        // 解释性前缀会改变审批请求的既有形状。
        let suspect = crate::domain::sandbox::write_escape_suspect(command)
            .expect("这条命令应被判为越界嫌疑");
        let request = harness.approvals.first_request();
        assert_eq!(
            request.reason.as_deref(),
            Some(suspect.as_str()),
            "升级理由应原样携带嫌疑文本"
        );
    }

    /// 相同的 Allow 规则下，workspace 内的命令不被升级。
    ///
    /// 缺少这条约束，升级会退化为"所有 run_command 都要审批"。用户被无谓
    /// 打扰后会习惯性放行，升级失去作用。
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

    /// 默认策略的基线放行 `git diff *` 一族，但该命令通过 git 的 `--output`
    /// 选项把内容写到 workspace 外。
    ///
    /// 它既没有写入型动词也没有 `>`，两个启发式判据都会漏掉，而基线又将其
    /// 放行。本机实测 `git diff --output=D:\x` 确实会写盘，存在真实风险。
    #[tokio::test]
    async fn default_baseline_still_gates_a_diff_that_writes_outside() {
        let run_command_spec = ToolSpec {
            name: "run_command".to_owned(),
            description: "跑命令".to_owned(),
            parameters: json!({"type": "object"}),
            annotations: ToolAnnotations::new(false, true, false, true),
        };
        let command = r"git diff --output=C:\Windows\temp\x";
        let mut harness = harness_with_policy(
            vec![
                MockTurn::tool_call("call_1", "run_command", json!({"command": command})),
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
        let suspect = crate::domain::sandbox::write_escape_suspect(command)
            .expect("这条命令应被判为越界嫌疑");
        assert_eq!(
            harness.approvals.first_request().reason.as_deref(),
            Some(suspect.as_str()),
            "理由应原样携带嫌疑文本"
        );
    }

    /// 审计仍记录策略原判（Allow），不记录升级后的裁决。
    ///
    /// 策略未变，变的只是本次调用是否需要用户过目。两者混在一条
    /// `PolicyEvaluated` 中，事后无法区分是配置放宽还是启发式报警。
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

    /// 策略层升级、用户放行后，硬边界仍然生效。审批闸门与执行层边界是两道
    /// 独立的闸。
    ///
    /// 本测试不验证升级判据，只锁定"用户同意后执行层仍可拒绝"这一落差，
    /// 避免误以为审批能覆盖机械边界，或把行为改成静默放行。
    #[tokio::test]
    async fn approval_cannot_override_the_tool_level_boundary_refusal() {
        let escape = r"echo hi > C:\Windows\temp\x";
        let boundary_path = crate::domain::workspace::WorkspaceRoot::new("D:/repo");
        assert!(
            crate::domain::sandbox::write_escape_beyond(escape, boundary_path.as_path()).is_some(),
            "本用例的命令必须真的越过根"
        );
        let policy = policy_with("run_command", PolicyVerdict::Allow, "shell 免审");
        let spec = ToolSpec {
            name: "run_command".to_owned(),
            description: "跑命令".to_owned(),
            parameters: json!({"type": "object"}),
            annotations: ToolAnnotations::new(false, true, false, true),
        };
        let mut harness = harness_with_policy(
            vec![
                MockTurn::tool_call("call_1", "run_command", json!({"command": escape})),
                MockTurn::text("看到拒绝"),
            ],
            vec![spec],
            policy,
        );

        harness.runner.run(&Budget::new(20)).await.unwrap();

        assert_eq!(harness.approvals.requested_tools(), vec!["run_command"]);
        let decisions: Vec<_> = harness
            .evidence
            .events()
            .into_iter()
            .filter_map(|entry| match entry.event {
                AuditEvent::ApprovalDecided { allowed, .. } => Some(allowed),
                _ => None,
            })
            .collect();
        assert_eq!(decisions, vec![true], "升级后闸门应被问过并获批准");
        assert!(
            boundaries(&harness).is_empty(),
            "未执行的拒绝调用不能记边界证据"
        );
    }

    // ── 执行边界证据 ──────────────────────────────────────────────────

    /// 取出证据里全部 `SandboxApplied`，断言时只看这条通道说了什么。
    fn boundaries(harness: &Harness) -> Vec<(String, bool, bool)> {
        harness
            .evidence
            .events()
            .into_iter()
            .filter_map(|e| match e.event {
                AuditEvent::SandboxApplied {
                    platform,
                    active,
                    boundary_checked,
                    ..
                } => Some((platform, active, boundary_checked)),
                _ => None,
            })
            .collect()
    }

    /// 实际执行过的 `run_command` 必须留下边界记录，内容以执行器提供的事实
    /// 为准。
    ///
    /// 这是"高风险工具的能力边界由运行时验证"在证据侧的一半：策略与审批
    /// 记录是否允许执行，这条记录执行时是否有保护。
    #[tokio::test]
    async fn a_ran_command_leaves_a_boundary_record() {
        let run_command_spec = ToolSpec {
            name: "run_command".to_owned(),
            description: "跑命令".to_owned(),
            parameters: json!({"type": "object"}),
            annotations: ToolAnnotations::new(false, true, false, true),
        };
        let invoker = Arc::new(RecordingInvoker::with_boundary(
            crate::domain::sandbox::ExecutionBoundary::process(true, true),
        ));
        let mut harness = harness_with_invoker(
            vec![
                MockTurn::tool_call("call_1", "run_command", json!({"command": "cargo test"})),
                MockTurn::text("跑完了"),
            ],
            vec![read_file_spec(), run_command_spec],
            PolicyEngine::new(),
            invoker,
        );

        harness.runner.run(&Budget::new(20)).await.unwrap();

        assert_eq!(
            boundaries(&harness),
            vec![(
                crate::domain::sandbox::PlatformCapabilities::detect()
                    .label()
                    .to_owned(),
                true,
                true
            )],
            "执行成功且未降级：一条生效的沙箱记录"
        );
    }

    /// MCP 调用无论是否进入 Job 都要留下边界记录，`active` 反映实际事实。
    ///
    /// 漏记比记错更危险：查看 JSONL 会以为所有执行都在沙箱中。两个分支都测，
    /// 因为报表的 `sandbox_protected_runs` 只看 `active`：纳入成功却写 false
    /// 会压低保护数，纳入失败却写 true 会抬高保护数，并抹掉命令行上的
    /// "本次执行无进程沙箱保护"提示。
    #[tokio::test]
    async fn mcp_boundary_records_follow_process_isolation() {
        for process_isolation in [false, true] {
            let mcp_spec = ToolSpec {
                name: "mcp__fs__read".to_owned(),
                description: "外部 Server 的读工具".to_owned(),
                parameters: json!({"type": "object"}),
                // 只读注解使它跳过审批，直接执行；本测试验证证据，不验证闸门。
                annotations: ToolAnnotations::new(true, false, true, true),
            };
            let invoker = Arc::new(RecordingInvoker::with_boundary(
                crate::domain::sandbox::ExecutionBoundary::mcp(process_isolation),
            ));
            let mut harness = harness_with_invoker(
                vec![
                    MockTurn::tool_call("call_1", "mcp__fs__read", json!({})),
                    MockTurn::text("读完了"),
                ],
                vec![mcp_spec],
                PolicyEngine::new(),
                invoker,
            );

            harness.runner.run(&Budget::new(20)).await.unwrap();

            assert_eq!(
                boundaries(&harness),
                vec![(
                    crate::domain::sandbox::MCP_BOUNDARY_LABEL.to_owned(),
                    process_isolation,
                    false
                )],
                "MCP 调用要单独标注它跑在外部进程里，active 反映进程是否真进了 \
                 Job（本次 process_isolation={process_isolation}）"
            );
        }
    }

    /// 取出全部 `McpCallFailed` 的 (轮次, 调用 ID, 工具名, 结果未知, 进程保护)。
    fn mcp_call_failures(harness: &Harness) -> Vec<(u32, String, String, bool, bool)> {
        harness
            .evidence
            .events()
            .into_iter()
            .filter_map(|entry| match entry.event {
                AuditEvent::McpCallFailed {
                    turn,
                    call_id,
                    tool,
                    failure,
                } => Some((
                    turn,
                    call_id.as_str().to_owned(),
                    tool,
                    failure.result_unknown,
                    failure.process_isolation,
                )),
                _ => None,
            })
            .collect()
    }

    /// MCP 调用故障记录为独立的审计事件，字段取自执行器事实。
    ///
    /// 报表与命令行靠它区分"超时（结果未知）"与"Server 报错（结果已知）"：
    /// 前者不能重放，后者可按普通失败处理。合并成一个字符串会丢失这一区别。
    #[tokio::test]
    async fn mcp_call_failure_becomes_its_own_audit_event() {
        let mcp_spec = ToolSpec {
            name: "mcp__demo__send_message".to_owned(),
            description: "外部 Server 的写工具".to_owned(),
            parameters: json!({"type": "object"}),
            annotations: ToolAnnotations::new(false, false, false, true),
        };
        let invoker = Arc::new(RecordingInvoker::with_mcp_failure(
            crate::domain::sandbox::ExecutionBoundary::mcp(true),
            crate::domain::mcp::McpCallFailure {
                server: "demo".to_owned(),
                generation: 3,
                kind: crate::domain::mcp::McpFaultKind::CallTimeout,
                result_unknown: true,
                process_isolation: true,
            },
        ));
        let mut harness = harness_with_invoker(
            vec![
                MockTurn::tool_call(
                    "call_9",
                    "mcp__demo__send_message",
                    json!({"message": "hi"}),
                ),
                MockTurn::text("看到超时"),
            ],
            vec![mcp_spec],
            PolicyEngine::new(),
            invoker,
        );

        harness.runner.run(&Budget::new(20)).await.unwrap();

        assert_eq!(
            mcp_call_failures(&harness),
            vec![(
                1,
                "call_9".to_owned(),
                "mcp__demo__send_message".to_owned(),
                true,
                true
            )],
            "故障事件要带上轮次、调用 ID、工具名与两个事实"
        );
        // 同一次调用仍留一条边界记录：连接事件不代替它，也不重复它。
        assert_eq!(boundaries(&harness).len(), 1);
    }

    /// 故障事实只写入审计，不进入观察与快照。
    ///
    /// `ToolObserved` 与快照的 JSON 形状是既有契约，历史证据文件与恢复路径
    /// 都按它读取。加入 `mcp_failure` 会让旧 reader 无法解析新快照，也会让
    /// 同一事实在两处重复且可能不一致。
    #[tokio::test]
    async fn mcp_failure_metadata_stays_out_of_observations_and_snapshots() {
        let mcp_spec = ToolSpec {
            name: "mcp__demo__fetch".to_owned(),
            description: "外部 Server 的读工具".to_owned(),
            parameters: json!({"type": "object"}),
            annotations: ToolAnnotations::new(true, false, true, true),
        };
        let invoker = Arc::new(RecordingInvoker::with_mcp_failure(
            crate::domain::sandbox::ExecutionBoundary::mcp(false),
            crate::domain::mcp::McpCallFailure {
                server: "demo".to_owned(),
                generation: 1,
                kind: crate::domain::mcp::McpFaultKind::Disconnected,
                result_unknown: true,
                process_isolation: false,
            },
        ));
        let mut harness = harness_with_invoker(
            vec![
                MockTurn::tool_call("call_1", "mcp__demo__fetch", json!({})),
                MockTurn::text("读完了"),
            ],
            vec![mcp_spec],
            PolicyEngine::new(),
            invoker,
        );

        harness.runner.run(&Budget::new(20)).await.unwrap();

        let observed: Vec<_> = harness
            .evidence
            .events()
            .into_iter()
            .filter_map(|entry| match entry.event {
                AuditEvent::ToolObserved { outcome, .. } => {
                    Some(serde_json::to_value(outcome).expect("观察可序列化"))
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            observed,
            vec![json!({"content": "文件内容", "status": "Success"})],
            "观察里只剩旧的两字段形状"
        );
        let snapshot = serde_json::to_value(harness.runner.snapshot()).expect("快照可序列化");
        let text = snapshot.to_string();
        assert!(
            !text.contains("mcp_failure"),
            "快照不该带上故障事实: {text}"
        );
        assert!(!text.contains("process_isolation"), "快照不该带上连接事实");
    }

    /// 没有故障事实的观察不产生 `McpCallFailed`。
    ///
    /// 看见 `Failed` 观察就补故障事件，是根据输出文本猜测分类。故障分类
    /// 必须来自执行器提供的事实。
    #[tokio::test]
    async fn an_ordinary_failure_produces_no_mcp_call_event() {
        let mut harness = harness(
            vec![
                MockTurn::tool_call("call_1", "empty_command", json!({"command": ""})),
                MockTurn::text("看到失败"),
            ],
            vec![ToolSpec {
                name: "empty_command".to_owned(),
                description: "空命令".to_owned(),
                parameters: json!({"type": "object"}),
                annotations: ToolAnnotations::new(true, false, true, false),
            }],
        );

        harness.runner.run(&Budget::new(20)).await.unwrap();

        assert!(mcp_call_failures(&harness).is_empty());
    }

    /// 文件类工具不记录边界：它们没有子进程维度。
    ///
    /// 路径校验结果已在 `ToolObserved` 的 outcome 中。额外发 `SandboxApplied`
    /// 会让"执行了多少次"难以统计。
    #[tokio::test]
    async fn file_tools_leave_no_boundary_record() {
        let mut harness = harness(
            vec![
                MockTurn::tool_call("call_1", "read_file", json!({"path": "Cargo.toml"})),
                MockTurn::text("读完了"),
            ],
            vec![read_file_spec()],
        );

        harness.runner.run(&Budget::new(20)).await.unwrap();

        assert!(boundaries(&harness).is_empty(), "文件工具不该发这条事件");
    }

    /// 参数错误、未启动进程或非 git 仓库不生成 `SandboxApplied`。这些路径
    /// 只有失败观察，没有关于运行保护的事实。
    #[tokio::test]
    async fn calls_without_a_started_process_leave_no_boundary_record() {
        for name in ["empty_command", "git_status"] {
            let spec = ToolSpec {
                name: name.to_owned(),
                description: "未启动进程的失败".to_owned(),
                parameters: json!({"type": "object"}),
                annotations: ToolAnnotations::new(true, false, true, false),
            };
            let mut harness = harness(
                vec![
                    MockTurn::tool_call("call_1", name, json!({})),
                    MockTurn::text("收到失败"),
                ],
                vec![spec],
            );

            harness.runner.run(&Budget::new(20)).await.unwrap();
            assert!(
                boundaries(&harness).is_empty(),
                "{name} 不应凭观察和工具名推断出受保护执行"
            );
        }
    }

    // ── 受保护路径经真实工具层的端到端验证 ─────────────────────────────

    /// 即使策略放行 `write_file`，也无法写入 `.git/config`。
    ///
    /// 单独的工具层测试只能证明工具自身会拒绝。本测试串联策略引擎、runner
    /// 与真实文件工具，证明保护是结构性的：无论配置如何，调用都到不了
    /// 文件系统。
    #[tokio::test]
    async fn allow_policy_cannot_write_into_a_protected_path() {
        // 创建真实的 .git/config：拒绝必须发生在文件系统操作之前，避免
        // "文件不存在"造成假通过。
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
                hooks: Arc::new(NoopHooks),
                context_policy: Arc::new(DefaultContextPolicy),
                additional_guards: Vec::new(),
            },
        );

        // 断言集中在证据、文件内容与闸门上。答案与保护无关，但 run() 必须
        // 正常结束，不能因保护拒绝而崩溃。
        let _outcome = runner.run(&Budget::new(20)).await.unwrap();

        // 模型应看到拒绝观察而非成功，否则它会基于"已写入"的错误前提继续推理。
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
        // 免审策略下闸门本来不会被询问。这里断言保护没有退化为审批：它应当
        // 直接拒绝。
        assert!(
            approvals.requested_tools().is_empty(),
            "受保护路径应直接拒绝，不该绕道审批"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── 上下文预算与历史裁剪 ────────────────────────────────────────────

    /// 观察内容随调用参数变化的执行器，便于裁剪测试区分轮次。
    struct EchoInvoker;

    impl ToolInvoker for EchoInvoker {
        fn invoke<'a>(&'a self, call: &'a ToolCall) -> BoxFuture<'a, ToolOutcome> {
            Box::pin(async move {
                let path = call
                    .arguments
                    .get("path")
                    .and_then(|value| value.as_str())
                    .unwrap_or("?");
                ToolOutcome::success(format!("OBSERVATION_FOR_{path}{}", "x".repeat(4000)))
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
                    max_tokens: Some(2200),
                    min_recent_rounds: 2,
                    keep_task: true,
                    summarize: None,
                },
                session_store: None,
                hooks: Arc::new(NoopHooks),
                context_policy: Arc::new(DefaultContextPolicy),
                additional_guards: Vec::new(),
            },
        );

        runner.run(&Budget::new(20)).await.unwrap();

        // 最后一次请求：任务在、占位说明在，只看得到第 5、6 轮的观察。
        let rendered = serde_json::to_string(&history(&mock, mock.request_count() - 1)).unwrap();
        assert!(
            rendered.contains("读六个文件"),
            "任务消息应保留: {rendered}"
        );
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

        // Session 保留全量数据：6 轮工具调用与观察均完整。最终答案走
        // RunState::Completed，不进入历史；历史只保存调用与观察。
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

    // ── 历史摘要 ────────────────────────────────────────────────────────

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
                hooks: Arc::new(NoopHooks),
                context_policy: Arc::new(DefaultContextPolicy),
                additional_guards: Vec::new(),
            },
        );
        (runner, evidence)
    }

    #[tokio::test]
    async fn summary_is_generated_once_and_replaces_the_placeholder() {
        // 脚本：3 轮工具调用后历史 3 > 预算 2；第 4 问触发裁剪（丢第 1 轮）、
        // 摘要调用①、r4 工具调用；第 5 问丢第 1-2 轮，现有摘要（第 1 轮）未
        // 覆盖全部，触发摘要调用②（增量合并），随后给出最终答案。
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
                max_tokens: Some(2200),
                min_recent_rounds: 2,
                keep_task: true,
                summarize: None,
            }
            .with_summarize(1),
        );

        runner.run(&Budget::new(20)).await.unwrap();

        // 5 次真实请求 + 2 次摘要调用。
        assert_eq!(
            mock.request_count(),
            7,
            "每次超出覆盖范围的裁剪各补一次摘要"
        );

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
        assert!(
            second_summary.contains("此前已有摘要"),
            "应带上旧摘要做增量合并"
        );
        assert!(
            second_summary.contains("OBSERVATION_FOR_r2.txt"),
            "第二次摘要应包含新被裁的第 2 轮"
        );

        // 最后一次请求：摘要替代占位说明，被裁轮次的信息以摘要形式回到上下文。
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
        // 每次摘要尝试都返回空文本（重试仍为空），全部失败，降级为占位说明，
        // 任务仍能完成。
        // 共 4 轮工具调用、2 次失败的摘要尝试和最终答案。空文本轮交替出现：
        // 每个空回复先触发一次重试，重试返回的 tool_call/text 推进任务。
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
        // 预算能放下两轮原始观察与说明，第三轮会触发裁剪。
        runner.context_budget.max_tokens = Some(2200);

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
        // 连续失败达到上限后不再重试，否则每次裁剪都会多出两次注定失败的调用。
        // 前两次摘要失败，第三个可摘要裁剪点直接跳过摘要，退回占位说明。
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
        runner.context_budget.max_tokens = Some(2200);

        let outcome = runner.run(&Budget::new(20)).await.unwrap();

        assert_eq!(outcome.answer, "跑完了");
        // 6 次真实请求 + 2 次摘要尝试 = 8，每个空文本回复再触发一次自动重试，
        // 合计 10；若没有上限，第三个裁剪点会再产生 4 次请求，共 14。
        assert_eq!(mock.request_count(), 10);
        // 失败的摘要也要留证，便于事后从 JSONL 查清这些轮次为何没有摘要。
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
                max_tokens: Some(2200),
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
        assert!(
            final_request.contains("已省略"),
            "占位说明照旧: {final_request}"
        );
    }

    // ── 整体超时与取消 ──────────────────────────────────────────────────

    /// 可编排耗时的时钟：`elapsed_since` 返回共享计数器中的值。
    ///
    /// 测试用它让时间在工具执行时跳跃，无需真正 `sleep`。时钟通过端口注入，
    /// domain 不直接取时间，因此可以替换。
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
                    Arc::new(SlowInvoker {
                        clock: clock.clone(),
                    }),
                    vec![read_file_spec()],
                ),
                approvals: Arc::new(CountingGate::default()),
                evidence: Arc::new(MemoryEvidenceSink::new()),
                clock: clock.clone(),
                policy: PolicyEngine::new(),
                roster: AgentRoster::builtin(),
                context_budget: ContextBudget::default(),
                session_store: None,
                hooks: Arc::new(NoopHooks),
                context_policy: Arc::new(DefaultContextPolicy),
                additional_guards: Vec::new(),
            },
        );

        let error = runner
            .run(&Budget::new(20).with_wall_clock(std::time::Duration::from_secs(30)))
            .await
            .unwrap_err();

        let ApplicationError::WallClockExhausted { elapsed, limit } = &error else {
            panic!("应当是墙钟超时错误，实际 {error:?}");
        };
        assert!(
            *elapsed >= std::time::Duration::from_secs(30),
            "{elapsed:?}"
        );
        assert_eq!(*limit, std::time::Duration::from_secs(30));
        assert!(error.to_string().contains("上限"));
        // 正在执行的轮次允许跑完：第一轮已提交，历史完整；第二次模型调用未发生。
        assert_eq!(runner.session().rounds(), 1);
        assert_eq!(mock.request_count(), 1);
    }

    #[tokio::test]
    async fn cancel_stops_before_the_next_model_call_and_keeps_history() {
        let mock = MockCompletionModel::from_turns(vec![
            MockTurn::tool_call("call_1", "read_file", json!({"path": "a"})),
            MockTurn::text("不该被消费"),
        ]);
        // 取消把手由 runner 持有，构造后才能取得，而 invoker 必须在构造 runner
        // 之前存在。用共享槽位交接，与生产代码"先建 runner、再分发把手"的顺序
        // 一致。
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
                    Arc::new(CancellingInvoker {
                        handle_slot: handle_slot.clone(),
                    }),
                    vec![read_file_spec()],
                ),
                approvals: Arc::new(CountingGate::default()),
                evidence: Arc::new(MemoryEvidenceSink::new()),
                clock: Arc::new(crate::infrastructure::clock::SystemClock),
                policy: PolicyEngine::new(),
                roster: AgentRoster::builtin(),
                context_budget: ContextBudget::default(),
                session_store: None,
                hooks: Arc::new(NoopHooks),
                context_policy: Arc::new(DefaultContextPolicy),
                additional_guards: Vec::new(),
            },
        );
        handle_slot
            .set(runner.cancel_handle())
            .expect("取消把手只交接一次");

        let error = runner.run(&Budget::new(20)).await.unwrap_err();

        // 错误类型可区分：这是被取消，既非执行失败也非超预算。
        assert!(matches!(error, ApplicationError::Cancelled), "{error:?}");
        assert!(error.to_string().contains("取消"));
        // 取消是协作式的：执行中的轮次跑完并提交，历史完整，之后不再调用模型。
        assert_eq!(runner.session().rounds(), 1);
        assert_eq!(mock.request_count(), 1);
    }

    #[tokio::test]
    async fn no_wall_clock_means_elapsed_time_never_stops_the_run() {
        // 回归保护：时钟报告已过 1 小时，但预算未设时限。max_wall_clock = None
        // 时不能仅凭已耗时中断运行。
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
                tools: registry_with(
                    Arc::new(RecordingInvoker::default()),
                    vec![read_file_spec()],
                ),
                approvals: Arc::new(CountingGate::default()),
                evidence: Arc::new(MemoryEvidenceSink::new()),
                clock,
                policy: PolicyEngine::new(),
                roster: AgentRoster::builtin(),
                context_budget: ContextBudget::default(),
                session_store: None,
                hooks: Arc::new(NoopHooks),
                context_policy: Arc::new(DefaultContextPolicy),
                additional_guards: Vec::new(),
            },
        );
        let outcome = runner.run(&Budget::new(20)).await.unwrap();

        assert_eq!(outcome.answer, "读完了");
        assert_eq!(outcome.turns_used, 2);
    }

    // ── 有界恢复 ────────────────────────────────────────────────────────
    //
    // 恢复的三种半轮状态，三种语义：
    //
    // - `Evaluating`：尚未执行，用**当前**策略重新裁决；
    // - `AwaitingApproval`：已询问但未答复，**再问一次**，用户需重新确认；
    // - `Dispatching`：正在执行，副作用可能已发生，**不自动重跑**，
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
            hooks: Arc::new(NoopHooks),
            context_policy: Arc::new(DefaultContextPolicy),
            additional_guards: Vec::new(),
        }
    }

    /// 停在"正在执行工具"时刻的快照。
    ///
    /// 中断发生在调用发起之后、观察记录之前，副作用可能已经发生，结果未知。
    /// 这是风险最高的中断形态。
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

    /// 停在"评估调用、尚未裁决"时刻的快照。
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

    /// 停在"等待审批"时刻的快照。
    ///
    /// 对应"上次已询问但未答复"：进程停在闸门前，用户尚未回答就被中断。
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

    /// 半轮快照的调用清单必须与待配平轮次一致。
    ///
    /// 不一致时恢复入口会拒绝加载，这份快照永远用不了。夹具若对不上，
    /// 只做 JSON 往返的测试照样通过，问题会藏到真正的恢复路径上。
    #[test]
    fn a_half_round_snapshot_whose_call_lists_disagree_cannot_be_resumed() {
        let mut snapshot = dispatching_snapshot(write_call("call_1"));
        // 状态机只记了一条调用，待配平轮次里有两条：两者对不上。
        let pending = snapshot.pending.as_mut().expect("夹具带 pending");
        pending.turn.calls.push(write_call("call_2"));

        let mock = MockCompletionModel::from_turns(vec![MockTurn::text("不会用到")]);
        let error = AgentRunner::resume(
            snapshot,
            resume_deps(
                &mock,
                Arc::new(RecordingInvoker::default()),
                Arc::new(MemoryEvidenceSink::new()),
                Arc::new(CountingGate::default()),
                PolicyEngine::new(),
            ),
        )
        .err()
        .expect("调用清单不一致的快照必须被拒绝");

        assert!(
            error.to_string().contains("调用清单"),
            "拒绝理由要指出是哪一处不一致: {error}"
        );
    }

    /// 恢复时，中断时正在执行的调用**不自动重跑**。
    ///
    /// 有界恢复的关键约束：副作用可能已发生，重跑会让同一次副作用执行两遍。
    /// 恢复必须把决定权交回模型。
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
        // 证据中需要一条"可能已执行"的记录，便于事后从 JSONL 确认这里差点
        // 发生重复副作用。
        let replays: Vec<_> = evidence
            .events()
            .into_iter()
            .filter(|entry| matches!(entry.event, AuditEvent::ToolCallReplayed { .. }))
            .collect();
        assert_eq!(replays.len(), 1, "应留下一条重放证据");
    }

    /// 模型能看到"该调用此前执行过，是否重试由你决定"。
    ///
    /// 提示必须给出执行事实。即使结果未知也要说明，否则模型会以为调用未
    /// 执行，从而原样重试。
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
        assert!(text.contains("结果未知"), "提示应说明结果未知: {text}");
        assert!(text.contains("由你判断"), "提示应把决定权交回模型: {text}");
    }

    /// 恢复时，审批必须**再问一次**，不能凭快照放行。
    ///
    /// 用户被询问但未答复，不构成同意。这是安全要求。
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

        // 第二步：询问闸门。用户会被问一次，快照中的那次发生在上一个进程。
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

    /// 恢复时使用**当前**策略引擎重新裁决，不使用快照中的旧结论。
    ///
    /// 快照未保存裁决结果（裁决只存在于审计事件），因此恢复时看到的是当前
    /// 策略。策略改为拒绝后，该调用必须被拒，即使中断时曾被放行。
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
                policy_with(
                    "write_file",
                    PolicyVerdict::Deny,
                    "恢复时的策略：禁止写文件",
                ),
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
        assert!(
            text.contains("恢复时的策略"),
            "拒绝理由应来自当前策略: {text}"
        );
    }

    /// 停在审批前时，当前策略改为拒绝，调用直接拒绝，不询问闸门。
    ///
    /// 策略重算必须生效：当前策略拒绝，调用就不能执行。
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
                policy_with(
                    "write_file",
                    PolicyVerdict::Deny,
                    "恢复时的策略：禁止写文件",
                ),
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
    /// 该请求在中断前已被判为需要用户过目。策略后来放宽，也不能替用户取消
    /// 这一判断，否则"未答复"会被误读为"已同意"。
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
                policy_with(
                    "write_file",
                    PolicyVerdict::Allow,
                    "恢复时的策略：写文件免审",
                ),
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

    /// 恢复后累计轮数**不重置**，预算继续累计。
    ///
    /// 快照中的 `consumption` 是恢复时的账本起点。若恢复时清零，长任务可以
    /// 通过反复中断与恢复绕过轮数预算。
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

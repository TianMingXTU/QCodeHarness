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
use std::time::UNIX_EPOCH;

use crate::application::error::ApplicationError;
use crate::application::port::{
    ApprovalGate, ApprovalRequest, Clock, EvidenceSink, ModelGateway, ModelReply,
    ModelRequest,
};
use crate::application::tool_registry::ToolRegistry;
use crate::domain::agent::{AgentSpec, AgentTurn};
use crate::domain::budget::{Budget, Consumption};
use crate::domain::context::{compact, ContextBudget};
use crate::domain::evidence::AuditEvent;
use crate::domain::ids::SessionId;
use crate::domain::policy::{PolicyDecision, PolicyEngine, PolicyVerdict};
use crate::domain::session::Session;
use crate::domain::tool::{ToolCall, ToolOutcome, ToolSpec};

/// 运行状态机。
///
/// 每一步都是可中断的挂点：超时在 `run` 外层包 `tokio::time::timeout`；
/// 审批停在 [`RunState::AwaitingApproval`]，把状态序列化后等外部回复；
/// 恢复就是反序列化状态继续 `step`。
#[derive(Debug, Clone, PartialEq)]
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
#[derive(Debug)]
struct PendingRound {
    /// 模型这一轮的完整回复。
    turn: AgentTurn,
    /// 与 `turn.calls` 同下标一一对应。
    outcomes: Vec<ToolOutcome>,
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
    /// 上下文预算：发给模型前按它裁剪历史。
    pub context_budget: ContextBudget,
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
    /// 上下文预算：发给模型前按它裁剪历史。
    ///
    /// 裁剪只改"发给模型时怎么看"，不改 `session` 本身——
    /// 证据、审计与将来的恢复都要完整历史。
    context_budget: ContextBudget,
    /// 取消开关，克隆给外部。
    cancel: CancelHandle,
    spec: AgentSpec,
    session: Session,
    state: RunState,
    consumption: Consumption,
    pending: Option<PendingRound>,
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
            context_budget,
        } = deps;
        let visible_specs = tools.specs();
        let session = Session::new(session_id, task);
        Self {
            model,
            tools,
            visible_specs,
            approvals,
            evidence,
            clock,
            policy,
            context_budget,
            cancel: CancelHandle::default(),
            spec,
            session,
            state: RunState::AwaitingModel,
            consumption: Consumption::default(),
            pending: None,
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

                match decision.verdict {
                    // 放行：不过闸门，直接进执行。
                    PolicyVerdict::Allow => {
                        self.state = RunState::Dispatching(calls.clone());
                        Ok(RunState::Dispatching(calls))
                    }
                    // 要审批：停在闸门前，策略理由一并带上，
                    // 让人（或未来的自动闸门）知道是哪条规则要他点头。
                    PolicyVerdict::RequireApproval => {
                        let request = ApprovalRequest {
                            turn: self.session.rounds() + 1,
                            call,
                            annotations: spec.annotations,
                        };
                        self.state = RunState::AwaitingApproval(request.clone());
                        Ok(RunState::AwaitingApproval(request))
                    }
                    // 拒绝：连闸门都不问，直接给模型一条拒绝观察。
                    // 内容里带上命中规则的理由，模型才知道该换哪条路。
                    PolicyVerdict::Deny => {
                        let outcome = ToolOutcome::refused(denied_by_policy_message(
                            &call.name,
                            &decision,
                        ));
                        self.record_and_advance(call, outcome).await
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
        let start = self.clock.now();
        loop {
            // 耗时每个循环都重测：Consumption 只是"已测量结果"的容器，
            // 测量必须走 Clock 端口，domain 不取时间。
            self.consumption.elapsed = self.clock.elapsed_since(start);
            // 取消、超时与轮数预算都只在"准备问模型"之前查：正在分发的
            // 轮次要允许跑完，已花出去的 token 不废在半途。
            if matches!(self.state, RunState::AwaitingModel) {
                if self.cancel.is_cancelled() {
                    return Err(ApplicationError::Cancelled);
                }
                if budget.exhausted(&self.consumption) {
                    // exhausted 命中后先分清维度：两种超限的对策不同，
                    // 错误要能指导调用方动作。
                    if let Some(limit) = budget.max_wall_clock {
                        if self.consumption.elapsed >= limit {
                            return Err(ApplicationError::WallClockExhausted {
                                elapsed: self.consumption.elapsed,
                                limit,
                            });
                        }
                    }
                    return Err(ApplicationError::BudgetExhausted {
                        max_turns: budget.max_turns,
                    });
                }
            }
            if let RunState::Completed(answer) = self.step().await? {
                return Ok(AgentOutcome {
                    answer,
                    turns_used: self.consumption.turns,
                });
            }
        }
    }

    /// 问一次模型，并把"思考"记进证据。
    async fn ask_model(&mut self) -> Result<ModelReply, ApplicationError> {
        // 先克隆 Arc：否则请求借着 self 的字段，而它们紧接着就要被改写。
        let model = self.model.clone();
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
        }
        let request = ModelRequest {
            history,
            spec: &self.spec,
            tools: &self.visible_specs,
        };
        let reply = model.complete(request).await?;
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
        Ok(RunState::AwaitingModel)
    }

    /// 记一条证据，时间戳由时钟取。
    fn record(&self, event: AuditEvent) {
        self.evidence.record(self.clock.now(), event);
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
    use rig::message::{AssistantContent, UserContent};
    use rig::test_utils::{MockCompletionModel, MockTurn};
    use serde_json::json;

    fn spec_with() -> AgentSpec {
        AgentSpec {
            role: AgentRole::Coder,
            system_prompt: "系统提示".to_owned(),
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

    /// 数一数闸门被问了多少次、问的是哪个工具。
    #[derive(Default)]
    struct CountingGate {
        requested: std::sync::Mutex<Vec<String>>,
    }

    impl ApprovalGate for CountingGate {
        fn request<'a>(&'a self, request: &'a ApprovalRequest) -> BoxFuture<'a, Decision> {
            Box::pin(async move {
                self.requested
                    .lock()
                    .unwrap()
                    .push(request.call.name.clone());
                Ok(())
            })
        }
    }

    impl CountingGate {
        fn requested_tools(&self) -> Vec<String> {
            self.requested.lock().unwrap().clone()
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
                context_budget: ContextBudget::default(),
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
                context_budget: ContextBudget::default(),
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
    async fn empty_reply_is_an_error() {
        let mut harness = harness(
            vec![MockTurn::from_contents(Vec::<AssistantContent>::new())],
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

        assert_eq!(
            harness.approvals.requested_tools(),
            vec!["run_command"],
            "默认策略下 run_command 必须过审批闸门"
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
                context_budget: ContextBudget {
                    max_rounds: 2,
                    keep_task: true,
                },
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
                context_budget: ContextBudget::default(),
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
                context_budget: ContextBudget::default(),
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
                context_budget: ContextBudget::default(),
            },
        );

        let outcome = runner.run(&Budget::new(20)).await.unwrap();

        assert_eq!(outcome.answer, "读完了");
        assert_eq!(outcome.turns_used, 2);
    }
}

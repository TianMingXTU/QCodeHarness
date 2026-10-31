//! AgentRunner：ReAct 循环的核心编排器。
//!
//! `AgentRunner` 是应用层的核心，它不直接依赖 rig 或 tokio，
//! 而是通过端口（trait）与外部世界交互，保证可测试性。
//!
//! 行为与重构前的 `main.rs` 循环完全一致（先搬后改）：
//! 每轮重建请求、无工具调用即给出最终答案、未注册工具生成拒绝观察、
//! 达到 `max_turns` 报错、空回复报错。

use std::sync::Arc;
use std::time::UNIX_EPOCH;

use crate::application::error::ApplicationError;
use crate::application::port::{
    ApprovalGate, ApprovalRequest, Clock, EvidenceSink, ModelGateway, ModelReply,
    ModelRequest, ToolInvoker,
};
use crate::domain::agent::{AgentSpec, AgentTurn};
use crate::domain::budget::{Budget, Consumption};
use crate::domain::evidence::AuditEvent;
use crate::domain::ids::SessionId;
use crate::domain::session::Session;
use crate::domain::tool::{ToolCall, ToolOutcome, ToolSpec};

/// 工具目录：模型可见的规格，加上统一的执行入口。
///
/// 规格与执行分开，是为了让"模型看到什么"和"Harness 真做什么"可以独立演进：
/// 换一个沙箱执行器，模型看到的工具定义不用动。
///
/// 不派生 `Debug`：里面是 `Arc<dyn ToolInvoker>`，而端口没有要求实现者可调试。
pub struct ToolCatalog {
    specs: Vec<ToolSpec>,
    invoker: Arc<dyn ToolInvoker>,
}

impl ToolCatalog {
    /// 用一个执行入口和一批规格构造目录。
    pub fn new(invoker: Arc<dyn ToolInvoker>, specs: Vec<ToolSpec>) -> Self {
        Self { specs, invoker }
    }

    /// 模型可见的全部规格。
    pub fn specs(&self) -> &[ToolSpec] {
        &self.specs
    }

    /// 按名字查规格。查不到说明模型指名了一个没注册的工具。
    pub fn find(&self, name: &str) -> Option<&ToolSpec> {
        self.specs.iter().find(|spec| spec.name == name)
    }

    /// 统一的执行入口。
    pub fn invoker(&self) -> &Arc<dyn ToolInvoker> {
        &self.invoker
    }
}

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

/// ReAct 循环的编排器。
pub struct AgentRunner {
    model: Arc<dyn ModelGateway>,
    tools: ToolCatalog,
    approvals: Arc<dyn ApprovalGate>,
    evidence: Arc<dyn EvidenceSink>,
    clock: Arc<dyn Clock>,
    spec: AgentSpec,
    session: Session,
    state: RunState,
    consumption: Consumption,
    pending: Option<PendingRound>,
}

impl AgentRunner {
    /// 组装一个 runner。
    ///
    /// `invoker` 是全部工具共用的执行入口；`spec.tools` 声明模型能看到哪些。
    /// 两者的名字必须对得上——目录只按名字查找，找不到就生成拒绝观察，
    /// 让模型自己纠正，而不是崩溃。
    pub fn new(
        spec: AgentSpec,
        task: impl Into<String>,
        model: Arc<dyn ModelGateway>,
        invoker: Arc<dyn ToolInvoker>,
        approvals: Arc<dyn ApprovalGate>,
        evidence: Arc<dyn EvidenceSink>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        let tools = ToolCatalog::new(invoker, spec.tools.clone());
        let session = Session::new(new_session_id(&*clock), task);
        Self {
            model,
            tools,
            approvals,
            evidence,
            clock,
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
                let Some(spec) = self.tools.find(&call.name) else {
                    let outcome = ToolOutcome::refused(unregistered_tool_message(
                        &call.name,
                        self.tools.specs(),
                    ));
                    return self.record_and_advance(call, outcome).await;
                };
                if spec.annotations.requires_approval() {
                    let request = ApprovalRequest {
                        turn: self.session.rounds() + 1,
                        call,
                        annotations: spec.annotations,
                    };
                    self.state = RunState::AwaitingApproval(request.clone());
                    Ok(RunState::AwaitingApproval(request))
                } else {
                    self.state = RunState::Dispatching(calls.clone());
                    Ok(RunState::Dispatching(calls))
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
                let outcome = self.tools.invoker().invoke(&call).await;
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
    /// [`ApplicationError::BudgetExhausted`]。
    pub async fn run(&mut self, budget: &Budget) -> Result<AgentOutcome, ApplicationError> {
        loop {
            // 只在"准备问模型"之前查预算：正在分发的轮次要允许跑完，
            // 否则已经花出去的 token 会废在半途。
            if matches!(self.state, RunState::AwaitingModel)
                && budget.exhausted(&self.consumption)
            {
                return Err(ApplicationError::BudgetExhausted {
                    max_turns: budget.max_turns,
                });
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
        // 先克隆 Arc：否则请求借着 self.session 和 self.spec，
        // 而它们紧接着就要被改写。
        let model = self.model.clone();
        let request = ModelRequest {
            session: &self.session,
            spec: &self.spec,
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
fn new_session_id(clock: &dyn Clock) -> SessionId {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::port::{BoxFuture, Decision};
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

    fn spec_with(tools: Vec<ToolSpec>) -> AgentSpec {
        AgentSpec {
            role: AgentRole::Coder,
            system_prompt: "系统提示".to_owned(),
            tools,
        }
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
        let mock = MockCompletionModel::from_turns(turns);
        let evidence = Arc::new(MemoryEvidenceSink::new());
        let approvals = Arc::new(CountingGate::default());
        let runner = AgentRunner::new(
            spec_with(tools),
            "读一下 Cargo.toml",
            Arc::new(RigChatGateway::new(mock.clone().erase())),
            Arc::new(RecordingInvoker::default()),
            approvals.clone(),
            evidence.clone(),
            Arc::new(crate::infrastructure::clock::SystemClock),
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
            spec_with(vec![read_file_spec(), write_file_spec()]),
            "写个文件",
            Arc::new(RigChatGateway::new(mock.clone().erase())),
            Arc::new(RecordingInvoker::default()),
            Arc::new(DenyAllGate),
            Arc::new(MemoryEvidenceSink::new()),
            Arc::new(crate::infrastructure::clock::SystemClock),
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
        assert!(
            harness
                .runner
                .session_id()
                .to_string()
                .starts_with("session-")
        );

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
    fn catalog_finds_spec_by_name() {
        let catalog = ToolCatalog::new(
            Arc::new(RecordingInvoker::default()),
            vec![read_file_spec(), write_file_spec()],
        );
        assert!(catalog.find("read_file").is_some());
        assert!(catalog.find("list_dir").is_none());
        assert_eq!(catalog.specs().len(), 2);
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
}

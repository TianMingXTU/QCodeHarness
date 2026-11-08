//! 运行报表：从证据日志统计成功率、工具结局、审批分布与成本。
//!
//! [`summarize`] 与 [`BenchmarkReport::from_sessions`] 是纯函数，无 IO、
//! 无时钟，相同输入始终得到相同摘要。证据读取
//! （[`crate::infrastructure::evidence::reader`]）与报表渲染都在函数之外，
//! 统计逻辑可直接用合成数据测试。
//!
//! [`SessionLog`] 放在本模块而非 reader：统计是它的消费方，且
//! infrastructure 依赖 application 是本仓库既定的依赖方向。

use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::domain::context::EvidenceIndex;
use crate::domain::evidence::{AuditEvent, Evidence};
use crate::domain::ids::SessionId;

/// 一次运行（一个会话）的证据汇总。
#[derive(Debug, Clone)]
pub struct SessionLog {
    /// 会话 ID（来自证据文件名，去掉 `.jsonl` 后缀）。
    pub session_id: SessionId,
    /// 全部事件，按文件顺序（写盘顺序）排列。
    pub events: Vec<Evidence>,
    /// 因解析失败被跳过的非空行数。
    ///
    /// 用于区分空会话与部分损坏的日志；只看 `events.len()` 无法区分两者。
    pub skipped_lines: usize,
}

impl SessionLog {
    /// 事件序号是否严格连续，即每条比前一条大 1。
    ///
    /// 同一 sink 生命周期内序号严格递增。乱序或跳号说明日志被并发写过，
    /// 或中途有写入失败被丢弃。这只影响解读，例如可判断序号 5 之前发生过
    /// 写失败，但不表示文件无效，因此仅提供查询，不在读取时报错。
    pub fn is_sequential(&self) -> bool {
        self.events
            .windows(2)
            .all(|pair| pair[1].sequence == pair[0].sequence + 1)
    }

    /// 首条事件的发生时刻。
    pub fn started_at(&self) -> Option<SystemTime> {
        self.events.first().map(|evidence| evidence.recorded_at)
    }

    /// 末条事件的发生时刻。
    pub fn ended_at(&self) -> Option<SystemTime> {
        self.events.last().map(|evidence| evidence.recorded_at)
    }
}

/// 一个会话的统计摘要。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionSummary {
    /// 会话 ID。
    pub session_id: SessionId,
    /// 读到的事件总数。
    pub events: usize,
    /// 跳过的坏行数。
    pub skipped_lines: usize,
    /// 模型调用次数（`Thought` 事件数）。
    pub model_calls: usize,
    /// 工具调用总数（`ToolRequested` 数）。
    pub tool_calls: usize,
    /// 工具执行成功次数。
    pub succeeded: usize,
    /// 工具失败次数。
    pub failed: usize,
    /// 工具被拒绝次数。
    pub refused: usize,
    /// 审批次数。
    pub approvals: usize,
    /// 审批放行次数。
    pub approved: usize,
    /// 审批否决次数。
    pub denied: usize,
    /// 等待人工审批的总毫秒数。
    ///
    /// 取每条 `RequireApproval` 裁决到紧随其后的 `ApprovalDecided` 的
    /// 时间差。审批次数无法反映等待成本：两次立即确认与两次长时间等待
    /// 的计数相同。
    ///
    /// 只统计经过审批闸门的调用。策略直接放行的调用不进入闸门，把它后续
    /// 的间隔算作等待会把工具执行时间记到人头上。越界写入升级记录为
    /// Allow，但同样经过闸门；该路径改取同轮次同工具最近一条 Allow 裁决
    /// 作为起点。
    pub approval_wait_ms: u64,
    /// 策略裁决为 Allow 的次数。
    pub policy_allow: usize,
    /// 策略裁决为 RequireApproval 的次数。
    pub policy_require_approval: usize,
    /// 策略裁决为 Deny 的次数。
    pub policy_deny: usize,
    /// 输入 token 合计。
    pub input_tokens: u64,
    /// 输出 token 合计。
    pub output_tokens: u64,
    /// 从 provider 缓存读回的输入 token 合计。
    pub cached_input_tokens: u64,
    /// 写入 provider 缓存的输入 token 合计。
    pub cache_creation_input_tokens: u64,
    /// 成本合计（美元）。任一次调用成本未知时为 `None`。
    /// 部分已知的合计会低估成本，比未知更危险。
    pub cost_usd: Option<f64>,
    /// 摘要调用次数（`SummaryGenerated` + `SummaryFailed`）。
    pub summary_calls: usize,
    /// 上下文裁剪次数（`ContextCompacted` 数）。
    pub compactions: usize,
    /// 上下文裁剪净省下的 token 合计（`ContextBudgeted` 事件的
    /// `dropped_tokens` 之和）。
    ///
    /// 轮数只表示裁剪次数，token 才反映成本。裁 3 轮可能只省两百个 token，
    /// 也可能省两万个。
    pub dropped_context_tokens: u64,
    /// 会话实际操作：读写的文件、执行的命令、失败的调用。
    ///
    /// 与模型上下文中的台账使用同一套判定规则，见
    /// [`EvidenceIndex::from_events`] 与 [`EvidenceIndex::from_session`]。
    /// 用户看到的"做过什么"必须与模型看到的一致。
    pub evidence_index: EvidenceIndex,
    /// 恢复时提示"调用可能已执行"的次数（`ToolCallReplayed` 数）。
    ///
    /// 统计的是可能的重复副作用，不是已发生的结果。程序不会自动重跑，
    /// 而是把决定权交回模型。非零值表示本次运行中有调用在结果未知时被中断。
    pub replays: usize,
    /// 恢复时重新询问审批的次数（`ApprovalReasked` 数）。
    ///
    /// 与 `approvals` 分开统计。重新询问需要用户再次确认，因此单独计数。
    pub approval_reasks: usize,
    /// 启用进程限额与进程树管理的执行次数（`SandboxApplied` 且 `active`）。
    #[serde(default)]
    pub sandbox_protected_runs: usize,
    /// 未启用进程限额与进程树管理的记录数（`SandboxApplied` 且非 `active`）。
    ///
    /// 包括非 Windows 平台的本地子进程、Windows 上 `assign` 失败的子进程，
    /// 以及外部 MCP Server 的调用尝试。
    #[serde(default)]
    pub sandbox_unprotected_runs: usize,
    /// 首事件时刻。
    pub started_at: Option<SystemTime>,
    /// 末事件时刻。
    pub ended_at: Option<SystemTime>,
    /// 会话首末事件的间隔（毫秒）。空会话为 0。
    ///
    /// 直接用证据中的事件时间戳计算，不另设时钟，避免多一处可能不一致的
    /// 时间来源。该值不含会话创建与证据文件打开的开销，这些发生在第一条
    /// 事件之前。
    pub wall_clock_ms: u64,
    /// 每轮模型调用的耗时（毫秒），按调用顺序排列。空会话为空。
    ///
    /// 一轮耗时取本轮与上一轮思考事件的时间差。该区间包含 prefill、
    /// decode 以及本轮触发的工具执行，即用户等待一轮的总时长。
    pub turn_latencies_ms: Vec<u64>,
    /// 缓存命中率：读回 token 占输入 token 的比例。分母为 0 时为 `None`。
    ///
    /// `0.0` 表示有输入但未命中，`None` 表示没有输入 token 可算（provider
    /// 不报用量或会话零调用）。将后者显示为 0% 会让用户以为缓存失效。
    /// 这与"成本未知不填 0"遵循同一原则。
    pub cache_hit_ratio: Option<f64>,
    /// 同一会话内重复调用（同工具名且参数相同）的工具名。
    ///
    /// 按名称去重，保留首次出现顺序。重复调用是轮次浪费的直接信号：
    /// 同一文件读两遍，说明上次观察未满足模型，或模型忘记已读过。
    /// 两种情况都指向提示词与工具描述可改进之处。
    pub repeated_calls: Vec<String>,
    /// MCP 连接建立失败次数。
    #[serde(default)]
    pub mcp_connect_failures: usize,
    /// MCP 连接意外断开次数（一次连接只记一次）。
    #[serde(default)]
    pub mcp_disconnects: usize,
    /// MCP 恢复成功次数。
    #[serde(default)]
    pub mcp_recoveries: usize,
    /// MCP 恢复失败次数。
    #[serde(default)]
    pub mcp_recovery_failures: usize,
    /// MCP 工具调用超时次数。
    #[serde(default)]
    pub mcp_call_timeouts: usize,
    /// MCP 调用已发出但结果未知的次数（超时或断连，不能当作已取消）。
    #[serde(default)]
    pub mcp_unknown_results: usize,
}

/// 多个会话的汇总。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BenchmarkReport {
    /// 各会话的摘要。
    pub sessions: Vec<SessionSummary>,
    /// 全部会话的输入 token 合计。
    pub total_input_tokens: u64,
    /// 全部会话的输出 token 合计。
    pub total_output_tokens: u64,
    /// 全部会话从缓存读回的输入 token 合计。
    pub total_cached_input_tokens: u64,
    /// 全部会话的墙钟合计（毫秒）。
    ///
    /// 会话串行执行，该合计是一轮基准耗时的下界，不含 fixture 拷贝、
    /// 判据命令与证据读回的开销。
    pub total_wall_clock_ms: u64,
    /// 全部会话的审批裁决次数合计（`ApprovalDecided` 事件数）。
    ///
    /// 基准使用 `AutoApproveGate`（无人值守），这个数字表示有多少次调用
    /// 走到了审批闸门，不是人工审批次数；真实使用中的等待情况同样通过
    /// `--report` 查看。
    pub total_approvals: usize,
    /// 全部会话的审批等待耗时合计（毫秒）。
    ///
    /// 与 [`Self::total_approvals`] 分开：次数表示被打断的次数，本字段
    /// 表示被打断的时长。减少等待需要同时降低两者；少问两次但每次都等
    /// 五分钟，用户感受不会改善。
    pub total_approval_wait_ms: u64,
    /// 全部会话的成本合计。任一会话成本未知时为 `None`。
    pub total_cost_usd: Option<f64>,
    /// 成功会话数：会话以 `Thought` 事件收尾。ReAct 循环仅在交付最终答案
    /// 时以思考收尾；被预算或超时打断的会话停在工具观察上。
    pub succeeded: usize,
    /// 失败会话数（未以 `Thought` 收尾）。
    pub failed: usize,
    /// 只含 MCP 连接事件的日志份数。
    ///
    /// 这类日志不是任务运行：没有模型调用与工具调用，不计成本和墙钟。
    /// 按事件内容识别，不按文件名。它从 [`Self::succeeded`]、
    /// [`Self::failed`] 与总成本、总墙钟中排除，只贡献 MCP 统计。否则
    /// "MCP Server 未启动"会被读成任务失败，启动次数会被读成调用次数。
    #[serde(default)]
    pub mcp_lifecycle_logs: usize,
}

impl BenchmarkReport {
    /// 从多个会话汇总。空清单返回全零报表，不 panic。
    pub fn from_sessions(logs: &[SessionLog]) -> Self {
        let sessions: Vec<SessionSummary> = logs.iter().map(|log| summarize(log)).collect();
        let mut total_input = 0u64;
        let mut total_output = 0u64;
        let mut total_cached = 0u64;
        let mut total_wall_clock = 0u64;
        let mut total_approvals = 0usize;
        let mut total_approval_wait = 0u64;
        // 没有成本数据（空清单或全部会话零调用）时为未知；已知成本累加，
        // 一旦遇到未知，后续已知成本也不能把合计改回已知。
        let mut total_cost = None;
        let mut cost_unknown = false;
        let mut succeeded = 0usize;
        let mut lifecycle_logs = 0usize;
        for (log, summary) in logs.iter().zip(&sessions) {
            // 只含连接事件的日志没有模型调用与工具调用，成本未知，墙钟只是
            // Server 存活时长。计入任何合计都会让报表失真：MCP 未启动会被
            // 当成任务失败，启动次数会被当成调用次数。其价值只在 MCP 统计，
            // 该统计已包含在 `sessions` 中。
            if is_lifecycle_only(log) {
                lifecycle_logs += 1;
                continue;
            }
            total_input += summary.input_tokens;
            total_output += summary.output_tokens;
            total_cached += summary.cached_input_tokens;
            total_wall_clock += summary.wall_clock_ms;
            total_approvals += summary.approvals;
            total_approval_wait += summary.approval_wait_ms;
            match summary.cost_usd {
                Some(cost) if !cost_unknown => {
                    total_cost = Some(total_cost.unwrap_or(0.0) + cost);
                }
                Some(_) => {}
                None => {
                    total_cost = None;
                    cost_unknown = true;
                }
            }
            if ends_with_answer(log) {
                succeeded += 1;
            }
        }
        Self {
            succeeded,
            failed: sessions.len() - lifecycle_logs - succeeded,
            sessions,
            total_input_tokens: total_input,
            total_output_tokens: total_output,
            total_cached_input_tokens: total_cached,
            total_wall_clock_ms: total_wall_clock,
            total_approvals,
            total_approval_wait_ms: total_approval_wait,
            total_cost_usd: total_cost,
            mcp_lifecycle_logs: lifecycle_logs,
        }
    }
}

/// 日志是否只包含 MCP 连接事件。
///
/// 只检查事件内容，不检查文件名。日志路径是装配层的选择，是否算作任务
/// 运行应由事件本身决定。按文件名判断时，改名或多写一条普通事件都会
/// 改变统计结果。
///
/// 空日志不算：没有事件表示读到空文件或坏文件，应由原有的"会话失败"
/// 判据处理，不能被本规则吸收。
pub fn is_lifecycle_only(log: &SessionLog) -> bool {
    !log.events.is_empty()
        && log
            .events
            .iter()
            .all(|evidence| matches!(evidence.event, AuditEvent::McpLifecycle { .. }))
}

/// 会话是否以最终答案收尾：最后一条任务事件是 `Thought`。
///
/// ReAct 循环仅在交付最终答案（模型回复不带工具调用）时以思考收尾；
/// 被预算、超时或取消打断的会话停在工具观察或审批上。这是"成功"的
/// 可检查代理判据，只回答是否交付答案，不判断答案是否正确；后者由
/// 基准任务的判据负责。
///
/// 连接事件被跳过。生命周期日志与任务日志混写时，末尾的 `Closed` 是
/// 进程退出时关闭 Server 留下的，模型此前已交付答案。把它当作最后一条
/// 事件会把成功会话判成失败。
fn ends_with_answer(log: &SessionLog) -> bool {
    matches!(
        log.events
            .iter()
            .rev()
            .map(|evidence| &evidence.event)
            .find(|event| !matches!(event, AuditEvent::McpLifecycle { .. })),
        Some(AuditEvent::Thought { .. })
    )
}

/// 从证据日志统计一个会话。
///
/// 纯函数，无 IO、无时钟。仅当每次调用的成本都已知时才给出成本合计。
/// token 对 `Thought` 与 `SummaryGenerated` 求和；摘要调用消耗真实配额，
/// 不计入会导致 token 统计不完整。
pub fn summarize(log: &SessionLog) -> SessionSummary {
    let mut summary = SessionSummary {
        session_id: log.session_id.clone(),
        events: log.events.len(),
        skipped_lines: log.skipped_lines,
        model_calls: 0,
        tool_calls: 0,
        succeeded: 0,
        failed: 0,
        refused: 0,
        approvals: 0,
        approved: 0,
        denied: 0,
        approval_wait_ms: 0,
        policy_allow: 0,
        policy_require_approval: 0,
        policy_deny: 0,
        input_tokens: 0,
        output_tokens: 0,
        cached_input_tokens: 0,
        cache_creation_input_tokens: 0,
        cost_usd: Some(0.0),
        summary_calls: 0,
        compactions: 0,
        dropped_context_tokens: 0,
        evidence_index: EvidenceIndex::default(),
        replays: 0,
        approval_reasks: 0,
        sandbox_protected_runs: 0,
        sandbox_unprotected_runs: 0,
        started_at: log.started_at(),
        ended_at: log.ended_at(),
        wall_clock_ms: wall_clock_of(log),
        turn_latencies_ms: Vec::new(),
        cache_hit_ratio: None,
        repeated_calls: Vec::new(),
        mcp_connect_failures: 0,
        mcp_disconnects: 0,
        mcp_recoveries: 0,
        mcp_recovery_failures: 0,
        mcp_call_timeouts: 0,
        mcp_unknown_results: 0,
    };

    // 上一轮思考事件的时刻，用于计算本轮延迟。相邻思考事件的差值即用户
    // 等待一轮的总时长。
    let mut previous_thought_at: Option<u64> = None;
    // 最近一条要求审批的裁决时刻。与紧随的 `ApprovalDecided` 之差即人工
    // 等待时间。
    let mut pending_approval_at: Option<u64> = None;
    // 已见过的调用（工具名加参数），用于识别重复。`Value` 按结构比较，
    // 与对象键序无关。
    let mut seen_calls: Vec<(String, serde_json::Value)> = Vec::new();
    let mut repeated: Vec<String> = Vec::new();

    for (index, evidence) in log.events.iter().enumerate() {
        match &evidence.event {
            AuditEvent::Thought { usage, .. } => {
                summary.model_calls += 1;
                add_usage(&mut summary, usage);
                let at = evidence.recorded_at_millis();
                if let Some(previous) = previous_thought_at {
                    summary
                        .turn_latencies_ms
                        .push(at.saturating_sub(previous));
                }
                previous_thought_at = Some(at);
            }
            AuditEvent::ToolRequested { call, .. } => {
                summary.tool_calls += 1;
                // 比较 `Value` 而非序列化字符串。`serde_json` 启用了
                // `preserve_order`（由 rig-core 引入），同一份参数换键序
                // 会得到不同字符串，导致重发的同一请求被漏判。`Value` 的
                // 对象相等性按结构比较，与键序无关。
                let key = (call.name.clone(), call.arguments.clone());
                // 首次见到的参数入表；再次见到时记录工具名，仅记录一次。
                // 两个分支互斥，重复参数不会重复入表。
                if !seen_calls.contains(&key) {
                    seen_calls.push(key);
                } else if !repeated.contains(&call.name) {
                    repeated.push(call.name.clone());
                }
            }
            AuditEvent::ToolObserved { outcome, .. } => match outcome.status {
                crate::domain::tool::OutcomeStatus::Success => summary.succeeded += 1,
                crate::domain::tool::OutcomeStatus::Failed => summary.failed += 1,
                crate::domain::tool::OutcomeStatus::Refused => summary.refused += 1,
            },
            AuditEvent::ApprovalDecided { allowed, turn, tool, .. } => {
                summary.approvals += 1;
                if *allowed {
                    summary.approved += 1;
                } else {
                    summary.denied += 1;
                }
                // 拿到结论后结算这段等待。
                let asked_at = pending_approval_at.take().or_else(|| {
                    // 越界写入升级的调用没有 RequireApproval 锚点。策略
                    // 记录的是原判 Allow（审计不改写策略原判），升级发生在
                    // 裁决记录之后。该路径同样产生人工等待，漏计会使等审
                    // 数据失真。向前查找最近一条同工具同轮次的 Allow 裁决
                    // 作为等待起点，即该调用最早需要人工确认的时刻。
                    log.events[..index]
                        .iter()
                        .rev()
                        .find_map(|prior| match &prior.event {
                            AuditEvent::PolicyEvaluated {
                                turn: prior_turn,
                                tool: prior_tool,
                                verdict: crate::domain::policy::PolicyVerdict::Allow,
                                ..
                            } if prior_turn == turn && prior_tool == tool => {
                                Some(prior.recorded_at_millis())
                            }
                            _ => None,
                        })
                });
                // 取不到锚点时不计。日志被截断或确实没有可追溯裁决时，间隔
                // 只能猜测，猜测值比缺失更糟。
                if let Some(asked_at) = asked_at {
                    summary.approval_wait_ms += evidence
                        .recorded_at_millis()
                        .saturating_sub(asked_at);
                }
            }
            AuditEvent::PolicyEvaluated { verdict, .. } => match verdict {
                crate::domain::policy::PolicyVerdict::Allow => summary.policy_allow += 1,
                crate::domain::policy::PolicyVerdict::RequireApproval => {
                    summary.policy_require_approval += 1;
                    pending_approval_at = Some(evidence.recorded_at_millis());
                }
                crate::domain::policy::PolicyVerdict::Deny => summary.policy_deny += 1,
            },
            AuditEvent::ContextCompacted { .. } => summary.compactions += 1,
            AuditEvent::ContextBudgeted { dropped_tokens, .. } => {
                summary.dropped_context_tokens += dropped_tokens;
            }
            AuditEvent::SummaryGenerated { usage, .. } => {
                summary.summary_calls += 1;
                add_usage(&mut summary, usage);
            }
            AuditEvent::SummaryFailed { .. } => summary.summary_calls += 1,
            // 恢复路径的两类事件：可能已执行的调用、重新询问的审批。
            AuditEvent::ToolCallReplayed { .. } => summary.replays += 1,
            AuditEvent::ApprovalReasked { .. } => summary.approval_reasks += 1,
            // 按每次调用的 active 计数。同一会话可混用本地进程与 MCP，
            // platform 标签可能不同，详情保留在 JSONL 中。
            AuditEvent::SandboxApplied { active, .. } => {
                if *active {
                    summary.sandbox_protected_runs += 1;
                } else {
                    summary.sandbox_unprotected_runs += 1;
                }
            }
            // `StepStarted` 不计业务计数器，但计入事件总数。
            AuditEvent::StepStarted { .. } => {}
            // 连接级与调用级 MCP 故障只进入各自的 MCP 计数，不计入工具
            // 结局（succeeded/failed）或沙箱执行次数。启动失败没有工具
            // 调用，混入调用次数会使报表自相矛盾。
            AuditEvent::McpLifecycle { fact, .. } => match fact.kind {
                crate::domain::mcp::McpLifecycleKind::ConnectFailed => {
                    summary.mcp_connect_failures += 1;
                }
                crate::domain::mcp::McpLifecycleKind::Disconnected => {
                    summary.mcp_disconnects += 1;
                }
                crate::domain::mcp::McpLifecycleKind::Recovered => {
                    summary.mcp_recoveries += 1;
                }
                crate::domain::mcp::McpLifecycleKind::RecoveryFailed => {
                    summary.mcp_recovery_failures += 1;
                }
                crate::domain::mcp::McpLifecycleKind::Connected
                | crate::domain::mcp::McpLifecycleKind::Closed => {}
            },
            AuditEvent::McpCallFailed { failure, .. } => {
                if failure.kind == crate::domain::mcp::McpFaultKind::CallTimeout {
                    summary.mcp_call_timeouts += 1;
                }
                if failure.result_unknown {
                    summary.mcp_unknown_results += 1;
                }
            }
        }
    }
    summary.repeated_calls = repeated;
    // 证据台账，与模型上下文中的一致。
    summary.evidence_index =
        EvidenceIndex::from_events(log.events.iter().map(|evidence| &evidence.event));

    // 缓存命中率为读回 token 除以输入 token。分母为 0 时返回 None；0.0
    // 表示有输入但未命中，与没有输入可算是两种含义。
    if summary.input_tokens > 0 {
        summary.cache_hit_ratio =
            Some(summary.cached_input_tokens as f64 / summary.input_tokens as f64);
    }

    // 空会话没有成本数据，记为未知，不记 0。
    if summary.model_calls == 0 && summary.summary_calls == 0 {
        summary.cost_usd = None;
    }
    summary
}

/// 首末事件的间隔（毫秒）。
///
/// 空会话或只有一条事件时为 0。调用方只需累加与比较，不必为单事件会话
/// 单独处理。
fn wall_clock_of(log: &SessionLog) -> u64 {
    match (log.started_at(), log.ended_at()) {
        (Some(start), Some(end)) => end
            .duration_since(start)
            .map(|span| span.as_millis() as u64)
            .unwrap_or(0),
        _ => 0,
    }
}

/// 累加一次调用的用量。成本未知时把整个会话的成本合计置为未知。
fn add_usage(summary: &mut SessionSummary, usage: &crate::domain::evidence::TokenUsage) {
    summary.input_tokens += usage.input_tokens;
    summary.output_tokens += usage.output_tokens;
    summary.cached_input_tokens += usage.cached_input_tokens;
    summary.cache_creation_input_tokens += usage.cache_creation_input_tokens;
    match usage.cost_usd {
        Some(cost) => {
            if let Some(total) = summary.cost_usd.as_mut() {
                *total += cost;
            }
        }
        None => summary.cost_usd = None,
    }
}

/// 成本列显示值：未知显示为"未知"，不显示 `$0.0000`。
/// 报表渲染与基准运行器进度行共用此规则。
pub(crate) fn cost_label(cost: Option<f64>) -> String {
    match cost {
        Some(value) => format!("${value:.4}"),
        None => "未知".to_owned(),
    }
}

/// 缓存命中率显示值：分母为 0 时显示"—"，不显示 0%。
///
/// 0% 表示有输入 token 但未命中缓存，`None` 表示没有输入 token 可算
/// （provider 不报用量或会话零调用）。显示成 0% 会让人误以为缓存失效。
/// 与成本列"未知不填 0"遵循同一原则。
pub(crate) fn hit_ratio_label(ratio: Option<f64>) -> String {
    match ratio {
        Some(value) => format!("{:.0}%", value * 100.0),
        None => "—".to_owned(),
    }
}

/// 终端对齐用的字符显示宽度：CJK 全角为 2，其余为 1。
/// 避免引入 unicode-width 依赖；报表中的非 ASCII 文本仅为中文标签与
/// 占位符。
fn display_width(text: &str) -> usize {
    text.chars()
        .map(|c| {
            if matches!(
                c as u32,
                0x1100..=0x115F
                    | 0x2E80..=0xA4CF
                    | 0xAC00..=0xD7A3
                    | 0xF900..=0xFAFF
                    | 0xFE30..=0xFE4F
                    | 0xFF00..=0xFF60
                    | 0xFFE0..=0xFFE6
            ) {
                2
            } else {
                1
            }
        })
        .sum()
}

/// 按显示宽度右补空格。
fn pad(text: &str, width: usize) -> String {
    let mut padded = text.to_owned();
    for _ in display_width(text)..width {
        padded.push(' ');
    }
    padded
}

/// 渲染为供人阅读的对齐表格，按字符显示宽度对齐。
///
/// 轮延迟不进表：一行会话有 N 个延迟值，表格装不下向量，该字段随
/// `--json` 输出。表格只放标量，如轮数、审批次数、等待时长、墙钟、
/// 缓存命中率与成本。
pub fn render_text(report: &BenchmarkReport) -> String {
    // 列定义：表头固定，数据取自摘要。
    let headers = [
        "会话", "事件", "模型", "工具成", "工具败", "工具拒", "审批", "等审ms", "输入tok", "缓存tok", "命中率", "墙钟ms", "成本",
    ];
    let rows_of = |summary: &SessionSummary| -> Vec<String> {
        vec![
            summary.session_id.as_str().to_owned(),
            summary.events.to_string(),
            summary.model_calls.to_string(),
            summary.succeeded.to_string(),
            summary.failed.to_string(),
            summary.refused.to_string(),
            summary.approvals.to_string(),
            summary.approval_wait_ms.to_string(),
            summary.input_tokens.to_string(),
            summary.cached_input_tokens.to_string(),
            hit_ratio_label(summary.cache_hit_ratio),
            summary.wall_clock_ms.to_string(),
            cost_label(summary.cost_usd),
        ]
    };

    let data: Vec<Vec<String>> = report.sessions.iter().map(rows_of).collect();
    let mut widths: Vec<usize> = headers
        .iter()
        .map(|header| display_width(header))
        .collect();
    for row in &data {
        for (index, cell) in row.iter().enumerate() {
            widths[index] = widths[index].max(display_width(cell));
        }
    }
    // 数值列右对齐；列宽至少为表头宽度。
    let render_row = |cells: &[String]| -> String {
        cells
            .iter()
            .enumerate()
            .map(|(index, cell)| {
                let width = widths[index];
                if index == 0 {
                    pad(cell, width)
                } else {
                    // 数字与成本右对齐。
                    let lead = width.saturating_sub(display_width(cell));
                    format!("{}{}", " ".repeat(lead), cell)
                }
            })
            .collect::<Vec<_>>()
            .join("  ")
    };

    let mut out = String::new();
    out.push_str(&render_row(
        &headers.iter().map(|h| h.to_string()).collect::<Vec<_>>(),
    ));
    out.push('\n');
    for row in &data {
        out.push_str(&render_row(row));
        out.push('\n');
    }
    let total = vec![
        "合计".to_owned(),
        report.sessions.iter().map(|s| s.events).sum::<usize>().to_string(),
        report.sessions.iter().map(|s| s.model_calls).sum::<usize>().to_string(),
        report.sessions.iter().map(|s| s.succeeded).sum::<usize>().to_string(),
        report.sessions.iter().map(|s| s.failed).sum::<usize>().to_string(),
        report.sessions.iter().map(|s| s.refused).sum::<usize>().to_string(),
        report.total_approvals.to_string(),
        report.total_approval_wait_ms.to_string(),
        report.total_input_tokens.to_string(),
        report.total_cached_input_tokens.to_string(),
        // 合计行命中率按合计读回除以合计输入计算，不取各行比率的平均值。
        // 平均会让小会话与大会话等权，而成本与时间按 token 计。
        hit_ratio_label(if report.total_input_tokens > 0 {
            Some(report.total_cached_input_tokens as f64 / report.total_input_tokens as f64)
        } else {
            None
        }),
        report.total_wall_clock_ms.to_string(),
        cost_label(report.total_cost_usd),
    ];
    out.push_str(&render_row(&total));

    // MCP 生命周期单独成段。连接事件与工具调用分开统计；混入表格会让
    // 读者把 Server 未启动读成工具调用失败。
    if let Some(line) = mcp_summary(report) {
        out.push_str("\nMCP 生命周期\n");
        out.push_str(&line);
        out.push('\n');
    }

    // 台账不进表格：每项都是列表，表格无法容纳。按会话列在表后，方便
    // 查看任务实际操作。
    let ledgers: Vec<&SessionSummary> = report
        .sessions
        .iter()
        .filter(|summary| !summary.evidence_index.is_empty())
        .collect();
    if !ledgers.is_empty() {
        out.push_str("\n证据台账\n");
        for summary in ledgers {
            out.push_str(&format!("  会话 {}\n", summary.session_id.as_str()));
            for line in summary.evidence_index.render().lines() {
                out.push_str(&format!("    {line}\n"));
            }
        }
    }
    out
}

/// MCP 故障与恢复的一行汇总。六项计数全为 0 且没有只含连接事件的日志时
/// 返回 `None`。
///
/// 只报告六项有诊断价值的计数，不含连接建立与主动关闭次数。这两项每次
/// 正常运行都会变化，放在故障旁容易被误认为异常。"结果未知"单独列出，
/// 因为容易被误读为"已取消"，而当前实现无法确认远端是否已取消。
fn mcp_summary(report: &BenchmarkReport) -> Option<String> {
    let sum = |pick: fn(&SessionSummary) -> usize| report.sessions.iter().map(pick).sum::<usize>();
    let (
        connect_failures,
        disconnects,
        recoveries,
        recovery_failures,
        call_timeouts,
        unknown_results,
    ) = (
        sum(|s| s.mcp_connect_failures),
        sum(|s| s.mcp_disconnects),
        sum(|s| s.mcp_recoveries),
        sum(|s| s.mcp_recovery_failures),
        sum(|s| s.mcp_call_timeouts),
        sum(|s| s.mcp_unknown_results),
    );
    if connect_failures
        + disconnects
        + recoveries
        + recovery_failures
        + call_timeouts
        + unknown_results
        == 0
        && report.mcp_lifecycle_logs == 0
    {
        return None;
    }
    let mut out = format!(
        "  连接失败 {connect_failures} 次，断连 {disconnects} 次；恢复成功 {recoveries} 次，恢复失败 {recovery_failures} 次\n  \
            调用超时 {call_timeouts} 次，结果未知 {unknown_results} 次（超时与断连都不等于远端已取消，原调用不会自动重放）"
    );
    if report.mcp_lifecycle_logs > 0 {
        out.push_str(&format!(
            "\n  另有 {} 份只有连接事件的日志，不计入会话成败与总成本/墙钟",
            report.mcp_lifecycle_logs
        ));
    }
    Some(out)
}

/// 渲染为机器可读的 JSON。
///
/// # Errors
///
/// 序列化失败时返回错误。正常数据不会触发。
pub fn render_json(report: &BenchmarkReport) -> Result<String, serde_json::Error> {
    serde_json::to_string_pretty(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::agent::AgentRole;
    use crate::domain::evidence::TokenUsage;
    use crate::domain::ids::CallId;
    use crate::domain::mcp::{McpCallFailure, McpFaultKind, McpLifecycleFact, McpLifecycleKind};
    use crate::domain::tool::{ToolCall, ToolOutcome};
    use serde_json::json;
    use std::time::{Duration, UNIX_EPOCH};

    fn call(name: &str) -> ToolCall {
        ToolCall {
            id: CallId::new("call_1"),
            name: name.to_owned(),
            arguments: json!({}),
        }
    }

    fn evidence(sequence: u64, event: AuditEvent) -> Evidence {
        // 毫秒间隔：序号即毫秒偏移，合成数据因此具有亚秒级时序，便于
        // 验证墙钟与轮延迟统计。
        Evidence::new(
            sequence,
            UNIX_EPOCH + Duration::from_millis(sequence * 250),
            event,
        )
    }

    /// 构造覆盖全部事件种类的会话，计数器可手工核对。
    fn synthetic_log(cost_of_second_thought: Option<f64>) -> SessionLog {
        SessionLog {
            session_id: SessionId::new("session-test-1"),
            events: vec![
                evidence(1, AuditEvent::StepStarted { turn: 1, role: AgentRole::Coder }),
                evidence(
                    2,
                    AuditEvent::Thought {
                        turn: 1,
                        text: "先写".to_owned(),
                        usage: TokenUsage { input_tokens: 100, output_tokens: 20, cached_input_tokens: 0, cache_creation_input_tokens: 0, cost_usd: Some(0.25) },
                    },
                ),
                evidence(
                    3,
                    AuditEvent::PolicyEvaluated {
                        turn: 1,
                        tool: "write_file".to_owned(),
                        verdict: crate::domain::policy::PolicyVerdict::RequireApproval,
                        rule_index: None,
                        reason: "默认".to_owned(),
                    },
                ),
                evidence(4, AuditEvent::ToolRequested { turn: 1, call: call("write_file") }),
                evidence(
                    5,
                    AuditEvent::ApprovalDecided {
                        turn: 1,
                        tool: "write_file".to_owned(),
                        allowed: true,
                        reason: None,
                    },
                ),
                evidence(
                    6,
                    AuditEvent::ToolObserved {
                        turn: 1,
                        call: call("write_file"),
                        outcome: ToolOutcome::success("已写入"),
                    },
                ),
                evidence(
                    7,
                    AuditEvent::Thought {
                        turn: 2,
                        text: "再读".to_owned(),
                        usage: TokenUsage {
                            input_tokens: 50,
                            output_tokens: 10,
                            cached_input_tokens: 0,
                            cache_creation_input_tokens: 0,
                            cost_usd: cost_of_second_thought,
                        },
                    },
                ),
                evidence(
                    8,
                    AuditEvent::PolicyEvaluated {
                        turn: 2,
                        tool: "read_file".to_owned(),
                        verdict: crate::domain::policy::PolicyVerdict::Allow,
                        rule_index: Some(0),
                        reason: "只读".to_owned(),
                    },
                ),
                evidence(9, AuditEvent::ToolRequested { turn: 2, call: call("read_file") }),
                evidence(
                    10,
                    AuditEvent::ToolObserved {
                        turn: 2,
                        call: call("read_file"),
                        outcome: ToolOutcome::failed("文件不存在"),
                    },
                ),
                evidence(
                    11,
                    AuditEvent::ContextCompacted {
                        turn: 3,
                        rounds_before: 20,
                        rounds_after: 16,
                        dropped: vec![1, 2],
                    },
                ),
                evidence(
                    12,
                    AuditEvent::SummaryGenerated {
                        turn: 3,
                        covers: (1, 2),
                        usage: TokenUsage {
                            input_tokens: 200,
                            output_tokens: 40,
                            cached_input_tokens: 0,
                            cache_creation_input_tokens: 0,
                            cost_usd: Some(0.125),
                        },
                    },
                ),
            ],
            skipped_lines: 0,
        }
    }

    #[test]
    fn summarize_counts_every_event_kind() {
        let summary = summarize(&synthetic_log(Some(0.5)));

        assert_eq!(summary.events, 12);
        assert_eq!(summary.model_calls, 2);
        assert_eq!(summary.tool_calls, 2);
        assert_eq!((summary.succeeded, summary.failed, summary.refused), (1, 1, 0));
        assert_eq!((summary.approvals, summary.approved, summary.denied), (1, 1, 0));
        assert_eq!(
            (
                summary.policy_allow,
                summary.policy_require_approval,
                summary.policy_deny
            ),
            (1, 1, 0)
        );
        assert_eq!(summary.summary_calls, 1);
        assert_eq!(summary.compactions, 1);
        assert_eq!(summary.sandbox_protected_runs, 0);
        assert_eq!(summary.sandbox_unprotected_runs, 0);
        // Thought(100/20) + Thought(50/10) + Summary(200/40)。
        assert_eq!(summary.input_tokens, 350);
        assert_eq!(summary.output_tokens, 70);
        // 0.25 + 0.5 + 0.125，全部是二进制可精确表示的小数。
        assert_eq!(summary.cost_usd, Some(0.875));
        assert!(summary.started_at.is_some() && summary.ended_at.is_some());
    }

    #[test]
    fn sandbox_counts_use_active_for_mixed_windows_and_mcp_calls() {
        let log = SessionLog {
            session_id: SessionId::new("mixed-boundaries"),
            events: [
                ("run_command", "Windows JobObject", true, true),
                ("git_status", "Windows JobObject", true, false),
                ("git_diff", "Windows JobObject", false, false),
                ("run_command", "Windows JobObject", false, true),
                ("mcp__server__search", "MCP subprocess", false, false),
            ]
            .into_iter()
            .enumerate()
            .map(|(index, (tool, platform, active, boundary_checked))| {
                evidence(
                    index as u64 + 1,
                    AuditEvent::SandboxApplied {
                        turn: 1,
                        call: call(tool),
                        platform: platform.to_owned(),
                        active,
                        boundary_checked,
                    },
                )
            })
            .collect(),
            skipped_lines: 0,
        };

        let summary = summarize(&log);
        assert_eq!(summary.sandbox_protected_runs, 2);
        assert_eq!(summary.sandbox_unprotected_runs, 3);

        let report = BenchmarkReport::from_sessions(&[log]);
        let text = render_json(&report).expect("序列化");
        let value: serde_json::Value = serde_json::from_str(&text).expect("JSON 报表");
        assert_eq!(value["sessions"][0]["sandbox_protected_runs"], 2);
        assert_eq!(value["sessions"][0]["sandbox_unprotected_runs"], 3);
        let back: BenchmarkReport = serde_json::from_str(&text).expect("反序列化");
        assert_eq!(back, report);
    }

    #[test]
    fn one_unknown_cost_makes_the_whole_session_unknown() {
        let summary = summarize(&synthetic_log(None));
        assert_eq!(
            summary.cost_usd, None,
            "任一次调用成本未知，会话合计就是未知，不能当 0"
        );
        // token 统计不受影响：成本未知不代表用量未知。
        assert_eq!(summary.input_tokens, 350);
    }

    #[test]
    fn empty_session_yields_all_zeroes_without_panicking() {
        let log = SessionLog {
            session_id: SessionId::new("empty"),
            events: vec![],
            skipped_lines: 0,
        };
        let summary = summarize(&log);
        assert_eq!(summary.events, 0);
        assert_eq!(summary.model_calls, 0);
        assert_eq!(summary.input_tokens, 0);
        assert_eq!(summary.cost_usd, None, "空会话没有成本数据：未知而非 0");
        assert_eq!(summary.started_at, None);
    }

    /// 给会话补一条收尾思考（最终答案），使其以成功态结束。
    fn with_final_answer(mut log: SessionLog, cost: Option<f64>) -> SessionLog {
        let turn = log.events.last().map(|e| e.event.turn()).unwrap_or(1) + 1;
        log.events.push(evidence(
            log.events.len() as u64 + 1,
            AuditEvent::Thought {
                turn,
                text: "最终答案".to_owned(),
                usage: TokenUsage { input_tokens: 10, output_tokens: 5, cached_input_tokens: 0, cache_creation_input_tokens: 0, cost_usd: cost },
            },
        ));
        log
    }

    #[test]
    fn report_classifies_by_final_answer_and_sums_costs() {
        // 会话 A 以 Thought 收尾（已交付答案），判为成功；会话 B 停在
        // 工具观察（被打断），判为失败。
        let mut log_a = with_final_answer(synthetic_log(Some(0.25)), Some(0.5));
        log_a.session_id = SessionId::new("a");
        let mut log_b = synthetic_log(None);
        log_b.session_id = SessionId::new("b");
        // 把 B 的收尾换成工具观察（截到最后一次观察为止）。
        log_b.events.truncate(10);

        let report = BenchmarkReport::from_sessions(&[log_a, log_b]);
        assert_eq!((report.succeeded, report.failed), (1, 1));
        // A: 350+10（补的收尾思考）；B 截到第 10 条（摘要的 200 被截掉）：150。
        assert_eq!(report.total_input_tokens, 510);
        // A 已知（0.25+0.25+0.125+0.5）、B 未知 → 合计未知。
        assert_eq!(report.total_cost_usd, None);
    }

    #[test]
    fn report_total_cost_is_some_only_when_every_session_is_known() {
        let mut log_a = synthetic_log(Some(0.25));
        log_a.session_id = SessionId::new("a");
        let mut log_b = synthetic_log(Some(0.5));
        log_b.session_id = SessionId::new("b");

        let report = BenchmarkReport::from_sessions(&[log_a.clone(), log_b.clone()]);
        assert_eq!(report.total_cost_usd, Some(1.5), "全部已知时合计正确");

        let mut log_c = synthetic_log(None);
        log_c.session_id = SessionId::new("c");
        let report = BenchmarkReport::from_sessions(&[log_a.clone(), log_c.clone()]);
        assert_eq!(report.total_cost_usd, None);
        assert_eq!(report.sessions.len(), 2);

        // 未知出现在中间时，后面的已知成本不能把合计改回已知。
        let report = BenchmarkReport::from_sessions(&[log_a, log_c, log_b]);
        assert_eq!(report.total_cost_usd, None, "已知、未知、已知仍为未知");
        assert_eq!(report.sessions.len(), 3);
    }

    #[test]
    fn empty_report_has_no_cost_data() {
        let report = BenchmarkReport::from_sessions(&[]);
        assert_eq!(report.sessions.len(), 0);
        assert_eq!(report.total_input_tokens, 0);
        assert_eq!(report.total_cost_usd, None, "没有任何数据时是未知而非 0");
        assert_eq!((report.succeeded, report.failed), (0, 0));
    }

    /// 输入 token 为 0 时缓存命中率为 `None`，不是 `0.0`。
    ///
    /// `Some(0.0)` 表示有输入但未命中，`None` 表示没有输入 token 可算
    /// （provider 不报用量）。显示成 0% 会让人误以为缓存失效。
    #[test]
    fn cache_hit_ratio_is_none_when_there_are_no_input_tokens() {
        let summary = summarize(&SessionLog {
            session_id: SessionId::new("no-usage"),
            events: vec![evidence(
                1,
                AuditEvent::Thought {
                    turn: 1,
                    text: "想".to_owned(),
                    // provider 不报用量：input/output 都是 0。
                    usage: TokenUsage::default(),
                },
            )],
            skipped_lines: 0,
        });

        assert_eq!(summary.cache_hit_ratio, None, "没有输入 token 时是未知而非 0%");
        assert_eq!(hit_ratio_label(summary.cache_hit_ratio), "—");
    }

    /// 命中率等于读回除以输入，多轮累加后仍按此口径计算。
    #[test]
    fn cache_hit_ratio_is_read_over_input() {
        let mut log = synthetic_log(Some(0.25));
        log.events.clear();
        log.events = vec![
            evidence(
                1,
                AuditEvent::Thought {
                    turn: 1,
                    text: "第一轮".to_owned(),
                    usage: TokenUsage {
                        input_tokens: 1_000,
                        output_tokens: 10,
                        cached_input_tokens: 0,
                        cache_creation_input_tokens: 1_000,
                        cost_usd: None,
                    },
                },
            ),
            evidence(
                2,
                AuditEvent::Thought {
                    turn: 2,
                    text: "第二轮".to_owned(),
                    usage: TokenUsage {
                        input_tokens: 1_000,
                        output_tokens: 10,
                        cached_input_tokens: 900,
                        cache_creation_input_tokens: 0,
                        cost_usd: None,
                    },
                },
            ),
        ];

        let summary = summarize(&log);

        // 900 / 2000 = 0.45。第一轮全量写入缓存，第二轮几乎全量命中，
        // 符合缓存首次成本高、后续成本低的特点。
        assert_eq!(summary.cached_input_tokens, 900);
        assert_eq!(summary.cache_creation_input_tokens, 1_000);
        assert_eq!(summary.cache_hit_ratio, Some(0.45));
    }

    /// 墙钟为末事件减首事件；轮延迟取相邻思考事件的间隔。
    #[test]
    fn wall_clock_and_turn_latencies_come_from_event_timestamps() {
        let mut log = synthetic_log(Some(0.25));
        // 合成数据的步进是 250ms：三轮思考 → 两个 250ms 间隔。
        log.events.clear();
        log.events = vec![
            evidence(1, AuditEvent::StepStarted { turn: 1, role: AgentRole::Coder }),
            evidence(
                2,
                AuditEvent::Thought {
                    turn: 1,
                    text: "一轮".to_owned(),
                    usage: TokenUsage::default(),
                },
            ),
            evidence(
                3,
                AuditEvent::Thought {
                    turn: 2,
                    text: "二轮".to_owned(),
                    usage: TokenUsage::default(),
                },
            ),
            evidence(
                4,
                AuditEvent::Thought {
                    turn: 3,
                    text: "三轮".to_owned(),
                    usage: TokenUsage::default(),
                },
            ),
        ];

        let summary = summarize(&log);

        assert_eq!(summary.wall_clock_ms, 750, "4 条事件、步进 250ms → 750ms");
        assert_eq!(summary.turn_latencies_ms, vec![250, 250], "两段间隔");
    }

    /// 按引入毫秒时间戳前的格式构造证据：没有 `format` 键，`recorded_at`
    /// 为整秒。
    ///
    /// 使用 `serde_json::from_str` 而非直接构造结构体，以覆盖旧格式的
    /// 反序列化路径。
    fn legacy_evidence(sequence: u64, seconds: u64, event: AuditEvent) -> Evidence {
        let line = serde_json::json!({
            "sequence": sequence,
            "recorded_at": seconds,
            "event": serde_json::to_value(&event).expect("AuditEvent 序列化不应失败"),
        });
        serde_json::from_str::<Evidence>(&line.to_string()).expect("旧格式必须能反序列化")
    }

    /// 旧格式（秒级）证据的轮延迟与等审时间必须是真实毫秒值。
    ///
    /// 反序列化已将秒换算为毫秒存入 `recorded_at`。若统计再按格式号乘
    /// 1000，旧文件的这两列会放大一千倍。墙钟直接读取 `recorded_at`，
    /// 会出现墙钟 60 秒、轮延迟 6000 万毫秒的矛盾。该问题只影响旧文件，
    /// 需要一条专门覆盖旧格式的测试锁定。
    #[test]
    fn legacy_format_latency_and_approval_wait_are_real_milliseconds() {
        let log = SessionLog {
            session_id: SessionId::new("session-legacy"),
            skipped_lines: 0,
            events: vec![
                legacy_evidence(
                    1,
                    100,
                    AuditEvent::Thought {
                        turn: 1,
                        text: "想".to_owned(),
                        usage: TokenUsage::default(),
                    },
                ),
                legacy_evidence(
                    2,
                    160,
                    AuditEvent::Thought {
                        turn: 2,
                        text: "再想".to_owned(),
                        usage: TokenUsage::default(),
                    },
                ),
                legacy_evidence(
                    3,
                    200,
                    AuditEvent::PolicyEvaluated {
                        turn: 3,
                        tool: "write_file".to_owned(),
                        verdict: crate::domain::policy::PolicyVerdict::RequireApproval,
                        rule_index: None,
                        reason: "默认".to_owned(),
                    },
                ),
                legacy_evidence(
                    4,
                    260,
                    AuditEvent::ApprovalDecided {
                        turn: 3,
                        tool: "write_file".to_owned(),
                        allowed: true,
                        reason: None,
                    },
                ),
            ],
        };

        let summary = summarize(&log);

        assert_eq!(summary.wall_clock_ms, 160_000, "墙钟：100 → 260 秒");
        assert_eq!(
            summary.turn_latencies_ms,
            vec![60_000],
            "两轮间隔 60 秒，不是 6000 万毫秒"
        );
        assert_eq!(summary.approval_wait_ms, 60_000, "等审 60 秒");
    }

    /// 越界写入升级产生的审批也计入等审时间。
    ///
    /// 升级路径没有 `RequireApproval` 锚点：策略记录原判 Allow（审计不改
    /// 写策略原判），升级发生在裁决记录之后。若统计只认
    /// `RequireApproval`，这类"策略放行但启发式拦下"的审批将始终显示
    /// 0ms。
    #[test]
    fn escalated_approval_wait_is_counted_from_the_original_verdict() {
        let log = SessionLog {
            session_id: SessionId::new("session-esc"),
            skipped_lines: 0,
            events: vec![
                evidence(
                    1,
                    AuditEvent::PolicyEvaluated {
                        turn: 1,
                        tool: "run_command".to_owned(),
                        verdict: crate::domain::policy::PolicyVerdict::Allow,
                        rule_index: None,
                        reason: "默认基线".to_owned(),
                    },
                ),
                // 同一轮紧接着的审批决定：升级路径上两者之间没有别的锚点。
                evidence(
                    2,
                    AuditEvent::ApprovalDecided {
                        turn: 1,
                        tool: "run_command".to_owned(),
                        allowed: true,
                        reason: None,
                    },
                ),
            ],
        };

        let summary = summarize(&log);

        assert_eq!(summary.policy_allow, 1, "证据里仍是策略原判");
        assert_eq!(summary.policy_require_approval, 0, "升级不增加 RequireApproval 计数");
        assert_eq!(summary.approvals, 1);
        assert_eq!(summary.approval_wait_ms, 250, "升级路径的等待必须计入");
    }

    /// 空会话没有时间数据：墙钟为 0，无轮延迟，无命中率。
    #[test]
    fn empty_session_has_no_timing_data() {
        let log = SessionLog {
            session_id: SessionId::new("empty"),
            events: vec![],
            skipped_lines: 0,
        };

        let summary = summarize(&log);

        assert_eq!(summary.wall_clock_ms, 0);
        assert!(summary.turn_latencies_ms.is_empty());
        assert_eq!(summary.cache_hit_ratio, None);
        assert!(summary.repeated_calls.is_empty());
    }

    /// 同工具且同参数的重复调用应被识别。
    #[test]
    fn repeated_calls_names_tools_called_twice_with_the_same_arguments() {
        let mut log = synthetic_log(Some(0.25));
        log.events.clear();
        let read = |path: &str| ToolCall {
            id: CallId::new("call_1"),
            name: "read_file".to_owned(),
            arguments: json!({"path": path}),
        };
        log.events = vec![
            evidence(1, AuditEvent::ToolRequested { turn: 1, call: read("Cargo.toml") }),
            evidence(1, AuditEvent::ToolRequested { turn: 1, call: read("src/lib.rs") }),
            // 同一个文件再来一遍：重复。
            evidence(2, AuditEvent::ToolRequested { turn: 2, call: read("Cargo.toml") }),
            // 同名不同参：不是重复。
            evidence(3, AuditEvent::ToolRequested { turn: 3, call: read("README.md") }),
            evidence(4, AuditEvent::ToolRequested { turn: 4, call: read("README.md") }),
        ];

        let summary = summarize(&log);

        assert_eq!(summary.tool_calls, 5);
        // `read_file` 被重复调用，两组参数各重复一次；名称去重后只列一次。
        assert_eq!(summary.repeated_calls, vec!["read_file"]);
    }

    /// 键序不同但内容相同的参数视为同一次调用。
    ///
    /// 若不做规范化比较，同一请求会因键序变化被误判为两次调用，而这类
    /// 重发是最常见的浪费。
    #[test]
    fn repeated_calls_survives_argument_key_reordering() {
        let log = SessionLog {
            session_id: SessionId::new("keyorder"),
            events: vec![
                evidence(
                    1,
                    AuditEvent::ToolRequested {
                        turn: 1,
                        call: ToolCall {
                            id: CallId::new("c1"),
                            name: "read_file".to_owned(),
                            arguments: json!({"path": "a.rs", "encoding": "utf8"}),
                        },
                    },
                ),
                evidence(
                    2,
                    AuditEvent::ToolRequested {
                        turn: 2,
                        call: ToolCall {
                            id: CallId::new("c2"),
                            name: "read_file".to_owned(),
                            arguments: json!({"encoding": "utf8", "path": "a.rs"}),
                        },
                    },
                ),
            ],
            skipped_lines: 0,
        };

        assert_eq!(summarize(&log).repeated_calls, vec!["read_file"]);
    }

    /// 报表合计行包含新增的三个维度。
    #[test]
    fn report_totals_carry_the_throughput_dimensions() {
        let mut log = synthetic_log(Some(0.25));
        log.session_id = SessionId::new("a");
        // 给会话补一条带缓存计数的收尾思考。
        let last = log.events.len() as u64 + 1;
        log.events.push(evidence(
            last,
            AuditEvent::Thought {
                turn: 9,
                text: "最终答案".to_owned(),
                usage: TokenUsage {
                    input_tokens: 400,
                    output_tokens: 20,
                    cached_input_tokens: 300,
                    cache_creation_input_tokens: 100,
                    cost_usd: Some(0.25),
                },
            },
        ));

        let report = BenchmarkReport::from_sessions(&[log]);

        assert_eq!(report.total_cached_input_tokens, 300);
        // 墙钟合计来自各会话 wall_clock_ms 之和（这里只有一个会话）。
        assert_eq!(report.total_wall_clock_ms, report.sessions[0].wall_clock_ms);
        assert_eq!(report.total_approvals, 1, "审批次数进合计");
    }

    /// 命中率列：有输入 token 但未命中时为 0%，没有输入 token 时为"—"。
    #[test]
    fn text_rendering_shows_hit_ratio_and_wall_clock() {
        let log = synthetic_log(Some(0.25));
        let report = BenchmarkReport::from_sessions(&[log]);
        let text = render_text(&report);

        assert!(text.contains("命中率"), "{text}");
        assert!(text.contains("墙钟ms"), "{text}");
        // 合成数据有 350 个输入 token 且未命中，应显示 0% 而非"—"。
        assert!(text.contains("0%"), "有输入但没命中应显示 0%: {text}");

        // 没有用量数据时才显示"—"。`SummaryGenerated` 也记录用量，需一并
        // 清零，否则分母不为 0。
        let mut bare = synthetic_log(Some(0.25));
        for evidence in &mut bare.events {
            match &mut evidence.event {
                AuditEvent::Thought { usage, .. }
                | AuditEvent::SummaryGenerated { usage, .. } => *usage = TokenUsage::default(),
                _ => {}
            }
        }
        let text = render_text(&BenchmarkReport::from_sessions(&[bare]));
        assert!(text.contains("—"), "没有输入 token 应显示破折号: {text}");
    }

    #[test]
    fn json_rendering_round_trips() {
        let mut log = synthetic_log(Some(0.25));
        log.session_id = SessionId::new("session-roundtrip");
        let report = BenchmarkReport::from_sessions(&[log]);

        let text = render_json(&report).expect("序列化");
        let back: BenchmarkReport = serde_json::from_str(&text).expect("反序列化");
        assert_eq!(back, report);
    }

    #[test]
    fn legacy_json_report_defaults_missing_sandbox_counts_to_zero() {
        let report = BenchmarkReport::from_sessions(&[synthetic_log(Some(0.25))]);
        let text = render_json(&report).expect("序列化");
        let mut value: serde_json::Value = serde_json::from_str(&text).expect("JSON 报表");
        let session = value["sessions"][0].as_object_mut().expect("会话摘要");
        session.remove("sandbox_protected_runs").expect("新增字段");
        session.remove("sandbox_unprotected_runs").expect("新增字段");

        let back: BenchmarkReport = serde_json::from_value(value).expect("旧报表必须能读");
        assert_eq!(back.sessions[0].sandbox_protected_runs, 0);
        assert_eq!(back.sessions[0].sandbox_unprotected_runs, 0);
        assert_eq!(back, report);
    }

    // ── 证据台账 ────────────────────────────────────────────────────────

    /// 报表中可见任务的实际操作。
    ///
    /// 台账每项都是列表，无法放入表格，单独列在表后。读者无需查看
    /// JSONL 即可知道操作过哪些文件、执行过哪些命令。
    #[test]
    fn the_text_report_lists_the_evidence_ledger() {
        let report = BenchmarkReport::from_sessions(&[synthetic_log(None)]);
        let text = render_text(&report);

        assert!(text.contains("证据台账"), "{text}");
        assert!(text.contains("写过的文件"), "{text}");
        // 没成功的调用必须出现在台账里：这是"未完成事项"的唯一来源。
        assert!(text.contains("没成功的调用"), "{text}");
        assert!(text.contains("文件不存在"), "{text}");
    }

    /// JSON 中包含同一份台账，供机器读取。
    #[test]
    fn the_json_report_carries_the_same_ledger() {
        let report = BenchmarkReport::from_sessions(&[synthetic_log(None)]);
        let text = render_json(&report).expect("序列化");

        assert!(text.contains("evidence_index"), "{text}");
        assert!(text.contains("文件不存在"), "{text}");
    }

    // ── 审批等待 ───────────────────────────────────────────────────────

    /// 等待时长等于"判定需审批"到"闸门给出结论"的间隔。
    ///
    /// 合成数据中事件 3（RequireApproval）在 750ms，事件 5
    /// （ApprovalDecided）在 1250ms，间隔为 500ms。
    #[test]
    fn approval_wait_spans_the_verdict_and_the_decision() {
        let summary = summarize(&synthetic_log(Some(0.5)));

        assert_eq!(summary.approval_wait_ms, 500);
    }

    /// 策略放行的调用不计入等待时间，因为它不经过审批闸门。
    ///
    /// 若把 Allow 到下一事件的间隔算作等待，会把工具执行时间记到人的
    /// 头上，显示为"人等了两秒"，而实际并未询问用户。
    #[test]
    fn a_policy_allowed_call_contributes_no_wait() {
        let log = SessionLog {
            session_id: SessionId::new("session-allow-only"),
            events: vec![
                evidence(
                    1,
                    AuditEvent::PolicyEvaluated {
                        turn: 1,
                        tool: "read_file".to_owned(),
                        verdict: crate::domain::policy::PolicyVerdict::Allow,
                        rule_index: Some(0),
                        reason: "只读".to_owned(),
                    },
                ),
                evidence(2, AuditEvent::ToolRequested { turn: 1, call: call("read_file") }),
                evidence(
                    3,
                    AuditEvent::ToolObserved {
                        turn: 1,
                        call: call("read_file"),
                        outcome: ToolOutcome::success("内容"),
                    },
                ),
            ],
            skipped_lines: 0,
        };

        assert_eq!(summarize(&log).approval_wait_ms, 0);
    }

    /// 多次审批的等待时间应累加，而不是只算最后一次。
    #[test]
    fn approval_wait_accumulates_across_calls() {
        let mut log = synthetic_log(Some(0.5));
        // 在第二条思考之后补一轮"要审 → 结论"：事件 13 在 3250ms、
        // 事件 14 在 3500ms，与原有的 500ms 合计 750ms。
        log.events.push(evidence(
            13,
            AuditEvent::PolicyEvaluated {
                turn: 3,
                tool: "write_file".to_owned(),
                verdict: crate::domain::policy::PolicyVerdict::RequireApproval,
                rule_index: None,
                reason: "默认".to_owned(),
            },
        ));
        log.events.push(evidence(
            14,
            AuditEvent::ApprovalDecided {
                turn: 3,
                tool: "write_file".to_owned(),
                allowed: false,
                reason: Some("不让写".to_owned()),
            },
        ));

        assert_eq!(summarize(&log).approval_wait_ms, 750);
    }

    /// 报表单独列等待时长，并给出跨会话合计。
    ///
    /// 审批成本的验收依据这一列：减少询问次数或确认操作，最终都应体现
    /// 为该值下降。否则"等待成本降低"无法核对。
    #[test]
    fn report_shows_approval_wait_beside_the_approval_count() {
        let report = BenchmarkReport::from_sessions(&[
            synthetic_log(Some(0.25)),
            synthetic_log(Some(0.25)),
        ]);

        assert_eq!(report.total_approvals, 2);
        assert_eq!(report.total_approval_wait_ms, 1000);

        let text = render_text(&report);
        assert!(text.contains("等审ms"), "表头应单列等人时长: {text}");
        assert!(text.contains("1000"), "合计行应给出等人时长: {text}");
    }

    #[test]
    fn text_rendering_marks_unknown_cost_honestly() {
        let log = synthetic_log(None);
        let report = BenchmarkReport::from_sessions(&[log]);
        let text = render_text(&report);

        assert!(text.contains("未知"), "成本未知必须显示'未知'：{text}");
        assert!(!text.contains("$0.0000"), "不能把未知渲染成 0：{text}");
        assert!(text.contains("合计"));
    }

    #[test]
    fn text_rendering_aligns_known_costs() {
        // 0.875（第二次思考）+ 0.25（第一次）+ 0.125（摘要）= 1.25。
        let log = synthetic_log(Some(0.875));
        let report = BenchmarkReport::from_sessions(&[log]);
        let text = render_text(&report);
        assert!(text.contains("$1.2500"), "{text}");
    }

    // ── MCP 生命周期统计 ───────────────────────────────────────────────

    fn lifecycle(sequence: u64, kind: McpLifecycleKind) -> Evidence {
        evidence(
            sequence,
            AuditEvent::McpLifecycle {
                turn: 0,
                fact: McpLifecycleFact {
                    server: "demo".to_owned(),
                    generation: 1,
                    kind,
                    fault: None,
                    call_id: None,
                    process_isolation: true,
                    tools_changed: false,
                },
            },
        )
    }

    fn call_failure(sequence: u64, kind: McpFaultKind, result_unknown: bool) -> Evidence {
        evidence(
            sequence,
            AuditEvent::McpCallFailed {
                turn: 1,
                call_id: CallId::new("call_1"),
                tool: "mcp__demo__send_message".to_owned(),
                failure: McpCallFailure {
                    server: "demo".to_owned(),
                    generation: 1,
                    kind,
                    result_unknown,
                    process_isolation: true,
                },
            },
        )
    }

    /// 连接事件与调用故障分别计数，不计入工具结局或沙箱执行次数。
    ///
    /// 启动失败没有工具调用，计为"工具失败 +1"会让人以为模型尝试过；
    /// 一次断连若按共享连接的每个工具各计一次，会产生多次不存在的故障。
    #[test]
    fn mcp_lifecycle_events_count_as_connection_facts_not_tool_outcomes() {
        let mut log = synthetic_log(Some(0.25));
        log.events.extend([
            lifecycle(14, McpLifecycleKind::ConnectFailed),
            lifecycle(15, McpLifecycleKind::Disconnected),
            lifecycle(16, McpLifecycleKind::Recovered),
            lifecycle(17, McpLifecycleKind::RecoveryFailed),
            lifecycle(18, McpLifecycleKind::Connected),
            lifecycle(19, McpLifecycleKind::Closed),
            call_failure(20, McpFaultKind::CallTimeout, true),
            call_failure(21, McpFaultKind::ServerError, false),
        ]);

        let summary = summarize(&log);
        assert_eq!(summary.mcp_connect_failures, 1);
        assert_eq!(summary.mcp_disconnects, 1);
        assert_eq!(summary.mcp_recoveries, 1);
        assert_eq!(summary.mcp_recovery_failures, 1);
        assert_eq!(summary.mcp_call_timeouts, 1);
        assert_eq!(
            summary.mcp_unknown_results, 1,
            "只有 result_unknown 的那次算未知结果"
        );
        // 连接事件不计入工具与沙箱统计。
        assert_eq!(summary.tool_calls, 2, "工具调用数只该来自 ToolRequested");
        assert_eq!(summary.succeeded, 1);
        assert_eq!(summary.failed, 1);
        assert_eq!(summary.sandbox_protected_runs, 0);
        assert_eq!(summary.sandbox_unprotected_runs, 0);
    }

    /// 只含连接事件的日志不算一次任务运行。
    ///
    /// 它没有模型调用，`cost_usd` 必然未知。按原有合计规则，一条这样的
    /// 日志会让整轮基准成本变为未知，并被记为一个失败会话，显示为
    /// "3 个任务中失败 1 个、成本未知"，而实际任务是全部完成的。
    #[test]
    fn lifecycle_only_log_is_not_counted_as_a_session_run() {
        let task_log = with_final_answer(synthetic_log(Some(0.25)), Some(0.5));
        let task_wall_clock = summarize(&task_log).wall_clock_ms;
        let mut connection_log = synthetic_log(Some(0.25));
        connection_log.session_id = SessionId::new("mcp-demo");
        connection_log.events = vec![
            lifecycle(1, McpLifecycleKind::ConnectFailed),
            lifecycle(2, McpLifecycleKind::Disconnected),
        ];

        let report = BenchmarkReport::from_sessions(&[task_log.clone(), connection_log.clone()]);
        assert!(is_lifecycle_only(&connection_log));
        assert!(!is_lifecycle_only(&task_log));
        assert_eq!(report.mcp_lifecycle_logs, 1);
        assert_eq!(report.succeeded, 1, "连接日志不该算失败会话");
        assert_eq!(report.failed, 0);
        assert_eq!(
            report.total_wall_clock_ms, task_wall_clock,
            "墙钟合计只能来自真正的任务会话"
        );
        assert!(
            report.total_cost_usd.is_some(),
            "连接日志没有成本数据，不该把已知成本拖成未知"
        );
        // 两份日志的 MCP 统计都保留：排除的只是会话成败。
        assert_eq!(report.sessions[1].mcp_connect_failures, 1);
        assert_eq!(report.sessions[1].mcp_disconnects, 1);
        assert_eq!(report.sessions[1].events, 2);
    }

    /// 末尾的 `Closed` 不应把已交付答案的会话判为失败。
    #[test]
    fn trailing_lifecycle_close_does_not_undo_a_delivered_answer() {
        // `with_final_answer` 以 Thought 收尾；再补一条关闭事件模拟进程退出。
        let mut log = with_final_answer(synthetic_log(Some(0.25)), Some(0.5));
        let last = log.events.len() as u64 + 1;
        log.events.push(lifecycle(last, McpLifecycleKind::Closed));

        let report = BenchmarkReport::from_sessions(&[log]);
        assert_eq!(report.succeeded, 1, "答案已交付，关闭事件只是收尾");
        assert_eq!(report.failed, 0);
    }

    /// 空日志不适用该规则：没有事件表示坏文件，不是连接日志。
    #[test]
    fn empty_log_is_not_a_lifecycle_log() {
        let log = SessionLog {
            session_id: SessionId::new("empty"),
            events: Vec::new(),
            skipped_lines: 0,
        };
        assert!(!is_lifecycle_only(&log));
    }

    /// 旧 JSON 报表缺少这些字段时仍可读取，默认值为 0。
    #[test]
    fn legacy_json_report_defaults_missing_mcp_counts_to_zero() {
        let report = BenchmarkReport::from_sessions(&[synthetic_log(Some(0.25))]);
        let text = render_json(&report).expect("序列化");
        let mut value: serde_json::Value = serde_json::from_str(&text).expect("JSON 报表");
        let session = value["sessions"][0].as_object_mut().expect("会话摘要");
        for field in [
            "mcp_connect_failures",
            "mcp_disconnects",
            "mcp_recoveries",
            "mcp_recovery_failures",
            "mcp_call_timeouts",
            "mcp_unknown_results",
        ] {
            session.remove(field).expect("新增字段必须在");
        }
        value
            .as_object_mut()
            .expect("报表")
            .remove("mcp_lifecycle_logs")
            .expect("新增报表字段必须在");

        let back: BenchmarkReport = serde_json::from_value(value).expect("旧报表必须能读");
        assert_eq!(back.sessions[0].mcp_connect_failures, 0);
        assert_eq!(back.sessions[0].mcp_unknown_results, 0);
        assert_eq!(back.mcp_lifecycle_logs, 0);
        assert_eq!(back, report);
    }

    /// 文本报表需说明故障与恢复，包括"结果未知不等于已取消"。
    #[test]
    fn text_report_summarizes_mcp_faults_and_recovery() {
        let mut log = synthetic_log(Some(0.25));
        log.events
            .push(lifecycle(14, McpLifecycleKind::Disconnected));
        log.events
            .push(call_failure(15, McpFaultKind::CallTimeout, true));
        let report = BenchmarkReport::from_sessions(&[log]);
        let text = render_text(&report);

        assert!(text.contains("MCP 生命周期"), "{text}");
        assert!(text.contains("断连 1 次"), "{text}");
        assert!(text.contains("结果未知 1 次"), "{text}");
        assert!(text.contains("不会自动重放"), "{text}");
    }

    /// 未涉及 MCP 的报表不显示该段，避免无信息占位。
    #[test]
    fn text_report_omits_the_mcp_section_when_unused() {
        let report = BenchmarkReport::from_sessions(&[synthetic_log(Some(0.25))]);
        let text = render_text(&report);
        assert!(!text.contains("MCP 生命周期"), "{text}");
    }
}

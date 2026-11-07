//! 运行报表：从证据日志统计出成功率、工具结局、审批分布与成本。
//!
//! [`summarize`] 与 [`BenchmarkReport::from_sessions`] 是纯函数——
//! 无 IO、无时钟，输入相同的 `SessionLog` 永远得到相同的摘要；
//! 证据怎么读进来（[`crate::infrastructure::evidence::reader`]）与
//! 报表怎么渲染出去都在这两个函数之外，统计逻辑因此可以直接用
//! 合成数据测试。
//!
//! [`SessionLog`] 放在本模块而不是 reader：统计是它的消费方，
//! 而 infrastructure 依赖 application 是本仓库既定的方向
//! （证据汇依赖端口同此理）。

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
    /// 统计要区分"这个会话本来就没干什么"和"日志坏了一部分"，
    /// 只看 `events.len()` 会把后者误读成前者。
    pub skipped_lines: usize,
}

impl SessionLog {
    /// 事件序号是否严格连续（每条比前一条大 1）。
    ///
    /// 同一个 sink 生命周期内序号严格递增；文件里出现乱序或跳号，
    /// 说明日志被并发写过、或中途有写入失败被丢弃。这只影响解读
    /// （比如"序号 5 之前发生过写失败"），不足以让整个文件作废，
    /// 所以只暴露查询，不在读取时报错。
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
    /// 工具调用按结局分类。
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
    /// 等人点头一共花了多少毫秒。
    ///
    /// 取每条 `RequireApproval` 裁决到紧随其后的 `ApprovalDecided` 的
    /// 时间差。P9-5 要减少的就是这段等待，所以它必须是个可测的数——
    /// 只数审批次数看不出等待成本：两次秒点的 `y` 和两次人去泡咖啡的
    /// `y` 在旧报表里一模一样。
    ///
    /// 只数"策略判了要审"的那几条：策略放行的调用压根不进闸门，把它
    /// 后续的间隔算成"等人"会把工具执行时间记到人头上。
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
    /// 成本合计（美元）。任一次调用成本未知时为 `None`——
    /// 部分已知的合计是被低估的数字，比"未知"更危险。
    pub cost_usd: Option<f64>,
    /// 摘要调用次数（`SummaryGenerated` + `SummaryFailed`）。
    pub summary_calls: usize,
    /// 上下文裁剪次数（`ContextCompacted` 数）。
    pub compactions: usize,
    /// 上下文裁剪净省下的 token 合计（`ContextBudgeted` 数）。
    ///
    /// 轮数是裁剪的手段，token 才是花掉的钱：长任务里"裁了 3 轮"可能
    /// 省了两百个 token，也可能省了两万个。这个数是后者的合计。
    pub dropped_context_tokens: u64,
    /// 这个会话动过什么：读过的文件、写过的文件、跑过的命令、没成功的调用。
    ///
    /// 与模型在上下文里看到的是**同一份账**（同一套判定规则，见
    /// [`EvidenceIndex::from_events`] 与 [`EvidenceIndex::from_session`]）：
    /// 用户在这里看到的"做过什么"，与模型看到的不许有出入。
    pub evidence_index: EvidenceIndex,
    /// 恢复时提示"这个调用可能已经跑过"的次数（`ToolCallReplayed` 数）。
    ///
    /// 它是**可能发生**的重复副作用的计数，不是已经发生的：程序在这类
    /// 情况下不会自动重跑，而是把决定权交回模型。数值不为 0 就说明
    /// 这一趟里有调用是在"结果未知"的状态下被打断的。
    pub replays: usize,
    /// 恢复时重新询问审批的次数（`ApprovalReasked` 数）。
    ///
    /// 与 `approvals` 分开记：它回答的是"人被问了几次"——重新询问时人
    /// 必须再点一次，那次的等待也真实地花在他身上。
    pub approval_reasks: usize,
    /// 启用进程限额与进程树管理的执行次数（`SandboxApplied` 且 `active`）。
    #[serde(default)]
    pub sandbox_protected_runs: usize,
    /// 未启用进程限额与进程树管理的记录数（`SandboxApplied` 且非 `active`）。
    ///
    /// 包括非 Windows 平台的本地子进程、Windows 上启动后 `assign`
    /// 失败的子进程，以及交给外部 MCP Server 的调用尝试。
    #[serde(default)]
    pub sandbox_unprotected_runs: usize,
    /// 首事件时刻。
    pub started_at: Option<SystemTime>,
    /// 末事件时刻。
    pub ended_at: Option<SystemTime>,
    /// 本次会话的首末事件间隔（毫秒）。空会话为 0。
    ///
    /// 用末事件减首事件而不是另设计时器：事件时间戳本来就在证据里，
    /// 再起一个时钟只会多一处可能对不上的真相。它不含会话创建与
    /// 证据文件打开的开销——那些发生在第一条事件之前。
    pub wall_clock_ms: u64,
    /// 每轮模型调用的耗时（毫秒），按调用顺序。空会话为空。
    ///
    /// 一轮的耗时取"本轮思考事件"与"上一轮思考事件"的时间差：模型
    /// 调用是这两点之间发生的事，工具执行的结果落在下一轮思考之前，
    /// 所以这个差值同时含 prefill 与 decode，也含这一轮触发的工具
    /// 执行——它是"用户等待了一轮"的总时长，正是要优化的量。
    pub turn_latencies_ms: Vec<u64>,
    /// 缓存命中率：读回的 token 占输入 token 的比例。分母为 0 时为 `None`。
    ///
    /// 分母为 0 时报 `None` 而不是 `0.0`：两者含义相反——0.0 是
    /// "有输入但一次都没命中"，`None` 是"压根没有输入 token 可算"
    /// （provider 不报用量，或会话零调用）。把后者显示成 0% 会让
    /// 用户以为缓存坏了，与"成本未知不填 0"是同一条诚实线。
    pub cache_hit_ratio: Option<f64>,
    /// 同一会话内被重复调用（同工具名 + 同一份参数）的工具名。
    ///
    /// 按名称去重后按首次出现排序。重复调用是轮次浪费最直接的信号：
    /// 同一个文件读两遍，说明上一条观察没能让模型满意，或者它忘了
    /// 自己已经读过——两者都指向提示词与工具描述可以改进的地方。
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
    /// 会话之间是串行跑的，所以这个合计就是"这一轮基准跑了多久"的
    /// 下界——它不含 fixture 拷贝、判据命令与证据读回的开销。
    pub total_wall_clock_ms: u64,
    /// 全部会话的审批次数合计。
    ///
    /// 基准强制 `AutoApproveGate`（无人值守），所以这里在基准里恒为 0；
    /// 真实使用中的审批次数要从 `--report` 看。两处的 `approvals`
    /// 是同一个字段，只是基准里那个闸门不放人进来。
    pub total_approvals: usize,
    /// 全部会话的等人点头耗时合计（毫秒）。
    ///
    /// 与 [`Self::total_approvals`] 分开：次数回答"被打断几回"，
    /// 这一列回答"被打断多久"。P9-5（默认基线免审 + `p` 前缀建议）
    /// 要同时压这两个数，而压不下来的是时间——少问两次但每次都等五分钟，
    /// 用户的体感没有任何改善。
    pub total_approval_wait_ms: u64,
    /// 全部会话的成本合计。任一会话成本未知时为 `None`。
    pub total_cost_usd: Option<f64>,
    /// 成功会话数：会话以 `Thought` 事件收尾（ReAct 循环只有交付
    /// 最终答案时才以思考收尾；被预算/超时打断的会话停在工具观察上）。
    pub succeeded: usize,
    /// 失败会话数（未以 `Thought` 收尾）。
    pub failed: usize,
    /// 只含 MCP 连接事件的日志份数。
    ///
    /// 这类日志不是一次任务运行：没有模型调用、没有工具调用，成本与墙钟
    /// 都无从谈起。它按事件内容识别（不看文件名），从 [`Self::succeeded`]、
    /// [`Self::failed`] 与总成本/总墙钟里排除，只贡献 MCP 统计——否则
    /// "MCP Server 没起来"会被读成"任务失败"，而启动次数会被读成调用次数。
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
        // 没有任何成本数据（空清单或全部会话零调用）时是"未知"；
        // 每有一个已知成本就累加，遇到未知即整体转未知。
        let mut total_cost = None;
        let mut succeeded = 0usize;
        let mut lifecycle_logs = 0usize;
        for (log, summary) in logs.iter().zip(&sessions) {
            // 只含连接事件的日志：它没有模型调用、没有工具调用，成本必然
            // "未知"、墙钟只是 Server 的存活时长。计进任何一项合计都会让
            // 报表说谎——把"MCP 没起来"说成"这轮基准又失败了一个任务"，
            // 把启动次数说成调用次数。它的价值只在 MCP 统计里，而那份统计
            // 在 `sessions` 中已经算好。
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
                Some(cost) => {
                    total_cost = Some(total_cost.unwrap_or(0.0) + cost);
                }
                None => total_cost = None,
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

/// 这份日志是不是只有 MCP 连接事件。
///
/// 判据只看事件内容，不看文件名：日志落在哪个文件里是装配层的选择，而
/// "这份日志算不算一次任务运行"是证据自己说明的事。按名字认，改名或多写
/// 一条普通事件就会悄悄改变统计口径。
///
/// 空日志不算：一条事件都没有意味着读到了一个空文件或坏文件，那正是
/// 原有"会话失败"判据要暴露的情况，不该被这条规则吸收。
pub fn is_lifecycle_only(log: &SessionLog) -> bool {
    !log.events.is_empty()
        && log
            .events
            .iter()
            .all(|evidence| matches!(evidence.event, AuditEvent::McpLifecycle { .. }))
}

/// 会话是否以最终答案收尾：最后一条**任务**事件是 `Thought`。
///
/// ReAct 循环只有交付最终答案（模型回复不带工具调用）时才以思考
/// 收尾；被预算、超时或取消打断的会话停在工具观察或审批上。
/// 这是"成功"的机器可查代理判据——它回答的是"有没有交付答案"，
/// 不是"答案对不对"，后者只有基准任务的判据（P6-3）能回答。
///
/// 连接事件被跳过：生命周期日志与任务日志混写时，收尾的那条 `Closed`
/// 是进程退出前关 Server 留下的，模型早就交付过答案了。把它当成"最后
/// 一条事件"会把成功会话判成失败——成败判据要回答的是任务做到哪一步，
/// 而连接什么时候断的不在这个问题的范围里。
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
/// 纯函数：无 IO、无时钟。成本只在**每次**调用都已知时才有合计；
/// token 对 `Thought` 与 `SummaryGenerated` 两类调用求和（摘要调用
/// 是真实花出去的配额，不计的话 token 账对不上）。
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

    // 上一轮思考事件的时刻。用它算本轮延迟：模型调用发生在两个思考
    // 事件之间，这个差值就是"用户等了一轮"的总时长。
    let mut previous_thought_at: Option<u64> = None;
    // 最近一条"要审批"裁决的时刻。等到紧随的 ApprovalDecided 时，
    // 两者的差就是人花了多久点头——P9-5 要压的正是这段。
    let mut pending_approval_at: Option<u64> = None;
    // 已见过的调用（工具名 + 参数），用于识别重复。`Value` 的相等性是
    // 结构化的：对象比内容不比键序。
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
                // 直接比 `Value` 而不是比序列化字符串：`serde_json` 开了
                // `preserve_order`（rig-core 带的），同一份参数换个键序
                // 序列化出来是两个不同的字符串，会把"模型重发了同一条
                // 请求"漏判成两次不同调用。而 `Value` 的对象相等性是
                // 结构化的，键序无关——正好是这里要的语义。
                let key = (call.name.clone(), call.arguments.clone());
                // 第一次见到某组参数：入表。之后再见到：同名则点名（只点
                // 一次）。两个分支互斥，重复的参数不会反复入表。
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
                // 等到了结论就把这段等待结算掉。
                let asked_at = pending_approval_at.take().or_else(|| {
                    // 越界写入升级的调用在这里没有锚点：策略记录的是原判
                    // Allow（审计不该改写策略的原判），升级发生在裁决记录
                    // 之后。它同样让人等了，漏掉会让 P9-5 的等审列对这条
                    // 真实路径失真。从本次决定往前找最近一条同工具同轮次
                    // 的 Allow 裁决，把它当作等待起点——那正是这条调用
                    // 最早"需要人过目"的时刻。
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
                // 取不到锚点（日志被截断、或这条决定前面确实没有可追溯的
                // 裁决）时不计——没有锚点的间隔是猜的，猜出来的数字比没有更糟。
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
            // 恢复路径的两类事件：可能已执行的调用与重新询问的审批。
            AuditEvent::ToolCallReplayed { .. } => summary.replays += 1,
            AuditEvent::ApprovalReasked { .. } => summary.approval_reasks += 1,
            // 按每次调用的 active 计数；同一会话可混用本地进程与 MCP，
            // platform 标签不一定相同，详情保留在 JSONL 中。
            AuditEvent::SandboxApplied { active, .. } => {
                if *active {
                    summary.sandbox_protected_runs += 1;
                } else {
                    summary.sandbox_unprotected_runs += 1;
                }
            }
            // StepStarted 不计任何业务计数器，但它算事件总数的一员。
            AuditEvent::StepStarted { .. } => {}
            // 连接级与调用级 MCP 故障：只进各自的 MCP 计数，不冒充工具
            // 结局（succeeded/failed）或沙箱执行次数——一次启动失败没有
            // 工具调用，把启动次数混进调用次数会让报表自相矛盾。
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
    // 证据台账：与模型在上下文里看到的是同一份账。
    summary.evidence_index =
        EvidenceIndex::from_events(log.events.iter().map(|evidence| &evidence.event));

    // 缓存命中率：分子读回、分母输入。分母 0 时是 None 而不是 0.0——
    // 0.0 的含义是"有输入但一次没命中"，与"没有输入可算"正好相反。
    if summary.input_tokens > 0 {
        summary.cache_hit_ratio =
            Some(summary.cached_input_tokens as f64 / summary.input_tokens as f64);
    }

    // 空会话没有任何成本数据："未知"而不是 0。
    if summary.model_calls == 0 && summary.summary_calls == 0 {
        summary.cost_usd = None;
    }
    summary
}

/// 首末事件的间隔（毫秒）。
///
/// 空会话或只有一条事件时是 0：没有"间隔"可算，填 0 比填 None 好用——
/// 调用方要做的是累加与比较，不需要为"一条事件的会话"单开一条分支。
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

/// 成本列的显示值：未知就写"未知"，绝不显示 `$0.0000`。
/// 报表渲染与基准运行器的进度行共用同一口径。
pub(crate) fn cost_label(cost: Option<f64>) -> String {
    match cost {
        Some(value) => format!("${value:.4}"),
        None => "未知".to_owned(),
    }
}

/// 缓存命中率的显示值：分母为 0 时写"—"而不是 0%。
///
/// 0% 的含义是"有输入 token，但一次都没命中缓存"，而 `None` 的含义是
/// "没有输入 token 可算"（provider 不报用量，或会话零调用）。把后者
/// 显示成 0% 会让人以为缓存坏了——与成本列"未知不填 0"是同一条线。
pub(crate) fn hit_ratio_label(ratio: Option<f64>) -> String {
    match ratio {
        Some(value) => format!("{:.0}%", value * 100.0),
        None => "—".to_owned(),
    }
}

/// 终端对齐时一个字符的显示宽度：CJK 全角算 2，其余算 1。
/// 手写这十行而不是引入 unicode-width 依赖——本报表只有
/// 表头和"未知"两处非 ASCII。
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

/// 渲染成人读的对齐表格。表头与"未知"两处 CJK 按显示宽度 2 参与对齐。
///
/// 墙钟、轮延迟与缓存命中率不进表：一行会话有 N 个轮延迟值，
/// 表格装不下向量。它们随 `--json` 输出，表格给的是"一眼看完"的
/// 标量——轮数、审批次数、等人时长、墙钟、缓存命中率、成本。
pub fn render_text(report: &BenchmarkReport) -> String {
    // 列定义：表头固定，数据从摘要取。
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
    // 数值列右对齐：宽度至少留出表头本身的宽度。
    let render_row = |cells: &[String]| -> String {
        cells
            .iter()
            .enumerate()
            .map(|(index, cell)| {
                let width = widths[index];
                if index == 0 {
                    pad(cell, width)
                } else {
                    // 右对齐数字与成本。
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
        // 合计行的命中率同样按"合计读回 / 合计输入"算，不是各行比率的平均：
        // 平均会让一个小会话和一个大会话等权，而钱和时间是按 token 花的。
        hit_ratio_label(if report.total_input_tokens > 0 {
            Some(report.total_cached_input_tokens as f64 / report.total_input_tokens as f64)
        } else {
            None
        }),
        report.total_wall_clock_ms.to_string(),
        cost_label(report.total_cost_usd),
    ];
    out.push_str(&render_row(&total));

    // MCP 生命周期单独一段：连接事件与工具调用是两本账。把它们塞进上面
    // 的表格列，读者会把"Server 没起来"读成"又一次工具调用失败"。
    if let Some(line) = mcp_summary(report) {
        out.push_str("\nMCP 生命周期\n");
        out.push_str(&line);
        out.push('\n');
    }

    // 台账不进表格：它每一项都是列表，表格装不下。单独按会话列在表后
    // ——"这个任务到底动过什么"是用户跑完长任务最想看的一句。
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

/// MCP 故障与恢复的一行汇总；一个数都没有时返回 `None`。
///
/// 只报有实际用途的六个数，不报连接建立次数和主动关闭次数：那两个数
/// 每次正常运行都会变，把"启了几次 Server"摆在故障旁边，读的人会把
/// 正常流水当成问题。`结果未知`单列是因为它最容易被误读成"已经取消"，
/// 而这正是本阶段不能承诺的事。
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

/// 渲染成机器可读的 JSON。
///
/// # Errors
///
/// 序列化失败时返回错误（正常数据不会走到这里）。
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
        // 毫秒间隔：序列号本身就是毫秒偏移，合成数据因此带真实的
        // 亚秒级时序，墙钟与每轮延迟的统计才有东西可算。
        Evidence::new(
            sequence,
            UNIX_EPOCH + Duration::from_millis(sequence * 250),
            event,
        )
    }

    /// 合成一个覆盖全部事件种类的会话，计数器手工可算。
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
        // token 不受影响：成本未知不等于用量未知。
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
        // 会话 A 以 Thought 收尾（交付了答案）→ 成功；会话 B 停在工具
        // 观察（被打断）→ 失败。
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
        let report = BenchmarkReport::from_sessions(&[log_a, log_c]);
        assert_eq!(report.total_cost_usd, None);
        assert_eq!(report.sessions.len(), 2);
    }

    #[test]
    fn empty_report_has_no_cost_data() {
        let report = BenchmarkReport::from_sessions(&[]);
        assert_eq!(report.sessions.len(), 0);
        assert_eq!(report.total_input_tokens, 0);
        assert_eq!(report.total_cost_usd, None, "没有任何数据时是未知而非 0");
        assert_eq!((report.succeeded, report.failed), (0, 0));
    }

    /// 缓存命中率的分母为 0 时是 `None`，不是 `0.0`。
    ///
    /// 两者的含义相反：`Some(0.0)` 是"有输入但一次都没命中"，
    /// `None` 是"压根没有输入 token 可算"（provider 不报用量）。
    /// 把后者显示成 0% 会让人以为缓存坏了。
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

    /// 命中率 = 读回 / 输入，多轮累加后仍然按这个口径算。
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

        // 900 / 2000 = 0.45。第一轮全量写入、第二轮几乎全量命中，
        // 正是"缓存第一次贵、后面便宜"的形状。
        assert_eq!(summary.cached_input_tokens, 900);
        assert_eq!(summary.cache_creation_input_tokens, 1_000);
        assert_eq!(summary.cache_hit_ratio, Some(0.45));
    }

    /// 墙钟 = 末事件 - 首事件；轮延迟按相邻思考事件的间隔算。
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

    /// 按 P9 之前的落盘格式造一条证据：没有 `format` 键，`recorded_at` 是整秒。
    ///
    /// 走 `serde_json::from_str` 而不是直接拼结构体：要验的正是"旧行读进来
    /// 之后"统计对不对，绕开反序列化就验不到想要的那条路径。
    fn legacy_evidence(sequence: u64, seconds: u64, event: AuditEvent) -> Evidence {
        let line = serde_json::json!({
            "sequence": sequence,
            "recorded_at": seconds,
            "event": serde_json::to_value(&event).expect("AuditEvent 序列化不应失败"),
        });
        serde_json::from_str::<Evidence>(&line.to_string()).expect("旧格式必须能反序列化")
    }

    /// 旧格式（秒级）证据的轮延迟与等审时间必须是真的毫秒。
    ///
    /// P9-1 提精度时，反序列化已经把秒换算成毫秒存进 `recorded_at` 了。
    /// 统计若再按格式号乘一次 1000，旧文件的这两列就会放大一千倍——
    /// 而墙钟不走同一条路（它直接读 `recorded_at`），于是两列互相矛盾：
    /// 墙钟说 60 秒，轮延迟说 6000 万毫秒。这类错位只出现在旧文件上，
    /// 新文件永远正确，所以必须有一条专门走旧格式的测试钉住它。
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

    /// 越界写入升级产生的审批也必须计入等审时间。
    ///
    /// 升级路径在证据里没有 `RequireApproval` 锚点：策略记录的是原判
    /// Allow（这是刻意的——审计不该改写策略的原判），升级发生在裁决
    /// 记录之后。等审统计若只认 `RequireApproval` 锚点，这类"策略放行
    /// 但启发式拦下"的审批就永远显示 0ms，P9-5 的等审列对升级路径失真。
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

    /// 空会话没有时间数据：墙钟 0、无轮延迟、无命中率。
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

    /// 同一个文件读两遍就是浪费：同工具 + 同参数的重复调用要被点名。
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
        // 只有 read_file 一个工具名被重复调用，且两组参数各重复一次；
        // 去重后只列一次名字。
        assert_eq!(summary.repeated_calls, vec!["read_file"]);
    }

    /// 键序不同但内容相同的参数算同一次调用。
    ///
    /// 不按 canonical 化比较的话，"模型重发了同一条请求"会因为键序
    /// 抖动被漏判成两次不同调用——而它恰恰是最典型的浪费。
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

    /// 报表的合计行要把新增的三个维度一起带上。
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

    /// 命中率列：有输入 token 但一次没命中时是 0%，没有输入 token 时是"—"。
    #[test]
    fn text_rendering_shows_hit_ratio_and_wall_clock() {
        let log = synthetic_log(Some(0.25));
        let report = BenchmarkReport::from_sessions(&[log]);
        let text = render_text(&report);

        assert!(text.contains("命中率"), "{text}");
        assert!(text.contains("墙钟ms"), "{text}");
        // 合成数据有 350 个输入 token、一次都没命中：0% 而不是"—"。
        // 这两者的区别正是本列要守住的诚实线。
        assert!(text.contains("0%"), "有输入但没命中应显示 0%: {text}");

        // 没有用量数据的会话才显示"—"（连 SummaryGenerated 一起清零：
        // 摘要调用也记用量，漏掉它分母就不是 0）。
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

    // ── P10 切片 3：证据台账 ──────────────────────────────────────────

    /// 报表里要看得见"这个任务动过什么"。
    ///
    /// 台账每一项都是列表，进不了表格——它单独列在表后。读报表的人
    /// 不用去翻 JSONL 才看得到动过哪些文件、跑过哪些命令。
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

    /// JSON 里也带着同一份台账：机器要能取到同一份账。
    #[test]
    fn the_json_report_carries_the_same_ledger() {
        let report = BenchmarkReport::from_sessions(&[synthetic_log(None)]);
        let text = render_json(&report).expect("序列化");

        assert!(text.contains("evidence_index"), "{text}");
        assert!(text.contains("文件不存在"), "{text}");
    }

    // ── P9-5 审批等待 ─────────────────────────────────────────────────

    /// 等人时长 = "判了要审"到"闸门给结论"的间隔。
    ///
    /// 合成数据里事件 3（RequireApproval）在 750ms、事件 5
    /// （ApprovalDecided）在 1250ms，所以是 500ms。
    #[test]
    fn approval_wait_spans_the_verdict_and_the_decision() {
        let summary = summarize(&synthetic_log(Some(0.5)));

        assert_eq!(summary.approval_wait_ms, 500);
    }

    /// 策略放行的调用不计等待：它压根不进闸门。
    ///
    /// 把 Allow 之后到下一个事件的间隔算成"等人"，会把工具执行时间
    /// 记到人头上——报表于是显示"人等了两秒"，而人根本没被问过。
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

    /// 多次审批的等待要累加，不是只算最后一次。
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

    /// 报表把等人时长单列一列，并给出跨会话合计。
    ///
    /// P9-5 的验收就落在这列上：默认基线少问几次、`p` 少敲几个字，
    /// 最终都该表现为它变小。没有它，"等待成本降了"只是句无法核对的话。
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

    // ── P11 切片 3：MCP 生命周期统计 ──────────────────────────────────

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

    /// 连接事件与调用故障各进各自的账，不冒充工具结局或沙箱执行次数。
    ///
    /// 这条守的是报表的可信度：一次启动失败压根没有工具调用，把它算成
    /// "工具失败 +1"会让人以为模型试过了；把一次断连按共享连接的每个工具
    /// 各算一次退出，会凭空造出好几次故障。
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
        // 连接事件不碰工具与沙箱两本账。
        assert_eq!(summary.tool_calls, 2, "工具调用数只该来自 ToolRequested");
        assert_eq!(summary.succeeded, 1);
        assert_eq!(summary.failed, 1);
        assert_eq!(summary.sandbox_protected_runs, 0);
        assert_eq!(summary.sandbox_unprotected_runs, 0);
    }

    /// 只有连接事件的日志不是一次任务运行。
    ///
    /// 它没有模型调用，`cost_usd` 必然是"未知"；按原有合计规则，一条这样的
    /// 日志会把整轮基准的成本拖成未知，还会被记成一个失败会话。用户看到的
    /// 结果就是"3 个任务里失败 1 个、成本未知"，而真相是"MCP 没起来，任务
    /// 全都跑完了"。
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
        // 两份日志的 MCP 统计都还在：排除的是"会话成败"，不是事实本身。
        assert_eq!(report.sessions[1].mcp_connect_failures, 1);
        assert_eq!(report.sessions[1].mcp_disconnects, 1);
        assert_eq!(report.sessions[1].events, 2);
    }

    /// 末尾那条 `Closed` 不该把交付过答案的会话判成失败。
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

    /// 空的连接文件不能被这条规则吸收：一条事件都没有是坏文件，不是连接日志。
    #[test]
    fn empty_log_is_not_a_lifecycle_log() {
        let log = SessionLog {
            session_id: SessionId::new("empty"),
            events: Vec::new(),
            skipped_lines: 0,
        };
        assert!(!is_lifecycle_only(&log));
    }

    /// 旧 JSON 报表缺这些字段要能读，且读成 0。
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

    /// 文本报表要能把故障与恢复说清楚，包括"结果未知不等于已取消"。
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

    /// 没碰过 MCP 的报表不该多出这一段：没有信息就占位是噪音。
    #[test]
    fn text_report_omits_the_mcp_section_when_unused() {
        let report = BenchmarkReport::from_sessions(&[synthetic_log(Some(0.25))]);
        let text = render_text(&report);
        assert!(!text.contains("MCP 生命周期"), "{text}");
    }
}

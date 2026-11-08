//! 工具守卫链：判断一次调用能否直接执行，从 Runner 的内联逻辑中抽成
//! 可扩展的责任链。
//!
//! 每个守卫返回一个 [`GuardVerdict`]：放行、直接拒绝或要求人工审批。
//! Runner 按顺序执行整条链，按最严格者胜的规则合并：任一 `Deny` 则拒绝；
//! 否则任一 `RequireApproval` 则进入审批；全部 `Pass` 才直接执行。守卫
//! 之间不互相覆盖，后出现的守卫不能替前面的守卫放行。
//!
//! 内置守卫保持原 Runner 的内联顺序：[`PolicyGuard`]（策略裁决）在前，
//! [`BoundaryEscalationGuard`]（越界写入升级）在后。升级只对策略放行的
//! 调用生效，审计中记录的仍是策略原判。

use std::sync::Arc;

use crate::domain::policy::{PolicyDecision, PolicyEngine, PolicyVerdict};
use crate::domain::sandbox::write_escape_suspect;
use crate::domain::tool::{ToolCall, ToolSpec};
use serde_json::Value;

/// 单个守卫对一次调用的裁决。
#[derive(Debug, Clone, PartialEq)]
pub enum GuardVerdict {
    /// 不拦截，交给下一个守卫；整条链无反对时直接执行。
    Pass,
    /// 直接拒绝。理由原样进入拒绝观察，不经过审批闸门。
    Deny(String),
    /// 要求人工审批。理由的用途由投票守卫决定：越界升级这类复核会把
    /// 嫌疑文本放入
    /// [`ApprovalRequest::reason`](crate::application::port::ApprovalRequest)，
    /// 闸门据此禁用会话信任。策略守卫的常规理由只写入 `PolicyEvaluated`
    /// 审计事件，不进入 `reason`，否则普通审批会被当作升级复核。
    RequireApproval(String),
}

/// 守卫裁决的上下文：被调工具的登记规格。
///
/// 规格包含风险注解（是否只读），守卫据此选择判定规则。
pub struct GuardContext<'a> {
    /// 被调工具的登记规格。
    pub spec: &'a ToolSpec,
}

impl<'a> GuardContext<'a> {
    /// 用被调工具的规格构造。
    pub fn new(spec: &'a ToolSpec) -> Self {
        Self { spec }
    }
}

/// 工具守卫：对即将执行的调用给出一个结论。
///
/// 与 [`crate::application::port::ApprovalGate`] 的分工：守卫做自动裁决，
/// 纯函数、无 IO、同步；闸门执行人的决定，可能等待输入，异步。守卫链的
/// 结论决定是否需要询问闸门。
pub trait ToolGuard: Send + Sync {
    /// 守卫名，用于测试与诊断。
    ///
    /// 不参与裁决合并（合并按链序与严重级），也不用于识别类型。Runner
    /// 按具体类型判断策略守卫，重名不影响语义。
    fn name(&self) -> &str;

    /// 对一次调用给出结论。
    fn inspect(&self, call: &ToolCall, ctx: &GuardContext<'_>) -> GuardVerdict;

    /// 按具体类型借用守卫，供 Runner 获取特定守卫的附加能力，例如策略
    /// 守卫的原始裁决。默认返回 `None`。
    fn as_policy_guard(&self) -> Option<&PolicyGuard> {
        None
    }
}

/// 策略守卫：把 [`PolicyEngine`] 的裁决映射为守卫结论。
///
/// 映射关系：`Allow` 对应 `Pass`，`RequireApproval` 对应
/// `RequireApproval`，`Deny` 对应 `Deny`。命中规则的理由原样传递，模型与
/// 审计据此定位规则。
pub struct PolicyGuard {
    engine: PolicyEngine,
}

impl PolicyGuard {
    /// 用给定策略引擎构造。
    pub fn new(engine: PolicyEngine) -> Self {
        Self { engine }
    }

    /// 策略裁决的原始结论。Runner 需要写入 `PolicyEvaluated` 证据，因此
    /// 除守卫结论外还提供原判。
    pub fn evaluate(&self, call: &ToolCall, spec: &ToolSpec) -> PolicyDecision {
        self.engine.evaluate(call, spec)
    }
}

impl ToolGuard for PolicyGuard {
    fn name(&self) -> &str {
        "policy"
    }

    fn inspect(&self, call: &ToolCall, ctx: &GuardContext<'_>) -> GuardVerdict {
        let decision = self.engine.evaluate(call, ctx.spec);
        match decision.verdict {
            PolicyVerdict::Allow => GuardVerdict::Pass,
            PolicyVerdict::RequireApproval => GuardVerdict::RequireApproval(decision.reason),
            PolicyVerdict::Deny => GuardVerdict::Deny(decision.reason),
        }
    }

    fn as_policy_guard(&self) -> Option<&PolicyGuard> {
        Some(self)
    }
}

/// 越界写入升级守卫：策略已放行，但命令疑似向 workspace 外写入时，
/// 追加一条要求人工审批的结论。
///
/// 投票理由为嫌疑文本本身，不加前缀。`ApprovalRequest.reason` 为 `Some`
/// 表示这是策略之外的复核，闸门据此禁用会话信任。理由仅用于展示，输出
/// 格式保持不变。
///
/// 与硬拒绝（`write_escape_beyond`）共用词法解析，但口径不同。此守卫
/// 没有 workspace 根可参考，所有盘符绝对路径都算嫌疑，宁可多问；硬拒绝
/// 是机械边界，必须准确。两道防线的差异见 README 的"安全边界"。
pub struct BoundaryEscalationGuard;

impl ToolGuard for BoundaryEscalationGuard {
    fn name(&self) -> &str {
        "boundary-escalation"
    }

    fn inspect(&self, call: &ToolCall, _ctx: &GuardContext<'_>) -> GuardVerdict {
        // 只有带 `command` 参数的调用（`run_command`）进入此启发式，
        // 其他工具没有命令文本。
        let Some(command) = call.arguments.get("command").and_then(Value::as_str) else {
            return GuardVerdict::Pass;
        };
        match write_escape_suspect(command) {
            Some(suspect) => GuardVerdict::RequireApproval(suspect),
            None => GuardVerdict::Pass,
        }
    }
}

/// 合并整条链的结论，最严格者胜。
///
/// 按顺序遍历，记录首个 `Deny` 或 `RequireApproval`，但不提前返回。返回值
/// 同时给出发出该结论的守卫下标，供调用方按身份查询类型；不使用名称，
/// 因为名称可能重复，不能承担语义判别。后续守卫仍会被调用，它们可能需要
/// 记录审计。守卫互不感知彼此的裁决（[`BoundaryEscalationGuard`] 就不读
/// 策略结果），合并只看最高严重级，同级取先到者。
pub fn run_guard_chain(
    guards: &[Arc<dyn ToolGuard>],
    call: &ToolCall,
    ctx: &GuardContext<'_>,
) -> (GuardVerdict, Option<usize>) {
    let mut strictest = GuardVerdict::Pass;
    let mut strictest_index: Option<usize> = None;
    for (index, guard) in guards.iter().enumerate() {
        let verdict = guard.inspect(call, ctx);
        let severity = match &verdict {
            GuardVerdict::Pass => 0,
            GuardVerdict::RequireApproval(_) => 1,
            GuardVerdict::Deny(_) => 2,
        };
        let current = match &strictest {
            GuardVerdict::Pass => 0,
            GuardVerdict::RequireApproval(_) => 1,
            GuardVerdict::Deny(_) => 2,
        };
        if severity > current {
            strictest = verdict;
            strictest_index = Some(index);
        }
    }
    (strictest, strictest_index)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::ids::CallId;
    use crate::domain::tool::ToolAnnotations;
    use serde_json::json;

    /// 只读工具规格：默认策略放行。
    fn read_spec() -> ToolSpec {
        ToolSpec {
            name: "read_file".to_owned(),
            description: "测试读".to_owned(),
            parameters: json!({"type": "object"}),
            annotations: ToolAnnotations::new(true, false, true, false),
        }
    }

    /// 写工具规格：默认策略要审批。
    fn write_spec() -> ToolSpec {
        ToolSpec {
            name: "write_file".to_owned(),
            description: "测试写".to_owned(),
            parameters: json!({"type": "object"}),
            annotations: ToolAnnotations::new(false, false, true, false),
        }
    }

    fn call(name: &str, arguments: serde_json::Value) -> ToolCall {
        ToolCall {
            id: CallId::new("call_1"),
            name: name.to_owned(),
            arguments,
        }
    }

    /// 恒 Deny 的守卫，用来验证合并取最严格者。
    struct DenyGuard;

    impl ToolGuard for DenyGuard {
        fn name(&self) -> &str {
            "deny-all"
        }

        fn inspect(&self, _call: &ToolCall, _ctx: &GuardContext<'_>) -> GuardVerdict {
            GuardVerdict::Deny("链上拒绝".to_owned())
        }
    }

    #[test]
    fn policy_guard_maps_verdicts_one_to_one() {
        let guard = PolicyGuard::new(PolicyEngine::new());
        let spec = read_spec();

        let read = call("read_file", json!({"path": "a.txt"}));
        assert_eq!(guard.inspect(&read, &GuardContext::new(&spec)), GuardVerdict::Pass);

        let write = call("write_file", json!({"path": "a.txt"}));
        assert!(matches!(
            guard.inspect(&write, &GuardContext::new(&write_spec())),
            GuardVerdict::RequireApproval(_)
        ));
    }

    #[test]
    fn deny_rule_becomes_deny_even_for_read_only_tool() {
        let mut engine = PolicyEngine::with_rules(vec![crate::domain::policy::PolicyRule {
            tool: Some("read_file".to_owned()),
            scope: None,
            verdict: PolicyVerdict::Deny,
            description: "测试禁读".to_owned(),
        }]);
        engine.rules.shrink_to_fit();
        let guard = PolicyGuard::new(engine);
        let read = call("read_file", json!({"path": "a.txt"}));
        assert_eq!(
            guard.inspect(&read, &GuardContext::new(&read_spec())),
            GuardVerdict::Deny("测试禁读".to_owned())
        );
    }

    #[test]
    fn escalation_guard_upgrades_out_of_workspace_write() {
        let guard = BoundaryEscalationGuard;
        let spec = read_spec();

        // 根内重定向：不升级。
        let inside = call("run_command", json!({"command": "echo x > out.txt"}));
        assert_eq!(
            guard.inspect(&inside, &GuardContext::new(&spec)),
            GuardVerdict::Pass
        );

        // 越界绝对路径：升级，理由就是嫌疑文本本身。
        let command = "echo x > D:/outside/x.txt";
        let outside = call("run_command", json!({"command": command}));
        let expected =
            crate::domain::sandbox::write_escape_suspect(command).expect("应判为嫌疑");
        assert_eq!(
            guard.inspect(&outside, &GuardContext::new(&spec)),
            GuardVerdict::RequireApproval(expected),
            "复核理由必须原样携带嫌疑文本，不加前缀"
        );

        // 没有 command 参数的调用不进入这个守卫的视野。
        let no_command = call("read_file", json!({"path": "a.txt"}));
        assert_eq!(
            guard.inspect(&no_command, &GuardContext::new(&spec)),
            GuardVerdict::Pass
        );
    }

    #[test]
    fn chain_takes_the_strictest_verdict() {
        let chain: Vec<Arc<dyn ToolGuard>> = vec![
            Arc::new(PolicyGuard::new(PolicyEngine::new())),
            Arc::new(DenyGuard),
        ];
        let read = call("read_file", json!({"path": "a.txt"}));
        let spec = read_spec();

        let (verdict, strictest_index) = run_guard_chain(&chain, &read, &GuardContext::new(&spec));
        assert_eq!(verdict, GuardVerdict::Deny("链上拒绝".to_owned()));
        assert_eq!(strictest_index, Some(1));
        assert_eq!(chain[1].name(), "deny-all");
    }

    #[test]
    fn chain_passes_when_every_guard_passes() {
        let chain: Vec<Arc<dyn ToolGuard>> = vec![
            Arc::new(PolicyGuard::new(PolicyEngine::new())),
            Arc::new(BoundaryEscalationGuard),
        ];
        let read = call("read_file", json!({"path": "a.txt"}));
        let spec = read_spec();

        let (verdict, strictest_index) = run_guard_chain(&chain, &read, &GuardContext::new(&spec));
        assert_eq!(verdict, GuardVerdict::Pass);
        assert_eq!(strictest_index, None);
    }

    #[test]
    fn chain_prefers_the_first_deny_but_reports_its_source() {
        // RequireApproval 之后又来了 Deny：合并取 Deny。
        let chain: Vec<Arc<dyn ToolGuard>> = vec![
            Arc::new(BoundaryEscalationGuard),
            Arc::new(DenyGuard),
        ];
        // 越界命令：升级守卫投 RequireApproval，DenyGuard 投 Deny。
        let outside = call("run_command", json!({"command": "echo x > D:/outside/x.txt"}));
        let spec = read_spec();

        let (verdict, strictest_index) = run_guard_chain(&chain, &outside, &GuardContext::new(&spec));
        assert!(matches!(verdict, GuardVerdict::Deny(_)));
        assert_eq!(strictest_index, Some(1));
        assert_eq!(chain[1].name(), "deny-all");
    }
}

//! 工具守卫链：把"这个调用能不能直接执行"的裁决从 Runner 的内联逻辑
//! 里抽出来，变成一条可扩展的责任链。
//!
//! 链上每个守卫回答同一道题：[`GuardVerdict`]——放行、直接拒绝、
//! 或要求人过目。Runner 按序执行整条链，然后按**最严格者胜**的原则
//! 合并结论：任一守卫 `Deny` 就拒绝；否则任一 `RequireApproval` 就进
//! 审批；全部 `Pass` 才直接执行。这个合并次序是有意的：守卫之间不
//! 互相覆盖，后来者不能替先到者放行。
//!
//! 内置两个守卫照搬原先 Runner 的内联顺序：[`PolicyGuard`]（策略
//! 裁决）在前，[`BoundaryEscalationGuard`]（越界写入升级）在后——
//! 升级只对策略放行的调用生效，审计里记的仍是策略的原判。

use std::sync::Arc;

use crate::domain::policy::{PolicyDecision, PolicyEngine, PolicyVerdict};
use crate::domain::sandbox::write_escape_suspect;
use crate::domain::tool::{ToolCall, ToolSpec};
use serde_json::Value;

/// 单个守卫对一次调用的裁决。
#[derive(Debug, Clone, PartialEq)]
pub enum GuardVerdict {
    /// 不拦，交给链上下一个守卫；链走完仍无反对即直接执行。
    Pass,
    /// 直接拒绝，理由会原样进拒绝观察——连审批闸门都不问。
    Deny(String),
    /// 要求人工审批，理由会进 [`crate::application::port::ApprovalRequest`]。
    RequireApproval(String),
}

/// 一次守卫裁决的上下文：调用本身与被调工具的登记规格。
///
/// 规格里带风险注解（只读与否），守卫按它取舍口径。
pub struct GuardContext<'a> {
    /// 被调工具的登记规格。
    pub spec: &'a ToolSpec,
}

impl<'a> GuardContext<'a> {
    /// 一次调用加它的规格。
    pub fn new(spec: &'a ToolSpec) -> Self {
        Self { spec }
    }
}

/// 工具守卫：对一次即将执行的调用给出一票结论。
///
/// 与 [`crate::application::port::ApprovalGate`] 的分工：守卫是
/// **自动裁决**（纯函数、无 IO、同步），闸门是**人的决定**（可能
/// 停下来等输入、异步）。守卫链的结论决定"要不要去问闸门"。
pub trait ToolGuard: Send + Sync {
    /// 守卫名，进证据与调试输出。
    fn name(&self) -> &str;

    /// 对一次调用给出一票结论。
    fn inspect(&self, call: &ToolCall, ctx: &GuardContext<'_>) -> GuardVerdict;

    /// 按具体类型借用守卫，供 Runner 取回链上特定守卫的附加能力
    /// （例如策略守卫的原始裁决）。默认返回 `None`。
    fn as_policy_guard(&self) -> Option<&PolicyGuard> {
        None
    }
}

/// 策略守卫：把 [`PolicyEngine`] 的裁决翻译成守卫的一票。
///
/// 裁决映射是直译：`Allow` → `Pass`，`RequireApproval` → `RequireApproval`，
/// `Deny` → `Deny`。命中规则的理由原样随行，模型与审计都靠它定位规则。
pub struct PolicyGuard {
    engine: PolicyEngine,
}

impl PolicyGuard {
    /// 用给定策略引擎构造。
    pub fn new(engine: PolicyEngine) -> Self {
        Self { engine }
    }

    /// 策略裁决的原始结论。Runner 要把它记进 `PolicyEvaluated` 证据，
    /// 所以除了守卫一票，这里还留原判可取。
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

/// 越界写入升级守卫：策略放行了，但命令看着像把写操作伸到 workspace
/// 外时，追加一票"要人过目"。
///
/// 投票理由是嫌疑文本本身，不加解释性前缀：`ApprovalRequest.reason`
/// 的 `Some` 就是"这是策略之外的复核"，闸门据此禁用会话信任；理由
/// 只是给人看的那句话，形状与改造前保持一致。
///
/// 与硬拒绝（`write_escape_beyond`）共用同一套词法解析，口径刻意不同：
/// 这里没有根可看，所有盘符绝对路径都算嫌疑——它是**兜底**，宁可多问；
/// 硬拒绝是机械边界，必须准。两道防线的落差见 README"安全边界"。
pub struct BoundaryEscalationGuard;

impl ToolGuard for BoundaryEscalationGuard {
    fn name(&self) -> &str {
        "boundary-escalation"
    }

    fn inspect(&self, call: &ToolCall, _ctx: &GuardContext<'_>) -> GuardVerdict {
        // 只有带 command 参数的调用（即 run_command）走得进这个启发式，
        // 其他工具没有命令文本可看。
        let Some(command) = call.arguments.get("command").and_then(Value::as_str) else {
            return GuardVerdict::Pass;
        };
        match write_escape_suspect(command) {
            Some(suspect) => GuardVerdict::RequireApproval(suspect),
            None => GuardVerdict::Pass,
        }
    }
}

/// 合并整条链的一票：**最严格者胜**。
///
/// 顺序遍历、先到先记 `Deny` / `RequireApproval`，但不提前返回——
/// 返回值还带上"投出这一票的是谁"，而链上后面的守卫仍会被询问
/// （它们可能还要据此记账）。追求的是与旧内联逻辑逐步一致：
/// 策略 `Deny` 时旧逻辑连升级检查都不做，但升级守卫对 `Deny` 的
/// 调用本来就不会再投 `RequireApproval` 之外的票……为避免行为分歧，
/// 合并**只看最高严重级**，同级的先到者胜。
pub fn run_guard_chain(
    guards: &[Arc<dyn ToolGuard>],
    call: &ToolCall,
    ctx: &GuardContext<'_>,
) -> (GuardVerdict, Option<String>) {
    let mut strictest = GuardVerdict::Pass;
    let mut strictest_guard: Option<String> = None;
    for guard in guards {
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
            strictest_guard = Some(guard.name().to_owned());
        }
    }
    (strictest, strictest_guard)
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

        let (verdict, guard_name) = run_guard_chain(&chain, &read, &GuardContext::new(&spec));
        assert_eq!(verdict, GuardVerdict::Deny("链上拒绝".to_owned()));
        assert_eq!(guard_name.as_deref(), Some("deny-all"));
    }

    #[test]
    fn chain_passes_when_every_guard_passes() {
        let chain: Vec<Arc<dyn ToolGuard>> = vec![
            Arc::new(PolicyGuard::new(PolicyEngine::new())),
            Arc::new(BoundaryEscalationGuard),
        ];
        let read = call("read_file", json!({"path": "a.txt"}));
        let spec = read_spec();

        let (verdict, guard_name) = run_guard_chain(&chain, &read, &GuardContext::new(&spec));
        assert_eq!(verdict, GuardVerdict::Pass);
        assert_eq!(guard_name, None);
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

        let (verdict, guard_name) = run_guard_chain(&chain, &outside, &GuardContext::new(&spec));
        assert!(matches!(verdict, GuardVerdict::Deny(_)));
        assert_eq!(guard_name.as_deref(), Some("deny-all"));
    }
}

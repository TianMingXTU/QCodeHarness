//! 策略领域模型：权限与策略中心的纯业务逻辑。
//!
//! P1 的核心是把"这个工具调用要不要审批"从硬编码的 `!read_only`
//! 变成可配置的规则引擎。这一层只做匹配和裁决，不涉及配置加载（IO）。
//! 配置加载在 `infrastructure/policy`。

use serde::{Deserialize, Serialize};

use super::tool::{ToolCall, ToolSpec};

/// 策略裁决：对匹配的工具调用给出什么决定。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PolicyVerdict {
    /// 放行，不过审批闸门。
    Allow,
    /// 必须过审批闸门，由闸门决定最终放行与否。
    RequireApproval,
    /// 直接拒绝，连闸门都不问。
    Deny,
}

/// 策略规则的作用域：匹配哪些参数条件。
///
/// P1 先只支持工具名匹配，但把字段位置留出来，P2 加 Shell/Git
/// 工具时可以扩展路径前缀、参数值等条件。
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PolicyScope {
    /// 匹配的参数条件。P1 暂未使用，预留扩展。
    /// 未来可以是 `{"path": {"prefix": "/safe/dir/"}}` 这类结构。
    #[allow(dead_code)]
    pub constraints: Option<serde_json::Value>,
}

/// 一条策略规则。
#[derive(Debug, Clone, PartialEq)]
pub struct PolicyRule {
    /// 匹配的工具名。`None` 表示匹配所有工具（通配规则）。
    pub tool: Option<String>,
    /// 匹配的参数作用域。P1 暂只支持工具名匹配，字段预留。
    pub scope: Option<PolicyScope>,
    /// 匹配时给出的裁决。
    pub verdict: PolicyVerdict,
    /// 规则说明，用于证据和给模型的解释。
    pub description: String,
}

impl PolicyRule {
    /// 检查这条规则是否匹配给定的工具调用。
    ///
    /// P1 只匹配工具名：`tool` 字段为 `None` 是通配，否则必须精确匹配。
    pub fn matches(&self, call: &ToolCall) -> bool {
        match &self.tool {
            None => true,
            Some(name) => name == &call.name,
        }
    }
}

/// 策略引擎的裁决结果。
#[derive(Debug, Clone, PartialEq)]
pub struct PolicyDecision {
    /// 最终裁决。
    pub verdict: PolicyVerdict,
    /// 命中规则的说明，用于证据和给模型的解释。
    pub reason: String,
    /// 命中的规则序号，便于审计追溯。`None` 表示走的是默认策略。
    pub rule_index: Option<usize>,
}

/// 策略引擎：一组有序规则 + 默认策略。
///
/// 规则按顺序匹配，第一条命中的生效；都不命中走默认策略。
/// 默认策略与 P0 行为一致：`read_only` 工具放行，非只读要求审批。
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PolicyEngine {
    /// 有序规则列表。
    pub rules: Vec<PolicyRule>,
}

impl PolicyEngine {
    /// 创建一个空规则引擎（只用默认策略）。
    pub fn new() -> Self {
        Self::default()
    }

    /// 从规则列表创建引擎。
    pub fn with_rules(rules: Vec<PolicyRule>) -> Self {
        Self { rules }
    }

    /// 评估一次工具调用，给出策略裁决。
    ///
    /// 这是纯函数，无 IO。评估逻辑：
    /// 1. 按顺序遍历规则，第一条匹配的生效
    /// 2. 都不匹配时走默认策略：`read_only` → `Allow`，非只读 → `RequireApproval`
    ///
    /// 注意：`Deny` 规则不需要特殊处理——规则本身就是有序的，
    /// 如果想让 `Deny` 优先，把它放前面就行。
    pub fn evaluate(&self, call: &ToolCall, spec: &ToolSpec) -> PolicyDecision {
        // 遍历规则，第一条匹配的生效
        for (index, rule) in self.rules.iter().enumerate() {
            if rule.matches(call) {
                return PolicyDecision {
                    verdict: rule.verdict,
                    reason: rule.description.clone(),
                    rule_index: Some(index),
                };
            }
        }

        // 默认策略：与 P0 行为一致
        let (verdict, reason) = if spec.annotations.read_only {
            (PolicyVerdict::Allow, "默认策略：只读工具自动放行".to_owned())
        } else {
            (
                PolicyVerdict::RequireApproval,
                "默认策略：非只读工具需要审批".to_owned(),
            )
        };

        PolicyDecision {
            verdict,
            reason,
            rule_index: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::ids::CallId;
    use crate::domain::tool::ToolAnnotations;
    use serde_json::json;

    fn test_call(name: &str) -> ToolCall {
        ToolCall {
            id: CallId::new("test"),
            name: name.to_owned(),
            arguments: json!({}),
        }
    }

    fn test_spec(read_only: bool) -> ToolSpec {
        ToolSpec {
            name: "test_tool".to_owned(),
            description: "test".to_owned(),
            parameters: json!({}),
            annotations: ToolAnnotations::new(read_only, false, false, false),
        }
    }

    /// 测试专用的规则构造捷径。
    ///
    /// 生产路径用 `PolicyEngine::with_rules` + `infrastructure::policy::parse`
    /// 装配，`PolicyRule::new` 只有测试在用，所以留在这里而不是留在公开 API 上。
    fn rule(
        tool: Option<&str>,
        verdict: PolicyVerdict,
        description: &str,
    ) -> PolicyRule {
        PolicyRule {
            tool: tool.map(ToOwned::to_owned),
            scope: None,
            verdict,
            description: description.to_owned(),
        }
    }

    /// 测试专用：构造只含一条规则的引擎。
    fn engine_with(tool: Option<&str>, verdict: PolicyVerdict, description: &str) -> PolicyEngine {
        PolicyEngine::with_rules(vec![rule(tool, verdict, description)])
    }

    #[test]
    fn default_policy_allows_read_only_tools() {
        let engine = PolicyEngine::new();
        let call = test_call("read_file");
        let spec = test_spec(true);

        let decision = engine.evaluate(&call, &spec);

        assert_eq!(decision.verdict, PolicyVerdict::Allow);
        assert!(decision.reason.contains("只读"));
        assert!(decision.rule_index.is_none());
    }

    #[test]
    fn default_policy_requires_approval_for_non_read_only() {
        let engine = PolicyEngine::new();
        let call = test_call("write_file");
        let spec = test_spec(false);

        let decision = engine.evaluate(&call, &spec);

        assert_eq!(decision.verdict, PolicyVerdict::RequireApproval);
        assert!(decision.reason.contains("非只读"));
        assert!(decision.rule_index.is_none());
    }

    #[test]
    fn explicit_allow_rule_overrides_default() {
        let engine = engine_with(Some("write_file"), PolicyVerdict::Allow, "测试规则：write_file 自动放行");

        let call = test_call("write_file");
        let spec = test_spec(false);

        let decision = engine.evaluate(&call, &spec);

        assert_eq!(decision.verdict, PolicyVerdict::Allow);
        assert_eq!(decision.rule_index, Some(0));
    }

    #[test]
    fn explicit_deny_rule_blocks_tool() {
        let engine = engine_with(
            Some("dangerous_tool"),
            PolicyVerdict::Deny,
            "安全策略：禁止调用此工具",
        );

        let call = test_call("dangerous_tool");
        let spec = test_spec(false);

        let decision = engine.evaluate(&call, &spec);

        assert_eq!(decision.verdict, PolicyVerdict::Deny);
        assert!(decision.reason.contains("禁止"));
    }

    #[test]
    fn first_matching_rule_wins() {
        let engine = PolicyEngine::with_rules(vec![
            rule(Some("write_file"), PolicyVerdict::Allow, "规则 0：放行"),
            // 第二条不会命中，因为第一条已匹配。
            rule(Some("write_file"), PolicyVerdict::Deny, "规则 1：拒绝"),
        ]);

        let call = test_call("write_file");
        let spec = test_spec(false);

        let decision = engine.evaluate(&call, &spec);

        assert_eq!(decision.verdict, PolicyVerdict::Allow);
        assert_eq!(decision.rule_index, Some(0));
    }

    #[test]
    fn deny_priority_via_rule_order() {
        // 想让 Deny 优先就把 Deny 规则放前面——规则顺序即优先级。
        let engine = PolicyEngine::with_rules(vec![
            rule(Some("write_file"), PolicyVerdict::Deny, "安全策略：禁止"),
            rule(Some("write_file"), PolicyVerdict::Allow, "信任规则：放行"),
        ]);

        let call = test_call("write_file");
        let spec = test_spec(false);

        let decision = engine.evaluate(&call, &spec);

        assert_eq!(decision.verdict, PolicyVerdict::Deny);
        assert_eq!(decision.rule_index, Some(0));
    }

    #[test]
    fn wildcard_rule_matches_all_tools() {
        let engine = engine_with(None, PolicyVerdict::RequireApproval, "安全策略：所有工具都要审批");

        let call1 = test_call("read_file");
        let call2 = test_call("write_file");
        let spec = test_spec(true);

        assert_eq!(
            engine.evaluate(&call1, &spec).verdict,
            PolicyVerdict::RequireApproval
        );
        assert_eq!(
            engine.evaluate(&call2, &spec).verdict,
            PolicyVerdict::RequireApproval
        );
    }

    #[test]
    fn unmatched_tool_falls_back_to_default() {
        let engine = engine_with(Some("other_tool"), PolicyVerdict::Deny, "只拒绝 other_tool");

        let call = test_call("read_file");
        let spec = test_spec(true);

        let decision = engine.evaluate(&call, &spec);
        assert_eq!(decision.verdict, PolicyVerdict::Allow);
        assert!(decision.rule_index.is_none());
    }
}

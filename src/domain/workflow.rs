//! 工作流模板：把任务类型展开成有序步骤。
//!
//! 模板是"步骤的配方"，[`WorkflowTemplate::expand`] 把它实例化成
//! [`TaskStep`] 列表。模板文本里的 `{goal}` 占位符在展开时替换成实际
//! 任务目标——步骤说明因此必须自包含（每步独立会话，模型看不到
//! 上一步的内容），不能只写"继续"。
//!
//! P4 只有内置模板；用户自定义模板的加载是产品化阶段的事。

use crate::domain::agent::AgentRole;
use crate::domain::task::{TaskKind, TaskStep};

/// 一个工作流模板：任务类型到步骤序列的映射。
#[derive(Debug, Clone)]
pub struct WorkflowTemplate {
    /// 模板标识，如 `bug-fix`。
    pub id: String,
    /// 适用任务类型。
    pub kind: TaskKind,
    /// 模板名，给人看。
    pub name: String,
    /// 步骤模板。文本中的 `{goal}` 会被替换成实际任务目标。
    pub steps: Vec<StepTemplate>,
}

/// 模板里的一步。与 [`TaskStep`] 同构，但没有实例化的任务目标。
#[derive(Debug, Clone)]
pub struct StepTemplate {
    /// 步骤说明。
    pub description: String,
    /// 这一步期望 Agent 扮演的角色。P5 起编排器按它查档案装配
    /// （系统提示词 + 工具子集），不再是统一的 Coder。
    pub role: AgentRole,
    /// 完成判据。
    pub done_when: String,
}

impl WorkflowTemplate {
    /// 展开成实际步骤：`{goal}` 替换为任务目标。
    pub fn expand(&self, goal: &str) -> Vec<TaskStep> {
        self.steps
            .iter()
            .map(|step| TaskStep {
                description: step.description.replace("{goal}", goal),
                role: step.role,
                done_when: step.done_when.replace("{goal}", goal),
            })
            .collect()
    }
}

/// 模板注册表：按任务类型查找模板。
#[derive(Debug, Clone)]
pub struct WorkflowCatalog {
    templates: Vec<WorkflowTemplate>,
}

impl WorkflowCatalog {
    /// 四个内置模板，步骤对齐架构文档 §04 与 README §4.3。
    /// 角色分派原则：只读分析交给 Planner/Reviewer，动手改动交给
    /// Coder，跑命令收集结果交给 Tester——角色能做的事与步骤要做的
    /// 事必须对得上，否则模型拿着只读工具被要求改文件，只会反复受挫。
    pub fn builtin() -> Self {
        let step = |role: AgentRole, description: &str, done_when: &str| StepTemplate {
            description: description.to_owned(),
            role,
            done_when: done_when.to_owned(),
        };
        Self {
            templates: vec![
                WorkflowTemplate {
                    id: "bug-fix".to_owned(),
                    kind: TaskKind::BugFix,
                    name: "修 Bug".to_owned(),
                    steps: vec![
                        step(
                            AgentRole::Planner,
                            // Planner 是只读角色，跑不了测试；"复现"由
                            // 后面的 Tester 步骤做，这里只要求读码定位。
                            "定位问题：{goal}。先读相关代码与测试，找出根因。",
                            "说清问题根因，引用读到的代码或测试输出",
                        ),
                        step(
                            AgentRole::Coder,
                            // "已完成修复"的出口必须给：实测（Xing4.0-29B）
                            // 第 1 步常顺手把修复做掉，判据若只认"已修改"，
                            // 弱模型会为字面达标反复提交空修改直到预算耗尽。
                            // 不提"根因"——步骤产出不跨步传递，指代会落空，
                            // 步骤说明必须自包含（模块头注释）。
                            "实施最小修改，修复 {goal}；若前面的步骤已完成修复，核实改动正确并说明即可，不要重复修改。",
                            "相关代码已修改（或确认此前已修复且测试通过），改动范围最小且不引入无关变更",
                        ),
                        step(
                            AgentRole::Tester,
                            "运行测试验证 {goal} 已修复，且没有破坏其他行为。",
                            "测试真实跑过并通过，或如实说明失败原因",
                        ),
                        step(
                            AgentRole::Coder,
                            "汇总 {goal} 的改动内容与遗留风险。",
                            "给出结论、依据与遗留风险",
                        ),
                    ],
                },
                WorkflowTemplate {
                    id: "feature".to_owned(),
                    kind: TaskKind::Feature,
                    name: "加功能".to_owned(),
                    steps: vec![
                        step(
                            AgentRole::Planner,
                            "理解需求：{goal}。读现有结构，弄清要改哪些地方。",
                            "列出需要改动的位置与理由",
                        ),
                        step(
                            AgentRole::Planner,
                            "规划 {goal} 的实现步骤与边界。",
                            "给出可执行的实现清单",
                        ),
                        step(
                            AgentRole::Coder,
                            // 不提"按计划"——Planner 的产出不跨步传递，
                            // Coder 只能自己读码定位，文本必须自包含。
                            "编写 {goal} 的实现。",
                            "功能代码已就位，风格与现有代码一致",
                        ),
                        step(
                            AgentRole::Tester,
                            // Tester 是只读 + 命令角色，写不了新测试；
                            // 验证（跑既有测试、实测新行为）才是它的职责。
                            "验证 {goal} 的实现：运行相关测试与功能验证，确认新行为生效。",
                            "相关测试通过，既有测试未被破坏",
                        ),
                    ],
                },
                WorkflowTemplate {
                    id: "review".to_owned(),
                    kind: TaskKind::Review,
                    name: "代码评审".to_owned(),
                    steps: vec![
                        step(
                            AgentRole::Reviewer,
                            "读取待评审的 diff 与相关文件，评审对象：{goal}。",
                            "准确复述改动范围",
                        ),
                        step(
                            AgentRole::Reviewer,
                            "理解 {goal} 相关的上下文：调用方、依赖与既有约定。",
                            "指出改动影响到的模块",
                        ),
                        step(
                            AgentRole::Reviewer,
                            "识别 {goal} 中的风险、bug 与规范问题。",
                            "列出问题清单，按严重程度排序",
                        ),
                        step(
                            AgentRole::Reviewer,
                            "对 {goal} 给出是否建议合并的结论。",
                            "给出明确结论、理由与改进建议",
                        ),
                    ],
                },
                WorkflowTemplate {
                    id: "refactor".to_owned(),
                    kind: TaskKind::Refactor,
                    name: "重构".to_owned(),
                    steps: vec![
                        step(
                            AgentRole::Tester,
                            "建立行为基线：先跑通 {goal} 相关的现有测试。",
                            "基线测试通过，或如实记录当前失败项",
                        ),
                        step(
                            AgentRole::Coder,
                            "分批实施 {goal} 的改造，每批保持行为等价。",
                            "改造完成且未改变外部行为",
                        ),
                        step(
                            AgentRole::Tester,
                            "回归测试：运行 {goal} 相关的既有测试与功能验证，确认改造后行为与改造前一致、未被破坏。",
                            "相关测试全部通过，行为与改造前一致",
                        ),
                        step(
                            AgentRole::Reviewer,
                            "对比 {goal} 改造前后的结果，汇总收益与风险。",
                            "给出对比结论与遗留风险",
                        ),
                    ],
                },
            ],
        }
    }

    /// 按任务类型找模板。每种类型恰好一个内置模板。
    pub fn find(&self, kind: TaskKind) -> Option<&WorkflowTemplate> {
        self.templates.iter().find(|template| template.kind == kind)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_builtin_template_expands_with_goal_substituted() {
        let catalog = WorkflowCatalog::builtin();
        let kinds = [
            (TaskKind::BugFix, 4),
            (TaskKind::Feature, 4),
            (TaskKind::Review, 4),
            (TaskKind::Refactor, 4),
        ];
        for (kind, expected_len) in kinds {
            let template = catalog.find(kind).expect("内置类型必有模板");
            let goal = format!("测试目标-{kind:?}");
            let steps = template.expand(&goal);

            assert_eq!(steps.len(), expected_len);
            for step in &steps {
                assert!(
                    step.description.contains(&goal),
                    "步骤说明应包含替换后的目标，实际: {}",
                    step.description
                );
                assert!(!step.description.contains("{goal}"), "占位符必须被替换干净");
            }
        }
    }

    #[test]
    fn builtin_templates_dispatch_roles_per_the_table() {
        // 角色表是 P5-1e 的验收项：只读分析交给 Planner/Reviewer，
        // 动手改动交给 Coder，跑命令收集结果交给 Tester。
        let catalog = WorkflowCatalog::builtin();
        let roles_of = |kind: TaskKind| -> Vec<AgentRole> {
            catalog
                .find(kind)
                .unwrap()
                .expand("目标")
                .iter()
                .map(|step| step.role)
                .collect()
        };
        assert_eq!(
            roles_of(TaskKind::BugFix),
            vec![
                AgentRole::Planner,
                AgentRole::Coder,
                AgentRole::Tester,
                AgentRole::Coder,
            ]
        );
        assert_eq!(
            roles_of(TaskKind::Feature),
            vec![
                AgentRole::Planner,
                AgentRole::Planner,
                AgentRole::Coder,
                AgentRole::Tester,
            ]
        );
        assert_eq!(
            roles_of(TaskKind::Review),
            vec![AgentRole::Reviewer; 4]
        );
        assert_eq!(
            roles_of(TaskKind::Refactor),
            vec![
                AgentRole::Tester,
                AgentRole::Coder,
                AgentRole::Tester,
                AgentRole::Reviewer,
            ]
        );
    }

    #[test]
    fn find_misses_kinds_not_in_the_catalog() {
        // 注册表只装一个模板时，查别的类型要返回 None。
        let catalog = WorkflowCatalog {
            templates: vec![WorkflowTemplate {
                id: "only".to_owned(),
                kind: TaskKind::Review,
                name: "评审".to_owned(),
                steps: Vec::new(),
            }],
        };
        assert!(catalog.find(TaskKind::Review).is_some());
        assert!(catalog.find(TaskKind::BugFix).is_none());
    }

    #[test]
    fn expand_leaves_text_without_placeholder_untouched() {
        let catalog = WorkflowCatalog::builtin();
        let template = catalog.find(TaskKind::Refactor).unwrap();
        let steps = template.expand("把模块拆小");
        // 第一步的说明里有 {goal}，第四步的"对比结果"没有占位符也不该被改动。
        assert!(steps[0].description.contains("把模块拆小"));
        assert!(steps[2].done_when == "相关测试全部通过，行为与改造前一致");
    }
}

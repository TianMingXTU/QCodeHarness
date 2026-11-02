//! 基准评测的领域类型：任务定义、成功判据与一次运行的结果。
//!
//! 判据全部是**机器可检查的客观条件**——测试命令的退出码、工作区
//! 有没有实际改动、最终答案有没有提到关键词。不靠人看答案打分，
//! 基准才能复现。本模块只有类型与纯判定逻辑，任务文件怎么读、
//! 模型怎么跑都在外层。

use serde::{Deserialize, Serialize};

use super::ids::SessionId;
use super::task::TaskKind;

/// 一个基准任务。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BenchmarkTask {
    /// 任务标识（与 `benchmarks/<id>/` 目录同名）。
    pub id: String,
    /// 任务类型，决定编排走哪个工作流模板。
    pub kind: TaskKind,
    /// 给模型的任务描述。
    pub description: String,
    /// 成功判据。
    pub criteria: SuccessCriteria,
}

/// 成功判据。全部是机器可检查的客观条件。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SuccessCriteria {
    /// 必须通过的测试命令（在任务仓库根执行，退出码 0 即通过）。
    pub test_command: String,
    /// 是否要求工作区有实际改动。修 bug 类任务"测试通过"可能只是
    /// fixture 本来就能过——要求有改动才能证明 agent 真的动了手。
    pub requires_changes: bool,
    /// 最终答案必须提及的词（大小写不敏感）。空清单表示不检查。
    pub answer_must_mention: Vec<String>,
}

/// 判据评估时能观察到的事实。与判据分开定义：判据是声明，
/// 事实是运行后采集到的数据，[`SuccessCriteria::evaluate`] 对照两者。
#[derive(Debug, Clone, Copy)]
pub struct CriteriaObservation<'a> {
    /// 测试命令是否以 0 退出。
    pub test_exit_success: bool,
    /// 工作区是否出现了实际改动。
    pub has_changes: bool,
    /// 最终答案文本。
    pub answer: &'a str,
}

impl SuccessCriteria {
    /// 逐条核对判据，返回**未满足**判据的说明（全部满足则为空）。
    pub fn evaluate(&self, observed: CriteriaObservation<'_>) -> Vec<String> {
        let mut failures = Vec::new();
        if !observed.test_exit_success {
            failures.push(format!(
                "测试命令 `{}` 未通过（非 0 退出）",
                self.test_command
            ));
        }
        if self.requires_changes && !observed.has_changes {
            failures.push("工作区没有任何实际改动".to_owned());
        }
        let answer = observed.answer.to_ascii_lowercase();
        for word in &self.answer_must_mention {
            if !answer.contains(&word.to_ascii_lowercase()) {
                failures.push(format!("最终答案未提及关键词 `{word}`"));
            }
        }
        failures
    }
}

/// 一次基准运行的结果。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BenchmarkResult {
    /// 任务标识。
    pub task_id: String,
    /// 该任务第一次**完成**步骤的会话 ID（证据文件的入口）。
    ///
    /// P9-3 起一个任务只有一个会话，且会话在第一步之前就开好——
    /// 所以只要任务真的跑起来过，哪怕是第一步就失败，这里也有 ID
    /// （失败步骤的观察在同一份证据里，可恢复可复盘）。只有任务还没
    /// 开跑就出错（fixture 拷贝、装载之类）时为 `None`。
    pub session_id: Option<SessionId>,
    /// 是否通过全部判据。
    pub passed: bool,
    /// 未满足的判据，逐条说明；任务运行本身失败时这里记录失败原因。
    pub failures: Vec<String>,
    /// 完成的步骤数。
    pub steps_completed: usize,
    /// 全部步骤消耗的模型调用轮数（这个任务唯一会话里的思考事件数）。
    pub turns_used: u32,
    /// 本次任务的墙钟（毫秒）。
    ///
    /// 取该会话的首末事件间隔：证据里本来就有每条事件的
    /// 时间戳，再起一个计时器只会多一处可能对不上的真相。它不含
    /// fixture 拷贝与判据命令的时间——那两段不是"等模型"。
    pub wall_clock_ms: u64,
    /// 审批次数合计（这个任务唯一会话里的 `ApprovalDecided` 事件数）。
    ///
    /// 基准强制 `AutoApproveGate`（无人值守），所以这里恒为 0。
    /// **真实使用中的审批次数要从 `--report` 看**，不要拿这个 0 得出
    /// "人工等待不是问题"的结论——它衡量的是另一条路径。
    pub approvals: usize,
    /// 缓存命中率：读回的输入 token 占全部输入 token 的比例。
    /// 没有任何输入 token 时为 `None`（含义是"没数据"，不是"0% 命中"）。
    pub cache_hit_ratio: Option<f64>,
    /// 成本合计（美元）。任一次调用成本未知时为 `None`——被低估的合计
    /// 比"未知"更危险。
    pub cost_usd: Option<f64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn criteria() -> SuccessCriteria {
        SuccessCriteria {
            test_command: "cargo test".to_owned(),
            requires_changes: true,
            answer_must_mention: vec!["panic".to_owned()],
        }
    }

    fn observation(
        test_exit_success: bool,
        has_changes: bool,
        answer: &str,
    ) -> CriteriaObservation<'_> {
        CriteriaObservation {
            test_exit_success,
            has_changes,
            answer,
        }
    }

    #[test]
    fn all_criteria_met_means_no_failures() {
        let failures = criteria().evaluate(observation(
            true,
            true,
            "根因是空名字触发了 panic，已改为占位问候",
        ));
        assert!(failures.is_empty());
    }

    #[test]
    fn each_unmet_criterion_is_reported_individually() {
        let failures = criteria().evaluate(observation(
            false,
            false,
            "修好了，测试应该能过",
        ));
        assert_eq!(failures.len(), 3, "三条判据各自未满足：{failures:?}");
        assert!(failures[0].contains("cargo test"));
        assert!(failures[1].contains("改动"));
        assert!(failures[2].contains("panic"));
    }

    #[test]
    fn keyword_match_is_case_insensitive() {
        let failures = criteria().evaluate(observation(true, true, "The PANIC is fixed"));
        assert!(failures.is_empty(), "大小写不应影响关键词匹配");
    }

    #[test]
    fn empty_keyword_list_skips_the_answer_check() {
        let loose = SuccessCriteria {
            answer_must_mention: vec![],
            ..criteria()
        };
        let failures = loose.evaluate(observation(true, true, ""));
        assert!(failures.is_empty());
    }

    #[test]
    fn changes_not_required_means_absence_is_fine() {
        let loose = SuccessCriteria {
            requires_changes: false,
            ..criteria()
        };
        let failures = loose.evaluate(observation(true, false, "提到 panic"));
        assert!(failures.is_empty());
    }

    #[test]
    fn result_round_trips_through_json() {
        let result = BenchmarkResult {
            task_id: "greet-npe".to_owned(),
            session_id: Some(SessionId::new("session-1")),
            passed: false,
            failures: vec!["测试命令 `cargo test` 未通过（非 0 退出）".to_owned()],
            steps_completed: 2,
            turns_used: 9,
            wall_clock_ms: 12_500,
            approvals: 0,
            cache_hit_ratio: Some(0.42),
            cost_usd: None,
        };
        let text = serde_json::to_string(&result).unwrap();
        assert_eq!(serde_json::from_str::<BenchmarkResult>(&text).unwrap(), result);
    }
}

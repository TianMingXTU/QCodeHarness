//! 基准评测的领域类型：任务定义、成功判据与运行结果。
//!
//! 判据全部是机器可检查的客观条件：测试命令退出码、工作区是否有实际
//! 改动、最终答案是否提到关键词。不靠人评分，基准才可复现。本模块只
//! 包含类型与纯判定逻辑，任务文件读取与模型执行都在外层。

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
    /// 任务所属套件：`default` 为短任务，`long` 为长任务。
    ///
    /// 两套跑法不同：长任务先耗尽预算再恢复，因此由不同运行器处理，
    /// 不共用执行路径。
    #[serde(default)]
    pub suite: String,
    /// fixture 仓库的主语言（`javascript` / `python` / `go` / 缺省 Rust）。
    ///
    /// 判据命令经 shell 直跑，框架不内置语言专属执行器；这个字段只服务
    /// 报告层按语言分组统计通过率。缺省 `None` 记为 Rust。
    #[serde(default)]
    pub language: Option<String>,
    /// 成功判据。
    pub criteria: SuccessCriteria,
    /// 两阶段运行配置。仅长任务套件使用，短任务留空即可。
    #[serde(default)]
    pub long_run: LongRunConfig,
}

/// 成功判据。全部是机器可检查的客观条件。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SuccessCriteria {
    /// 必须通过的测试命令（在任务仓库根执行，退出码 0 即通过）。
    pub test_command: String,
    /// 是否要求工作区有实际改动。
    ///
    /// 修 bug 类任务中"测试通过"可能只是 fixture 本来就能过，要求有改动
    /// 才能证明 agent 确实修改了代码。
    pub requires_changes: bool,
    /// 最终答案必须提及的词（大小写不敏感）。空清单表示不检查。
    pub answer_must_mention: Vec<String>,
    /// 最少模型调用轮数。
    ///
    /// 长任务据此确认运行轮数达标，三轮就结束的运行不应算作长任务。
    /// `None` 表示不限。
    #[serde(default)]
    pub min_turns: Option<u32>,
    /// 禁止重复执行的命令。
    ///
    /// 恢复后重跑已执行过的命令会重复触发副作用，证据里可以数出重复次数。
    /// `None` 表示不检查。
    #[serde(default)]
    pub no_repeated_commands: Option<Vec<String>>,
    /// 上下文压缩后，模型输出中仍必须出现的词。
    ///
    /// 用于判断关键上下文是否保留：历史被裁剪后，模型仍需知道它改过哪些
    /// 文件、跑过哪些命令，这是继续执行任务的依据。空清单表示不检查。
    #[serde(default)]
    pub must_mention_after_compaction: Vec<String>,
}

/// 长任务的两阶段运行配置。
///
/// 短任务一次跑完；长任务在第一次运行中故意耗尽预算，再从任务快照
/// 恢复跑完。两个参数写在任务文件里，不同任务的耗尽位置不同，不能
/// 写死在运行器里。
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct LongRunConfig {
    /// 第一次运行的轮数上限，故意设置得会耗尽，以触发中断与恢复。
    ///
    /// 省略时运行器取默认值 2 轮，仍然会在第一步内中断。
    #[serde(default)]
    pub exhaust_turns: Option<u32>,
    /// 上下文 token 预算，应压小到必然触发裁剪。
    ///
    /// 否则长任务历史可能达不到上限，"压缩后上下文是否保留"的判据
    /// 无从核对。
    #[serde(default)]
    pub context_max_tokens: Option<usize>,
}

/// 判据评估时观察到的事实。
///
/// 与判据分开定义：判据是声明，事实是运行后采集的数据，
/// [`SuccessCriteria::evaluate`] 对照两者。
#[derive(Debug, Clone, Copy)]
pub struct CriteriaObservation<'a> {
    /// 测试命令是否以 0 退出。
    pub test_exit_success: bool,
    /// 工作区是否出现了实际改动。
    pub has_changes: bool,
    /// 最终答案文本。
    pub answer: &'a str,
    /// 本次运行的模型调用轮数（跨中断与恢复累计）。
    pub turns_used: u32,
    /// 重复执行过的命令（同一命令文本被调用了不止一次）。
    pub repeated_commands: &'a [String],
    /// 第一次上下文压缩后模型的输出。
    ///
    /// `None` 表示本次运行未发生压缩。
    pub text_after_compaction: Option<&'a str>,
}

impl SuccessCriteria {
    /// 逐条核对判据，返回未满足判据的说明（全部满足时为空）。
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
        if let Some(minimum) = self.min_turns
            && observed.turns_used < minimum {
                failures.push(format!(
                    "只跑了 {} 轮，长任务要求至少 {minimum} 轮",
                    observed.turns_used
                ));
            }
        // 判定与展示使用同一份违规清单：runner 打印"重复副作用 N 次"时也数
        // 这份清单，不统计所有重复过的命令。模型修完缺陷后重跑全量测试属于
        // 正常迭代，不算副作用重复。
        for command in self.forbidden_repeats(observed.repeated_commands) {
            failures.push(format!("命令 `{command}` 被重复执行——副作用重复触发"));
        }
        if !self.must_mention_after_compaction.is_empty() {
            // 没有压缩就不存在"压缩之后"。这条判据无从核对，应当如实报告，
            // 不能放行，否则未执行的检查会被当作已通过。
            let Some(text) = observed.text_after_compaction else {
                failures.push("本次运行没有发生上下文压缩，判据无从核对".to_owned());
                return failures;
            };
            let lowered = text.to_ascii_lowercase();
            for word in &self.must_mention_after_compaction {
                if !lowered.contains(&word.to_ascii_lowercase()) {
                    failures.push(format!(
                        "上下文压缩之后，模型输出未提及 `{word}`——关键上下文没保住"
                    ));
                }
            }
        }
        failures
    }

    /// `no_repeated_commands` 中确实被重复执行的命令。
    ///
    /// 判据未声明（`None`）时返回空。这是"重复副作用"的唯一判定口径，
    /// 展示层同样使用它，两处保持一致。
    pub fn forbidden_repeats(&self, repeated_commands: &[String]) -> Vec<String> {
        let Some(forbidden) = &self.no_repeated_commands else {
            return Vec::new();
        };
        forbidden
            .iter()
            .filter(|command| repeated_commands.iter().any(|seen| seen == *command))
            .cloned()
            .collect()
    }
}

/// 单个安全用例的客观结果。`passed = None` 表示未运行或结论未知。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SecurityCaseResult {
    /// 用例标识，例如 `sandbox_workspace_write`。
    pub case_id: String,
    /// 是否通过；`None` 表示未知。
    #[serde(default)]
    pub passed: Option<bool>,
    /// 原始证据位置。
    #[serde(default)]
    pub evidence_path: Option<String>,
    /// 未通过或未知的原因。
    #[serde(default)]
    pub reason: Option<String>,
}

/// 基准结果的机器可读扩展指标。
///
/// 基础结果保留最小字段以便旧报告继续读取；耗时、token 分项与未解决的
/// 失败放在这里，`Option::None` 表示数据来源没有报告，不能按 0 处理。
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct BenchmarkMetrics {
    /// provider 报告的输入 token 合计；`None` 表示没有报告用量。
    #[serde(default)]
    pub input_tokens: Option<u64>,
    /// provider 报告的输出 token 合计；`None` 表示没有报告用量。
    #[serde(default)]
    pub output_tokens: Option<u64>,
    /// 从 provider 缓存读回的输入 token 合计；`None` 表示没有报告用量。
    #[serde(default)]
    pub cached_input_tokens: Option<u64>,
    /// 写入 provider 缓存的输入 token 合计；`None` 表示没有报告用量。
    #[serde(default)]
    pub cache_creation_input_tokens: Option<u64>,
    /// 逐轮模型调用耗时（毫秒），按调用顺序排列。
    #[serde(default)]
    pub turn_latencies_ms: Vec<u64>,
    /// 人工业务救援次数。运行器无法观测外部救援，`None` 表示未知。
    #[serde(default)]
    pub manual_interventions: Option<usize>,
    /// 安全用例结果。空清单表示本次运行没有安全用例，不能当作全绿。
    #[serde(default)]
    pub security_cases: Vec<SecurityCaseResult>,
    /// 运行结束时仍未解决的失败原因，与结果的 `failures` 一致。
    #[serde(default)]
    pub unresolved_failures: Vec<String>,
}

/// 一次基准运行的结果。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BenchmarkResult {
    /// 任务标识。
    pub task_id: String,
    /// 任务所属语言（来自任务定义，`None` 为 Rust）。
    ///
    /// 结果按任务组织，把语言一并带上，报告层才能按语言分组统计通过率，
    /// 不必回头再读一遍任务文件。`serde(default)` 让旧结果文件仍能读。
    #[serde(default)]
    pub language: Option<String>,
    /// 该任务第一次完成步骤的会话 ID，也是证据文件的入口。
    ///
    /// 一个任务只有一个会话，会话在第一步之前创建。任务只要开始运行，
    /// 即使第一步失败也有 ID，失败步骤的观察记录在同一份证据里，可恢复、
    /// 可复盘。任务未开始就出错（如 fixture 拷贝、装载）时为 `None`。
    pub session_id: Option<SessionId>,
    /// 是否通过全部判据。
    pub passed: bool,
    /// 未满足的判据，逐条说明；任务运行本身失败时这里记录失败原因。
    pub failures: Vec<String>,
    /// 完成的步骤数。
    pub steps_completed: usize,
    /// 全部步骤消耗的模型调用轮数（这个任务唯一会话里的思考事件数）。
    pub turns_used: u32,
    /// 本次任务的墙钟耗时（毫秒）。
    ///
    /// 取会话首末事件的时间戳间隔。证据里已有每条事件的时间戳，另起计时器
    /// 会多一处可能不一致的数据。不含 fixture 拷贝与判据命令的时间，这两
    /// 段不属于等待模型。
    pub wall_clock_ms: u64,
    /// 审批裁决次数合计，即该任务唯一会话中的 `ApprovalDecided` 事件数。
    ///
    /// 基准使用 `AutoApproveGate`（无人值守），这个数字表示有多少次调用
    /// 走到了审批闸门，不是人工审批次数。真实使用中的审批情况应查看
    /// `--report`，两者衡量的是不同路径。
    pub approvals: usize,
    /// 缓存命中率：读回的输入 token 占全部输入 token 的比例。
    /// 没有输入 token 时为 `None`，表示没有数据，不是 0% 命中。
    pub cache_hit_ratio: Option<f64>,
    /// 成本合计（美元）。任一次调用成本未知时为 `None`。低估的合计比
    /// 未知更危险。
    pub cost_usd: Option<f64>,
    /// 扩展指标。旧结果文件缺省时按空指标读取，不把未知写成 0。
    #[serde(default)]
    pub metrics: BenchmarkMetrics,
}

/// 长任务两段式运行的结果。
///
/// 第一次运行的失败与恢复后的结果分开记录，否则无法判断恢复是否
/// 挽回了任务。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LongBenchmarkResult {
    /// 任务标识。
    pub task_id: String,
    /// 任务所属语言（来自任务定义，`None` 为 Rust）。供报告按语言分组。
    #[serde(default)]
    pub language: Option<String>,
    /// 会话 ID（两段共用同一个会话：恢复接着上一次的会话跑）。
    pub session_id: Option<SessionId>,
    /// 第一次运行的失败原因。空串表示第一次运行没有失败，通常说明
    /// "故意耗尽预算"未生效，本次运行没有测到目标场景。
    pub first_run_failure: String,
    /// 第一次运行完成的步数。
    pub first_run_steps: usize,
    /// 第一次运行消耗的模型调用轮数。
    pub first_run_turns: u32,
    /// 第一次运行的轮数上限（故意设小的那一个）。
    pub first_run_turn_limit: u32,
    /// 恢复运行的失败原因。`None` 表示恢复跑完了。
    pub resume_failure: Option<String>,
    /// 恢复运行消耗的模型调用轮数。
    pub resume_turns: u32,
    /// 两段合计的模型调用轮数。
    pub total_turns: u32,
    /// 任务总步数。
    pub steps_total: usize,
    /// 两段合计完成的步数。第一段单独记在 `first_run_steps`。
    pub steps_completed: usize,
    /// 是否通过全部判据。
    pub passed: bool,
    /// 未满足的判据，逐条说明。
    pub failures: Vec<String>,
    /// 重复执行过的命令。空清单表示没有重复副作用。
    pub repeated_commands: Vec<String>,
    /// 成本合计（美元）。成本未知时为 `None`。
    pub cost_usd: Option<f64>,
    /// 墙钟（毫秒）：两段合计。
    pub wall_clock_ms: u64,
    /// 扩展指标。旧结果文件缺省时按空指标读取，不把未知写成 0。
    #[serde(default)]
    pub metrics: BenchmarkMetrics,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn criteria() -> SuccessCriteria {
        SuccessCriteria {
            test_command: "cargo test".to_owned(),
            requires_changes: true,
            answer_must_mention: vec!["panic".to_owned()],
            min_turns: None,
            no_repeated_commands: None,
            must_mention_after_compaction: Vec::new(),
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
            turns_used: 0,
            repeated_commands: &[],
            text_after_compaction: None,
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
            language: None,
            session_id: Some(SessionId::new("session-1")),
            passed: false,
            failures: vec!["测试命令 `cargo test` 未通过（非 0 退出）".to_owned()],
            steps_completed: 2,
            turns_used: 9,
            wall_clock_ms: 12_500,
            approvals: 0,
            cache_hit_ratio: Some(0.42),
            cost_usd: None,
            metrics: BenchmarkMetrics {
                input_tokens: Some(100),
                output_tokens: Some(20),
                cached_input_tokens: Some(10),
                cache_creation_input_tokens: Some(5),
                turn_latencies_ms: vec![120, 340],
                manual_interventions: None,
                security_cases: vec![SecurityCaseResult {
                    case_id: "sandbox_workspace_write".to_owned(),
                    passed: None,
                    evidence_path: None,
                    reason: Some("本机未运行该用例".to_owned()),
                }],
                unresolved_failures: vec!["测试命令 `cargo test` 未通过（非 0 退出）".to_owned()],
            },
        };
        let text = serde_json::to_string(&result).unwrap();
        assert_eq!(serde_json::from_str::<BenchmarkResult>(&text).unwrap(), result);
    }

    #[test]
    fn legacy_results_default_to_unknown_extended_metrics() {
        let text = r#"{"task_id":"greet-npe","language":null,"session_id":null,"passed":true,
            "failures":[],"steps_completed":2,"turns_used":9,"wall_clock_ms":12500,
            "approvals":0,"cache_hit_ratio":null,"cost_usd":null}"#;

        let result: BenchmarkResult = serde_json::from_str(text).unwrap();

        assert_eq!(result.metrics, BenchmarkMetrics::default());
        assert_eq!(result.metrics.input_tokens, None, "旧文件没报用量，不能补 0");
    }

    // ── 长任务判据 ───────────────────────────────────────────────────

    /// 长任务观察样本：补齐轮数、重复命令与压缩后的模型输出。
    ///
    /// 答案包含基础判据的关键词（panic），使其他判据保持满足，用例只关注
    /// 长任务判据。
    fn long_observation<'a>(
        turns_used: u32,
        repeated_commands: &'a [String],
        text_after_compaction: Option<&'a str>,
    ) -> CriteriaObservation<'a> {
        CriteriaObservation {
            test_exit_success: true,
            has_changes: true,
            answer: "根因是 panic，已修复",
            turns_used,
            repeated_commands,
            text_after_compaction,
        }
    }

    #[test]
    fn too_few_turns_fails_the_long_task_criterion() {
        let criteria = SuccessCriteria {
            min_turns: Some(20),
            ..criteria()
        };

        let failures = criteria.evaluate(long_observation(8, &[], Some("继续")));

        assert_eq!(failures.len(), 1, "{failures:?}");
        assert!(failures[0].contains("8 轮"), "{failures:?}");
        assert!(failures[0].contains("20 轮"), "{failures:?}");
    }

    #[test]
    fn enough_turns_passes_the_long_task_criterion() {
        let criteria = SuccessCriteria {
            min_turns: Some(20),
            ..criteria()
        };

        let failures = criteria.evaluate(long_observation(22, &[], Some("继续")));

        assert!(failures.is_empty(), "{failures:?}");
    }

    #[test]
    fn a_repeated_command_fails_the_criterion() {
        // 恢复之后把已经跑过的命令再跑一遍：副作用重复了。
        let criteria = SuccessCriteria {
            no_repeated_commands: Some(vec!["cargo test".to_owned()]),
            ..criteria()
        };
        let repeated = vec!["cargo test".to_owned()];

        let failures = criteria.evaluate(long_observation(22, &repeated, Some("继续")));

        assert_eq!(failures.len(), 1, "{failures:?}");
        assert!(failures[0].contains("cargo test"), "{failures:?}");
        assert!(failures[0].contains("重复"), "{failures:?}");
    }

    #[test]
    fn a_command_that_ran_once_is_fine() {
        let criteria = SuccessCriteria {
            no_repeated_commands: Some(vec!["cargo test".to_owned()]),
            ..criteria()
        };

        let failures = criteria.evaluate(long_observation(22, &[], Some("继续")));

        assert!(failures.is_empty(), "{failures:?}");
    }

    #[test]
    fn a_repeat_outside_the_forbidden_list_is_not_a_side_effect_violation() {
        // 模型修完缺陷后重跑全量测试：重复发生了，但任务只禁止
        // `cargo test --test utils`。未声明的重复属于正常迭代。
        // 判定与展示共用 forbidden_repeats，这条测试锁定该口径。
        let criteria = SuccessCriteria {
            no_repeated_commands: Some(vec!["cargo test --test utils".to_owned()]),
            ..criteria()
        };
        let repeated = vec!["cargo test".to_owned()];

        let failures = criteria.evaluate(long_observation(22, &repeated, Some("继续")));

        assert!(failures.is_empty(), "{failures:?}");
        assert!(
            criteria.forbidden_repeats(&repeated).is_empty(),
            "违规清单应为空: {:?}",
            criteria.forbidden_repeats(&repeated)
        );
    }

    #[test]
    fn a_repeat_without_a_forbidden_list_is_never_a_violation() {
        // no_repeated_commands 缺省（None）：没有违规清单，任何重复都不算
        // 违规，展示层同样报告"没有违规"。
        let criteria = criteria();
        let repeated = vec!["cargo test".to_owned(), "cargo build".to_owned()];

        let failures = criteria.evaluate(long_observation(22, &repeated, Some("继续")));

        assert!(failures.is_empty(), "{failures:?}");
        assert!(criteria.forbidden_repeats(&repeated).is_empty());
    }

    #[test]
    fn key_context_must_survive_compaction() {
        let criteria = SuccessCriteria {
            must_mention_after_compaction: vec!["parser.rs".to_owned()],
            ..criteria()
        };

        let failures = criteria.evaluate(long_observation(22, &[], Some("我改完了 parser.rs")));

        assert!(failures.is_empty(), "{failures:?}");
    }

    #[test]
    fn losing_key_context_after_compaction_fails() {
        let criteria = SuccessCriteria {
            must_mention_after_compaction: vec!["parser.rs".to_owned()],
            ..criteria()
        };

        let failures = criteria.evaluate(long_observation(22, &[], Some("我改完了别的文件")));

        assert_eq!(failures.len(), 1, "{failures:?}");
        assert!(failures[0].contains("parser.rs"), "{failures:?}");
    }

    #[test]
    fn a_run_without_compaction_cannot_claim_the_context_survived() {
        // 没有压缩就不存在"压缩之后"。这条判据必须报告"无从核对"，
        // 不能放行，否则未执行的检查会被当作已通过。
        let criteria = SuccessCriteria {
            must_mention_after_compaction: vec!["parser.rs".to_owned()],
            ..criteria()
        };

        let failures = criteria.evaluate(long_observation(22, &[], None));

        assert_eq!(failures.len(), 1, "{failures:?}");
        assert!(failures[0].contains("没有发生上下文压缩"), "{failures:?}");
    }

    #[test]
    fn the_long_result_round_trips_through_json() {
        let result = LongBenchmarkResult {
            task_id: "long-task-a".to_owned(),
            language: None,
            session_id: Some(SessionId::new("session-long")),
            first_run_failure: "第 2 步失败: 预算耗尽".to_owned(),
            first_run_steps: 1,
            first_run_turns: 4,
            first_run_turn_limit: 4,
            resume_failure: None,
            resume_turns: 18,
            total_turns: 22,
            steps_total: 4,
            steps_completed: 4,
            passed: true,
            failures: Vec::new(),
            repeated_commands: Vec::new(),
            cost_usd: None,
            wall_clock_ms: 90_000,
            metrics: BenchmarkMetrics {
                input_tokens: None,
                output_tokens: None,
                cached_input_tokens: None,
                cache_creation_input_tokens: None,
                turn_latencies_ms: vec![500, 700],
                manual_interventions: None,
                security_cases: Vec::new(),
                unresolved_failures: Vec::new(),
            },
        };
        let text = serde_json::to_string(&result).unwrap();
        assert_eq!(
            serde_json::from_str::<LongBenchmarkResult>(&text).unwrap(),
            result
        );
    }
}

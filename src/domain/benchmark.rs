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
    /// 任务属于哪个套件：`default` 是短任务，`long` 是长任务。
    ///
    /// 两套任务跑法不同——长任务要先故意耗尽预算再恢复——所以它们
    /// 由不同的运行器接手，不混在一条路上。
    #[serde(default)]
    pub suite: String,
    /// 成功判据。
    pub criteria: SuccessCriteria,
    /// 两阶段运行配置。只在长任务套件里用得上，短任务留空即可。
    #[serde(default)]
    pub long_run: LongRunConfig,
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
    /// 最少模型调用轮数。
    ///
    /// 长任务用它证明"确实跑了那么多"：一个三轮就交差的运行不该被
    /// 算作长任务。`None` 表示不限。
    #[serde(default)]
    pub min_turns: Option<u32>,
    /// 不许重复执行的命令。
    ///
    /// 恢复之后把已经跑过的命令再跑一遍，副作用就重复了——而命令
    /// 是不是"已经跑过"，证据里数得出来。`None` 表示不检查。
    #[serde(default)]
    pub no_repeated_commands: Option<Vec<String>>,
    /// 上下文被压缩之后，模型输出里仍然必须出现的词。
    ///
    /// 这是"压缩后关键上下文还在不在"的判据：历史被裁掉之后，模型
    /// 仍然得知道它动过哪些文件、跑过什么命令——那正是它继续干活的
    /// 依据。空清单表示不检查。
    #[serde(default)]
    pub must_mention_after_compaction: Vec<String>,
}

/// 长任务的两阶段运行配置。
///
/// 长任务与短任务的跑法不一样：短任务一遍跑完，长任务要**故意**在
/// 第一次运行里把预算耗尽，再从任务快照恢复着跑完。这两个旋钮就
/// 写在这儿——写在任务文件里，而不是写死在运行器里：不同任务的
/// "故意耗尽"位置不一样。
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct LongRunConfig {
    /// 第一次运行的轮数上限：故意让它耗尽，逼出中断与恢复。
    ///
    /// `None` 表示不刻意制造中断（长任务也可以一遍跑完）。
    #[serde(default)]
    pub exhaust_turns: Option<u32>,
    /// 上下文 token 预算：压小到必然触发裁剪。
    ///
    /// 不压小的话，长任务的历史可能一直撑不到上限，"压缩后上下文
    /// 还在不在"这条判据就无从核对。
    #[serde(default)]
    pub context_max_tokens: Option<usize>,
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
    /// 本次运行的模型调用轮数（跨中断与恢复累计）。
    pub turns_used: u32,
    /// 重复执行过的命令（同一命令文本被调用了不止一次）。
    pub repeated_commands: &'a [String],
    /// 第一次上下文压缩之后模型说的话。
    ///
    /// `None` 表示这次运行没有发生过压缩。
    pub text_after_compaction: Option<&'a str>,
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
        if let Some(minimum) = self.min_turns {
            if observed.turns_used < minimum {
                failures.push(format!(
                    "只跑了 {} 轮，长任务要求至少 {minimum} 轮",
                    observed.turns_used
                ));
            }
        }
        // 判定与展示必须共用同一本账：runner 打印"重复副作用 N 次"时
        // 数的也是这份违规清单，而不是所有重复过的命令——模型修完一个
        // 缺陷重跑全量测试是正常迭代，不是副作用重复。
        for command in self.forbidden_repeats(&observed.repeated_commands) {
            failures.push(format!("命令 `{command}` 被重复执行——副作用重复触发"));
        }
        if !self.must_mention_after_compaction.is_empty() {
            // 没有压缩就没有"压缩之后"：这条判据无从核对，如实说出来，
            // 而不是悄悄放行——放行等于把一条没跑的检查当成跑过了。
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

    /// `no_repeated_commands` 里真正被重复执行了的命令。
    ///
    /// 判据未声明（`None`）时没有可违规的清单，返回空。这是"重复
    /// 副作用"的唯一判定口径，展示层也用它，两处不会走样。
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

/// 长任务两段式运行的结果。
///
/// **第一次运行的失败与恢复后的结果是两件事**，分开记：混在一起就
/// 看不出"恢复到底救回来了没有"。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LongBenchmarkResult {
    /// 任务标识。
    pub task_id: String,
    /// 会话 ID（两段共用同一个会话：恢复接着上一次的会话跑）。
    pub session_id: Option<SessionId>,
    /// 第一次运行的失败原因。空串表示第一次运行没失败——
    /// 那通常意味着"故意耗尽预算"没生效，这一趟没测到想测的东西。
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
    /// 恢复后完成的步数。
    pub steps_completed: usize,
    /// 是否通过全部判据。
    pub passed: bool,
    /// 未满足的判据，逐条说明。
    pub failures: Vec<String>,
    /// 重复执行过的命令。空清单表示没有重复副作用。
    pub repeated_commands: Vec<String>,
    /// 成本合计（美元）。未知就是未知。
    pub cost_usd: Option<f64>,
    /// 墙钟（毫秒）：两段合计。
    pub wall_clock_ms: u64,
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

    // ── P10 切片 4：长任务判据 ───────────────────────────────────────

    /// 长任务观察：补上轮数、重复命令与压缩之后的模型输出。
    ///
    /// 答案带上基础判据的关键词（panic）：这里的用例只关心长任务判据
    /// 本身，别的判据都该是满足的。
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
        // 模型修完一个缺陷重跑全量测试：重复发生了，但任务只禁止
        // `cargo test --test utils`——未声明的重复是正常迭代。
        // 判定与展示共用 forbidden_repeats，这里锁的是这个口径。
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
        // no_repeated_commands 缺省（None）：没有可违规的清单，
        // 任何重复都不算违规——展示层同样说"没有违规"。
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
        // 没有压缩就没有"压缩之后"：这条判据必须说"无从核对"，
        // 而不是悄悄放行——放行等于把没跑的检查当成跑过了。
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
        };
        let text = serde_json::to_string(&result).unwrap();
        assert_eq!(
            serde_json::from_str::<LongBenchmarkResult>(&text).unwrap(),
            result
        );
    }
}

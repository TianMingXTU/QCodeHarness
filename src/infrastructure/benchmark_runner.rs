//! 基准运行器：逐个运行任务并按判据打分。
//!
//! 单个任务的流程：复制 fixture 到临时目录 → 编排器驱动 agent 动手 →
//! 执行判据中的测试命令 → 按判据打分 → 读回证据统计轮数与成本。
//! 每个任务使用全新的 fixture 拷贝与独立的工具注册表（workspace 根指向
//! 拷贝），任务之间互不影响。单个任务失败（编排中断或判据未通过）
//! 只记为未通过，后续任务继续：基准需要在一次运行中看到全貌。
//! 进度写 stderr，stdout 由 CLI 输出最终结果，避免污染 JSON 管道。
//!
//! 这里直接使用文件、进程与证据的具体实现，因此位于基础设施层。它组合
//! application 的编排器完成运行，依赖方向仍为 infrastructure →
//! application/domain。

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::application::agent_run::AgentDependencies;
use crate::application::error::ApplicationError;
use crate::application::orchestrator::{EvidenceFactory, Orchestrator};
use crate::application::port::EvidenceSink;
use crate::application::report::{cost_label, hit_ratio_label, summarize};
use crate::application::snapshot::TaskStore;
use crate::application::tool_registry::ToolRegistry;
use crate::domain::benchmark::{BenchmarkResult, BenchmarkTask, CriteriaObservation, LongBenchmarkResult};
use crate::domain::budget::Budget;
use crate::domain::ids::{SessionId, TaskId};
use crate::domain::workspace::WorkspaceRoot;
use crate::infrastructure::benchmark::{load_long_tasks, load_tasks, BenchmarkLoadError};
use crate::infrastructure::evidence::jsonl::{session_path, JsonlEvidenceSink};
use crate::infrastructure::evidence::reader::read_session;
use crate::infrastructure::tool::shell::run_in_workspace;
use crate::infrastructure::tool::all_tools;

/// 判据测试命令的超时。fixture 都是小 crate，几分钟足够；卡死的命令
/// 不应拖垮整个基准。
const TEST_COMMAND_TIMEOUT: Duration = Duration::from_secs(300);

/// 失败诊断保留的命令输出行数。失败原因通常在结尾；判据命令的输出
/// 不进证据文件，完整内容到此即丢，全部写进结果会淹没判据清单。
const FAILURE_OUTPUT_LINES: usize = 20;

/// 运行整个基准任务集，逐个打分。
///
/// `deps` 是装配模板：模型、审批闸、时钟、策略、上下文预算原样复用。
/// `tools` 与 `evidence` 按任务重建：工具 workspace 根必须指向任务的
/// fixture 拷贝，证据必须写入统一的会话目录。`workspace` 是证据落盘根，
/// 证据写在 `.qcodeharness/sessions/<id>.jsonl`，`--report` 按同一目录
/// 统计，基准运行因此一并计入。
///
/// # Errors
///
/// 任务集装载失败（根目录缺失、任一任务配置非法）时返回装载错误。
/// 单个任务的运行失败不传播，记为该任务未通过。
pub async fn run_benchmark(
    deps: &AgentDependencies,
    budget: &Budget,
    benchmarks_root: &Path,
    workspace: &Path,
) -> Result<Vec<BenchmarkResult>, BenchmarkLoadError> {
    let tasks = load_tasks(benchmarks_root)?;
    let total = tasks.len();
    let mut results = Vec::with_capacity(total);
    for (index, task) in tasks.iter().enumerate() {
        eprintln!("▶ [{}/{}] {}", index + 1, total, task.description);
        let result = match run_one(deps, budget, benchmarks_root, workspace, task).await {
            Ok(result) => result,
            Err(failure) => BenchmarkResult {
                task_id: task.id.clone(),
                language: task.language.clone(),
                session_id: None,
                passed: false,
                failures: vec![failure],
                steps_completed: 0,
                turns_used: 0,
                wall_clock_ms: 0,
                approvals: 0,
                cache_hit_ratio: None,
                cost_usd: None,
            },
        };
        let verdict = if result.passed { "✓ 通过" } else { "✗ 未通过" };
        eprintln!(
            "{} {}：{} 步 / {} 轮 / 墙钟 {} / 审批 {} 次 / 缓存 {} / 成本 {}",
            verdict,
            result.task_id,
            result.steps_completed,
            result.turns_used,
            wall_clock_label(result.wall_clock_ms),
            result.approvals,
            hit_ratio_label(result.cache_hit_ratio),
            cost_label(result.cost_usd),
        );
        for reason in &result.failures {
            eprintln!("  - {reason}");
        }
        results.push(result);
    }
    Ok(results)
}

/// 运行长任务基准任务集：每个任务先故意耗尽预算，再恢复跑完。
///
/// 输出将两次运行分开记录。第一次的失败是设计好的实验条件，不记为任务
/// 失败；恢复后的结果才参与评测。混在一起会把"第一次预算耗尽"记成
/// 一次失败，长任务永远无法通过。
///
/// # Errors
///
/// 任务集装载失败时返回装载错误。单个任务两段运行的失败不传播。
pub async fn run_long_benchmark(
    deps: &AgentDependencies,
    budget: &Budget,
    benchmarks_root: &Path,
    workspace: &Path,
) -> Result<Vec<LongBenchmarkResult>, BenchmarkLoadError> {
    let tasks = load_long_tasks(benchmarks_root)?;
    let total = tasks.len();
    let mut results = Vec::with_capacity(total);
    for (index, task) in tasks.iter().enumerate() {
        eprintln!("▶ [{}/{}] 长任务：{}", index + 1, total, task.description);
        let result = match run_long_one(deps, budget, benchmarks_root, workspace, task).await {
            Ok(result) => result,
            Err(failure) => LongBenchmarkResult {
                task_id: task.id.clone(),
                language: task.language.clone(),
                session_id: None,
                first_run_failure: failure.clone(),
                first_run_steps: 0,
                first_run_turns: 0,
                first_run_turn_limit: task.long_run.exhaust_turns.unwrap_or_default(),
                resume_failure: Some(failure.clone()),
                resume_turns: 0,
                total_turns: 0,
                steps_total: 0,
                steps_completed: 0,
                passed: false,
                failures: vec![failure],
                repeated_commands: Vec::new(),
                cost_usd: None,
                wall_clock_ms: 0,
            },
        };
        let verdict = if result.passed { "✓ 通过" } else { "✗ 未通过" };
        if result.first_run_failure.is_empty() {
            eprintln!("  第一次运行：没有失败——任务一遍跑完了，这次没有测到中断恢复");
        } else {
            eprintln!(
                "  第一次运行：{}（第 {} 步后停 / {} 轮 / 上限 {} 轮）",
                result.first_run_failure,
                result.first_run_steps,
                result.first_run_turns,
                result.first_run_turn_limit,
            );
        }
        match &result.resume_failure {
            Some(reason) => eprintln!("  恢复运行：失败 —— {reason}"),
            None => eprintln!(
                "  恢复运行：从任务快照继续 → {}/{} 步 / 本段 {} 轮 / 总 {} 轮",
                result.steps_completed, result.steps_total, result.resume_turns, result.total_turns,
            ),
        }
        eprintln!(
            "{} 重复副作用 {} 次 / 墙钟 {} / 成本 {}",
            verdict,
            // 与判据共用同一份统计：这里数的是 no_repeated_commands 中
            // 真正违规的命令。所有重复命令中还包含修完缺陷后重跑全量测试
            // 这类正常迭代。
            task.criteria.forbidden_repeats(&result.repeated_commands).len(),
            wall_clock_label(result.wall_clock_ms),
            cost_label(result.cost_usd),
        );
        for reason in &result.failures {
            eprintln!("  - {reason}");
        }
        results.push(result);
    }
    Ok(results)
}

/// 把基准结果按语言分组，供报告层统计各语言通过率。
///
/// 语言是任务的属性，结果按任务组织；把任务的语言带进结果后，分组只需
/// 按 `language` 归桶。缺省（Rust）也成一组，报告时显示为 `rust`。
///
/// 返回 `Vec` 而非 `HashMap`：语言集合固定，排序稳定即可复现，
/// 不需要哈希表的无序性。桶内保留结果原序。
pub fn group_by_language<'a, T>(results: &'a [T]) -> Vec<(&'a str, Vec<&'a T>)>
where
    T: HasLanguage,
{
    let mut buckets: Vec<(&'a str, Vec<&'a T>)> = Vec::new();
    for result in results {
        let key = result.language_key();
        // 可变借用只存在于 if 条件里；else 分支已释放，才能安全 push。
        if let Some(group) = buckets.iter_mut().find(|(name, _)| *name == key) {
            group.1.push(result);
        } else {
            buckets.push((key, vec![result]));
        }
    }
    buckets
}

/// 结果携带语言的统一接口。短任务与长任务结果各实现一份，分组共用逻辑。
pub trait HasLanguage {
    /// 分组键：语言名，缺省 `rust`。
    fn language_key(&self) -> &str;
    /// 语言原文（`None` 为 Rust），供报告与 JSON 输出。
    fn language(&self) -> Option<&str>;
    /// 是否通过全部判据。汇总按语言统计通过率用。
    fn passed(&self) -> bool;
}

impl HasLanguage for BenchmarkResult {
    fn language_key(&self) -> &str {
        self.language.as_deref().unwrap_or("rust")
    }
    fn language(&self) -> Option<&str> {
        self.language.as_deref()
    }
    fn passed(&self) -> bool {
        self.passed
    }
}

impl HasLanguage for LongBenchmarkResult {
    fn language_key(&self) -> &str {
        self.language.as_deref().unwrap_or("rust")
    }
    fn language(&self) -> Option<&str> {
        self.language.as_deref()
    }
    fn passed(&self) -> bool {
        self.passed
    }
}

/// 按语言生成汇总行：每行一个语言的通过数/总数。
///
/// 供基准运行后打印，回答"哪种语言的 fixture 通过率更低"。只列有任务
/// 的语言，无信息不占位。缺省语言显示为 `rust`，与实际语言名保持一致。
pub fn language_summary_lines<T>(results: &[T]) -> Vec<String>
where
    T: HasLanguage,
{
    group_by_language(results)
        .into_iter()
        .map(|(language, group)| {
            let passed = group.iter().filter(|result| result.passed()).count();
            format!("{language}: {passed}/{} 通过", group.len())
        })
        .collect()
}

/// 墙钟的显示值：以秒为单位，保留一位小数。///
/// 基准任务通常耗时几十秒到几分钟。按秒显示便于比较，同时不会把
/// 200ms 的差异显示成 0。
pub(crate) fn wall_clock_label(millis: u64) -> String {
    format!("{:.1}s", millis as f64 / 1000.0)
}

/// 运行一个任务并打分。`Err` 只覆盖无法运行任务的装配级失败（fixture
/// 复制失败等）。编排中断与判据未通过属于正常结果。
async fn run_one(
    deps: &AgentDependencies,
    budget: &Budget,
    benchmarks_root: &Path,
    workspace: &Path,
    task: &BenchmarkTask,
) -> Result<BenchmarkResult, String> {
    let repo = copy_fixture(benchmarks_root, task)?;
    let before = snapshot_files(&repo);

    let mut task_deps = deps.clone();
    task_deps.tools = ToolRegistry::new(all_tools(WorkspaceRoot::new(repo.clone())));
    let evidence_factory: EvidenceFactory = {
        let workspace = workspace.to_path_buf();
        Arc::new(move |session_id: &SessionId| {
            let path = session_path(&workspace, session_id.as_str());
            let jsonl = JsonlEvidenceSink::open(&path)?;
            Ok(Arc::new(jsonl) as Arc<dyn EvidenceSink>)
        })
    };
    let mut orchestrator = Orchestrator::new(task_deps, *budget, evidence_factory);
    let mut orchestrated =
        orchestrator.create_task(TaskId::new(format!("bench-{}", task.id)), task.kind, &task.description);

    let (answer, steps_completed, session_id, mut failures) =
        match orchestrator.run(&mut orchestrated).await {
            Ok(outcome) => (
                outcome.answer,
                outcome.steps_completed,
                Some(outcome.session_id),
                Vec::new(),
            ),
            Err(ApplicationError::StepFailed {
                session_id,
                steps_completed,
                message,
                ..
            }) => (String::new(), steps_completed, Some(session_id), vec![message]),
            Err(error) => (String::new(), 0, None, vec![error.to_string()]),
        };

    let Some(session_id) = session_id else {
        // 没有会话说明任务没有执行过程，判据无从谈起。
        let _ = fs::remove_dir_all(&repo);
        return Ok(BenchmarkResult {
            task_id: task.id.clone(),
            language: task.language.clone(),
            session_id: None,
            passed: false,
            failures,
            steps_completed,
            turns_used: 0,
            wall_clock_ms: 0,
            approvals: 0,
            cache_hit_ratio: None,
            cost_usd: None,
        });
    };

    // 编排中断时判据没有意义：任务未走完，测试结果无法证明问题已解决，
    // 也不再执行最长五分钟的测试命令。
    if failures.is_empty() {
        let has_changes = snapshot_files(&repo) != before;
        let observation = CriteriaObservation {
            test_exit_success: run_criterion_command(&repo, &task.criteria.test_command, &mut failures).await,
            has_changes,
            answer: &answer,
            turns_used: 0,
            repeated_commands: &[],
            text_after_compaction: None,
        };
        failures.extend(task.criteria.evaluate(observation));
    }

    let (turns_used, cost_usd, wall_clock_ms, approvals, cache_hit_ratio) =
        usage_of(workspace, &session_id).await;
    let result = BenchmarkResult {
        task_id: task.id.clone(),
        language: task.language.clone(),
        session_id: Some(session_id),
        passed: failures.is_empty(),
        failures,
        steps_completed,
        turns_used,
        wall_clock_ms,
        approvals,
        cache_hit_ratio,
        cost_usd,
    };
    // 结果已定，尽力清理临时仓库。失败诊断依赖结果中的输出末尾与证据。
    let _ = fs::remove_dir_all(&repo);
    Ok(result)
}

/// 运行一个长任务：第一段故意耗尽预算，第二段从任务快照恢复。
///
/// 第一段把轮数上限压到任务配置的 `exhaust_turns`，编排器会在中途报
/// "预算耗尽"，这就是要测的中断。失败后任务快照已落盘，`--resume-task`
/// 恢复用的就是它。
///
/// 第二段读回任务快照，从步骤边界继续：
/// - 已完成步骤不重做（`completed_outputs` 原样带回）；
/// - 消耗从快照接着算（`consumption` 在快照里）；
/// - 上下文按 `context_max_tokens` 裁剪。这一段历史最长，压缩判据的
///   数据由此而来。
///
/// 两段共用同一个 fixture 拷贝与同一个会话，恢复接着上一次的会话跑。
async fn run_long_one(
    deps: &AgentDependencies,
    budget: &Budget,
    benchmarks_root: &Path,
    workspace: &Path,
    task: &BenchmarkTask,
) -> Result<LongBenchmarkResult, String> {
    let repo = copy_fixture(benchmarks_root, task)?;
    let before = snapshot_files(&repo);
    let task_id = TaskId::new(format!("bench-long-{}", task.id));

    let turn_limit = task
        .long_run
        .exhaust_turns
        .unwrap_or(DEFAULT_EXHAUST_TURNS);

    // ── 第一段：故意耗尽预算 ─────────────────────────────────────
    let mut first_deps = deps.clone();
    first_deps.tools = ToolRegistry::new(all_tools(WorkspaceRoot::new(repo.clone())));
    // 恢复前提：会话快照（半轮状态）与任务快照（步骤进度）都要落盘。
    let store = crate::infrastructure::session_store::json::JsonSessionStore::new(workspace);
    let task_store = crate::infrastructure::task_store::JsonTaskStore::new(workspace);
    first_deps.session_store = Some(std::sync::Arc::new(store) as _);
    let evidence_factory: EvidenceFactory = shared_evidence_factory(workspace);
    // 同样装上工作区扫描器：快照要记录当时的 fixture 拷贝，恢复段的
    // 一致性检查才有基准。基准工作区是独占的临时拷贝，中断期间没有
    // 外部改动，检查会通过。安装扫描器是为了让恢复走与真实用户相同的
    // 路径。
    let mut orchestrator = Orchestrator::new(first_deps, *budget, evidence_factory)
        .with_task_store(std::sync::Arc::new(task_store))
        .with_worktree_scanner(std::sync::Arc::new(
            crate::infrastructure::worktree::RootedWorktreeScanner::new(repo.clone()),
        ));
    let mut orchestrated = orchestrator.create_task(task_id.clone(), task.kind, &task.description);

    let first_budget = Budget::new(turn_limit);
    let first = orchestrator.run_with_budget(&mut orchestrated, &first_budget).await;
    let (first_failure, first_steps, session_id) = match &first {
        Err(ApplicationError::StepFailed {
            session_id,
            steps_completed,
            message,
            ..
        }) => (message.clone(), *steps_completed, Some(session_id.clone())),
        Err(error) => (error.to_string(), 0, None),
        // 一遍跑完，没有测到中断。仍带出会话 ID 与完成步数，但结果中
        // 必须明确记录没有发生中断，否则这次基准会在未测到恢复链路时
        // 显示为通过。
        Ok(outcome) => (String::new(), outcome.steps_completed, Some(outcome.session_id.clone())),
    };
    let Some(session_id) = session_id else {
        let _ = fs::remove_dir_all(&repo);
        return Err(first_failure);
    };
    // 恢复段追加在同一个证据文件里，无法按文件分段得到第一段轮数；
    // 用完成步数乘以每步轮数同样不可靠。轮数以任务快照为准：恢复从
    // 快照接账，`consumption.turns` 就是第一段跑完时的轮数。
    let first_turns = turns_at_snapshot(workspace, &task_id)
        .unwrap_or_else(|| turns_of(workspace, &session_id));

    // ── 第二段：恢复 ─────────────────────────────────────────────
    // 第一次运行未失败时不存在恢复，恢复段记为"没测到"，不能当作恢复成功。
    let resume = if first_failure.is_empty() {
        Err("第一次运行没有失败，没有可恢复的东西".to_owned())
    } else {
        run_long_resume(deps, budget, workspace, &repo, &task_id, task).await
    };
    let (resume_failure, resumed_steps, answer) = match resume {
        Ok((steps, answer)) => (None, steps, answer),
        Err(reason) => (Some(reason.clone()), 0, String::new()),
    };
    // 两段共用一个会话，证据文件为两段追加。本段轮数为文件总轮数减去
    // 第一段轮数。
    let resume_turns = turns_of(workspace, &session_id).saturating_sub(first_turns);
    let steps_completed = first_steps + resumed_steps;

    let total_turns = first_turns + resume_turns;
    let (cost_usd, wall_clock_ms, repeated) = usage_of_long(workspace, &session_id).await;
    // 判据在两段都结束后核对，覆盖整个过程。
    //
    // 前提是恢复确实发生过。第一次运行未失败（`exhaust_turns` 配置过大）
    // 时没有恢复，任务判据可能满足，但"中断 → 恢复"链路没有发生。
    // 这种运行判为不通过，并在失败原因中说明。
    let mut failures = Vec::new();
    if first_failure.is_empty() {
        failures.push(
            "第一次运行没有失败：exhaust_turns 配置过大，中断恢复链路没有被测到".to_owned(),
        );
    } else if resume_failure.is_none() {
        let has_changes = snapshot_files(&repo) != before;
        let text_after_compaction = text_after_first_compaction(workspace, &session_id);
        let observation = CriteriaObservation {
            test_exit_success: run_criterion_command(&repo, &task.criteria.test_command, &mut failures).await,
            has_changes,
            answer: &answer,
            turns_used: total_turns,
            repeated_commands: &repeated,
            text_after_compaction: text_after_compaction.as_deref(),
        };
        failures.extend(task.criteria.evaluate(observation));
    } else {
        failures.push(resume_failure.clone().unwrap_or_default());
    }

    let result = LongBenchmarkResult {
        task_id: task.id.clone(),
        language: task.language.clone(),
        session_id: Some(session_id),
        first_run_failure: first_failure,
        first_run_steps: first_steps,
        first_run_turns: first_turns,
        first_run_turn_limit: turn_limit,
        resume_failure,
        resume_turns,
        total_turns,
        steps_total: orchestrated.steps().len(),
        steps_completed,
        passed: failures.is_empty(),
        failures,
        repeated_commands: repeated,
        cost_usd,
        wall_clock_ms,
    };
    let _ = fs::remove_dir_all(&repo);
    Ok(result)
}

/// 执行恢复段：读取任务快照，把剩余步骤跑完。
///
/// 与 `run_long_one` 的第一段分开实现，因为装配不同：上下文预算按任务
/// 配置压小（长任务历史最长，压缩发生在这一段），轮数上限放开，避免
/// 再被故意设小的预算打断。
async fn run_long_resume(
    deps: &AgentDependencies,
    budget: &Budget,
    workspace: &Path,
    repo: &Path,
    task_id: &TaskId,
    task: &BenchmarkTask,
) -> Result<(usize, String), String> {
    let task_store = crate::infrastructure::task_store::JsonTaskStore::new(workspace);
    let Some(snapshot) = task_store.load(task_id).map_err(|error| format!("读取任务快照失败: {error}"))? else {
        return Err(format!("任务 {task_id} 的快照不存在，无法恢复"));
    };

    let mut resume_deps = deps.clone();
    resume_deps.tools = ToolRegistry::new(all_tools(WorkspaceRoot::new(repo.to_path_buf())));
    resume_deps.session_store = Some(std::sync::Arc::new(
        crate::infrastructure::session_store::json::JsonSessionStore::new(workspace),
    ) as _);
    // 压缩判据要求恢复段的上下文预算压到任务配置的值。
    if let Some(max_tokens) = task.long_run.context_max_tokens {
        resume_deps.context_budget.max_tokens = Some(max_tokens);
    }
    let evidence_factory: EvidenceFactory = shared_evidence_factory(workspace);
    // 与第一段同样装配扫描器，恢复前核对工作区，走真实用户的路径。
    let mut resume_orchestrator = Orchestrator::new(resume_deps, *budget, evidence_factory)
        .with_task_store(std::sync::Arc::new(task_store))
        .with_worktree_scanner(std::sync::Arc::new(
            crate::infrastructure::worktree::RootedWorktreeScanner::new(repo.to_path_buf()),
        ));

    let outcome = resume_orchestrator
        .resume(snapshot)
        .await
        .map_err(|error| error.to_string())?;
    // `steps_completed` 是本段完成的步数。结果需要任务总进度，由调用方
    // 加上第一段的步数。
    Ok((outcome.steps_completed, outcome.answer))
}

/// 任务快照里记录的已消耗轮数，即第一段结束时的轮数。
///
/// 两段共用一个证据文件，中间没有可辨认的分界，从快照读取比按文件
/// 分段统计可靠。
fn turns_at_snapshot(workspace: &Path, task_id: &TaskId) -> Option<u32> {
    let store = crate::infrastructure::task_store::JsonTaskStore::new(workspace);
    let snapshot = store.load(task_id).ok()??;
    Some(snapshot.run.consumption.turns)
}

/// 任务证据汇工厂：两段运行共用同一个文件。
fn shared_evidence_factory(workspace: &Path) -> EvidenceFactory {
    let workspace = workspace.to_path_buf();
    Arc::new(move |session_id: &SessionId| {
        let path = session_path(&workspace, session_id.as_str());
        let jsonl = JsonlEvidenceSink::open(&path)?;
        Ok(Arc::new(jsonl) as Arc<dyn EvidenceSink>)
    })
}

/// 长任务"故意耗尽"的默认轮数上限。
///
/// 任务配置未写 `exhaust_turns` 时使用。它足够小，第一步内必然被打断，
/// 又至少能让每步执行一轮。
const DEFAULT_EXHAUST_TURNS: u32 = 2;

/// 统计一个会话到目前为止的模型调用轮数。
fn turns_of(workspace: &Path, session: &SessionId) -> u32 {
    let path = session_path(workspace, session.as_str());
    match read_session(&path) {
        Ok(Some(log)) => summarize(&log).model_calls as u32,
        _ => 0,
    }
}

/// 第一次上下文压缩之后模型的最后一条思考文本。
///
/// 供压缩判据使用，回答"历史被裁掉后模型是否还提到关键内容"。取思考
/// 而非最终答案：最终答案是润色后的结论，压缩后的工作过程只出现在
/// 思考中。
fn text_after_first_compaction(workspace: &Path, session: &SessionId) -> Option<String> {
    let path = session_path(workspace, session.as_str());
    let log = read_session(&path).ok()??;
    let first_compaction = log
        .events
        .iter()
        .position(|evidence| matches!(evidence.event, crate::domain::evidence::AuditEvent::ContextCompacted { .. }))?;
    log.events[first_compaction..]
        .iter()
        .rev()
        .find_map(|evidence| match &evidence.event {
            crate::domain::evidence::AuditEvent::Thought { text, .. } if !text.trim().is_empty() => {
                Some(text.clone())
            }
            _ => None,
        })
}

/// 汇总长任务两段的成本、墙钟与重复执行的命令。
///
/// 重复命令按 `ToolObserved` 统计，只计算真正执行过的调用。恢复路径上
/// 仅有 `ToolCallReplayed` 记录而未重跑的调用，以及被拒绝的调用，都不
/// 构成副作用重复。
async fn usage_of_long(
    workspace: &Path,
    session: &SessionId,
) -> (Option<f64>, u64, Vec<String>) {
    let path = session_path(workspace, session.as_str());
    let mut cost = None;
    let mut wall_clock = 0u64;
    let mut repeated = Vec::new();
    if let Ok(Some(log)) = read_session(&path) {
        let summary = summarize(&log);
        cost = summary.cost_usd;
        wall_clock = summary.wall_clock_ms;
        for evidence in &log.events {
            if let crate::domain::evidence::AuditEvent::ToolObserved { call, .. } = &evidence.event {
                if call.name == "run_command" {
                    if let Some(command) = call.arguments.get("command").and_then(|v| v.as_str()) {
                        // 先 trim，首尾空白不影响判定。
                        let command = command.trim().to_owned();
                        if repeated.contains(&command) {
                            continue;
                        }
                        if seen_twice(&log.events, &command) {
                            repeated.push(command);
                        }
                    }
                }
            }
        }
    }
    (cost, wall_clock, repeated)
}

/// 判断某条命令在证据里是否被执行了不止一次。
///
/// 只统计实际执行过的调用。被拒绝的调用（策略拦截、审批否决、工具未
/// 注册）没有执行，也没有副作用，计入会误报。执行后失败的调用仍然
/// 计入：命令已运行，副作用可能已经发生。
fn seen_twice(events: &[crate::domain::evidence::Evidence], command: &str) -> bool {
    let mut count = 0usize;
    for evidence in events {
        if let crate::domain::evidence::AuditEvent::ToolObserved { call, outcome, .. } =
            &evidence.event
        {
            if outcome.status == crate::domain::tool::OutcomeStatus::Refused {
                continue;
            }
            if call.name == "run_command"
                && call
                    .arguments
                    .get("command")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    == Some(command)
            {
                count += 1;
                if count > 1 {
                    return true;
                }
            }
        }
    }
    false
}

/// 在 fixture 拷贝中执行判据的测试命令，返回是否以 0 退出。命令执行
/// 失败（无法启动、超时）同样计为未通过，原因追加到 `failures`。
async fn run_criterion_command(repo: &Path, command: &str, failures: &mut Vec<String>) -> bool {
    let root = WorkspaceRoot::new(repo.to_path_buf());
    match run_in_workspace(&root, command, TEST_COMMAND_TIMEOUT).await {
        Ok(output) if output.code == 0 => true,
        Ok(output) => {
            failures.push(format!(
                "测试命令 `{command}` 未通过（退出码 {}）\n{}",
                output.code,
                tail(&output.stdout, &output.stderr),
            ));
            false
        }
        Err(error) => {
            failures.push(format!("测试命令 `{command}` 执行失败: {error}"));
            false
        }
    }
}

/// 拼接 stdout 与 stderr 的末尾若干行作为失败诊断。
fn tail(stdout: &str, stderr: &str) -> String {
    let mut text = String::new();
    for (label, part) in [("── stdout ──", stdout), ("── stderr ──", stderr)] {
        if part.trim().is_empty() {
            continue;
        }
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(label);
        text.push('\n');
        let lines: Vec<&str> = part.trim_end().lines().collect();
        let dropped = lines.len().saturating_sub(FAILURE_OUTPUT_LINES);
        if dropped > 0 {
            text.push_str(&format!("…（前面还有 {dropped} 行）\n"));
        }
        text.push_str(&lines[lines.len().saturating_sub(FAILURE_OUTPUT_LINES)..].join("\n"));
    }
    if text.is_empty() {
        text.push_str("（命令没有输出）");
    }
    text
}

/// 汇总任务会话的轮数、墙钟、审批次数、缓存命中率与成本。
///
/// 轮数是思考事件的条数，每轮模型调用记一次。墙钟是首末事件的间隔，
/// 不含 fixture 拷贝与判据命令的时间。
///
/// 一个任务只有一个会话，读一份证据即可。
///
/// 成本沿用报表口径：未知即未知，低估的合计比未知更危险。命中率按合计
/// 计算（合计读回 / 合计输入），不对各步比率取平均，以免短步骤与长
/// 步骤等权。
async fn usage_of(
    workspace: &Path,
    session: &SessionId,
) -> (u32, Option<f64>, u64, usize, Option<f64>) {
    let path = session_path(workspace, session.as_str());
    let mut turns = 0u32;
    let mut cost = None;
    let mut wall_clock = 0u64;
    let mut approvals = 0usize;
    let mut input_tokens = 0u64;
    let mut cached_tokens = 0u64;
    match read_session(&path) {
        Ok(Some(log)) => {
            let summary = summarize(&log);
            turns = summary.model_calls as u32;
            wall_clock = summary.wall_clock_ms;
            approvals = summary.approvals;
            input_tokens = summary.input_tokens;
            cached_tokens = summary.cached_input_tokens;
            cost = summary.cost_usd;
        }
        // 无证据（空文件）或读取失败：成本按未知处理，轮数记 0。
        Ok(None) => {}
        Err(error) => {
            eprintln!("警告：证据 {} 读不出来，成本按未知处理: {error}", path.display());
        }
    }
    // 输入 token 为 0 时返回 None，表示无法计算，不能按 0% 命中处理。
    let hit_ratio = if input_tokens > 0 {
        Some(cached_tokens as f64 / input_tokens as f64)
    } else {
        None
    };
    (turns, cost, wall_clock, approvals, hit_ratio)
}

/// 把任务的 fixture 仓库复制到临时目录，返回拷贝根。
///
/// 目录名包含任务 id 与原子自增序号。只带任务 id 时，同一进程内并行
/// 运行的同名任务（集成测试即是如此）会互相删除拷贝。序号保证进程内
/// 唯一，进程 id 保证跨进程不冲突。
fn copy_fixture(benchmarks_root: &Path, task: &BenchmarkTask) -> Result<PathBuf, String> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT_COPY: AtomicU64 = AtomicU64::new(0);
    let source = benchmarks_root.join(&task.id).join("repo");
    let serial = NEXT_COPY.fetch_add(1, Ordering::Relaxed);
    let dest = std::env::temp_dir().join(format!(
        "qch-bench-{}-{}-{serial}",
        task.id,
        std::process::id()
    ));
    COPIES_MADE.store(serial + 1, Ordering::Relaxed);
    let _ = fs::remove_dir_all(&dest);
    copy_dir(&source, &dest).map_err(|error| format!("复制 fixture {} 失败: {error}", source.display()))?;
    Ok(dest)
}

/// 本进程已创建的拷贝份数。测试用它定位最后一次拷贝的序号。
static COPIES_MADE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// 递归复制目录，跳过 `target`。fixture 中可能残留本地构建产物，
/// 不应进入基准环境。
fn copy_dir(source: &Path, dest: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dest)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        if entry.file_name() == "target" {
            continue;
        }
        let target = dest.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

/// 工作区快照：相对路径 → 文件内容。
///
/// 跳过 `target` 目录与 `Cargo.lock`。它们是 `cargo test` 的产物，
/// 混入会把未修改误判为已修改。
fn snapshot_files(root: &Path) -> HashMap<String, Vec<u8>> {
    let mut files = HashMap::new();
    walk(root, root, &mut files);
    files
}

fn walk(root: &Path, dir: &Path, files: &mut HashMap<String, Vec<u8>>) {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) => {
            eprintln!("警告：快照读不到目录 {}: {error}", dir.display());
            return;
        }
    };
    for entry in entries {
        let Ok(entry) = entry else { continue };
        if entry.file_name() == "target" {
            continue;
        }
        let path = entry.path();
        if path.is_dir() {
            walk(root, &path, files);
        } else if path.is_file() {
            let relative = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            if relative == "Cargo.lock" {
                continue;
            }
            match fs::read(&path) {
                Ok(contents) => {
                    files.insert(relative, contents);
                }
                Err(error) => eprintln!("警告：快照读不到 {}: {error}", path.display()),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::agent_roster::AgentRoster;
    use crate::application::context_policy::DefaultContextPolicy;
    use crate::application::port::{ApprovalGate, NoopHooks};
    use crate::domain::evidence::AuditEvent;
    use crate::domain::policy::PolicyEngine;
    use crate::domain::tool::{OutcomeStatus, ToolOutcome};
    use crate::infrastructure::approval::auto::AutoApproveGate;
    use crate::infrastructure::clock::SystemClock;
    use crate::application::memory_evidence::MemoryEvidenceSink;
    use crate::infrastructure::model::rig_chat::RigChatGateway;

    use std::time::SystemTime;

    use rig::test_utils::{MockCompletionModel, MockTurn};
    use serde_json::{Value, json};

    /// 一条工具执行完毕的证据，结局自选。
    fn observed(
        name: &str,
        arguments: Value,
        status: OutcomeStatus,
    ) -> crate::domain::evidence::Evidence {
        crate::domain::evidence::Evidence::new(
            1,
            SystemTime::now(),
            AuditEvent::ToolObserved {
                turn: 1,
                call: crate::domain::tool::ToolCall {
                    id: crate::domain::ids::CallId::new("c"),
                    name: name.to_owned(),
                    arguments,
                },
                outcome: match status {
                    OutcomeStatus::Success => ToolOutcome::success("ok"),
                    OutcomeStatus::Failed => ToolOutcome::failed("坏"),
                    OutcomeStatus::Refused => ToolOutcome::refused("拒绝"),
                },
            },
        )
    }

    /// 装配模板：模型用脚本化 mock，工具由运行器按任务重建。
    fn bench_deps(mock: &MockCompletionModel) -> AgentDependencies {
        AgentDependencies {
            model: Arc::new(RigChatGateway::new(mock.clone().erase())),
            tools: ToolRegistry::new(vec![]),
            approvals: Arc::new(AutoApproveGate) as Arc<dyn ApprovalGate>,
            evidence: Arc::new(MemoryEvidenceSink::new()) as Arc<dyn EvidenceSink>,
            clock: Arc::new(SystemClock),
            policy: PolicyEngine::new(),
            roster: AgentRoster::builtin(),
            context_budget: Default::default(),
            session_store: None,
            hooks: Arc::new(NoopHooks),
            context_policy: Arc::new(DefaultContextPolicy),
            additional_guards: Vec::new(),
        }
    }

    /// 独立的临时基准根目录。
    fn temp_benchmarks(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("qch-benchrun-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("创建临时目录");
        dir
    }

    /// 写一个任务目录：repo 里一个文本文件 + task.toml。
    fn write_task(root: &Path, id: &str, test_command: &str, requires_changes: bool) {
        let dir = root.join(id);
        fs::create_dir_all(dir.join("repo")).expect("建任务目录");
        fs::write(dir.join("repo/notes.txt"), "第一版").expect("写 fixture 文件");
        let toml = format!(
            "id = \"{id}\"\nkind = \"bug-fix\"\ndescription = \"把 notes.txt 更新成第二版\"\n\n[criteria]\ntest_command = \"{test_command}\"\nrequires_changes = {requires_changes}\nanswer_must_mention = []\n"
        );
        fs::write(dir.join("task.toml"), toml).expect("写 task.toml");
    }

    #[tokio::test]
    async fn a_completed_task_passes_all_criteria() {
        let benchmarks = temp_benchmarks("pass");
        let workspace = temp_benchmarks("pass-ws");
        write_task(&benchmarks, "demo-fix", "exit 0", true);

        // 四步脚本：规划（1 轮）→ 修改文件（2 轮）→ 验证（1 轮）→
        // 汇总（1 轮）。最终答案含判据关键词；本任务不检查关键词，但
        // 答案内容仍需完整。
        let mock = MockCompletionModel::from_turns(vec![
            MockTurn::text("先看 notes.txt 现状"),
            MockTurn::tool_call(
                "call_w1",
                "write_file",
                json!({"path": "notes.txt", "content": "第二版"}),
            ),
            MockTurn::text("已改好文件"),
            MockTurn::text("复核确认改动完整"),
            MockTurn::text("已改好：notes.txt 已更新为第二版，验证通过。"),
        ]);

        let results = run_benchmark(&bench_deps(&mock), &Budget::new(10), &benchmarks, &workspace)
            .await
            .expect("装载");

        assert_eq!(results.len(), 1);
        let result = &results[0];
        assert_eq!(result.task_id, "demo-fix");
        assert!(result.passed, "应通过全部判据: {:?}", result.failures);
        assert!(result.failures.is_empty());
        assert_eq!(result.steps_completed, 4, "bug-fix 模板四步全跑完");
        assert_eq!(result.turns_used, 5, "五次模型调用各记一轮思考");
        assert!(result.session_id.is_some(), "有动手过程就有会话");
        assert_eq!(result.cost_usd, None, "mock 模型没有定价信息：成本是未知而非 0");
        // 运行器跑完会清理临时仓库，文件改动已由判据在清理前确认。
        // 拷贝目录名带进程内原子自增序号，本次运行的拷贝是序号最大且
        // 最新的那个。只检查它，历史运行残留的目录不属于本测试范围。
        let latest = (0..COPIES_MADE.load(std::sync::atomic::Ordering::Relaxed))
            .rev()
            .find_map(|serial| {
                let path = std::env::temp_dir().join(format!(
                    "qch-bench-demo-fix-{}-{serial}",
                    std::process::id()
                ));
                path.exists().then_some(path)
            });
        assert!(
            latest.is_none(),
            "临时仓库应已清理: {:?}",
            latest.map(|path| path.display().to_string())
        );

        let _ = fs::remove_dir_all(&benchmarks);
        let _ = fs::remove_dir_all(&workspace);
    }

    #[tokio::test]
    async fn a_failed_task_does_not_stop_the_next_task() {
        // 基准任务失败不中断后续任务。任务 a 第 1 步两次空回复（原始与
        // 重试）导致步骤失败，任务 b 随后照常跑完。
        let benchmarks = temp_benchmarks("continue");
        let workspace = temp_benchmarks("continue-ws");
        write_task(&benchmarks, "a-collapses", "exit 0", false);
        write_task(&benchmarks, "b-passes", "exit 0", false);

        let mut turns = vec![MockTurn::text(""), MockTurn::text("")];
        turns.extend(["b1", "b2", "b3", "b4"].map(MockTurn::text));
        let mock = MockCompletionModel::from_turns(turns);

        let results = run_benchmark(&bench_deps(&mock), &Budget::new(10), &benchmarks, &workspace)
            .await
            .expect("装载");

        assert_eq!(results.len(), 2, "两个任务都要有结果");
        let failed = &results[0];
        assert_eq!(failed.task_id, "a-collapses");
        assert!(!failed.passed);
        assert!(
            failed.failures.iter().any(|reason| reason.contains("第 1 步失败")),
            "失败原因应说明步骤在哪断的: {:?}",
            failed.failures
        );
        assert_eq!(failed.steps_completed, 0);
        // 一个任务只有一个会话，且在第一步之前创建。第 1 步失败时 run
        // 已生成会话 ID 并随 StepFailed 返回，失败同样能带出会话 ID。
        assert!(
            failed.session_id.is_some(),
            "第 1 步失败也应带出会话 ID，否则用户没有证据可翻"
        );
        let passed = &results[1];
        assert_eq!(passed.task_id, "b-passes");
        assert!(passed.passed, "后续任务不该被前一个的失败拖累: {:?}", passed.failures);

        let _ = fs::remove_dir_all(&benchmarks);
        let _ = fs::remove_dir_all(&workspace);
    }

    #[tokio::test]
    async fn failing_criteria_marks_the_task_failed() {
        // 编排成功但判据未通过：测试命令退出码非 0，失败说明中包含
        // 命令输出。
        let benchmarks = temp_benchmarks("criteria");
        let workspace = temp_benchmarks("criteria-ws");
        write_task(&benchmarks, "red-test", "exit 1", false);

        let mock = MockCompletionModel::from_turns(["1", "2", "3", "4"].map(MockTurn::text));
        let results = run_benchmark(&bench_deps(&mock), &Budget::new(10), &benchmarks, &workspace)
            .await
            .expect("装载");

        assert_eq!(results.len(), 1);
        assert!(!results[0].passed);
        assert!(
            results[0].failures.iter().any(|reason| reason.contains("测试命令 `exit 1` 未通过")),
            "应指明是哪条判据没过: {:?}",
            results[0].failures
        );
        assert_eq!(results[0].steps_completed, 4, "编排本身是成功的");

        let _ = fs::remove_dir_all(&benchmarks);
        let _ = fs::remove_dir_all(&workspace);
    }

    #[tokio::test]
    async fn missing_benchmarks_root_is_a_load_error() {
        let mock = MockCompletionModel::from_turns(vec![]);
        let root = temp_benchmarks("ghost").join("no-such-dir");
        let error = run_benchmark(&bench_deps(&mock), &Budget::new(10), &root, &root)
            .await
            .expect_err("目录不存在应报装载错误");
        assert!(matches!(error, BenchmarkLoadError::RootIo(_)), "{error}");
    }

    #[test]
    fn tail_keeps_the_last_lines_of_both_streams() {
        let stdout = (1..=30).map(|i| format!("out{i}")).collect::<Vec<_>>().join("\n");
        let stderr = "error[E0412]: cannot find type\nhelp: consider importing";

        let text = tail(&stdout, stderr);

        assert!(text.starts_with("── stdout ──"), "{text}");
        assert!(text.contains("…（前面还有 10 行）"));
        assert!(text.contains("out11"), "保留末尾 20 行的第一条: {text}");
        assert!(text.contains("out30"), "保留末尾: {text}");
        assert!(!text.contains("out10"), "被丢弃的前 10 行不该出现");
        assert!(text.contains("── stderr ──"));
        assert!(text.contains("E0412"), "stderr 是编译错误的主战场: {text}");
    }

    #[test]
    fn snapshot_ignores_build_artifacts_but_sees_source_changes() {
        let root = temp_benchmarks("snapshot");
        fs::write(root.join("lib.rs"), "v1").expect("写源文件");

        let before = snapshot_files(&root);
        assert_eq!(before.get("lib.rs").map(Vec::as_slice), Some(b"v1".as_slice()));

        // agent 修改源文件并产生构建产物：只有源文件算改动。
        fs::write(root.join("lib.rs"), "v2").expect("改源文件");
        fs::create_dir_all(root.join("target/debug")).expect("造 target");
        fs::write(root.join("target/debug/blob.o"), "binary").expect("写产物");
        fs::write(root.join("Cargo.lock"), "version = 4").expect("写锁文件");

        assert_ne!(snapshot_files(&root), before, "源文件改动必须被看见");
        let after = snapshot_files(&root);
        assert_eq!(after.len(), 1, "target/ 与 Cargo.lock 不进快照: {:?}", after.keys());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn refused_repeats_are_not_side_effect_repeats() {
        // 被拒绝的调用没有执行，也没有副作用。把两次拒绝算作重复副作用
        // 会误报一次未发生的执行。
        let events = vec![
            observed("run_command", json!({"command": "cargo test"}), OutcomeStatus::Refused),
            observed("run_command", json!({"command": "cargo test"}), OutcomeStatus::Refused),
        ];
        assert!(!seen_twice(&events, "cargo test"), "两次拒绝不构成重复执行");
    }

    #[test]
    fn executed_repeats_count_as_side_effect_repeats() {
        // 成功执行两次，副作用发生两遍，判定为重复。
        let events = vec![
            observed("run_command", json!({"command": "cargo test"}), OutcomeStatus::Success),
            observed("run_command", json!({"command": "cargo test"}), OutcomeStatus::Success),
        ];
        assert!(seen_twice(&events, "cargo test"), "两次成功执行是重复副作用");

        // 执行后失败：命令已运行，副作用可能已发生，仍然计入。
        let failed = vec![
            observed("run_command", json!({"command": "cargo test"}), OutcomeStatus::Failed),
            observed("run_command", json!({"command": "cargo test"}), OutcomeStatus::Failed),
        ];
        assert!(seen_twice(&failed, "cargo test"), "失败也算执行过");

        // 一次成功加一次拒绝：实际执行只有一次，不算重复。
        let mixed = vec![
            observed("run_command", json!({"command": "cargo test"}), OutcomeStatus::Success),
            observed("run_command", json!({"command": "cargo test"}), OutcomeStatus::Refused),
        ];
        assert!(!seen_twice(&mixed, "cargo test"), "一次执行一次拒绝不算重复");
    }

    #[test]
    fn group_by_language_buckets_by_fixture_language_defaulting_to_rust() {
        let results = vec![
            result("greet", None),
            result("py", Some("python".into())),
            result("greet2", None),
        ];

        let groups = group_by_language(&results);
        let names: Vec<_> = groups.iter().map(|(name, _)| *name).collect();
        let counts: Vec<_> = groups.iter().map(|(_, g)| g.len()).collect();
        assert_eq!(names, ["rust", "python"], "缺省（None）归 rust");
        assert_eq!(counts, [2, 1], "缺省组保留两条");
    }

    #[test]
    fn group_by_language_works_on_long_results() {
        let results = vec![
            long_result("a", Some("python".into())),
            long_result("b", None),
        ];

        let groups = group_by_language(&results);
        let names: Vec<_> = groups.iter().map(|(name, _)| *name).collect();
        assert_eq!(names, ["python", "rust"], "分组顺序取首次出现");
    }

    fn result(task_id: &str, language: Option<String>) -> BenchmarkResult {
        BenchmarkResult {
            task_id: task_id.into(),
            language,
            session_id: None,
            passed: true,
            failures: Vec::new(),
            steps_completed: 0,
            turns_used: 0,
            wall_clock_ms: 0,
            approvals: 0,
            cache_hit_ratio: None,
            cost_usd: None,
        }
    }

    fn long_result(task_id: &str, language: Option<String>) -> LongBenchmarkResult {
        LongBenchmarkResult {
            task_id: task_id.into(),
            language,
            session_id: None,
            first_run_failure: String::new(),
            first_run_steps: 0,
            first_run_turns: 0,
            first_run_turn_limit: 0,
            resume_failure: None,
            resume_turns: 0,
            total_turns: 0,
            steps_total: 0,
            steps_completed: 0,
            passed: true,
            failures: Vec::new(),
            repeated_commands: Vec::new(),
            cost_usd: None,
            wall_clock_ms: 0,
        }
    }

    #[test]
    fn language_summary_lines_report_pass_rate_per_language() {
        let mut results = vec![
            result("greet", None),
            result("py-ok", Some("python".into())),
            result("py-bad", Some("python".into())),
        ];
        results[2].passed = false;

        let lines = language_summary_lines(&results);
        assert_eq!(
            lines,
            ["rust: 1/1 通过", "python: 1/2 通过"],
            "通过率按语言统计，未通过也计入分母"
        );
    }

    #[test]
    fn seen_twice_only_looks_at_run_command_with_matching_command() {
        // 其他工具即使参数同名，也不计入命令重复。
        let other_tool = vec![
            observed("write_file", json!({"command": "cargo test"}), OutcomeStatus::Success),
            observed("write_file", json!({"command": "cargo test"}), OutcomeStatus::Success),
        ];
        assert!(!seen_twice(&other_tool, "cargo test"), "非 run_command 不计");

        // 不同命令分别计数，各执行一次不算重复。
        let different = vec![
            observed("run_command", json!({"command": "cargo test"}), OutcomeStatus::Success),
            observed("run_command", json!({"command": "cargo build"}), OutcomeStatus::Success),
        ];
        assert!(!seen_twice(&different, "cargo test"), "cargo build 不该算进 cargo test");
        assert!(!seen_twice(&different, "cargo build"), "反向同理");

        // 参数先 trim，模型常带首尾空格。
        let spaced = vec![
            observed("run_command", json!({"command": "cargo test "}), OutcomeStatus::Success),
            observed("run_command", json!({"command": " cargo test"}), OutcomeStatus::Success),
        ];
        assert!(seen_twice(&spaced, "cargo test"), "首尾空白不影响同一条命令的认定");
    }
}

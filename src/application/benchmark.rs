//! 基准运行器：把任务集逐个跑一遍并按判据打分。
//!
//! 一个任务 = 复制 fixture 到临时目录 → 编排器驱动 agent 动手 →
//! 跑判据里的测试命令 → 按判据打分 → 读回证据统计轮数与成本。
//! 每个任务拿到全新的 fixture 拷贝与独立的工具注册表（workspace 根
//! 指向拷贝），任务之间互不污染。单个任务失败——编排中断或判据
//! 不过——只记为该任务未通过，后面的任务照跑：基准的价值在一次
//! 运行看清全貌，跑一半就崩等于白跑。
//!
//! 分层说明：本模块按任务书放在 application，但装配真实工具注册表
//! （[`all_tools`]）、执行测试命令（[`run_in_workspace`]）与读写证据
//! （[`JsonlEvidenceSink`] / [`read_session`]）都直接引用了
//! infrastructure 的现成入口。这是有意的取舍：为运行器单造一层
//! 端口抽象属于过度设计，等出现第二个运行器再抽不迟。

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

/// 判据测试命令的超时。fixture 都是小 crate，几分钟绰绰有余；
/// 卡死的命令不该拖垮整个基准。
const TEST_COMMAND_TIMEOUT: Duration = Duration::from_secs(300);

/// 失败诊断里保留的命令输出行数：失败原因几乎总在结尾，全量
/// 输出属于证据文件与重跑，塞进结果里只会淹没判据清单。
const FAILURE_OUTPUT_LINES: usize = 20;

/// 跑完基准任务集，逐个打分。
///
/// `deps` 是装配模板：模型、审批闸、时钟、策略、上下文预算原样
/// 复用；`tools` 与 `evidence` 由本函数按任务重建——工具的 workspace
/// 根必须指向任务的 fixture 拷贝，证据必须落进统一的会话目录。
/// `workspace` 是证据落盘根（与会话快照共用 `.qcodeharness/sessions/`，
/// 这样 `--report` 能把基准运行一并统计）。
///
/// # Errors
///
/// 任务集装载失败（根目录缺失、任一任务配置非法）时返回装载错误。
/// 单个任务的运行失败不传播——记为该任务未通过。
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
        println!("▶ [{}/{}] {}", index + 1, total, task.description);
        let result = match run_one(deps, budget, benchmarks_root, workspace, task).await {
            Ok(result) => result,
            Err(failure) => BenchmarkResult {
                task_id: task.id.clone(),
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
        println!(
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
            println!("  - {reason}");
        }
        results.push(result);
    }
    Ok(results)
}

/// 跑完长任务基准任务集：每个任务先故意耗尽预算，再恢复着跑完。
///
/// 输出把**两次运行分开**——第一次的失败是设计好的实验条件，
/// 不是任务失败；恢复后的结果才是被评测的东西。混在一起的话，
/// "第一次运行预算耗尽"会被当成一次失败记录，长任务永远通不过。
///
/// # Errors
///
/// 任务集装载失败时返回装载错误。单个任务的两段运行失败不传播。
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
        println!("▶ [{}/{}] 长任务：{}", index + 1, total, task.description);
        let result = match run_long_one(deps, budget, benchmarks_root, workspace, task).await {
            Ok(result) => result,
            Err(failure) => LongBenchmarkResult {
                task_id: task.id.clone(),
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
            println!("  第一次运行：没有失败——任务一遍跑完了，这次没有测到中断恢复");
        } else {
            println!(
                "  第一次运行：{}（第 {} 步后停 / {} 轮 / 上限 {} 轮）",
                result.first_run_failure,
                result.first_run_steps,
                result.first_run_turns,
                result.first_run_turn_limit,
            );
        }
        match &result.resume_failure {
            Some(reason) => println!("  恢复运行：失败 —— {reason}"),
            None => println!(
                "  恢复运行：从任务快照继续 → {}/{} 步 / 本段 {} 轮 / 总 {} 轮",
                result.steps_completed, result.steps_total, result.resume_turns, result.total_turns,
            ),
        }
        println!(
            "{} 重复副作用 {} 次 / 墙钟 {} / 成本 {}",
            verdict,
            // 与判据共用同一本账：数的是 no_repeated_commands 里真正
            // 违规的命令，不是所有重复过的命令——后者包括模型修完
            // 一个缺陷重跑全量测试这类正常迭代。
            task.criteria.forbidden_repeats(&result.repeated_commands).len(),
            wall_clock_label(result.wall_clock_ms),
            cost_label(result.cost_usd),
        );
        for reason in &result.failures {
            println!("  - {reason}");
        }
        results.push(result);
    }
    Ok(results)
}

/// 墙钟的显示值：秒级可读，亚秒保留一位小数。
///
/// 毫秒数对人没有意义（"跑了 12345"），而基准里一个任务的量级是
/// 几十秒到几分钟——按秒显示再带一位小数，既能一眼比大小，
/// 又不会把 200ms 的差异抹成 0。
pub(crate) fn wall_clock_label(millis: u64) -> String {
    format!("{:.1}s", millis as f64 / 1000.0)
}

/// 跑一个任务并打分。`Err` 只覆盖"任务根本没法跑"的装配级失败
/// （fixture 拷不出来等）；编排中断与判据不过都是正常产物。
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
        // 一个会话都没跑起来：任务没有动手过程，判据无从谈起。
        let _ = fs::remove_dir_all(&repo);
        return Ok(BenchmarkResult {
            task_id: task.id.clone(),
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

    // 编排中断时判据不再有意义：任务没走完，测试通过与否都证明不了
    // 问题被解决，不再烧一次最多五分钟的测试命令。
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
    // 结果已定，临时仓库尽力清理：失败诊断看结果里的输出末尾与证据。
    let _ = fs::remove_dir_all(&repo);
    Ok(result)
}

/// 跑一个长任务：第一段故意耗尽预算，第二段从任务快照恢复。
///
/// **第一段**：把轮数上限压到任务配置的 `exhaust_turns`（故意设小），
/// 编排器必然在中途报"预算耗尽"——这就是要测的中断。失败后任务快照
/// 已经落盘（`--resume-task` 恢复用的就是它）。
///
/// **第二段**：把任务快照读回来，从步骤边界继续。此时：
/// - 已完成步骤不重做（`completed_outputs` 原样带回）；
/// - 消耗账从快照接着算（`consumption` 在快照里）；
/// - 上下文按任务配置的 `context_max_tokens` 裁剪——这一段的历史最长，
///   压缩判据的数据就从这里来。
///
/// 两段共用同一个 fixture 拷贝与同一个会话：恢复接着上一次的会话跑，
/// 这才是"同一个任务"。
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
    // 快照落盘是恢复的前提：会话快照（半轮现场）与任务快照（步骤进度）
    // 都要有。
    let store = crate::infrastructure::session_store::json::JsonSessionStore::new(workspace);
    let task_store = crate::infrastructure::task_store::JsonTaskStore::new(workspace);
    first_deps.session_store = Some(std::sync::Arc::new(store) as _);
    let evidence_factory: EvidenceFactory = shared_evidence_factory(workspace);
    // 工作区扫描器也装上：快照要记下"当时的 fixture 拷贝长什么样"，
    // 恢复段的一致性检查才拿得到基准。基准的工作区是独占的临时拷贝，
    // 中断期间不会有外部改动，检查必然通过——装上它不是让基准变严，
    // 是让恢复走的是与真实用户同一条路。
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
        // 一遍跑完了：没测到想测的东西。如实带出会话 ID 与完成步数，
        // 但"没有中断"这件事要在结果里说得清清楚楚——把一遍跑完当成
        // "恢复成功"会让这次基准什么都没测到却显示通过。
        Ok(outcome) => (String::new(), outcome.steps_completed, Some(outcome.session_id.clone())),
    };
    let Some(session_id) = session_id else {
        let _ = fs::remove_dir_all(&repo);
        return Err(first_failure);
    };
    // 第一次运行的轮数从证据文件分段数不出来（恢复段追加在同一个文件
    // 里），用完成步数 × 每步脚本的轮数也不可靠。它有另一个来源：
    // 恢复是从快照接账的，快照里的 `consumption.turns` 就是第一段跑完
    // 时的账——从任务快照里取。
    let first_turns = turns_at_snapshot(workspace, &task_id)
        .unwrap_or_else(|| turns_of(workspace, &session_id));

    // ── 第二段：恢复 ─────────────────────────────────────────────
    // 第一次运行没失败时没有"恢复"可言：恢复段标记为"没测到"，
    // 而不是把空跑一遍当成恢复成功。
    let resume = if first_failure.is_empty() {
        Err("第一次运行没有失败，没有可恢复的东西".to_owned())
    } else {
        run_long_resume(deps, budget, workspace, &repo, &task_id, task).await
    };
    let (resume_failure, resumed_steps, answer) = match resume {
        Ok((steps, answer)) => (None, steps, answer),
        Err(reason) => (Some(reason.clone()), 0, String::new()),
    };
    // 恢复段的轮数：两段共用一个会话，证据文件是两段追加起来的，
    // 所以"本段轮数" = 文件总数 − 第一段的账。
    let resume_turns = turns_of(workspace, &session_id).saturating_sub(first_turns);
    let steps_completed = first_steps + resumed_steps;

    let total_turns = first_turns + resume_turns;
    let (cost_usd, wall_clock_ms, repeated) = usage_of_long(workspace, &session_id).await;
    // 判据在两段都结束后核对：它对"整个过程"说话。
    //
    // 前提是**恢复真的发生过**。第一次运行没失败（`exhaust_turns` 配
    // 大了）就没有恢复可言——任务的判据可能都满足，但这次基准要测的
    // "中断 → 恢复"链路根本没发生。这种运行不通过，失败原因明说。
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

/// 恢复一段：读任务快照，接着把剩下的步骤跑完。
///
/// 与 `run_long_one` 的第一段分开写：恢复用的装配与第一次运行不同——
/// 上下文预算按任务配置压小（长任务的历史最长，压缩就发生在这一段），
/// 而轮数上限放开（恢复不该再被"故意设小"的预算打断）。
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
    // 压缩判据的前提：恢复段的上下文预算压到任务配置的值。
    if let Some(max_tokens) = task.long_run.context_max_tokens {
        resume_deps.context_budget.max_tokens = Some(max_tokens);
    }
    let evidence_factory: EvidenceFactory = shared_evidence_factory(workspace);
    // 与第一段同口径装配扫描器：恢复前核对工作区，走真实用户那条路。
    let mut resume_orchestrator = Orchestrator::new(resume_deps, *budget, evidence_factory)
        .with_task_store(std::sync::Arc::new(task_store))
        .with_worktree_scanner(std::sync::Arc::new(
            crate::infrastructure::worktree::RootedWorktreeScanner::new(repo.to_path_buf()),
        ));

    let outcome = resume_orchestrator
        .resume(snapshot)
        .await
        .map_err(|error| error.to_string())?;
    // steps_completed 是**本段**完成的步数；结果里要报的是任务总进度，
    // 由调用方（run_long_one）加上第一段的步数。
    Ok((outcome.steps_completed, outcome.answer))
}

/// 任务快照里记的已消耗轮数（第一段结束时的账）。
///
/// 恢复从快照接账，这份账就是第一段的轮数——比从证据文件里分段数
/// 可靠得多：两段追加在同一个文件里，中间没有可辨认的分界。
fn turns_at_snapshot(workspace: &Path, task_id: &TaskId) -> Option<u32> {
    let store = crate::infrastructure::task_store::JsonTaskStore::new(workspace);
    let snapshot = store.load(task_id).ok()??;
    Some(snapshot.run.consumption.turns)
}

/// 一个任务的证据汇工厂：两段跑法共用同一个文件。
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
/// 任务配置没写 `exhaust_turns` 时用它：小到必然在第一步内被打断，
/// 又不至于一轮都跑不完（每步至少要有机会动一次手）。
const DEFAULT_EXHAUST_TURNS: u32 = 2;

/// 统计一个会话到目前为止的模型调用轮数。
fn turns_of(workspace: &Path, session: &SessionId) -> u32 {
    let path = session_path(workspace, session.as_str());
    match read_session(&path) {
        Ok(Some(log)) => summarize(&log).model_calls as u32,
        _ => 0,
    }
}

/// 第一次上下文压缩**之后**模型说的话（最后一条思考文本）。
///
/// 压缩判据的数据来源：它回答"历史被裁掉之后，模型还有没有提过
/// 关键的东西"——取压缩之后的最后一条思考，而不是最终答案：最终
/// 答案是润色过的结论，压缩之后的工作过程只在中间的思考里。
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

/// 汇总长任务两段的账：成本、墙钟、重复执行过的命令。
///
/// 重复命令按 `ToolObserved` 数（规格口径），且只算真正跑过的执行：
/// 只有跑完才谈得上副作用，恢复路径上只留 `ToolCallReplayed` 记录
/// 而没有重跑的调用、以及被拒绝的调用，都不算"副作用重复"。
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
                        // 只记正文，前导空白不影响判定。
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

/// 某条命令在证据里是否被执行了不止一次。
///
/// 只数**真正跑过**的执行：被拒绝的调用（策略拦下、审批否决、工具未
/// 注册）根本没执行，没有副作用——把它们算成"重复执行"，判据会冤枉
/// 一次没有发生的副作用。执行了但失败的算：命令跑了，副作用可能已经
/// 发生，只是结果不对。
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

/// 在 fixture 拷贝里跑判据的测试命令。返回是否 0 退出；命令执行
/// 本身失败（起不来、超时）同样按未通过处理，原因追加进 failures。
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
        Err(message) => {
            failures.push(format!("测试命令 `{command}` 执行失败: {message}"));
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

/// 汇总一个任务会话的轮数、墙钟、审批次数、缓存命中率与成本。
///
/// 轮数 = 会话里思考事件的条数（每轮模型调用记一次）；墙钟 = 首末事件
/// 的间隔——它不含 fixture 拷贝与判据命令的时间，那两段不是"等模型"。
///
/// P9-3 起一个任务只有一个会话，所以这里读一份证据就够。早先是逐步
/// 会话累加，还要防着同一个 ID 在列表里出现两次被重复累计。
///
/// 成本按报表的口径——未知就是未知，被低估的合计比"未知"更危险。
/// 命中率按合计算（合计读回 / 合计输入），不是各步比率的平均：平均会
/// 让一个短步骤和一个长步骤等权。
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
        // 没有可读证据（空文件）或读不出来：成本按未知处理，轮数按 0 计
        // ——缺数据不能伪造数据。
        Ok(None) => {}
        Err(error) => {
            eprintln!("警告：证据 {} 读不出来，成本按未知处理: {error}", path.display());
        }
    }
    // 分母为 0 时是 None：没有输入 token 可算，不是"0% 命中"。
    let hit_ratio = if input_tokens > 0 {
        Some(cached_tokens as f64 / input_tokens as f64)
    } else {
        None
    };
    (turns, cost, wall_clock, approvals, hit_ratio)
}

/// 把任务的 fixture 仓库复制到临时目录，返回拷贝根。
///
/// 目录名带任务 id 与一个**原子自增序号**：只带任务 id 的话，同一进程
/// 内两个同名任务并行跑（集成测试就是这么跑的）会互相删对方的拷贝。
/// 序号保证进程内唯一；进程 id 保证跨进程不串。
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

/// 本进程至今造出的拷贝份数。测试用它定位"最后一次拷贝"的序号。
static COPIES_MADE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// 递归拷目录，跳过 `target`——fixture 里可能残留本地构建产物，
/// 不该跟着进基准环境。
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

/// 工作区快照：相对路径 → 文件内容。跳过 `target` 目录与
/// `Cargo.lock`——它们是 `cargo test` 的产物而不是 agent 的修改，
/// 混进来会把"没动手"误判成"动过手"。
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
    use crate::application::port::ApprovalGate;
    use crate::domain::evidence::AuditEvent;
    use crate::domain::policy::PolicyEngine;
    use crate::domain::tool::{OutcomeStatus, ToolOutcome};
    use crate::infrastructure::approval::auto::AutoApproveGate;
    use crate::infrastructure::clock::SystemClock;
    use crate::infrastructure::evidence::memory::MemoryEvidenceSink;
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

        // 四步脚本的节奏：规划（1 轮）→ 动手改文件（2 轮）→ 验证
        // （1 轮）→ 汇总（1 轮）。最终答案含判据关键词（本任务恰好
        // 不查关键词，但答案内容仍要像人话）。
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
        // 运行器跑完会清理临时仓库；文件改动已由判据在清理前确认。
        // 拷贝目录带序号（进程内原子自增），这次运行造出的拷贝必然是
        // 序号最大（且最新）的那个——只查它，不去管历史运行留下的
        // 残骸（那些不是本测试的事）。
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
        // 验收项：基准任务运行失败不中断后续任务。任务 a 第 1 步两次
        // 空回复（原始 + 重试）→ 步骤失败；任务 b 随后照常跑完。
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
        // P9-3 起一个任务只有一个会话，会话在第一步之前就开好了：即使
        // 第 1 步当场失败，它的两次空回复也已经在证据文件里。所以失败
        // 也带得出会话 ID——那正是要去翻的证据。
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
        // 编排成功但判据不过：测试命令退出码非 0 → 未通过，且失败
        // 说明里带得回命令输出。
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

        // agent 改源文件 + cargo 造产物：只有源文件算改动。
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
        // 被拒绝的调用从没执行过，没有副作用。把两次"拒绝"算成重复
        // 副作用，判据会冤枉一次根本没发生的执行——这正是拒绝的本意。
        let events = vec![
            observed("run_command", json!({"command": "cargo test"}), OutcomeStatus::Refused),
            observed("run_command", json!({"command": "cargo test"}), OutcomeStatus::Refused),
        ];
        assert!(!seen_twice(&events, "cargo test"), "两次拒绝不构成重复执行");
    }

    #[test]
    fn executed_repeats_count_as_side_effect_repeats() {
        // 成功执行两次：副作用发生了两遍，这是判据要抓的重复。
        let events = vec![
            observed("run_command", json!({"command": "cargo test"}), OutcomeStatus::Success),
            observed("run_command", json!({"command": "cargo test"}), OutcomeStatus::Success),
        ];
        assert!(seen_twice(&events, "cargo test"), "两次成功执行是重复副作用");

        // 执行了但失败：命令跑了，副作用可能已经发生只是结果不对，
        // 与拒绝不同，仍计入。
        let failed = vec![
            observed("run_command", json!({"command": "cargo test"}), OutcomeStatus::Failed),
            observed("run_command", json!({"command": "cargo test"}), OutcomeStatus::Failed),
        ];
        assert!(seen_twice(&failed, "cargo test"), "失败也算执行过");

        // 一次成功 + 一次拒绝：真正执行只有一次，不算重复。
        let mixed = vec![
            observed("run_command", json!({"command": "cargo test"}), OutcomeStatus::Success),
            observed("run_command", json!({"command": "cargo test"}), OutcomeStatus::Refused),
        ];
        assert!(!seen_twice(&mixed, "cargo test"), "一次执行一次拒绝不算重复");
    }

    #[test]
    fn seen_twice_only_looks_at_run_command_with_matching_command() {
        // 其他工具即使同名参数也不该参与"命令重复"的账。
        let other_tool = vec![
            observed("write_file", json!({"command": "cargo test"}), OutcomeStatus::Success),
            observed("write_file", json!({"command": "cargo test"}), OutcomeStatus::Success),
        ];
        assert!(!seen_twice(&other_tool, "cargo test"), "非 run_command 不计");

        // 不同的命令不互相计数：各执行一次都不算重复。
        let different = vec![
            observed("run_command", json!({"command": "cargo test"}), OutcomeStatus::Success),
            observed("run_command", json!({"command": "cargo build"}), OutcomeStatus::Success),
        ];
        assert!(!seen_twice(&different, "cargo test"), "cargo build 不该算进 cargo test");
        assert!(!seen_twice(&different, "cargo build"), "反向同理");

        // 参数里多出的空白按 trim 归一：模型常带尾随空格。
        let spaced = vec![
            observed("run_command", json!({"command": "cargo test "}), OutcomeStatus::Success),
            observed("run_command", json!({"command": " cargo test"}), OutcomeStatus::Success),
        ];
        assert!(seen_twice(&spaced, "cargo test"), "首尾空白不影响同一条命令的认定");
    }
}

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
use crate::application::tool_registry::ToolRegistry;
use crate::domain::benchmark::{BenchmarkResult, BenchmarkTask, CriteriaObservation};
use crate::domain::budget::Budget;
use crate::domain::ids::{SessionId, TaskId};
use crate::domain::workspace::WorkspaceRoot;
use crate::infrastructure::benchmark::{load_tasks, BenchmarkLoadError};
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
fn copy_fixture(benchmarks_root: &Path, task: &BenchmarkTask) -> Result<PathBuf, String> {
    let source = benchmarks_root.join(&task.id).join("repo");
    // 目录名带任务 id：多个任务的临时目录互不覆盖，崩溃后也认得出是谁的。
    let dest = std::env::temp_dir().join(format!("qch-bench-{}-{}", task.id, std::process::id()));
    let _ = fs::remove_dir_all(&dest);
    copy_dir(&source, &dest).map_err(|error| format!("复制 fixture {} 失败: {error}", source.display()))?;
    Ok(dest)
}

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
    use crate::infrastructure::approval::auto::AutoApproveGate;
    use crate::infrastructure::clock::SystemClock;
    use crate::infrastructure::evidence::memory::MemoryEvidenceSink;
    use crate::infrastructure::model::rig_chat::RigChatGateway;
    use crate::domain::policy::PolicyEngine;

    use rig::test_utils::{MockCompletionModel, MockTurn};
    use serde_json::json;

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
        // fixture 真的被改了（has_changes 判据的事实来源）。
        let notes = fs::read_to_string(std::env::temp_dir().join(format!(
            "qch-bench-demo-fix-{}",
            std::process::id()
        ))).ok();
        // 运行器跑完会清理临时仓库；文件改动已由判据在清理前确认。
        assert!(notes.is_none(), "临时仓库应已清理");

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
}

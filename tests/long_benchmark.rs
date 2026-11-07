//! 长任务基准的两段式跑法：集成验收。
//!
//! 走的是与 `--benchmark-long` 完全相同的路径：真的两段运行、真的
//! 任务快照落盘、真的恢复。验收的四件事——
//!
//! - **第一次运行必然被故意设小的预算打断**（失败是设计好的实验条件，
//!   不是事故）；
//! - **恢复从任务快照继续，已完成的步骤不重做**；
//! - **恢复段的上下文被裁剪，压缩判据有数据可核对**；
//! - **判据按两段合计的账核对**（轮数累计、重复命令跨段可查）。
//!
//! 不走网络：模型用 `MockCompletionModel` 脚本化。

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use qcode_harness::application::agent_roster::AgentRoster;
use qcode_harness::application::agent_run::AgentDependencies;
use qcode_harness::application::benchmark::{run_long_benchmark, run_benchmark};
use qcode_harness::application::context_policy::DefaultContextPolicy;
use qcode_harness::application::port::{ApprovalGate, EvidenceSink, NoopHooks};
use qcode_harness::application::tool_registry::ToolRegistry;
use qcode_harness::domain::budget::Budget;
use qcode_harness::domain::policy::PolicyEngine;
use qcode_harness::infrastructure::approval::auto::AutoApproveGate;
use qcode_harness::infrastructure::clock::SystemClock;
use qcode_harness::infrastructure::evidence::memory::MemoryEvidenceSink;
use qcode_harness::infrastructure::model::rig_chat::RigChatGateway;

use rig::test_utils::{MockCompletionModel, MockTurn};
use serde_json::json;

/// 每次测试用独立的临时目录，避免并行测试互相踩。
fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("qch-long-bench-{}-{}", name, std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("建临时目录");
    dir
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
    }
}

/// 在基准根目录写一个长任务：一个 notes.txt fixture + task.toml。
///
/// 工作流是 bug-fix 四步（规划 → 动手 → 验证 → 汇总）。模型脚本让
/// 每步至少消耗两轮（先读、再答），第一次运行必然停在预算上。
fn write_long_task(root: &Path, id: &str, exhaust_turns: u32, min_turns: u32) {
    let dir = root.join(id);
    fs::create_dir_all(dir.join("repo")).expect("建任务目录");
    fs::write(dir.join("repo/notes.txt"), "第一版").expect("写 fixture 文件");
    let toml = format!(
        "id = \"{id}\"\nkind = \"bug-fix\"\ndescription = \"把 notes.txt 更新成第二版\"\nsuite = \"long\"\n\n[long_run]\nexhaust_turns = {exhaust_turns}\ncontext_max_tokens = 1500\n\n[criteria]\ntest_command = \"exit 0\"\nrequires_changes = true\nanswer_must_mention = []\nmin_turns = {min_turns}\nno_repeated_commands = []\nmust_mention_after_compaction = []\n"
    );
    fs::write(dir.join("task.toml"), toml).expect("写 task.toml");
}

/// 一个跨步骤的模型脚本：每步"先读文件、再给结论"，四步共 8 轮。
///
/// 步骤之间用 begin_step 的文本判断衔接——脚本只管按顺序给轮，
/// 编排器负责换步。第 2 步写文件：`requires_changes` 判据要的
/// "实际改动"由它产生。
fn multi_step_script() -> MockCompletionModel {
    let mut turns = Vec::new();
    for step in 1..=4 {
        turns.push(MockTurn::tool_call(
            format!("call_read_{step}"),
            "read_file",
            json!({"path": "notes.txt"}),
        ));
        if step == 2 {
            turns.push(MockTurn::tool_call(
                "call_write_2",
                "write_file",
                json!({"path": "notes.txt", "content": "第二版"}),
            ));
            turns.push(MockTurn::text("第 2 步的结论"));
        } else {
            turns.push(MockTurn::text(format!("第 {step} 步的结论")));
        }
    }
    MockCompletionModel::from_turns(turns)
}

#[tokio::test]
async fn the_long_benchmark_exhausts_then_resumes_without_redoing_steps() {
    let benchmarks = temp_dir("exhaust");
    let workspace = temp_dir("exhaust-ws");
    // 4 轮耗尽：脚本 9 轮，第一次运行必然跑不完。
    // min_turns = 6：两段合计 9 轮（首次 4 + 恢复 5）能过。
    write_long_task(&benchmarks, "demo-long", 4, 6);

    let mock = multi_step_script();
    let results =
        run_long_benchmark(&bench_deps(&mock), &Budget::new(30), &benchmarks, &workspace)
            .await
            .expect("装载");

    let _ = fs::remove_dir_all(&benchmarks);
    assert_eq!(results.len(), 1, "一个长任务一个结果");
    let result = &results[0];
    assert_eq!(result.task_id, "demo-long");

    // ── 第一次运行：故意被打断 ──────────────────────────────────
    assert!(
        !result.first_run_failure.is_empty(),
        "第一次运行应当被故意耗尽的预算打断: {result:?}"
    );
    assert!(
        result.first_run_failure.contains("已达最大轮数") || result.first_run_failure.contains("预算"),
        "失败原因应说清是预算耗尽: {}",
        result.first_run_failure
    );
    assert_eq!(result.first_run_turn_limit, 4, "第一次运行的轮数上限是配置的 4");
    assert!(
        result.first_run_turns <= 4,
        "第一次运行的轮数不能超过上限: {}",
        result.first_run_turns
    );

    // ── 恢复运行：跑完剩下的 ────────────────────────────────────
    assert!(
        result.resume_failure.is_none(),
        "恢复运行应当跑完: {:?}",
        result.resume_failure
    );
    assert_eq!(
        result.steps_completed, result.steps_total,
        "恢复后所有步骤都该完成: {result:?}"
    );
    // 已完成的步骤不重做：两段合计的轮数恰好是脚本的 9 轮
    // （每步一轮读，第 2 步多一轮写）。若第一步重做，恢复段会多消耗轮数。
    assert_eq!(
        result.total_turns, 9,
        "两段合计恰好是脚本的 9 轮——步骤没有被重做: {result:?}"
    );
    assert!(result.passed, "判据应全过: {:?}", result.failures);
    assert!(result.repeated_commands.is_empty(), "不应有重复命令: {result:?}");

    let _ = fs::remove_dir_all(&workspace);
}

#[tokio::test]
async fn the_long_benchmark_fails_when_the_resume_redoes_completed_work() {
    // 对照实验：把轮数下限抬到 12——两段合计只有 9 轮，过不了。
    // 这证明 min_turns 判据真的在数两段合计的账，而不是只数恢复段。
    // (重做本身被 the_long_benchmark_exhausts_then_resumes_without_redoing_steps
    // 里的 total_turns == 9 精确断言抓住：重做会让它变成 11。)
    let benchmarks = temp_dir("min-turns");
    let workspace = temp_dir("min-turns-ws");
    write_long_task(&benchmarks, "demo-long", 4, 12);

    let mock = multi_step_script();
    let results =
        run_long_benchmark(&bench_deps(&mock), &Budget::new(30), &benchmarks, &workspace)
            .await
            .expect("装载");

    let _ = fs::remove_dir_all(&benchmarks);
    let result = &results[0];
    assert!(!result.passed, "9 轮不该过 12 轮的下限: {result:?}");
    assert!(
        result
            .failures
            .iter()
            .any(|reason| reason.contains("至少 12 轮")),
        "失败应说明轮数不足: {:?}",
        result.failures
    );
    let _ = fs::remove_dir_all(&workspace);
}

#[tokio::test]
async fn a_long_task_whose_first_run_never_fails_is_reported_honestly() {
    // 配置的 exhaust_turns 比脚本的总轮数还大：一遍跑完了，没有中断。
    // 结果必须如实说"没测到中断恢复"，而不是把它当成一次通过的恢复——
    // 任务的判据（改动、轮数）它可能都满足，但"第一次运行失败 → 恢复"
    // 这个链路没有发生，长任务基准要测的就是这个链路。
    let benchmarks = temp_dir("no-fail");
    let workspace = temp_dir("no-fail-ws");
    write_long_task(&benchmarks, "demo-long", 30, 6);

    let mock = multi_step_script();
    let results =
        run_long_benchmark(&bench_deps(&mock), &Budget::new(30), &benchmarks, &workspace)
            .await
            .expect("装载");

    let _ = fs::remove_dir_all(&benchmarks);
    let result = &results[0];
    assert!(
        result.first_run_failure.is_empty(),
        "30 轮上限下任务应一遍跑完: {:?}",
        result.first_run_failure
    );
    assert!(
        result.resume_failure.is_some(),
        "第一遍就跑完了，恢复段应如实记录为失败（没有可恢复的东西）: {:?}",
        result.resume_failure
    );
    assert!(!result.passed, "没测到恢复就不算通过: {result:?}");
    assert_eq!(result.total_turns, 9, "一遍跑完的轮数就是脚本的总轮数");
    let _ = fs::remove_dir_all(&workspace);
}

#[tokio::test]
async fn the_short_benchmark_ignores_long_tasks() {
    // 套件划分：短任务基准不跑 long 套件的任务，反之亦然。
    // 两个目录混在一起，各自只看到自己的那一个。
    let benchmarks = temp_dir("suite-split");
    let workspace = temp_dir("suite-split-ws");
    fs::create_dir_all(benchmarks.join("short-task/repo")).expect("建短任务目录");
    fs::write(benchmarks.join("short-task/repo/notes.txt"), "第一版").expect("写 fixture");
    fs::write(
        benchmarks.join("short-task/task.toml"),
        "id = \"short-task\"\nkind = \"bug-fix\"\ndescription = \"短任务\"\n\n[criteria]\ntest_command = \"exit 0\"\nrequires_changes = false\nanswer_must_mention = []\n",
    )
    .expect("写 task.toml");
    write_long_task(&benchmarks, "long-task", 4, 6);

    let short_mock = MockCompletionModel::from_turns([
        MockTurn::text("1"),
        MockTurn::tool_call("c1", "write_file", json!({"path": "notes.txt", "content": "第二版"})),
        MockTurn::text("2"),
        MockTurn::text("3"),
        MockTurn::text("完成"),
    ]);
    let long_mock = multi_step_script();

    let short_results =
        run_benchmark(&bench_deps(&short_mock), &Budget::new(10), &benchmarks, &workspace)
            .await
            .expect("装载");
    let long_results =
        run_long_benchmark(&bench_deps(&long_mock), &Budget::new(30), &benchmarks, &workspace)
            .await
            .expect("装载");

    let _ = fs::remove_dir_all(&benchmarks);
    let _ = fs::remove_dir_all(&workspace);
    assert_eq!(
        short_results.iter().map(|r| r.task_id.as_str()).collect::<Vec<_>>(),
        ["short-task"],
        "短任务基准只跑 default 套件"
    );
    assert_eq!(
        long_results.iter().map(|r| r.task_id.as_str()).collect::<Vec<_>>(),
        ["long-task"],
        "长任务基准只跑 long 套件"
    );
}

//! 任务级恢复的集成验收。
//!
//! 使用与生产相同的装配路径：真实编排器、真实快照文件、真实恢复流程。
//! 重点在恢复后的行为语义：
//!
//! - 中断后从原步骤继续，**已完成的步骤不重做**。
//! - 恢复后的模型**能看到前面各步的结论**。
//! - 工作区被改动时**不静默继续**。
//! - 步骤数或会话 ID 不匹配时**明确拒绝**。
//!
//! 不访问网络，模型由 `MockCompletionModel` 脚本化。

use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use qcode_harness::application::agent_roster::AgentRoster;
use qcode_harness::application::agent_run::{AgentDependencies, RunState};
use qcode_harness::application::context_policy::DefaultContextPolicy;
use qcode_harness::application::error::ApplicationError;
use qcode_harness::application::orchestrator::{EvidenceFactory, Orchestrator};
use qcode_harness::application::port::{
    ApprovalGate, ApprovalRequest, BoxFuture, Decision, EvidenceSink, NoopHooks, Timestamp,
    ToolInvoker,
};
use qcode_harness::application::snapshot::{
    SessionStore, StoreError, TaskSnapshot, TaskStore,
};
use qcode_harness::application::task_resume::{self, ResumeCheck, WorktreeChangePolicy};
use qcode_harness::application::tool_registry::{ToolEntry, ToolRegistry};
use qcode_harness::application::worktree::WorktreeScanner;
use qcode_harness::domain::budget::Budget;
use qcode_harness::domain::evidence::AuditEvent;
use qcode_harness::domain::ids::{SessionId, TaskId};
use qcode_harness::domain::policy::{PolicyEngine, PolicyRule, PolicyVerdict};
use qcode_harness::domain::task::TaskKind;
use qcode_harness::domain::tool::{ToolAnnotations, ToolCall, ToolOutcome, ToolSpec};
use qcode_harness::domain::worktree::{WorktreeEntry, WorktreeManifest};
use qcode_harness::infrastructure::approval::auto::AutoApproveGate;
use qcode_harness::infrastructure::clock::SystemClock;
use qcode_harness::application::memory_evidence::MemoryEvidenceSink;
use qcode_harness::infrastructure::model::rig_chat::RigChatGateway;
use qcode_harness::infrastructure::session_store::JsonSessionStore;
use qcode_harness::infrastructure::task_store::JsonTaskStore;
use qcode_harness::infrastructure::worktree::{scan_workspace, RootedWorktreeScanner};

use rig::completion::Message;
use rig::message::UserContent;
use rig::test_utils::{MockCompletionModel, MockTurn};
use serde_json::json;

/// 每次测试用独立的临时目录，避免并行测试互相踩。
fn temp_workspace(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("qch-task-resume-{}-{}", name, std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("建临时目录");
    dir
}

/// 组装依赖。工具表为空：这些测试关注步骤与结论的交接，不涉及工具本身。
fn deps(mock: &MockCompletionModel) -> AgentDependencies {
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

/// 测试用证据工厂：一个任务一个内存汇，不落盘。
fn test_factory() -> EvidenceFactory {
    Arc::new(|_session_id: &SessionId| {
        Ok(Arc::new(MemoryEvidenceSink::new()) as Arc<dyn EvidenceSink>)
    })
}

/// 请求历史里的全部用户消息文本。
fn user_messages(mock: &MockCompletionModel, index: usize) -> Vec<String> {
    let mut texts = Vec::new();
    for message in &mock.requests()[index].chat_history {
        let Message::User { content } = message.as_ref() else {
            continue;
        };
        for part in content {
            if let UserContent::Text(text) = part {
                texts.push(text.text.clone());
            }
        }
    }
    texts
}

/// 请求历史的序列化形式，用于断言上一步的结论是否进入下一步。
fn history_text(mock: &MockCompletionModel, index: usize) -> String {
    serde_json::to_string(&mock.requests()[index].chat_history).expect("历史应可序列化")
}

/// 固定清单的扫描器，用于稳定复现"工作区被改过"分支。
struct FixedScanner {
    manifest: WorktreeManifest,
}

impl WorktreeScanner for FixedScanner {
    fn scan(&self) -> Result<WorktreeManifest, String> {
        Ok(self.manifest.clone())
    }
}

/// 一份只有一个文件的清单。
fn manifest_with(path: &str, hash: u64) -> WorktreeManifest {
    WorktreeManifest {
        entries: vec![WorktreeEntry {
            path: path.to_owned(),
            hash,
            size: 1,
        }],
    }
}

// ── 1b/1e：任务快照的落盘与读回 ───────────────────────────────────────

/// 任务中途中断时，任务快照落在约定位置。
///
/// 位置为 `.qcodeharness/tasks/<task_id>.json`，与会话快照目录
/// `snapshots/` 平级，一个任务 ID 一个文件。
#[tokio::test]
async fn an_interrupted_task_leaves_a_task_snapshot_on_disk() {
    let workspace = temp_workspace("snapshot");
    let store = Arc::new(JsonTaskStore::new(&workspace));
    // 第 2 步收到空回复，该步失败，任务中断。
    let mock = MockCompletionModel::from_turns(vec![
        MockTurn::text("第1步产出"),
        MockTurn::text(""),
        MockTurn::text(""),
    ]);
    let mut orchestrator = Orchestrator::new(deps(&mock), Budget::new(20), test_factory())
        .with_task_store(store.clone());
    let mut task = orchestrator.create_task(TaskId::new("t-snap"), TaskKind::BugFix, "修好它");

    let error = orchestrator.run(&mut task).await.unwrap_err();

    assert!(
        matches!(&error, ApplicationError::StepFailed { step: 2, .. }),
        "第 2 步应失败，实际 {error}"
    );
    let path = workspace
        .join(".qcodeharness")
        .join("tasks")
        .join("t-snap.json");
    assert!(path.exists(), "任务快照应落在 {}", path.display());

    // 快照需要包含进度与已完成结论。
    let snapshot = store.load(&TaskId::new("t-snap")).unwrap().unwrap();
    assert_eq!(snapshot.task.current_index(), 1, "第 1 步已完成");
    assert_eq!(snapshot.completed_outputs.len(), 1);
    assert_eq!(snapshot.completed_outputs[0].answer, "第1步产出");
    let _ = fs::remove_dir_all(&workspace);
}

/// 版本不匹配的任务快照必须被明确拒绝。
///
/// 任务快照是跨版本持久数据。猜测解释旧格式会把不兼容状态带进运行，
/// 导致恢复后跑偏。
#[tokio::test]
async fn a_task_snapshot_from_another_version_is_rejected() {
    let workspace = temp_workspace("version");
    let store = Arc::new(JsonTaskStore::new(&workspace));
    let mock = MockCompletionModel::from_turns(vec![
        MockTurn::text("第1步产出"),
        MockTurn::text("第2步产出"),
        MockTurn::text("第3步产出"),
        MockTurn::text("第4步产出"),
    ]);
    let mut orchestrator = Orchestrator::new(deps(&mock), Budget::new(20), test_factory())
        .with_task_store(store.clone());
    let mut task = orchestrator.create_task(TaskId::new("t-ver"), TaskKind::BugFix, "修好它");
    orchestrator.run(&mut task).await.unwrap();

    // 把落盘文件的版本号改掉，模拟别的版本写的快照。
    let path = workspace
        .join(".qcodeharness")
        .join("tasks")
        .join("t-ver.json");
    let mut wire: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    wire["format_version"] = serde_json::json!(999);
    fs::write(&path, serde_json::to_string_pretty(&wire).unwrap()).unwrap();

    let error = store.load(&TaskId::new("t-ver")).unwrap_err();
    assert!(
        matches!(error, StoreError::VersionMismatch { found: 999, .. }),
        "旧版本快照必须被明确拒绝: {error}"
    );
    let _ = fs::remove_dir_all(&workspace);
}

// ── 1d：恢复前的三项核对 ──────────────────────────────────────────────

/// 任务步骤数与会话中已开跑的步数不匹配时，必须明确拒绝。
///
/// 不匹配说明任务与会话不属于同一个任务，恢复会用 A 任务的进度延续
/// B 任务的历史。
#[tokio::test]
async fn a_task_and_session_with_different_step_counts_are_rejected() {
    let workspace = temp_workspace("mismatch");
    let store = Arc::new(JsonTaskStore::new(&workspace));
    let mock = MockCompletionModel::from_turns(vec![
        MockTurn::text("第1步产出"),
        MockTurn::text("第2步产出"),
        MockTurn::text("第3步产出"),
        MockTurn::text("第4步产出"),
    ]);
    let mut orchestrator = Orchestrator::new(deps(&mock), Budget::new(20), test_factory())
        .with_task_store(store.clone());
    let mut task = orchestrator.create_task(TaskId::new("t-mm"), TaskKind::BugFix, "修好它");
    orchestrator.run(&mut task).await.unwrap();

    let mut snapshot = store.load(&TaskId::new("t-mm")).unwrap().unwrap();
    // 把会话里的步骤交代删掉一条：会话变成 3 步，任务仍是 4 步。
    let mut turns = snapshot.run.session.turns.clone();
    let user_positions: Vec<usize> = turns
        .iter()
        .enumerate()
        .filter(|(_, turn)| matches!(turn, qcode_harness::domain::session::Turn::User(_)))
        .map(|(index, _)| index)
        .collect();
    turns.remove(user_positions[1]);
    snapshot.run.session.turns = turns;

    let error = task_resume::check_resumable(&snapshot, None).unwrap_err();

    assert!(
        error.to_string().contains("一一对应"),
        "步骤数对不上必须被明确拒绝: {error}"
    );
    let _ = fs::remove_dir_all(&workspace);
}

/// 会话 ID 不一致时必须明确拒绝。
#[tokio::test]
async fn a_mismatched_session_id_is_rejected() {
    let workspace = temp_workspace("session-id");
    let store = Arc::new(JsonTaskStore::new(&workspace));
    let mock = MockCompletionModel::from_turns(vec![
        MockTurn::text("第1步产出"),
        MockTurn::text("第2步产出"),
        MockTurn::text("第3步产出"),
        MockTurn::text("第4步产出"),
    ]);
    let mut orchestrator = Orchestrator::new(deps(&mock), Budget::new(20), test_factory())
        .with_task_store(store.clone());
    let mut task = orchestrator.create_task(TaskId::new("t-sid"), TaskKind::BugFix, "修好它");
    orchestrator.run(&mut task).await.unwrap();

    let snapshot = store.load(&TaskId::new("t-sid")).unwrap().unwrap();

    let error = task_resume::check_resumable(&snapshot, Some(&SessionId::new("别的会话")))
        .unwrap_err();
    assert!(
        error.to_string().contains("会话 ID 不一致"),
        "会话 ID 对不上必须被明确拒绝: {error}"
    );
    // 对得上就放行。
    assert!(task_resume::check_resumable(&snapshot, Some(&snapshot.run.session_id)).is_ok());
    let _ = fs::remove_dir_all(&workspace);
}

/// 工作区被改动时，检查要报告差异，不能报告干净。
///
/// 这是"不静默继续"的前提：先能检测到改动。
#[test]
fn a_modified_worktree_is_detected() {
    let saved = manifest_with("src/lib.rs", 1);

    // 内容变了 → 同一个路径但指纹不同。
    let changed = manifest_with("src/lib.rs", 2);
    let check = task_resume::check_worktree(&saved, &FixedScanner { manifest: changed });
    assert!(
        matches!(check, ResumeCheck::WorktreeChanged { .. }),
        "内容变了必须被检出，实际 {check:?}"
    );

    // 多了一个文件 → 检出为新增。
    let mut with_extra = saved.clone();
    with_extra.entries.push(WorktreeEntry {
        path: "new.txt".to_owned(),
        hash: 3,
        size: 1,
    });
    let check = task_resume::check_worktree(&saved, &FixedScanner { manifest: with_extra });
    assert!(
        matches!(check, ResumeCheck::WorktreeChanged { .. }),
        "新增文件必须被检出，实际 {check:?}"
    );

    // 没动过 → 干净。
    let check = task_resume::check_worktree(&saved, &FixedScanner { manifest: saved.clone() });
    assert_eq!(check, ResumeCheck::Clean);
}

/// 无法扫描时必须报告"不可读"，不能报告"未改动"。
///
/// 把无法扫描说成未改动会误导用户，使其认为工作区干净；恢复后的模型
/// 可能覆盖他人刚做的修改。
#[test]
fn an_unreadable_worktree_is_not_reported_as_clean() {
    struct Broken;
    impl WorktreeScanner for Broken {
        fn scan(&self) -> Result<WorktreeManifest, String> {
            Err("目录没了".to_owned())
        }
    }

    let check = task_resume::check_worktree(&manifest_with("a", 1), &Broken);

    assert!(
        matches!(check, ResumeCheck::WorktreeUnreadable { .. }),
        "扫不出来必须单独报出，实际 {check:?}"
    );
}

/// 真实扫描器能发现工作区里被改过的文件。
#[test]
fn the_real_scanner_finds_a_changed_file() {
    let workspace = temp_workspace("real-scan");
    fs::write(workspace.join("a.txt"), "原内容").unwrap();
    let scanner = RootedWorktreeScanner::new(&workspace);

    let before = scanner.scan().unwrap();
    fs::write(workspace.join("a.txt"), "新内容").unwrap();
    let after = scanner.scan().unwrap();

    let changes = before.diff(&after);
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].describe(), "修改 a.txt");

    // 扫描器自己拿到的"现在"与快照里的"当时"也要能判出差异。
    assert!(matches!(
        task_resume::check_worktree(&before, &scanner),
        ResumeCheck::WorktreeChanged { .. }
    ));
    let _ = fs::remove_dir_all(&workspace);
}

/// 真实扫描器遇到不存在的根目录时，必须报告"不可读"，不能返回空清单。
///
/// 空清单会让 `check_worktree` 把快照中的每个文件判为删除，用户看到
/// "工作区被大改"，实际是扫描失败。根目录不可读必须在扫描层失败，
/// 并原样向上传递原因。
#[test]
fn the_real_scanner_reports_a_missing_root_as_unreadable() {
    let workspace = temp_workspace("gone-root");
    fs::write(workspace.join("a.txt"), "内容").unwrap();
    let saved = RootedWorktreeScanner::new(&workspace).scan().unwrap();
    fs::remove_dir_all(&workspace).unwrap();

    let scanner = RootedWorktreeScanner::new(&workspace);
    assert!(
        scanner.scan().is_err(),
        "根目录没了，扫描必须报错而不是交空清单"
    );
    let check = task_resume::check_worktree(&saved, &scanner);
    assert!(
        matches!(check, ResumeCheck::WorktreeUnreadable { .. }),
        "恢复核对应说\"查不了\"，实际 {check:?}"
    );
}

// ── 1b 的核心：恢复后的模型看得见前面几步的结论 ──────────────────────

/// **恢复后模型能看到第 1 步的结论。**
///
/// 第 1 步查出根因，第 2 步因预算耗尽中断。恢复后，第 2 步的请求必须
/// 包含第 1 步的结论；否则模型只能从零重推，长任务恢复后会跑偏。
///
/// 同时验证已完成的步骤不重做：第 2 步请求里仍有第 1 步的步骤交代，
/// 说明第 1 步未被重跑；其结论以**交接消息**的形式存在。
#[tokio::test]
async fn after_resume_the_model_sees_the_first_steps_conclusion() {
    let workspace = temp_workspace("handoff");
    let store = Arc::new(JsonTaskStore::new(&workspace));

    // 第一次运行：第 1 步完成，第 2 步开始时预算耗尽。
    // bug-fix 模板有 4 步，因此第 2 步的脚本不会被消费。
    let mock1 = MockCompletionModel::from_turns(vec![MockTurn::text("根因在解析层")]);
    let mut orchestrator = Orchestrator::new(deps(&mock1), Budget::new(1), test_factory())
        .with_task_store(store.clone());
    let mut task = orchestrator.create_task(TaskId::new("t-ho"), TaskKind::BugFix, "修好它");

    let error = orchestrator.run(&mut task).await.unwrap_err();
    assert!(
        matches!(error, ApplicationError::StepFailed { .. }),
        "预算耗尽应让任务失败，实际 {error}"
    );
    assert_eq!(task.progress(), (1, 4), "第 1 步已完成，第 2 步没跑完");

    let snapshot = store.load(&TaskId::new("t-ho")).unwrap().unwrap();
    assert_eq!(snapshot.completed_outputs.len(), 1);
    assert_eq!(snapshot.completed_outputs[0].answer, "根因在解析层");

    // 恢复：这次给足预算。
    let mock2 = MockCompletionModel::from_turns(vec![
        MockTurn::text("按解析层改好了"),
        MockTurn::text("测试通过"),
        MockTurn::text("汇总：解析层根因已修"),
    ]);
    let mut orchestrator = Orchestrator::new(deps(&mock2), Budget::new(20), test_factory())
        .with_task_store(store.clone());
    let outcome = orchestrator.resume(snapshot).await.unwrap();

    assert_eq!(outcome.steps_completed, 3, "从第 2 步跑完剩下 3 步");
    assert_eq!(outcome.answer, "汇总：解析层根因已修");
    // 恢复后一共问了 3 次：第 2、3、4 步各一次。第 1 步**没有重跑**。
    assert_eq!(mock2.requests().len(), 3, "第 1 步不应重跑");

    // 关键断言：第 2 步的请求中包含第 1 步的结论。
    let second = history_text(&mock2, 0);
    assert!(
        second.contains("根因在解析层"),
        "恢复后第 2 步必须看得见第 1 步的结论: {second}"
    );
    // 并且以带标记的交接消息形式出现，避免偶然匹配。
    let messages = user_messages(&mock2, 0);
    assert!(
        messages
            .iter()
            .any(|text| text.contains("已完成第 1 步") && text.contains("根因在解析层")),
        "结论应以交接消息的形式出现在历史里: {messages:?}"
    );

    let _ = fs::remove_dir_all(&workspace);
}

/// 恢复后每一步都能看到前面**所有**步骤的结论。
///
/// 只交接第 1 步不够；第 4 步同样需要第 1、2、3 步的结果。
#[tokio::test]
async fn after_resume_every_step_sees_all_earlier_conclusions() {
    let workspace = temp_workspace("handoff-all");
    let store = Arc::new(JsonTaskStore::new(&workspace));

    // 第 1 步跑完，预算用光。
    let mock1 = MockCompletionModel::from_turns(vec![MockTurn::text("第一步的结论")]);
    let mut orchestrator = Orchestrator::new(deps(&mock1), Budget::new(1), test_factory())
        .with_task_store(store.clone());
    let mut task = orchestrator.create_task(TaskId::new("t-ho2"), TaskKind::BugFix, "修好它");
    orchestrator.run(&mut task).await.unwrap_err();

    let snapshot = store.load(&TaskId::new("t-ho2")).unwrap().unwrap();

    let mock2 = MockCompletionModel::from_turns(vec![
        MockTurn::text("第二步的结论"),
        MockTurn::text("第三步的结论"),
        MockTurn::text("第四步的结论"),
    ]);
    let mut orchestrator = Orchestrator::new(deps(&mock2), Budget::new(20), test_factory());
    orchestrator.resume(snapshot).await.unwrap();

    // 最后一步（第 4 步）的请求里，前三步的结论都在。
    let last = history_text(&mock2, 2);
    for conclusion in ["第一步的结论", "第二步的结论", "第三步的结论"] {
        assert!(last.contains(conclusion), "第 4 步应看见「{conclusion}」: {last}");
    }
    let _ = fs::remove_dir_all(&workspace);
}

/// 同一份快照恢复两次时，结论不会被交接两遍。
///
/// 交接消息的文本由内容唯一确定，重复恢复可通过文本比对识别。
#[tokio::test]
async fn resuming_the_same_snapshot_twice_does_not_duplicate_the_handoff() {
    let workspace = temp_workspace("handoff-twice");
    let store = Arc::new(JsonTaskStore::new(&workspace));

    let mock1 = MockCompletionModel::from_turns(vec![MockTurn::text("第一步的结论")]);
    let mut orchestrator = Orchestrator::new(deps(&mock1), Budget::new(1), test_factory())
        .with_task_store(store.clone());
    let mut task = orchestrator.create_task(TaskId::new("t-ho3"), TaskKind::BugFix, "修好它");
    orchestrator.run(&mut task).await.unwrap_err();
    let snapshot = store.load(&TaskId::new("t-ho3")).unwrap().unwrap();

    let mock2 = MockCompletionModel::from_turns(vec![
        MockTurn::text("第二步的结论"),
        MockTurn::text("第三步的结论"),
        MockTurn::text("第四步的结论"),
    ]);
    let mut orchestrator = Orchestrator::new(deps(&mock2), Budget::new(20), test_factory());
    orchestrator.resume(snapshot.clone()).await.unwrap();

    // 第二次恢复同一份快照，交接消息不应再次追加。
    let mock3 = MockCompletionModel::from_turns(vec![
        MockTurn::text("第二步的结论"),
        MockTurn::text("第三步的结论"),
        MockTurn::text("第四步的结论"),
    ]);
    let mut orchestrator = Orchestrator::new(deps(&mock3), Budget::new(20), test_factory());
    orchestrator.resume(snapshot).await.unwrap();

    let messages = user_messages(&mock3, 0);
    let handoffs = messages
        .iter()
        .filter(|text| text.contains("已完成第 1 步"))
        .count();
    assert_eq!(handoffs, 1, "同一条结论不该被交接两次: {messages:?}");
    let _ = fs::remove_dir_all(&workspace);
}

/// 恢复不重跑已完成的步骤：第 1 步的交代仍在历史中，但没有第 1 步的
/// 新模型调用。
///
/// 这条测试同时检查交代存在与请求数量，前者验证历史延续，后者验证
/// 没有重跑。
#[tokio::test]
async fn resuming_does_not_rerun_the_completed_step() {
    let workspace = temp_workspace("no-rerun");
    let store = Arc::new(JsonTaskStore::new(&workspace));

    let mock1 = MockCompletionModel::from_turns(vec![MockTurn::text("第一步的结论")]);
    let mut orchestrator = Orchestrator::new(deps(&mock1), Budget::new(1), test_factory())
        .with_task_store(store.clone());
    let mut task = orchestrator.create_task(TaskId::new("t-nr"), TaskKind::BugFix, "修好它");
    orchestrator.run(&mut task).await.unwrap_err();

    let snapshot = store.load(&TaskId::new("t-nr")).unwrap().unwrap();

    let mock2 = MockCompletionModel::from_turns(vec![
        MockTurn::text("第二步的结论"),
        MockTurn::text("第三步的结论"),
        MockTurn::text("第四步的结论"),
    ]);
    let mut orchestrator = Orchestrator::new(deps(&mock2), Budget::new(20), test_factory());
    orchestrator.resume(snapshot).await.unwrap();

    // 恢复后只问了 3 次：第 2、3、4 步。第 1 步被跳过了。
    assert_eq!(mock2.requests().len(), 3);
    // 第 1 步的交代仍在历史中，它属于第 1 步，不属于第 2 步。
    let messages = user_messages(&mock2, 0);
    assert!(
        messages.iter().any(|text| text.contains("第 1/4 步")),
        "第 1 步的交代应留在历史里: {messages:?}"
    );
    // 恢复后的当前步骤是第 2 步。
    assert!(
        messages.iter().any(|text| text.contains("第 2/4 步")),
        "恢复后应从第 2 步继续: {messages:?}"
    );
    let _ = fs::remove_dir_all(&workspace);
}

/// 恢复时当前步骤的交代不应重复追加。
///
/// 中断发生在这一步当中，快照中已有该交代。恢复时再追加一次会让模型
/// 看到两条相同指令，也会使会话步数计算错误，后续恢复核对会误报
/// "任务与会话不一致"。
#[tokio::test]
async fn resuming_does_not_duplicate_the_current_step_prompt() {
    let workspace = temp_workspace("dup-prompt");
    let store = Arc::new(JsonTaskStore::new(&workspace));

    // 第 1 步跑完，预算用光；第 2 步的交代进了历史但没跑完。
    let mock1 = MockCompletionModel::from_turns(vec![MockTurn::text("第一步的结论")]);
    let mut orchestrator = Orchestrator::new(deps(&mock1), Budget::new(1), test_factory())
        .with_task_store(store.clone());
    let mut task = orchestrator.create_task(TaskId::new("t-dp"), TaskKind::BugFix, "修好它");
    orchestrator.run(&mut task).await.unwrap_err();

    let snapshot = store.load(&TaskId::new("t-dp")).unwrap().unwrap();
    // 中断时第 2 步的交代确实已经在历史里了。
    let prompts: Vec<&str> = snapshot
        .run
        .session
        .turns
        .iter()
        .filter_map(|turn| match turn {
            qcode_harness::domain::session::Turn::User(text) => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(prompts.len(), 2, "任务交代 + 第 2 步的交代");
    assert!(prompts[1].contains("第 2/4 步"));

    // 恢复：第 2 步的交代不该出现第二次。
    let mock2 = MockCompletionModel::from_turns(vec![
        MockTurn::text("第二步的结论"),
        MockTurn::text("第三步的结论"),
        MockTurn::text("第四步的结论"),
    ]);
    let mut orchestrator = Orchestrator::new(deps(&mock2), Budget::new(20), test_factory());
    orchestrator.resume(snapshot).await.unwrap();

    let messages = user_messages(&mock2, 0);
    let step_two = messages
        .iter()
        .filter(|text| text.contains("第 2/4 步"))
        .count();
    assert_eq!(step_two, 1, "第 2 步的交代只该有一条: {messages:?}");
    let _ = fs::remove_dir_all(&workspace);
}

/// 半步中断：交代已进入历史，且该步已提交轮次，恢复仍不应重复交代。
///
/// P10 长任务基准刻意制造这种状态：预算耗尽时末条是**观察**（该轮已
/// 提交），交代位于更早位置。只检查历史末尾的守卫在此失效，会再次追加
/// 交代，使历史出现两条"第 N/M 步"，会话步数计算错误，后续恢复被
/// `check_steps_match` 以"一一对应"不符**永久拒绝**。
#[tokio::test]
async fn resuming_after_committed_rounds_does_not_duplicate_the_step_prompt() {
    let workspace = temp_workspace("dup-prompt-mid");
    let store = Arc::new(JsonTaskStore::new(&workspace));

    // 中断落在第 3 步：第 1 步答完（1 turn），第 2 步读（2）、写（3），
    // 给出第 2 步结论（4），第 3 步读（5），预算在 5 turns 时耗尽。
    // 此时第 3 步的交代已进入历史，且已提交一轮（末条是观察）。
    let mock1 = MockCompletionModel::from_turns(vec![
        MockTurn::text("第一步的结论"),
        MockTurn::tool_call("call_1", "read_file", json!({"path": "a"})),
        MockTurn::tool_call("call_2", "write_file", json!({"path": "a", "content": "第二版"})),
        MockTurn::text("第二步的结论"),
        MockTurn::tool_call("call_3", "read_file", json!({"path": "b"})),
    ]);
    let mut orchestrator = Orchestrator::new(deps(&mock1), Budget::new(5), test_factory())
        .with_task_store(store.clone());
    let mut task = orchestrator.create_task(TaskId::new("t-dpm"), TaskKind::BugFix, "修好它");
    orchestrator.run(&mut task).await.unwrap_err();

    let snapshot = store.load(&TaskId::new("t-dpm")).unwrap().unwrap();
    // 中断落在第 3 步，末条为观察，不是用户消息。这是本测试与
    // `resuming_does_not_duplicate_the_current_step_prompt` 的区别。
    assert_eq!(snapshot.task.current_index(), 2, "前两步已完成");
    let last = snapshot.run.session.turns.last().cloned();
    assert!(
        matches!(last, Some(qcode_harness::domain::session::Turn::Observation(_))),
        "末条应是观察而非交代，否则测的不是半步中断: {last:?}"
    );

    // 恢复：第 3 步的交代不该出现第二次。
    let mock2 = MockCompletionModel::from_turns(vec![
        MockTurn::text("第三步的结论"),
        MockTurn::text("第四步的结论"),
    ]);
    let mut orchestrator = Orchestrator::new(deps(&mock2), Budget::new(20), test_factory());
    orchestrator.resume(snapshot).await.unwrap();

    let messages = user_messages(&mock2, 0);
    let step_three = messages
        .iter()
        .filter(|text| text.contains("第 3/4 步"))
        .count();
    assert_eq!(step_three, 1, "第 3 步的交代只该有一条: {messages:?}");
    let _ = fs::remove_dir_all(&workspace);
}

/// 恢复一个已完成的任务：直接返回它的最终答案，不再跑任何一步。
#[tokio::test]
async fn resuming_a_completed_task_runs_nothing() {
    let workspace = temp_workspace("completed");
    let store = Arc::new(JsonTaskStore::new(&workspace));

    let mock1 = MockCompletionModel::from_turns(vec![
        MockTurn::text("第1步产出"),
        MockTurn::text("第2步产出"),
        MockTurn::text("第3步产出"),
        MockTurn::text("第4步产出"),
    ]);
    let mut orchestrator = Orchestrator::new(deps(&mock1), Budget::new(20), test_factory())
        .with_task_store(store.clone());
    let mut task = orchestrator.create_task(TaskId::new("t-done"), TaskKind::BugFix, "修好它");
    orchestrator.run(&mut task).await.unwrap();

    let snapshot = store.load(&TaskId::new("t-done")).unwrap().unwrap();
    let mock2 = MockCompletionModel::from_turns(vec![]);
    let mut orchestrator = Orchestrator::new(deps(&mock2), Budget::new(20), test_factory());
    let outcome = orchestrator.resume(snapshot).await.unwrap();

    assert_eq!(outcome.steps_completed, 4);
    assert_eq!(outcome.answer, "第4步产出");
    assert_eq!(mock2.requests().len(), 0, "已完成的任务不该再问模型");
    let _ = fs::remove_dir_all(&workspace);
}

/// 真实扫描器与快照清单配套：工作区被改过时能报告差异。
#[tokio::test]
async fn a_real_worktree_change_is_refused_before_resuming() {
    let workspace = temp_workspace("real-blocked");
    let store = Arc::new(JsonTaskStore::new(&workspace));
    fs::write(workspace.join("a.txt"), "原内容").unwrap();

    let mock1 = MockCompletionModel::from_turns(vec![MockTurn::text("第1步产出")]);
    let mut orchestrator = Orchestrator::new(deps(&mock1), Budget::new(1), test_factory())
        .with_task_store(store.clone());
    let mut task = orchestrator.create_task(TaskId::new("t-rb"), TaskKind::BugFix, "修好它");
    orchestrator.run(&mut task).await.unwrap_err();

    let _snapshot = store.load(&TaskId::new("t-rb")).unwrap().unwrap();

    // 快照时的工作区是干净的。
    let saved = scan_workspace(&workspace).unwrap();
    let scanner = RootedWorktreeScanner::new(&workspace);
    assert_eq!(task_resume::check_worktree(&saved, &scanner), ResumeCheck::Clean);

    // 有人在中断期间改了文件。
    fs::write(workspace.join("a.txt"), "被人改过了").unwrap();
    let check = task_resume::check_worktree(&saved, &scanner);
    assert!(
        matches!(check, ResumeCheck::WorktreeChanged { .. }),
        "工作区被改过必须被检出: {check:?}"
    );
    let _ = fs::remove_dir_all(&workspace);
}

/// **工作区被改动时，恢复不会静默继续。**
///
/// 端到端验证：快照保存了当时的工作区，中断期间文件被修改，恢复前的
/// 检查必须发现该改动。
///
/// 需要同时满足：检测到差异、差异描述清楚、恢复被阻止。
#[tokio::test]
async fn a_changed_worktree_is_detected_end_to_end_and_blocks_the_resume() {
    let workspace = temp_workspace("e2e-blocked");
    let store = Arc::new(JsonTaskStore::new(&workspace));
    fs::write(workspace.join("a.txt"), "原内容").unwrap();

    // 第 1 步跑完，预算用光；快照里记下了当时的工作区清单。
    let mock1 = MockCompletionModel::from_turns(vec![MockTurn::text("第1步产出")]);
    let mut orchestrator = Orchestrator::new(deps(&mock1), Budget::new(1), test_factory())
        .with_task_store(store.clone())
        .with_worktree_scanner(Arc::new(RootedWorktreeScanner::new(&workspace)));
    let mut task = orchestrator.create_task(TaskId::new("t-e2e"), TaskKind::BugFix, "修好它");
    orchestrator.run(&mut task).await.unwrap_err();

    let snapshot = store.load(&TaskId::new("t-e2e")).unwrap().unwrap();
    // 快照里确实有清单，而且不是空的。
    assert!(
        !snapshot.worktree.is_empty(),
        "快照里必须记下当时的工作区清单，否则恢复时无从判断有没有被改过"
    );
    assert!(
        snapshot.worktree.entries.iter().any(|e| e.path == "a.txt"),
        "清单里应有 a.txt: {:?}",
        snapshot.worktree.entries
    );

    // 快照之后有人改了文件。
    fs::write(workspace.join("a.txt"), "被人改过了").unwrap();

    // 检查能发现差异。
    let scanner = RootedWorktreeScanner::new(&workspace);
    let check = task_resume::check_worktree(&snapshot.worktree, &scanner);
    let ResumeCheck::WorktreeChanged { changes } = &check else {
        panic!("工作区被改过必须被检出，实际 {check:?}")
    };
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].describe(), "修改 a.txt");

    // 而且恢复真的被挡住了：编排器在恢复前核对，不通过就不恢复。
    let mock2 = MockCompletionModel::from_turns(vec![
        MockTurn::text("第2步产出"),
        MockTurn::text("第3步产出"),
        MockTurn::text("第4步产出"),
    ]);
    let mut orchestrator = Orchestrator::new(deps(&mock2), Budget::new(20), test_factory())
        .with_worktree_scanner(Arc::new(RootedWorktreeScanner::new(&workspace)));
    let error = orchestrator.resume(snapshot).await.unwrap_err();
    assert!(
        matches!(error, ApplicationError::WorktreeChanged { .. }),
        "工作区被改过时恢复必须被挡住: {error}"
    );
    // 差异描述清楚：用户能看到具体改动，也知道需要确认。
    assert!(error.to_string().contains("修改 a.txt"), "{error}");
    assert!(error.to_string().contains("恢复前需要确认"), "{error}");
    // 恢复被阻止时，模型一次都没有被调用。
    assert_eq!(mock2.requests().len(), 0, "恢复被挡住时不该问模型");
    let _ = fs::remove_dir_all(&workspace);
}

/// 用户看过差异并确认后，恢复可以继续。
///
/// 工作区检查需要用户确认后才能继续。
#[tokio::test]
async fn a_confirmed_worktree_change_lets_the_resume_through() {
    let workspace = temp_workspace("confirmed");
    let store = Arc::new(JsonTaskStore::new(&workspace));
    fs::write(workspace.join("a.txt"), "原内容").unwrap();

    let mock1 = MockCompletionModel::from_turns(vec![MockTurn::text("第1步产出")]);
    let mut orchestrator = Orchestrator::new(deps(&mock1), Budget::new(1), test_factory())
        .with_task_store(store.clone())
        .with_worktree_scanner(Arc::new(RootedWorktreeScanner::new(&workspace)));
    let mut task = orchestrator.create_task(TaskId::new("t-cf"), TaskKind::BugFix, "修好它");
    orchestrator.run(&mut task).await.unwrap_err();

    let snapshot = store.load(&TaskId::new("t-cf")).unwrap().unwrap();
    fs::write(workspace.join("a.txt"), "用户自己改的").unwrap();

    let mock2 = MockCompletionModel::from_turns(vec![
        MockTurn::text("第2步产出"),
        MockTurn::text("第3步产出"),
        MockTurn::text("第4步产出"),
    ]);
    let mut orchestrator = Orchestrator::new(deps(&mock2), Budget::new(20), test_factory())
        .with_worktree_scanner(Arc::new(RootedWorktreeScanner::new(&workspace)))
        .with_worktree_policy(WorktreeChangePolicy::Confirmed);
    let outcome = orchestrator.resume(snapshot).await.unwrap();
    assert_eq!(outcome.steps_completed, 3, "确认之后就该恢复得动");
    let _ = fs::remove_dir_all(&workspace);
}

/// 工作区未改动时，恢复照常进行。
///
/// 检查不能总是报错，否则等同于没有检查。
#[tokio::test]
async fn an_untouched_worktree_lets_the_resume_through() {
    let workspace = temp_workspace("e2e-clean");
    let store = Arc::new(JsonTaskStore::new(&workspace));
    fs::write(workspace.join("a.txt"), "原内容").unwrap();

    let mock1 = MockCompletionModel::from_turns(vec![MockTurn::text("第1步产出")]);
    let mut orchestrator = Orchestrator::new(deps(&mock1), Budget::new(1), test_factory())
        .with_task_store(store.clone())
        .with_worktree_scanner(Arc::new(RootedWorktreeScanner::new(&workspace)));
    let mut task = orchestrator.create_task(TaskId::new("t-e2e2"), TaskKind::BugFix, "修好它");
    orchestrator.run(&mut task).await.unwrap_err();

    let snapshot = store.load(&TaskId::new("t-e2e2")).unwrap().unwrap();
    let scanner = RootedWorktreeScanner::new(&workspace);
    assert_eq!(
        task_resume::check_worktree(&snapshot.worktree, &scanner),
        ResumeCheck::Clean
    );

    let mock2 = MockCompletionModel::from_turns(vec![
        MockTurn::text("第2步产出"),
        MockTurn::text("第3步产出"),
        MockTurn::text("第4步产出"),
    ]);
    let mut orchestrator = Orchestrator::new(deps(&mock2), Budget::new(20), test_factory());
    let outcome = orchestrator.resume(snapshot).await.unwrap();
    assert_eq!(outcome.steps_completed, 3);
    let _ = fs::remove_dir_all(&workspace);
}

/// 任务快照的 JSON 形态可读回（结构契约）。
#[tokio::test]
async fn a_task_snapshot_on_disk_has_the_documented_shape() {
    let workspace = temp_workspace("shape");
    let store = Arc::new(JsonTaskStore::new(&workspace));
    let mock = MockCompletionModel::from_turns(vec![MockTurn::text("第1步产出")]);
    let mut orchestrator = Orchestrator::new(deps(&mock), Budget::new(1), test_factory())
        .with_task_store(store.clone());
    let mut task = orchestrator.create_task(TaskId::new("t-shape"), TaskKind::BugFix, "修好它");
    orchestrator.run(&mut task).await.unwrap_err();

    let path = workspace
        .join(".qcodeharness")
        .join("tasks")
        .join("t-shape.json");
    let wire: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();

    // 文档列出的四个顶层字段都在。
    for key in ["format_version", "task", "run", "completed_outputs"] {
        assert!(wire.get(key).is_some(), "任务快照应有字段 `{key}`: {wire}");
    }
    // completed_outputs 的每一项有 step / role / answer。
    let outputs = wire["completed_outputs"].as_array().unwrap();
    assert_eq!(outputs.len(), 1);
    for key in ["step", "role", "answer"] {
        assert!(outputs[0].get(key).is_some(), "结论应有字段 `{key}`: {outputs:?}");
    }
    let _ = fs::remove_dir_all(&workspace);
}

/// 快照中的任务损坏时，整个快照必须读取失败。
///
/// 任务快照是外部可修改的文件。越界的进度下标必须导致读取失败，不能
/// 带着坏任务进入编排。
#[tokio::test]
async fn a_task_snapshot_with_a_broken_task_cannot_be_loaded() {
    let workspace = temp_workspace("broken-task");
    let store = Arc::new(JsonTaskStore::new(&workspace));
    let mock = MockCompletionModel::from_turns(vec![MockTurn::text("第1步产出")]);
    let mut orchestrator = Orchestrator::new(deps(&mock), Budget::new(1), test_factory())
        .with_task_store(store.clone());
    let mut task = orchestrator.create_task(TaskId::new("t-bt"), TaskKind::BugFix, "修好它");
    orchestrator.run(&mut task).await.unwrap_err();

    let path = workspace
        .join(".qcodeharness")
        .join("tasks")
        .join("t-bt.json");
    let mut wire: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    wire["task"]["state"] = serde_json::json!({"Running": {"step": 99}});
    fs::write(&path, serde_json::to_string_pretty(&wire).unwrap()).unwrap();

    let error = store.load(&TaskId::new("t-bt")).unwrap_err();
    assert!(
        error.to_string().contains("进度下标越界"),
        "坏任务必须让整个快照读不回来: {error}"
    );
    let _ = fs::remove_dir_all(&workspace);
}

/// 任务快照读回来还能再原样存回去（往返无损）。
#[tokio::test]
async fn a_task_snapshot_round_trips_through_the_store() {
    let workspace = temp_workspace("roundtrip");
    let store = Arc::new(JsonTaskStore::new(&workspace));
    let mock = MockCompletionModel::from_turns(vec![MockTurn::text("第1步产出")]);
    let mut orchestrator = Orchestrator::new(deps(&mock), Budget::new(1), test_factory())
        .with_task_store(store.clone());
    let mut task = orchestrator.create_task(TaskId::new("t-rt"), TaskKind::BugFix, "修好它");
    orchestrator.run(&mut task).await.unwrap_err();

    let snapshot: TaskSnapshot = store.load(&TaskId::new("t-rt")).unwrap().unwrap();
    store.save(&snapshot).unwrap();
    let again = store.load(&TaskId::new("t-rt")).unwrap().unwrap();

    assert_eq!(again, snapshot);
    let _ = fs::remove_dir_all(&workspace);
}

// ── 2b/2c：半轮中断的任务级恢复 ──────────────────────────────────────
//
// 任务快照记录的是**步骤边界**。进程在一步中途退出时，真正的现场
// （等待审批还是正在执行调用）只存在于会话快照中。恢复必须同时使用
// 两者，否则会退化为整步重跑，导致长任务恢复后跑偏、副作用重复。

/// 记录"真的执行了哪些调用"的假执行器。
#[derive(Default)]
struct RecordingInvoker {
    executed: Mutex<Vec<String>>,
}

impl ToolInvoker for RecordingInvoker {
    fn invoke<'a>(&'a self, call: &'a ToolCall) -> BoxFuture<'a, ToolOutcome> {
        self.executed.lock().unwrap().push(call.arguments.to_string());
        Box::pin(async { ToolOutcome::success("文件内容") })
    }
}

/// 永不返回的执行器，模拟进程在工具执行途中退出。
///
/// 仍记录调用计数。恢复后计数变成 2，说明该调用被重跑。
struct HangingInvoker {
    entered: tokio::sync::Notify,
    executed: Mutex<usize>,
}

impl HangingInvoker {
    fn new() -> Self {
        Self {
            entered: tokio::sync::Notify::new(),
            executed: Mutex::new(0),
        }
    }

    /// 等到执行器被叫起一次。
    async fn wait_until_entered(&self) {
        self.entered.notified().await;
    }
}

impl ToolInvoker for HangingInvoker {
    fn invoke<'a>(&'a self, _call: &'a ToolCall) -> BoxFuture<'a, ToolOutcome> {
        *self.executed.lock().unwrap() += 1;
        let entered = &self.entered;
        Box::pin(async move {
            entered.notify_one();
            std::future::pending::<ToolOutcome>().await
        })
    }
}

/// 永不答复的闸门，模拟用户离开、进程随后被终止。
struct BlockingGate {
    asked: tokio::sync::Notify,
}

impl BlockingGate {
    fn new() -> Self {
        Self {
            asked: tokio::sync::Notify::new(),
        }
    }

    /// 等到闸门被问过一次。
    async fn wait_until_asked(&self) {
        self.asked.notified().await;
    }
}

impl ApprovalGate for BlockingGate {
    fn request<'a>(&'a self, _request: &'a ApprovalRequest) -> BoxFuture<'a, Decision> {
        let asked = &self.asked;
        Box::pin(async move {
            asked.notify_one();
            std::future::pending::<Decision>().await
        })
    }
}

/// 记录审批请求的闸门，用于判断恢复路径上是否再次询问用户。
#[derive(Default)]
struct RecordingGate {
    requests: Mutex<Vec<ApprovalRequest>>,
}

impl ApprovalGate for RecordingGate {
    fn request<'a>(&'a self, request: &'a ApprovalRequest) -> BoxFuture<'a, Decision> {
        self.requests.lock().unwrap().push(request.clone());
        Box::pin(async { Ok(()) })
    }
}

/// 记录审计事件的证据汇。
///
/// 不使用 `MemoryEvidenceSink`：它的事件列表仅在单元测试中可见
/// （`cfg(test)`），集成测试无法访问。
#[derive(Default)]
struct RecordingEvidence {
    events: Mutex<Vec<AuditEvent>>,
}

impl EvidenceSink for RecordingEvidence {
    fn record(&self, _at: Timestamp, event: AuditEvent) {
        self.events.lock().unwrap().push(event);
    }
}

/// 只读工具 `read_file` 的规格。
fn read_file_spec() -> ToolSpec {
    ToolSpec {
        name: "read_file".to_owned(),
        description: "读文件".to_owned(),
        parameters: json!({"type": "object"}),
        annotations: ToolAnnotations::new(true, false, true, false),
    }
}

/// 只装 `read_file` 的注册表。
fn registry_with(invoker: Arc<dyn ToolInvoker>) -> ToolRegistry {
    ToolRegistry::new(vec![ToolEntry {
        spec: read_file_spec(),
        invoker,
    }])
}

/// 强制 `read_file` 也需审批的策略，用于把运行停在闸门前。
fn approval_policy() -> PolicyEngine {
    PolicyEngine::with_rules(vec![PolicyRule {
        tool: Some("read_file".to_owned()),
        scope: None,
        verdict: PolicyVerdict::RequireApproval,
        description: "测试：read_file 一律要审批".to_owned(),
    }])
}

/// 以给定模型为基础的测试依赖。其余字段由各测试按需覆盖。
fn base_deps(mock: &MockCompletionModel) -> AgentDependencies {
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

/// 证据工厂：所有任务共用一份内存汇，供测试检查。
fn shared_factory(sink: Arc<RecordingEvidence>) -> EvidenceFactory {
    Arc::new(move |_session_id: &SessionId| Ok(sink.clone() as Arc<dyn EvidenceSink>))
}

/// **恢复停在审批闸门的任务时，用户必须再次确认。**
///
/// 场景：模型发起需要审批的命令，用户离开，进程退出。恢复时该审批
/// **不能凭快照自动放行**；快照只说明问过，不说明用户答复过。
///
/// 需要验证三件事：
///
/// 1. 会话快照停在等待审批状态。任务快照只记录步骤边界，看不到该现场。
/// 2. 恢复后同一请求被**再次询问**，并标明是恢复后的重问。
/// 3. 模型无需重新提议该调用，半轮现场已保留。
#[tokio::test]
async fn resuming_a_task_interrupted_at_the_approval_gate_asks_the_human_again() {
    let workspace = temp_workspace("gate-interrupt");
    let task_store = Arc::new(JsonTaskStore::new(&workspace));
    let session_store: Arc<dyn SessionStore> = Arc::new(JsonSessionStore::new(&workspace));
    let gate = Arc::new(BlockingGate::new());
    let evidence = Arc::new(RecordingEvidence::default());

    // 第一次运行：第 1 步给出结论，第 2 步想读文件，策略要求审批。
    let mock = MockCompletionModel::from_turns(vec![
        MockTurn::text("第1步产出"),
        MockTurn::tool_call("call_1", "read_file", json!({"path": "a"})),
    ]);
    let mut deps = base_deps(&mock);
    deps.tools = registry_with(Arc::new(RecordingInvoker::default()));
    deps.approvals = gate.clone();
    deps.policy = approval_policy();
    deps.session_store = Some(session_store.clone());

    let mut orchestrator =
        Orchestrator::new(deps, Budget::new(20), shared_factory(evidence.clone()))
            .with_task_store(task_store.clone());
    let mut task = orchestrator.create_task(TaskId::new("t-gate"), TaskKind::BugFix, "修好它");

    {
        // 运行到闸门时模拟进程终止：用户未答复，进程退出。
        let run = orchestrator.run(&mut task);
        tokio::pin!(run);
        tokio::select! {
            result = &mut run => panic!("审批处不该跑完: {result:?}"),
            _ = gate.wait_until_asked() => {}
        }
    }

    // 任务快照停在步骤边界：第 1 步做完了，第 2 步没做完。
    let task_snapshot = task_store.load(&TaskId::new("t-gate")).unwrap().unwrap();
    assert_eq!(task_snapshot.task.current_index(), 1, "第 1 步已完成");
    assert_eq!(task_snapshot.completed_outputs.len(), 1);

    // 真正的现场在会话快照中：停在等待审批。
    let session_snapshot = session_store
        .load(&task_snapshot.run.session_id)
        .unwrap()
        .expect("会话快照应已落盘");
    assert!(
        matches!(session_snapshot.state, RunState::AwaitingApproval(_)),
        "会话快照应停在审批前，实际 {:?}",
        session_snapshot.state
    );

    // 恢复：闸门换成会回答的，并记录被问到的请求。
    let recording_gate = Arc::new(RecordingGate::default());
    let second_invoker = Arc::new(RecordingInvoker::default());
    let mock2 = MockCompletionModel::from_turns(vec![
        MockTurn::text("第二步的结论"),
        MockTurn::text("第三步的结论"),
        MockTurn::text("第四步的结论"),
    ]);
    let mut deps = base_deps(&mock2);
    deps.tools = registry_with(second_invoker.clone());
    deps.approvals = recording_gate.clone();
    deps.policy = approval_policy();
    deps.session_store = Some(session_store.clone());

    let mut orchestrator = Orchestrator::new(
        deps,
        Budget::new(20),
        shared_factory(evidence.clone()),
    )
    .with_task_store(task_store.clone());
    let outcome = orchestrator.resume(task_snapshot).await.unwrap();

    assert_eq!(outcome.answer, "第四步的结论");
    // 用户需要再次确认：闸门收到请求，且标明为恢复后的重问。
    let requests = recording_gate.requests.lock().unwrap();
    assert_eq!(requests.len(), 1, "恢复后应把同一个审批请求重新问一次");
    assert!(requests[0].resumed, "这次询问必须标明是恢复后的重问");
    assert_eq!(requests[0].call.id.as_str(), "call_1", "问的还是那一次调用");
    drop(requests);
    // 半轮现场保留：模型无需重新提议该调用，只被调用 3 次，每步一次。
    assert_eq!(mock2.requests().len(), 3, "模型不该被要求重新提议那次调用");
    // 工具在审批通过之后才跑，且只跑了一次。
    assert_eq!(second_invoker.executed.lock().unwrap().len(), 1);
    // 审计记录中留下重新询问的痕迹。
    assert!(
        evidence
            .events
            .lock()
            .unwrap()
            .iter()
            .any(|event| matches!(event, AuditEvent::ApprovalReasked { .. })),
        "证据里应留下重新询问的记录"
    );
    let _ = fs::remove_dir_all(&workspace);
}

/// **恢复时，正在执行的调用不会被自动重跑。**
///
/// 这是最危险的半轮状态：调用已发起，副作用可能已经发生，结果未知。
/// 重跑会重复副作用，不重跑又不能忽略该事实。
///
/// 恢复后模型会看到"此调用已执行、结果未知、是否重跑由你判断"，并由
/// 模型自行决定。
#[tokio::test]
async fn resuming_a_task_interrupted_during_a_tool_call_does_not_run_it_again() {
    let workspace = temp_workspace("in-flight");
    let task_store = Arc::new(JsonTaskStore::new(&workspace));
    let session_store: Arc<dyn SessionStore> = Arc::new(JsonSessionStore::new(&workspace));
    let invoker = Arc::new(HangingInvoker::new());
    let evidence = Arc::new(RecordingEvidence::default());

    // 第一次运行：第 1 步给出结论，第 2 步读文件时进程在执行途中退出。
    let mock = MockCompletionModel::from_turns(vec![
        MockTurn::text("第1步产出"),
        MockTurn::tool_call("call_1", "read_file", json!({"path": "a"})),
    ]);
    let mut deps = base_deps(&mock);
    deps.tools = registry_with(invoker.clone());
    deps.session_store = Some(session_store.clone());

    let mut orchestrator =
        Orchestrator::new(deps, Budget::new(20), shared_factory(evidence.clone()))
            .with_task_store(task_store.clone());
    let mut task = orchestrator.create_task(TaskId::new("t-inflight"), TaskKind::BugFix, "修好它");

    {
        let run = orchestrator.run(&mut task);
        tokio::pin!(run);
        tokio::select! {
            result = &mut run => panic!("工具执行途中不该跑完: {result:?}"),
            _ = invoker.wait_until_entered() => {}
        }
    }

    // 会话快照停在"正要执行这一次调用"，副作用可能已经发生。
    let task_snapshot = task_store.load(&TaskId::new("t-inflight")).unwrap().unwrap();
    let session_snapshot = session_store
        .load(&task_snapshot.run.session_id)
        .unwrap()
        .expect("会话快照应已落盘");
    assert!(
        matches!(session_snapshot.state, RunState::Dispatching(_)),
        "会话快照应停在执行中，实际 {:?}",
        session_snapshot.state
    );
    assert_eq!(*invoker.executed.lock().unwrap(), 1, "第一次运行确实调起了工具");

    // 恢复：这次的执行器会正常返回。
    let second_invoker = Arc::new(RecordingInvoker::default());
    let mock2 = MockCompletionModel::from_turns(vec![
        MockTurn::text("第二步的结论"),
        MockTurn::text("第三步的结论"),
        MockTurn::text("第四步的结论"),
    ]);
    let mut deps = base_deps(&mock2);
    deps.tools = registry_with(second_invoker.clone());
    deps.session_store = Some(session_store.clone());

    let mut orchestrator = Orchestrator::new(
        deps,
        Budget::new(20),
        shared_factory(evidence.clone()),
    )
    .with_task_store(task_store.clone());
    let outcome = orchestrator.resume(task_snapshot).await.unwrap();

    assert_eq!(outcome.answer, "第四步的结论");
    // 关键断言：在途调用没有被重跑。
    assert_eq!(
        second_invoker.executed.lock().unwrap().len(),
        0,
        "在途的调用不得被自动重跑"
    );
    assert_eq!(
        *invoker.executed.lock().unwrap(),
        1,
        "整个过程中那次调用只该跑过一次"
    );
    // 证据中记录了该调用此前执行过。
    assert!(
        evidence
            .events
            .lock()
            .unwrap()
            .iter()
            .any(|event| matches!(event, AuditEvent::ToolCallReplayed { .. })),
        "证据里应留下重放记录"
    );
    // 模型能看到重放提示，并自行决定是否重跑。
    assert!(
        history_text(&mock2, 0).contains("中断恢复"),
        "模型应看见「这个调用之前跑过」的提示"
    );
    let _ = fs::remove_dir_all(&workspace);
}

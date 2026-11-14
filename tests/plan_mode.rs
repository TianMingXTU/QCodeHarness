//! Plan mode 的行为验收。
//!
//! 用户可见行为：规划项绑定任务快照的步骤下标；恢复只从第一个未完成项
//! 继续，已完成步骤不重跑。测试不依赖真实模型，也不依赖 R1 的 Todo 类型，
//! 只使用任务快照的序列化往返与应用层纯编排入口。

use std::fs;
use std::path::PathBuf;

use qcode_harness::application::agent_run::{PendingRound, RunState};
use qcode_harness::application::plan_mode::{PlanMode, PlanStepStatus, StepExecutor};
use qcode_harness::application::snapshot::{RUN_FORMAT, TASK_FORMAT, TaskSnapshot};
use qcode_harness::domain::agent::{AgentRole, AgentSpec, AgentTurn};
use qcode_harness::domain::budget::Consumption;
use qcode_harness::domain::ids::{SessionId, TaskId};
use qcode_harness::domain::session::{Session, Turn};
use qcode_harness::domain::task::{Task, TaskFailure, TaskKind, TaskState, TaskStep};

/// 每次测试用独立的临时目录，避免并行运行互相踩。
fn temp_workspace(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("qch-plan-mode-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("建临时目录");
    dir
}

/// 三条已写进任务步骤的计划项；中间一项已完成。
fn plan() -> PlanMode {
    PlanMode::from_todo_items(vec![
        ("todo-1", "读代码", "Pending"),
        ("todo-2", "改代码", "Completed"),
        ("todo-3", "跑测试", "InProgress"),
    ])
    .expect("计划项合法")
}

/// 一份停在第 2 步、且持久化过的任务快照。
fn snapshot(step: usize) -> TaskSnapshot {
    let steps = ["读代码", "改代码", "跑测试"]
        .iter()
        .map(|description| TaskStep {
            description: (*description).to_owned(),
            role: AgentRole::Coder,
            done_when: "有产出".to_owned(),
        })
        .collect();
    let mut task = Task::new(
        TaskId::new("task-plan"),
        TaskKind::Feature,
        "完成 R4",
        steps,
    );
    if step < 3 {
        // 序列化边界只接受构造路径能产生的状态，这里用领域方法推进。
        task.start().expect("首次启动");
        for _ in 0..step {
            task.advance().expect("推进步骤");
        }
    } else {
        task.start().expect("首次启动");
        task.advance().expect("推进步骤");
        task.advance().expect("推进步骤");
        task.advance().expect("全部完成");
    }
    let mut session = Session::new(SessionId::new("session-plan"), "完成 R4");
    for index in 0..=step.min(2) {
        session.push_user(format!("第 {} 步的交代", index + 1));
        session
            .commit_round(
                AgentTurn {
                    text: String::new(),
                    calls: Vec::new(),
                },
                Vec::new(),
            )
            .expect("空调用轮次可提交");
    }
    TaskSnapshot {
        format_version: TASK_FORMAT,
        task,
        run: qcode_harness::application::snapshot::RunSnapshot {
            format_version: RUN_FORMAT,
            session_id: SessionId::new("session-plan"),
            spec: AgentSpec::new(AgentRole::Planner, "规划者"),
            session: session.snapshot(),
            state: RunState::Completed("规划完毕".to_owned()),
            consumption: Consumption::default(),
            pending: None::<PendingRound>,
        },
        completed_outputs: Vec::new(),
        worktree: Default::default(),
    }
}

/// 计划项与快照步骤按下标绑定；重复恢复不会重跑已完成项。
#[test]
fn plan_items_bind_to_snapshot_steps_and_resume_skips_completed_work() {
    let plan = plan();
    assert_eq!(plan.steps()[0].snapshot_step_index, 0);
    assert_eq!(plan.steps()[1].snapshot_step_index, 1);
    assert_eq!(plan.steps()[2].snapshot_step_index, 2);
    assert_eq!(plan.steps()[1].status, PlanStepStatus::Completed);

    let boundary = plan.resume_boundary().expect("存在未完成步骤");
    assert_eq!(boundary.next_step_index, 0, "第一项 Pending 是计划边界");
    assert_eq!(boundary.completed_step_indexes, vec![1]);

    let mut executor = StepExecutor::default();
    let (executed, outcome) = executor.execute_from(&boundary, &[0, 1, 2]);
    assert_eq!(executed, vec![0, 2], "Completed 的步骤不重跑");
    assert_eq!(outcome.completed_step_indexes, vec![0, 2]);
}

/// 计划阶段中断后，落盘再恢复仍停在同一边界，不会从已完成步骤重新开始。
#[test]
fn a_serialized_plan_boundary_stays_at_the_same_step_after_resume() {
    let workspace = temp_workspace("boundary");
    let path = workspace.join("task-plan.json");
    let saved = snapshot(2);
    fs::write(&path, serde_json::to_string(&saved).expect("快照可序列化")).expect("写快照");
    let loaded: TaskSnapshot =
        serde_json::from_str(&fs::read_to_string(&path).expect("读快照")).expect("快照可往返");
    assert_eq!(loaded.task.current_index(), 2);

    let plan = plan().with_completed_prefix(2);
    let boundary = plan.resume_boundary().expect("仍有未完成项");
    assert_eq!(boundary.next_step_index, 2, "恢复停在计划边界");
    assert_eq!(boundary.completed_step_indexes, vec![0, 1]);

    let mut executor = StepExecutor::default();
    let (first, _) = executor.execute_from(&boundary, &[0, 1, 2]);
    let (second, _) = executor.execute_from(&boundary, &[0, 1, 2]);
    assert_eq!(first, vec![2], "只跑第 3 步");
    assert_eq!(second, Vec::<usize>::new(), "重复恢复不重跑已执行步骤");

    let _ = fs::remove_dir_all(&workspace);
}

/// 计划项数或下标与任务快照步骤不一致时拒绝恢复，避免把 A 计划接到 B 任务。
#[test]
fn a_plan_with_mismatched_snapshot_steps_is_rejected() {
    let mismatched = PlanMode::from_todo_items(vec![("todo-1", "读代码", "Pending")])
        .expect("计划项合法")
        .with_binding_count(3);
    let error = mismatched.verify(&snapshot(1)).expect_err("必须拒绝");
    assert!(
        error.contains("计划项") && error.contains("步骤"),
        "错误信息应说明冲突: {error}"
    );
}

/// 计划项状态非法时不能构造计划：静默修正会让边界落在错误步骤上。
#[test]
fn an_unknown_todo_status_is_rejected() {
    let error = PlanMode::from_todo_items(vec![("todo-1", "读代码", "Later")])
        .expect_err("未知状态不能通过");
    assert!(error.contains("Later"), "错误要带出原始状态: {error}");
}

/// 恢复时识别任务快照的失败详情，不把已失败的步骤当作全新计划重跑。
#[test]
fn a_failed_task_does_not_move_the_plan_boundary_backwards() {
    let mut failed = snapshot(2);
    failed.task.fail("预算耗尽");
    assert!(matches!(failed.task.state(), TaskState::Failed));
    assert_eq!(
        failed.task.failure().map(|value: &TaskFailure| value.step),
        Some(2)
    );
    let boundary = plan()
        .with_completed_prefix(2)
        .resume_boundary()
        .expect("仍有未完成项");
    assert_eq!(boundary.next_step_index, 2);
    assert_eq!(boundary.completed_step_indexes, vec![0, 1]);
    assert_eq!(
        serde_json::to_value(Turn::User("第 3 步的交代".to_owned())).expect("回合可序列化"),
        serde_json::json!({"User": "第 3 步的交代"}),
        "快照步骤消息是普通用户消息，恢复核对不会误判"
    );
}

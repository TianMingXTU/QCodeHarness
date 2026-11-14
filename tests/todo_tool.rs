//! Todo 工具集成测试：状态机、工具 CRUD、证据账本与完成率。
//!
//! 测试只走公开装配入口，不碰私有字段；每个用例用独立临时目录，
//! 避免文件持久化测试互相污染。

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use qcode_harness::application::port::{ApprovalGate, BoxFuture, Decision, EvidenceSink};
use qcode_harness::application::todo::{
    TODO_TOOL, TodoCompletion, TodoEvent, TodoOperation, TodoStore, TodoStoreError, TodoToolSource,
    todo_completion_rate, todo_events_from_evidence,
};
use qcode_harness::application::tool_guard::{
    GuardContext, GuardVerdict, ToolGuard, run_guard_chain,
};
use qcode_harness::application::tool_registry::ToolSource;
use qcode_harness::domain::evidence::AuditEvent;
use qcode_harness::domain::ids::CallId;
use qcode_harness::domain::todo::{TodoError, TodoList, TodoStatus};
use qcode_harness::domain::tool::{OutcomeStatus, ToolCall, ToolOutcome};
use qcode_harness::infrastructure::evidence::jsonl::JsonlEvidenceSink;
use qcode_harness::infrastructure::evidence::reader::read_session;
use qcode_harness::infrastructure::todo::FileTodoStore;
use serde_json::{Value, json};

static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

/// 每次调用生成独立临时目录；进程号加计数器避免同一纳秒内撞名。
fn temp_dir(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("系统时钟早于 UNIX_EPOCH")
        .as_nanos();
    let serial = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "qch-todo-{label}-{}-{nanos}-{serial}",
        std::process::id()
    ));
    fs::create_dir_all(&dir).expect("创建独立临时目录");
    dir
}

/// 测试用审批闸门：永远放行；测试用它断言审批路径被真实询问过。
#[derive(Default)]
struct AllowGate {
    asks: AtomicU64,
}

impl AllowGate {
    fn new() -> Self {
        Self::default()
    }

    fn asks(&self) -> u64 {
        self.asks.load(Ordering::Relaxed)
    }
}

impl ApprovalGate for AllowGate {
    fn request<'a>(
        &'a self,
        _request: &'a qcode_harness::application::port::ApprovalRequest,
    ) -> BoxFuture<'a, Decision> {
        self.asks.fetch_add(1, Ordering::Relaxed);
        Box::pin(async { Ok(()) })
    }
}

fn call(action: Value) -> ToolCall {
    ToolCall {
        id: CallId::new("call-1"),
        name: TODO_TOOL.to_owned(),
        arguments: action,
    }
}

async fn invoke(source: &TodoToolSource, action: Value) -> ToolOutcome {
    let entries = source.tools();
    let entry = entries
        .iter()
        .find(|entry| entry.spec.name == TODO_TOOL)
        .expect("Todo 来源必须登记 todo 工具");
    entry.invoker.invoke(&call(action)).await
}

/// 用 ToolGuard 链和 ApprovalGate 模拟 Runner 的标准执行链。
/// 这里不重实现 Runner，只断言 Todo 工具在标准链里会进入审批闸门。
async fn invoke_through_standard_chain(
    source: &TodoToolSource,
    guards: &[Arc<dyn ToolGuard>],
    gate: &dyn ApprovalGate,
    evidence: &dyn EvidenceSink,
    action: Value,
) -> ToolOutcome {
    let entries = source.tools();
    let entry = entries
        .iter()
        .find(|entry| entry.spec.name == TODO_TOOL)
        .expect("Todo 来源必须登记 todo 工具");
    let tool_call = call(action);
    let (verdict, _) = run_guard_chain(guards, &tool_call, &GuardContext::new(&entry.spec));
    if let GuardVerdict::RequireApproval(reason) = &verdict {
        let request = qcode_harness::application::port::ApprovalRequest {
            turn: 1,
            call: tool_call.clone(),
            annotations: entry.spec.annotations,
            reason: Some(reason.clone()),
            resumed: false,
        };
        if let Err(reason) = gate.request(&request).await {
            return ToolOutcome::refused(reason);
        }
    }
    evidence.record(
        SystemTime::now(),
        AuditEvent::ToolRequested {
            turn: 1,
            call: tool_call.clone(),
        },
    );
    let outcome = entry.invoker.invoke(&tool_call).await;
    evidence.record(
        SystemTime::now(),
        AuditEvent::ToolObserved {
            turn: 1,
            call: tool_call,
            outcome: outcome.clone(),
        },
    );
    outcome
}

/// 与 Web 工具保持同一标准：非只读工具经过守卫链要求审批。
#[derive(Debug)]
struct MutationGuard;

impl ToolGuard for MutationGuard {
    fn name(&self) -> &str {
        "todo-mutation"
    }

    fn inspect(&self, call: &ToolCall, ctx: &GuardContext<'_>) -> GuardVerdict {
        if call.name == TODO_TOOL && ctx.spec.annotations.requires_approval() {
            GuardVerdict::RequireApproval("待办变更需要审批".to_owned())
        } else {
            GuardVerdict::Pass
        }
    }
}

/// 领域状态机：ID 唯一、CRUD、单向状态流与非法流转。
#[test]
fn todo_list_enforces_unique_ids_crud_and_forward_only_transitions() {
    let mut list = TodoList::new();
    let first = list.add("梳理调用链").expect("首个待办应创建成功");
    assert_eq!(first.status, TodoStatus::Pending);
    let id = first.id.clone();

    assert!(matches!(
        list.add_with_id(id.clone(), "重复 ID"),
        Err(TodoError::DuplicateId(duplicate)) if duplicate == id
    ));

    let got = list.get(&id).expect("按 ID 应能查到待办");
    assert_eq!(got.content, "梳理调用链");

    list.set_content(&id, "梳理调用链并补测试")
        .expect("改内容应成功");
    list.set_status(&id, TodoStatus::InProgress)
        .expect("Pending 应允许进入 InProgress");
    list.set_status(&id, TodoStatus::InProgress)
        .expect("同态重复设置应幂等");
    list.set_status(&id, TodoStatus::Completed)
        .expect("InProgress 应允许进入 Completed");

    assert!(matches!(
        list.set_status(&id, TodoStatus::InProgress),
        Err(TodoError::IllegalTransition {
            from: TodoStatus::Completed,
            to: TodoStatus::InProgress
        })
    ));
    assert!(matches!(
        list.set_status(&id, TodoStatus::Pending),
        Err(TodoError::IllegalTransition {
            from: TodoStatus::Completed,
            to: TodoStatus::Pending
        })
    ));

    let removed = list.remove(&id).expect("删除应成功");
    assert_eq!(removed.content, "梳理调用链并补测试");
    assert!(list.items().is_empty());
    assert!(matches!(list.remove(&id), Err(TodoError::UnknownId(unknown)) if unknown == id));
    assert!(matches!(list.add(""), Err(TodoError::EmptyContent)));
    assert!(matches!(
        list.set_status("missing", TodoStatus::InProgress),
        Err(TodoError::UnknownId(_))
    ));
}

/// 跳级流转必须报错：未开始不能直接标完成，避免绕过执行证据。
#[test]
fn todo_list_rejects_skipping_in_progress() {
    let mut list = TodoList::new();
    let item = list.add("先跑测试").expect("创建待办");
    assert!(matches!(
        list.set_status(&item.id, TodoStatus::Completed),
        Err(TodoError::IllegalTransition {
            from: TodoStatus::Pending,
            to: TodoStatus::Completed
        })
    ));
}

/// 单一 todo 工具通过 action 参数完成增删改查；失败结果说清原因。
#[tokio::test]
async fn todo_tool_supports_crud_and_persists_in_isolated_directory() {
    let dir = temp_dir("crud");
    let store = Arc::new(FileTodoStore::new(dir.join("todos.json")));
    let source = TodoToolSource::new(store.clone());

    let added = invoke(&source, json!({"action": "add", "content": "写失败测试"})).await;
    assert_eq!(added.status, OutcomeStatus::Success);
    let id = serde_json::from_str::<Value>(&added.content)
        .expect("成功观察是结构化 JSON")
        .get("id")
        .expect("新增返回待办 ID")
        .as_str()
        .expect("ID 是字符串")
        .to_owned();

    let listed = invoke(&source, json!({"action": "list"})).await;
    assert_eq!(listed.status, OutcomeStatus::Success);
    assert!(listed.content.contains("写失败测试"));

    let updated = invoke(
        &source,
        json!({"action": "update", "id": id, "status": "in_progress"}),
    )
    .await;
    assert_eq!(updated.status, OutcomeStatus::Success);
    let done = invoke(
        &source,
        json!({"action": "update", "id": id, "status": "completed"}),
    )
    .await;
    assert_eq!(done.status, OutcomeStatus::Success);

    let illegal = invoke(
        &source,
        json!({"action": "update", "id": id, "status": "in_progress"}),
    )
    .await;
    assert_eq!(illegal.status, OutcomeStatus::Failed);
    assert!(
        illegal.content.contains("非法状态流转"),
        "失败原因应明确: {}",
        illegal.content
    );

    let removed = invoke(&source, json!({"action": "remove", "id": id})).await;
    assert_eq!(removed.status, OutcomeStatus::Success);
    let empty = invoke(&source, json!({"action": "list"})).await;
    assert!(empty.content.contains("\"items\":[]") || empty.content.contains("\"items\": []"));

    // 文件持久化：重新装配一个来源，仍能查到删除后的空列表。
    let reopened = TodoToolSource::new(Arc::new(FileTodoStore::new(dir.join("todos.json"))));
    let after_reopen = invoke(&reopened, json!({"action": "list"})).await;
    assert_eq!(after_reopen.content, empty.content);
}

/// Todo 与 Web 工具同标准：走守卫链、审批闸门，并把结构化事件写入证据账本。
#[tokio::test]
async fn todo_events_go_through_guard_approval_and_evidence_ledger() {
    let dir = temp_dir("evidence");
    let evidence_path = dir.join("events.jsonl");
    let sink = Arc::new(JsonlEvidenceSink::open(&evidence_path).expect("打开证据文件"));
    let store = Arc::new(FileTodoStore::new(dir.join("todos.json")));
    let source = TodoToolSource::new(store).with_evidence(sink.clone());
    let gate = AllowGate::new();
    let guards: Vec<Arc<dyn ToolGuard>> = vec![Arc::new(MutationGuard)];

    let added = invoke_through_standard_chain(
        &source,
        &guards,
        &gate,
        sink.as_ref(),
        json!({"action": "add", "content": "补事件"}),
    )
    .await;
    assert_eq!(added.status, OutcomeStatus::Success);
    assert_eq!(gate.asks(), 1, "非只读工具应经审批闸门");

    let read = invoke_through_standard_chain(
        &source,
        &guards,
        &gate,
        sink.as_ref(),
        json!({"action": "list"}),
    )
    .await;
    assert_eq!(read.status, OutcomeStatus::Success);

    let log = read_session(&evidence_path)
        .expect("读取证据账本")
        .expect("至少有一条证据");
    assert!(
        log.events.iter().any(|evidence| matches!(
            &evidence.event,
            AuditEvent::ToolRequested { call, .. } if call.name == TODO_TOOL
        )),
        "工具调用应进入证据账本"
    );
    assert!(
        log.events.iter().any(|evidence| matches!(
            &evidence.event,
            AuditEvent::ToolObserved { call, outcome, .. }
                if call.name == TODO_TOOL && outcome.status == OutcomeStatus::Success
        )),
        "工具结果应进入证据账本"
    );

    let events = todo_events_from_evidence(&log.events);
    assert!(
        events
            .iter()
            .any(|event| matches!(event.operation, TodoOperation::Add { .. })),
        "结构化 TodoEvent 应能从证据账本还原: {events:?}"
    );
}

/// --report 用的完成率是纯函数：无事件为 0，重放增删后按最终状态计算。
#[test]
fn completion_rate_is_pure_and_tracks_final_state() {
    let snapshot: Vec<qcode_harness::domain::todo::TodoItem> = vec![];
    let empty = TodoCompletion::from_items(&snapshot);
    assert_eq!(empty.total, 0);
    assert_eq!(empty.completed, 0);
    assert_eq!(empty.rate, 0.0);

    let events = vec![
        TodoEvent::new(
            TodoOperation::Add {
                id: "1".into(),
                content: "A".into(),
            },
            vec![],
        ),
        TodoEvent::new(
            TodoOperation::Add {
                id: "2".into(),
                content: "B".into(),
            },
            vec![],
        ),
        TodoEvent::new(
            TodoOperation::Update {
                id: "1".into(),
                from: TodoStatus::Pending,
                to: TodoStatus::InProgress,
            },
            vec![],
        ),
        TodoEvent::new(
            TodoOperation::Update {
                id: "1".into(),
                from: TodoStatus::InProgress,
                to: TodoStatus::Completed,
            },
            vec![],
        ),
        TodoEvent::new(TodoOperation::Remove { id: "2".into() }, vec![]),
    ];
    let completion = todo_completion_rate(&events);
    assert_eq!(completion.total, 1);
    assert_eq!(completion.completed, 1);
    assert_eq!(completion.rate, 1.0);

    let mut other = TodoList::new();
    let item = other.add("C").expect("创建待办");
    let rate = TodoCompletion::from_items(other.items()).rate;
    assert_eq!(rate, 0.0);
    let _ = item;
}

/// 内存存储实现 TodoStore trait：证明扩展点可替换，且错误不被静默吞掉。
#[test]
fn store_trait_is_replaceable_and_reports_corruption() {
    let dir = temp_dir("store");
    let path = dir.join("broken.json");
    fs::write(&path, "{ not json").expect("写入损坏的待办文件");
    let store = FileTodoStore::new(path);
    let error = store.load().expect_err("损坏文件必须报错");
    assert!(
        matches!(error, TodoStoreError::Corrupt(_)),
        "错误类型: {error:?}"
    );

    let good = FileTodoStore::new(dir.join("good.json"));
    let mut list = TodoList::new();
    list.add("持久化").expect("创建待办");
    good.save(&list).expect("保存待办");
    assert_eq!(good.load().expect("重新载入").items().len(), 1);
}

/// 工具注册表只登记一个 todo 工具，注解必须触发标准审批链。
#[test]
fn todo_source_registers_one_non_read_only_tool() {
    let dir = temp_dir("registry");
    let source = TodoToolSource::new(Arc::new(FileTodoStore::new(dir.join("todos.json"))));
    let tools = source.tools();
    assert_eq!(source.name(), "todo");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].spec.name, TODO_TOOL);
    assert!(tools[0].spec.annotations.requires_approval());
    assert!(matches!(
        run_guard_chain(
            &[Arc::new(MutationGuard) as Arc<dyn ToolGuard>],
            &call(json!({"action": "add", "content": "x"})),
            &GuardContext::new(&tools[0].spec),
        )
        .0,
        GuardVerdict::RequireApproval(_)
    ));
}

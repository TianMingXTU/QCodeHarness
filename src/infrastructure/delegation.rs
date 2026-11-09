//! 隔离子任务工具与有界运行时。
//!
//! 编排规则：先校验整批声明，再准备共同基线；每个子任务在独立工作区里
//! 由真实 `AgentRunner` 执行，捕获变更并通过独立验证后才参与整批合并。
//! 任一步失败都不写回父工作区，已创建的工作区无论成败都要清理。
//!
//! 并发使用 tokio 的任务调度；应用层只提供纯校验与结果结构。失败时不
//! 直接 abort 同批任务：取消是协作式的，正在执行的命令允许跑到自己的
//! 安全点，超时由批次墙钟兜底，结果按"未知"如实上报。

use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::json;
use tokio::process::Command;
use tokio::task::JoinSet;
use tokio::time::{Instant, timeout, timeout_at};

use crate::application::agent_run::AgentRunner;
use crate::application::delegation::{
    ChildAgentFactory, CleanupFailure, DelegationResult, Subtask, SubtaskOutcome, SubtaskStatus,
    parse_request,
};
use crate::application::isolation::{ChangeSet, IsolationWorkspace, WorkspaceIsolation, WorkspaceVerifier};
use crate::application::port::{BoxFuture, CancelHandle, ToolInvoker};
use crate::application::tool_registry::{ToolEntry, ToolSource};
use crate::domain::agent::AgentSpec;
use crate::domain::budget::Budget;
use crate::domain::evidence::AuditEvent;
use crate::domain::ids::SessionId;
use crate::domain::platform::Platform;
use crate::domain::tool::{ToolAnnotations, ToolCall, ToolOutcome, ToolSpec};

/// 委派工具名；模型通过它提交整批子任务。
pub const DELEGATE_TOOL: &str = "delegate_subtasks";

/// 面向模型的工具来源。
pub struct SubAgentToolSource {
    isolation: Arc<dyn WorkspaceIsolation>,
    factory: Arc<dyn ChildAgentFactory>,
    verifier: Arc<dyn WorkspaceVerifier>,
    budget: Budget,
    max_subtasks: usize,
}

impl SubAgentToolSource {
    /// `budget` 是每个子运行的轮数与整批墙钟上限；`max_subtasks` 必须
    /// 在 `1..=16` 之间，非法值在调用时拒绝而不是构造时 panic。
    pub fn new(
        isolation: Arc<dyn WorkspaceIsolation>,
        factory: Arc<dyn ChildAgentFactory>,
        verifier: Arc<dyn WorkspaceVerifier>,
        budget: Budget,
        max_subtasks: usize,
    ) -> Self {
        Self {
            isolation,
            factory,
            verifier,
            budget,
            max_subtasks,
        }
    }
}

impl ToolSource for SubAgentToolSource {
    fn name(&self) -> &str {
        "sub-agent"
    }

    fn tools(&self) -> Vec<ToolEntry> {
        vec![ToolEntry {
            spec: ToolSpec {
                name: DELEGATE_TOOL.to_owned(),
                description: concat!(
                    "在隔离工作区并行执行多个子任务；每个子任务的变更独立验证，",
                    "整批通过后才合并回当前工作区。"
                )
                .to_owned(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "tasks": {
                            "type": "array",
                            "minItems": 1,
                            "maxItems": crate::application::delegation::MAX_SUBTASKS,
                            "items": {
                                "type": "object",
                                "properties": {
                                    "id": {"type": "string", "description": "批内唯一的子任务 ID"},
                                    "goal": {"type": "string", "description": "子任务目标"},
                                    "role": {
                                        "type": "string",
                                        "enum": ["planner", "coder", "tester", "reviewer"],
                                        "description": "执行角色，省略时用 coder"
                                    },
                                    "scope": {
                                        "type": "string",
                                        "description": "关注目录，相对隔离工作区根，省略时用 ."
                                    }
                                },
                                "required": ["id", "goal"],
                                "additionalProperties": false
                            }
                        }
                    },
                    "required": ["tasks"],
                    "additionalProperties": false
                }),
                annotations: ToolAnnotations::new(false, true, false, true),
            },
            invoker: Arc::new(DelegateInvoker {
                isolation: self.isolation.clone(),
                factory: self.factory.clone(),
                verifier: self.verifier.clone(),
                budget: self.budget,
                max_subtasks: self.max_subtasks,
            }),
        }]
    }
}

struct DelegateInvoker {
    isolation: Arc<dyn WorkspaceIsolation>,
    factory: Arc<dyn ChildAgentFactory>,
    verifier: Arc<dyn WorkspaceVerifier>,
    budget: Budget,
    max_subtasks: usize,
}

impl ToolInvoker for DelegateInvoker {
    fn invoke<'a>(&'a self, call: &'a ToolCall) -> BoxFuture<'a, ToolOutcome> {
        Box::pin(async move {
            let subtasks = match parse_request(&call.arguments, self.max_subtasks) {
                Ok(subtasks) => subtasks,
                Err(error) => return ToolOutcome::refused(format!("委派参数无效: {error}")),
            };
            let invocation_id = unique_id();
            let mut result = DelegationResult::placeholder(invocation_id.clone(), &subtasks);
            let deadline = self
                .budget
                .max_wall_clock
                .map(|limit| Instant::now() + limit);

            let base = match self.isolation.prepare().await {
                Ok(base) => base,
                Err(error) => {
                    result.error = Some(format!("准备隔离工作区失败: {error}"));
                    return finish(result);
                }
            };

            let mut workspaces: Vec<Option<IsolationWorkspace>> =
                (0..subtasks.len()).map(|_| None).collect();
            for (index, subtask) in subtasks.iter().enumerate() {
                // 工作区 ID 跨调用唯一：同一批里复用 `task-0` 会让第二次
                // 委派撞上上一次的目录。结果里仍用模型给的子任务 ID。
                let workspace_id = workspace_id(&subtask.id);
                match self.isolation.create(&workspace_id, &base).await {
                    Ok(workspace) => {
                        result.tasks[index].workspace_path = workspace.root.clone();
                        workspaces[index] = Some(workspace);
                    }
                    Err(error) => {
                        result.tasks[index].status = SubtaskStatus::Failed;
                        result.tasks[index].error =
                            Some(format!("创建工作区失败: {error}"));
                        result.error = Some(format!("创建工作区失败: {error}"));
                        // 已创建的工作区仍要清理；未创建的任务保持 Skipped。
                        cleanup(
                            &*self.isolation,
                            &subtasks,
                            &workspaces,
                            &mut result,
                        )
                        .await;
                        return finish(result);
                    }
                }
            }

            let control = Arc::new(BatchControl::default());
            let mut changes: Vec<Option<ChangeSet>> =
                (0..subtasks.len()).map(|_| None).collect();
            let mut next = 0;
            while next < subtasks.len() && !control.is_cancelled() {
                let end = (next + self.max_subtasks).min(subtasks.len());
                let mut set = JoinSet::new();
                for index in next..end {
                    let workspace = workspaces[index]
                        .clone()
                        .expect("创建工作区成功后才进入运行阶段");
                    let subtask = subtasks[index].clone();
                    let factory = self.factory.clone();
                    let verifier = self.verifier.clone();
                    let budget = self.budget;
                    let control = control.clone();
                    let session_id = result.tasks[index].session_id.clone();
                    let isolation = self.isolation.clone();
                    set.spawn(async move {
                        run_child(
                            index,
                            subtask,
                            workspace,
                            session_id,
                            isolation,
                            factory,
                            verifier,
                            budget,
                            deadline,
                            control,
                        )
                        .await
                    });
                }
                while let Some(joined) = set.join_next().await {
                    match joined {
                        Ok(done) => {
                            let index = done.index;
                            let failed = done.outcome.status == SubtaskStatus::Failed;
                            // 超时分支不知道原任务 ID，沿用请求里的声明。
                            let mut outcome = done.outcome;
                            outcome.id = subtasks[index].id.clone();
                            outcome.session_id = result.tasks[index].session_id.clone();
                            outcome.workspace_path = workspaces[index]
                                .as_ref()
                                .map(|workspace| workspace.root.clone())
                                .unwrap_or_default();
                            result.tasks[index] = outcome;
                            changes[index] = done.change;
                            if failed {
                                control.cancel_all();
                            }
                        }
                        Err(error) => {
                            control.cancel_all();
                            result
                                .error
                                .get_or_insert_with(|| format!("子任务异常终止: {error}"));
                        }
                    }
                }
                next = end;
            }
            for task in &mut result.tasks {
                if task.status == SubtaskStatus::Skipped {
                    task.error = Some("同批其他子任务失败，本任务未启动".to_owned());
                }
            }

            let all_ready = result.tasks.iter().all(|task| {
                task.status == SubtaskStatus::Completed && task.verified
            });
            if all_ready && result.error.is_none() {
                let batch: Vec<ChangeSet> = changes.into_iter().flatten().collect();
                if batch.len() != subtasks.len() {
                    result.error = Some("有子任务未产出变更，拒绝合并".to_owned());
                } else {
                    match self.isolation.merge(&base, &batch, &*self.verifier).await {
                        Ok(()) => result.merged = true,
                        Err(error) => result.error = Some(format!("合并失败: {error}")),
                    }
                }
            }

            cleanup(&*self.isolation, &subtasks, &workspaces, &mut result).await;
            finish(result)
        })
    }
}

/// 一个子任务的运行结局；`index` 用于把并发结果放回请求顺序。
struct ChildDone {
    index: usize,
    outcome: SubtaskOutcome,
    change: Option<ChangeSet>,
}

/// 批次级取消开关。协作式取消不丢弃正在执行的命令，只阻止下一次模型
/// 调用；超时另由墙钟兜底。
#[derive(Default)]
struct BatchControl {
    cancelled: AtomicBool,
    handles: Mutex<Vec<CancelHandle>>,
}

impl BatchControl {
    fn register(&self, handle: CancelHandle) {
        let mut handles = self.handles.lock().unwrap_or_else(|error| error.into_inner());
        if self.cancelled.load(Ordering::SeqCst) {
            handle.cancel();
        }
        handles.push(handle);
    }

    fn cancel_all(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        let handles = self.handles.lock().unwrap_or_else(|error| error.into_inner());
        for handle in handles.iter() {
            handle.cancel();
        }
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
}

/// 执行一个子任务：装配依赖、运行到终态、捕获并独立验证变更。
///
/// 失败时返回带原因的 `Failed`，并请求同批任务取消；工作区清理由调用方
/// 统一处理，失败路径也不遗漏。
#[allow(clippy::too_many_arguments)]
async fn run_child(
    index: usize,
    subtask: Subtask,
    workspace: IsolationWorkspace,
    session_id: SessionId,
    isolation: Arc<dyn WorkspaceIsolation>,
    factory: Arc<dyn ChildAgentFactory>,
    verifier: Arc<dyn WorkspaceVerifier>,
    budget: Budget,
    deadline: Option<Instant>,
    control: Arc<BatchControl>,
) -> ChildDone {
    let mut outcome = SubtaskOutcome {
        id: subtask.id.clone(),
        status: SubtaskStatus::Failed,
        verified: false,
        workspace_removed: false,
        workspace_path: workspace.root.clone(),
        session_id: session_id.clone(),
        evidence_id: None,
        error: None,
    };
    let fail = |mut outcome: SubtaskOutcome, error: String, control: &BatchControl| {
        outcome.status = SubtaskStatus::Failed;
        outcome.error = Some(error);
        control.cancel_all();
        ChildDone {
            index,
            outcome,
            change: None,
        }
    };

    if control.is_cancelled() {
        return fail(outcome, "同批其他子任务失败，本任务未运行".to_owned(), &control);
    }
    // scope 必须在子工作区内真实存在，否则项目指令与文件工具会落在
    // 两个不同的根上。物理路径再校验一次，防止链接逃逸。
    if let Err(error) = check_scope(&workspace.root, &subtask.scope) {
        return fail(outcome, error, &control);
    }
    let deps = match factory.dependencies(&workspace, subtask.role, &session_id, &subtask.scope) {
        Ok(deps) => deps,
        Err(error) => {
            return fail(outcome, format!("装配子运行失败: {error}"), &control);
        }
    };
    outcome.evidence_id = Some(session_id.clone());
    let Some(profile) = deps.roster.find(subtask.role) else {
        return fail(
            outcome,
            format!("角色 `{}` 没有档案，无法装配子运行", subtask.role.name()),
            &control,
        );
    };
    // 子会话是全新会话，第一步即第 1 轮；与编排器记录 StepStarted 的
    // 口径一致，事后按证据复盘时能认出执行者。
    deps.evidence.record(
        deps.clock.now(),
        AuditEvent::StepStarted {
            turn: 1,
            role: subtask.role,
        },
    );
    let spec = AgentSpec {
        role: subtask.role,
        system_prompt: profile.system_prompt.clone(),
        model: profile.model.clone(),
        effort: profile.effort,
        verbosity: profile.verbosity,
    };
    let prompt = format!(
        "子任务目标：{}\n关注目录：{}（相对隔离工作区根）\n\
         完成后给出结论。改动会被独立验证，整批通过后才合并回父工作区。",
        subtask.goal,
        subtask.scope.display()
    );
    let mut runner = AgentRunner::new(spec, session_id, prompt, deps);
    control.register(runner.cancel_handle());

    let run = runner.run(&budget);
    let run_result = match deadline {
        Some(deadline) => match timeout_at(deadline, run).await {
            Ok(result) => result,
            Err(_) => {
                return fail(
                    outcome,
                    "子任务超过整批墙钟预算；正在执行的调用结果未知，未合并".to_owned(),
                    &control,
                );
            }
        },
        None => run.await,
    };
    if let Err(error) = run_result {
        return fail(outcome, format!("子运行失败: {error}"), &control);
    }
    if control.is_cancelled() {
        return fail(
            outcome,
            "同批其他子任务失败，本任务虽已结束但不参与合并".to_owned(),
            &control,
        );
    }
    let change = match isolation.capture(&workspace).await {
        Ok(change) => change,
        Err(error) => {
            return fail(outcome, format!("捕获子工作区变更失败: {error}"), &control);
        }
    };
    if let Err(error) = verifier.verify(&workspace.root).await {
        return fail(outcome, format!("子工作区验证失败: {error}"), &control);
    }
    outcome.status = SubtaskStatus::Completed;
    outcome.verified = true;
    ChildDone {
        index,
        outcome,
        change: Some(change),
    }
}

fn check_scope(root: &Path, scope: &Path) -> Result<(), String> {
    let root = std::fs::canonicalize(root)
        .map_err(|error| format!("无法解析子工作区根 {}：{error}", root.display()))?;
    let path = root.join(scope);
    let physical = std::fs::canonicalize(&path)
        .map_err(|error| format!("scope 目录 {} 不存在或不可读：{error}", path.display()))?;
    if !physical.starts_with(&root) {
        return Err(format!("scope 目录越出子工作区：{}", path.display()));
    }
    if !physical.is_dir() {
        return Err(format!("scope 不是目录：{}", path.display()));
    }
    Ok(())
}

async fn cleanup(
    isolation: &dyn WorkspaceIsolation,
    subtasks: &[Subtask],
    workspaces: &[Option<IsolationWorkspace>],
    result: &mut DelegationResult,
) {
    for (index, workspace) in workspaces.iter().enumerate() {
        let Some(workspace) = workspace else { continue };
        match isolation.remove(workspace).await {
            Ok(()) => result.tasks[index].workspace_removed = true,
            Err(reason) => result.cleanup_failures.push(CleanupFailure {
                id: subtasks[index].id.clone(),
                path: workspace.root.clone(),
                reason,
            }),
        }
    }
}

fn finish(result: DelegationResult) -> ToolOutcome {
    let success = result.merged
        && result.cleanup_failures.is_empty()
        && result.tasks.iter().all(|task| {
            task.status == SubtaskStatus::Completed && task.verified
        });
    let content = serde_json::to_string(&result).unwrap_or_else(|error| {
        format!(
            "{{\"invocation_id\":{},\"merged\":false,\"tasks\":[],\"cleanup_failures\":[],\"error\":\"结果序列化失败: {error}\"}}",
            json!(result.invocation_id)
        )
    });
    if success {
        ToolOutcome::success(content)
    } else {
        ToolOutcome::failed(content)
    }
}

fn unique_id() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    format!(
        "delegate-{}-{nanos}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

/// 每次创建都不同、且不超过 64 字节的工作区 ID。
///
/// 子任务 ID 最长 56 字节，前缀最多 8 字节（`w` + 6 位 36 进制 + `-`），
/// 正好满足隔离实现的上限。计数器回绕只会让创建报"ID 已使用"，不会
/// 静默覆盖已有工作区。
fn workspace_id(subtask_id: &str) -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    const SLOTS: u64 = 36 * 36 * 36 * 36 * 36 * 36;
    let slot = NEXT.fetch_add(1, Ordering::Relaxed) % SLOTS;
    format!("w{}-{subtask_id}", base36(slot))
}

fn base36(mut value: u64) -> String {
    const DIGITS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    if value == 0 {
        return "0".to_owned();
    }
    let mut digits = Vec::new();
    while value > 0 {
        digits.push(DIGITS[(value % 36) as usize]);
        value /= 36;
    }
    digits.reverse();
    String::from_utf8(digits).expect("36 进制数字都是 ASCII")
}

/// 验证命令的输出上限；超出的部分不进入结果，避免把整份测试日志写回模型。
const VERIFY_OUTPUT_LIMIT: usize = 8 * 1024;

/// 在候选工作区运行项目自己的验证命令。
///
/// 验证命令由配置提供，跑在合并候选的副本里，因此它的副作用不会落到父
/// 工作区。超时与输出上限是硬约束：一条挂住的测试命令不能让整批委派
/// 永远等下去。命令失败时把输出尾部带回错误，模型据此判断是代码问题
/// 还是环境问题。
pub struct CommandVerifier {
    command: String,
    timeout: Duration,
}

impl CommandVerifier {
    /// `command` 经平台 shell 执行（Windows `cmd /C`，其余 `sh -c`）。
    pub fn new(command: impl Into<String>, timeout: Duration) -> Self {
        Self {
            command: command.into(),
            timeout,
        }
    }
}

impl WorkspaceVerifier for CommandVerifier {
    fn verify<'a>(&'a self, root: &'a Path) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            let (program, args) = Platform::current().shell();
            let mut command = Command::new(program);
            command
                .args(args)
                .arg(&self.command)
                .current_dir(root)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true);
            let child = command
                .spawn()
                .map_err(|error| format!("启动验证命令失败: {error}"))?;
            let output = match timeout(self.timeout, child.wait_with_output()).await {
                Ok(Ok(output)) => output,
                Ok(Err(error)) => return Err(format!("验证命令执行失败: {error}")),
                Err(_) => {
                    return Err(format!(
                        "验证命令超过 {} 秒，已终止；结果未知",
                        self.timeout.as_secs()
                    ));
                }
            };
            if output.status.success() {
                return Ok(());
            }
            let stdout = tail(&output.stdout);
            let stderr = tail(&output.stderr);
            Err(format!(
                "验证命令 `{}` 失败（{}）：\n{}{}",
                self.command, output.status, stdout, stderr
            ))
        })
    }
}

/// 取输出尾部：失败原因通常在最后几行，截头部只会留下无关的进度信息。
fn tail(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let text = text.trim();
    if text.len() <= VERIFY_OUTPUT_LIMIT {
        return text.to_owned();
    }
    let mut start = text.len() - VERIFY_OUTPUT_LIMIT;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    format!("…（前段已省略）\n{}", &text[start..])
}

/// 不跑命令的验证器：只依赖隔离实现的结构校验。
///
/// 未配置 `parallel.verify_command` 时使用。合并前仍会检查补丁可应用、
/// 路径受保护、父工作区干净，只是不运行项目自己的测试。
pub struct StructuralVerifier;

impl WorkspaceVerifier for StructuralVerifier {
    fn verify<'a>(&'a self, _root: &'a Path) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn invocation_ids_are_unique_within_a_process() {
        let ids: BTreeSet<String> = (0..100).map(|_| unique_id()).collect();
        assert_eq!(ids.len(), 100);
    }

    #[test]
    fn workspace_ids_fit_the_isolation_limit_and_stay_unique() {
        let longest = "a".repeat(56);
        let ids: BTreeSet<String> = (0..100).map(|_| workspace_id(&longest)).collect();
        assert_eq!(ids.len(), 100);
        assert!(ids.iter().all(|id| id.len() <= 64), "{:?}", ids);
        assert!(ids.iter().all(|id| id.ends_with(&longest)));
    }
}

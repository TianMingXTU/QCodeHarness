//! 面向服务客户端的逐次异步审批。不缓存会话信任，每个请求都有独立结论。

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use tokio::sync::oneshot;
use tokio::time::{MissedTickBehavior, interval};

use crate::application::port::{ApprovalGate, ApprovalRequest, BoxFuture, CancelHandle, Decision};
use crate::application::service::EventJournal;
use crate::application::service_factory::ApprovalFactory;
use crate::domain::service::{
    EventContext, PendingApproval, ServiceError, ServiceEventPayload,
};
use crate::infrastructure::service_events::context_for_call;

const CANCEL_CHECK_INTERVAL: Duration = Duration::from_millis(25);

struct PendingEntry {
    request: PendingApproval,
    context: EventContext,
    cancel: CancelHandle,
    response: oneshot::Sender<Decision>,
}

#[derive(Default)]
struct BrokerState {
    pending: BTreeMap<String, PendingEntry>,
    cancelled_executions: HashMap<String, String>,
    last_approval: u64,
    closed: bool,
    failure: Option<ServiceError>,
}

struct BrokerInner {
    journal: Arc<dyn EventJournal>,
    state: Mutex<BrokerState>,
}

/// 全服务的审批入口。闸门按执行创建，取消绑定不会跨执行共享。
///
/// 所有结论先写入日志，再唤醒对应等待方。丢弃 broker 等同关闭；仍持有闸门
/// 的任务也会被释放。每次放行都以该次结论能否持久化为准，首个日志错误保留在
/// `health` 供诊断，一次瞬态写入失败不会让整个服务永久失去审批能力。
pub struct ApprovalBroker {
    inner: Arc<BrokerInner>,
}

impl ApprovalBroker {
    /// 绑定共享事件日志；创建本身不读取或执行历史中的审批。
    pub fn new(journal: Arc<dyn EventJournal>) -> Self {
        Self { inner: Arc::new(BrokerInner { journal, state: Mutex::new(BrokerState::default()) }) }
    }

    /// 为一次执行创建新闸门；每个闸门持有独立且可替换的取消绑定。
    pub fn gate(&self, context: EventContext) -> Arc<AsyncApprovalGate> {
        Arc::new(AsyncApprovalGate {
            inner: self.inner.clone(), context, cancel: Mutex::new(CancelHandle::default()),
        })
    }

    /// 返回尚待决定的请求；传入任务标识时只返回该任务的审批。
    pub fn pending(&self, task_id: Option<&str>) -> Vec<PendingApproval> {
        self.inner.lock().pending.values()
            .filter(|entry| task_id.is_none_or(|task| entry.request.task_id == task))
            .map(|entry| entry.request.clone()).collect()
    }

    /// 决定单次审批。任务与执行归属必须同时匹配，重复决定返回错误。
    ///
    /// `Ok(())` 表示结论已持久化并交付；失败或已取消时绝不交付放行结论。
    pub fn decide(
        &self,
        approval_id: &str,
        task_id: &str,
        execution_id: &str,
        allow: bool,
    ) -> Result<(), ServiceError> {
        let mut state = self.inner.lock();
        let entry = state.pending.get(approval_id).ok_or_else(not_pending)?;
        if entry.request.task_id != task_id || entry.request.execution_id != execution_id {
            return Err(ServiceError::new("approval_owner_mismatch", "审批不属于给定任务和执行"));
        }
        if entry.cancel.is_cancelled() || state.cancelled_executions.contains_key(execution_id) {
            self.inner.cancel_execution_locked(&mut state, execution_id, "执行已取消");
            return Err(ServiceError::new("execution_cancelled", "执行已取消，不能批准"));
        }
        let Some(entry) = state.pending.remove(approval_id) else { return Err(not_pending()); };
        if entry.response.is_closed() {
            let _ = self.inner.resolve(&mut state, entry, false, Some("审批等待方已退出".into()));
            return Err(not_pending());
        }
        let reason = (!allow).then(|| "用户拒绝审批".to_owned());
        self.inner.resolve(&mut state, entry, allow, reason)
    }

    /// 取消某次执行的全部审批；后续同执行请求也直接拒绝。
    ///
    /// 日志失败不会阻止解除等待，错误保留在 `health`，取消不会转成批准。
    pub fn cancel_execution(&self, execution_id: &str, reason: &str) {
        let mut state = self.inner.lock();
        self.inner.cancel_execution_locked(&mut state, execution_id, reason);
    }

    /// 拒绝并释放全部等待方；关闭后新请求直接拒绝。
    pub fn shutdown(&self) {
        let mut state = self.inner.lock();
        state.closed = true;
        self.inner.reject_matching(&mut state, None, "审批服务已关闭");
    }

    /// 返回首个日志失败。取消与关闭接口没有错误通道，装配方可在这里诊断。
    pub fn health(&self) -> Result<(), ServiceError> {
        self.inner.lock().failure.clone().map_or(Ok(()), Err)
    }
}

impl ApprovalFactory for ApprovalBroker {
    fn gate(&self, context: EventContext) -> Arc<dyn ApprovalGate> {
        ApprovalBroker::gate(self, context)
    }
}

impl Drop for ApprovalBroker {
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl BrokerInner {
    fn lock(&self) -> MutexGuard<'_, BrokerState> {
        match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => {
                let mut state = poisoned.into_inner();
                state.closed = true;
                fail_pending(&mut state, ServiceError::new("approval_unhealthy", "审批状态锁已损坏"));
                state
            }
        }
    }

    fn begin(
        &self,
        context: &EventContext,
        request: &ApprovalRequest,
        cancel: CancelHandle,
    ) -> Result<(String, oneshot::Receiver<Decision>), String> {
        let mut state = self.lock();
        if state.closed {
            return Err("审批服务已关闭".into());
        }
        // `state.failure` 只记录诊断信息。授权依据是本次请求能否写入日志：
        // 上一次写入失败不应让后续请求永久失效，否则一次瞬时 IO 故障要重启服务。
        if let Some(reason) = state.cancelled_executions.get(&context.execution_id) {
            return Err(reason.clone());
        }
        if cancel.is_cancelled() {
            self.cancel_execution_locked(&mut state, &context.execution_id, "执行已取消");
            return Err("执行已取消".into());
        }
        let latest = match self.journal.latest() {
            Ok(latest) => latest,
            Err(error) => {
                cancel.cancel();
                let reason = format!("无法创建审批: {error}");
                fail_pending(&mut state, error);
                return Err(reason);
            }
        };
        // 等待事件落盘后的日志序号至少达到此值，重启不会重用已发布的审批 ID。
        let serial = state.last_approval.max(latest).checked_add(1)
            .ok_or_else(|| "审批标识已耗尽".to_owned())?;
        state.last_approval = serial;
        let approval_id = format!("approval-{serial}");
        let context = context_for_call(context, &request.call);
        let pending = PendingApproval {
            approval_id: approval_id.clone(),
            task_id: context.task_id.clone(),
            session_id: context.session_id.clone(),
            execution_id: context.execution_id.clone(),
            turn: request.turn,
            call: request.call.clone(),
            annotations: request.annotations,
            reason: request.reason.clone(),
            resumed: request.resumed,
            origin: context.origin.clone(),
        };
        if let Err(error) = self.journal.append(Some(&context), ServiceEventPayload::ApprovalWaiting { request: pending.clone() }) {
            cancel.cancel();
            let reason = format!("审批请求写入失败: {error}");
            fail_pending(&mut state, error);
            return Err(reason);
        }
        let (response, receiver) = oneshot::channel();
        state.pending.insert(approval_id.clone(), PendingEntry { request: pending, context, cancel, response });
        Ok((approval_id, receiver))
    }

    fn resolve(
        &self,
        state: &mut BrokerState,
        entry: PendingEntry,
        allowed: bool,
        reason: Option<String>,
    ) -> Result<(), ServiceError> {
        // 结论能否交付只看本次写入。历史错误留在 `health`，不替当前调用决定。
        let persisted = self
            .journal
            .append(
                Some(&entry.context),
                ServiceEventPayload::ApprovalResolved {
                    approval_id: entry.request.approval_id,
                    allowed,
                    reason: reason.clone(),
                },
            )
            .map(|_| ());
        if let Err(error) = persisted {
            entry.cancel.cancel();
            let _ = entry.response.send(Err(format!("审批结论写入失败: {error}")));
            fail_pending(state, error.clone());
            return Err(error);
        }
        let decision = if allowed { Ok(()) } else { Err(reason.unwrap_or_else(|| "审批被拒绝".into())) };
        entry.response.send(decision).map_err(|_| not_pending())
    }

    fn cancel_execution_locked(&self, state: &mut BrokerState, execution_id: &str, reason: &str) {
        state.cancelled_executions.entry(execution_id.to_owned()).or_insert_with(|| reason.to_owned());
        self.reject_matching(state, Some(execution_id), reason);
    }

    fn reject_matching(&self, state: &mut BrokerState, execution_id: Option<&str>, reason: &str) {
        let ids: Vec<_> = state.pending.iter()
            .filter(|(_, entry)| execution_id.is_none_or(|id| entry.request.execution_id == id))
            .map(|(id, _)| id.clone()).collect();
        for id in ids {
            if let Some(entry) = state.pending.remove(&id) {
                entry.cancel.cancel();
                // 失败由 resolve 写入健康状态；通道已丢失时也必须继续释放其他等待方。
                let _ = self.resolve(state, entry, false, Some(reason.to_owned()));
            }
        }
    }

    fn abandon(&self, approval_id: &str) {
        let mut state = self.lock();
        if let Some(entry) = state.pending.remove(approval_id) {
            entry.cancel.cancel();
            let _ = self.resolve(&mut state, entry, false, Some("审批等待方已退出".into()));
        }
    }
}

fn fail_pending(state: &mut BrokerState, error: ServiceError) {
    state.failure.get_or_insert_with(|| error.clone());
    for (_, entry) in std::mem::take(&mut state.pending) {
        entry.cancel.cancel();
        let _ = entry.response.send(Err(format!("审批服务不可用: {error}")));
    }
}

fn not_pending() -> ServiceError {
    ServiceError::new("approval_not_pending", "审批不存在、已决定或等待方已退出")
}

/// 绑定一次执行的审批闸门。每次请求等待自己的 oneshot，不占用 broker 锁。
pub struct AsyncApprovalGate {
    inner: Arc<BrokerInner>,
    context: EventContext,
    cancel: Mutex<CancelHandle>,
}

impl ApprovalGate for AsyncApprovalGate {
    fn request<'a>(&'a self, request: &'a ApprovalRequest) -> BoxFuture<'a, Decision> {
        Box::pin(async move {
            let cancel = self.cancel.lock().map_err(|_| "审批取消通道已损坏".to_owned())?.clone();
            let (approval_id, mut receiver) = self.inner.begin(&self.context, request, cancel.clone())?;
            let _wait = PendingWait { inner: self.inner.clone(), approval_id };
            let mut checks = interval(CANCEL_CHECK_INTERVAL);
            checks.set_missed_tick_behavior(MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    biased;
                    _ = checks.tick() => {
                        if cancel.is_cancelled() {
                            let mut state = self.inner.lock();
                            self.inner.cancel_execution_locked(&mut state, &self.context.execution_id, "执行已取消");
                            return Err("执行已取消".into());
                        }
                    }
                    result = &mut receiver => {
                        if cancel.is_cancelled() {
                            return Err("执行已取消".into());
                        }
                        return result.unwrap_or_else(|_| Err("审批结论通道已关闭".into()));
                    }
                }
            }
        })
    }

    fn bind_cancel(&self, cancel: CancelHandle) {
        match self.cancel.lock() {
            Ok(mut binding) => *binding = cancel,
            Err(_) => cancel.cancel(),
        }
    }
}

struct PendingWait {
    inner: Arc<BrokerInner>,
    approval_id: String,
}

impl Drop for PendingWait {
    fn drop(&mut self) {
        self.inner.abandon(&self.approval_id);
    }
}

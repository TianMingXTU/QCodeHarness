//! 同一 Server 的连接状态与显式恢复入口；没有自动重试队列。
//!
//! 状态机边界如下：
//!
//! - **调用超时或断连只放弃请求，不承诺取消。** 请求已发出后的超时与断连
//!   记为 `result_unknown = true`，原调用不会自动重放，因为副作用可能已经
//!   发生。协议错误与 Server 报告的业务错误结果已知（远端已经答了），
//!   不记为未知。
//! - **`isError` 是业务错误，不关闭连接。** ServerError 与
//!   Disconnected / Protocol 分开分类，连接存亡只由传输事实决定。
//! - **一个 Server 一条连接，断连只记录一次。** 多个工具共享状态机，
//!   断开事件按连接记录，不按工具重复。
//! - **恢复是有界的单次尝试。** 并发请求共享同一次结果（包括失败），
//!   不会各自启动实例。恢复只重建通信，不重放调用，也不绕过注册与审批。
//! - **工具清单变化不能静默改变契约。** 首次注册的规格不会热更新，
//!   重连后的再次发现只用于比对。定义变化或工具消失时按旧定义拒绝调用。

#[cfg(test)]
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use tokio::sync::Mutex;
use tokio::task::JoinHandle;

use crate::application::mcp_lifecycle::McpLifecycleRecorder;
use crate::application::port::BoxFuture;
use crate::domain::ids::CallId;
use crate::domain::mcp::{McpCallFailure, McpFaultKind, McpLifecycleFact, McpLifecycleKind};
use crate::domain::sandbox::ExecutionBoundary;
use crate::domain::tool::{ToolCall, ToolOutcome, ToolSpec};

use super::{McpClientHandle, McpError, McpTimeouts, tool_specs};

pub(crate) trait Connector: Send + Sync {
    /// 返回已握手的连接；失败时实现必须清理自己启动的进程。
    fn connect(
        &self,
        timeout: Duration,
    ) -> BoxFuture<'_, Result<Box<dyn McpClientHandle>, McpError>>;
}

enum Connection {
    Ready(Box<dyn McpClientHandle>),
    Disconnected,
    Closed,
}

struct State {
    connection: Connection,
    generation: u64,
    current_tools: Vec<ToolSpec>,
    #[cfg(test)]
    last_recovery: Result<bool, McpError>,
}

pub(crate) struct ManagedServer {
    pub server: String,
    /// 首次注册的规格永不热更新，审批与调用共用同一份定义。
    pub tools: Vec<ToolSpec>,
    #[cfg(test)]
    connector: Option<Arc<dyn Connector>>,
    recorder: Option<Arc<McpLifecycleRecorder>>,
    timeouts: McpTimeouts,
    state: Mutex<State>,
    /// 恢复轮次编号。同一次恢复的结果由后来排队的请求共享。
    #[cfg(test)]
    recovery_revision: AtomicU64,
    watcher: StdMutex<Option<JoinHandle<()>>>,
}

impl ManagedServer {
    /// 建立新连接：有界握手与工具发现；握手失败也要记录事实。
    ///
    /// `generation: 0` 用于连接未成为任何一代实例的失败，此时进程连
    /// 握手都没完成。发现失败记为第 1 代，因为握手已完成。
    pub(crate) async fn establish(
        server: &str,
        connector: Arc<dyn Connector>,
        recorder: Option<Arc<McpLifecycleRecorder>>,
        timeouts: McpTimeouts,
    ) -> Result<Arc<Self>, McpError> {
        let client = match connector.connect(timeouts.connect).await {
            Ok(client) => client,
            Err(error) => {
                if let Some(recorder) = &recorder {
                    recorder.record(fact(
                        server,
                        0,
                        McpLifecycleKind::ConnectFailed,
                        Some(error.kind),
                        None,
                        error.process_isolation,
                        false,
                    ));
                }
                return Err(error);
            }
        };
        Self::discover(server, client, Some(connector), recorder, timeouts).await
    }

    pub(crate) async fn discover(
        server: &str,
        mut client: Box<dyn McpClientHandle>,
        #[cfg(test)] connector: Option<Arc<dyn Connector>>,
        #[cfg(not(test))] _connector: Option<Arc<dyn Connector>>,
        recorder: Option<Arc<McpLifecycleRecorder>>,
        timeouts: McpTimeouts,
    ) -> Result<Arc<Self>, McpError> {
        let active = client.process_isolation();
        let tools = match discover_tools(server, client.as_ref(), timeouts.discovery).await {
            Ok(tools) => tools,
            Err(error) => {
                if let Some(recorder) = &recorder {
                    recorder.record(fact(
                        server,
                        1,
                        McpLifecycleKind::ConnectFailed,
                        Some(error.kind),
                        None,
                        active,
                        false,
                    ));
                }
                client.close().await;
                return Err(error);
            }
        };
        #[cfg(test)]
        let managed = Arc::new(Self {
            server: server.to_owned(),
            tools: tools.clone(),
            connector,
            recorder,
            timeouts,
            state: Mutex::new(State {
                connection: Connection::Ready(client),
                generation: 1,
                current_tools: tools,
                last_recovery: Ok(false),
            }),
            recovery_revision: AtomicU64::new(0),
            watcher: StdMutex::new(None),
        });
        #[cfg(not(test))]
        let managed = Arc::new(Self {
            server: server.to_owned(),
            tools: tools.clone(),
            recorder,
            timeouts,
            state: Mutex::new(State {
                connection: Connection::Ready(client),
                generation: 1,
                current_tools: tools,
            }),
            watcher: StdMutex::new(None),
        });
        managed.record(1, McpLifecycleKind::Connected, None, None, active, false);
        {
            let state = managed.state.lock().await;
            managed.watch(&state);
        }
        Ok(managed)
    }

    fn record(
        &self,
        generation: u64,
        kind: McpLifecycleKind,
        fault: Option<McpFaultKind>,
        call_id: Option<CallId>,
        active: bool,
        changed: bool,
    ) {
        if let Some(recorder) = &self.recorder {
            recorder.record(fact(
                &self.server,
                generation,
                kind,
                fault,
                call_id,
                active,
                changed,
            ));
        }
    }

    /// 监视器只持有 Weak 与通知 Receiver，不持有 service / transport / Job，
    /// 不会延长旧连接的生命周期。
    fn watch(self: &Arc<Self>, state: &State) {
        self.stop_watcher();
        let Connection::Ready(client) = &state.connection else {
            return;
        };
        let Some(closed) = client.closed() else {
            return;
        };
        let generation = state.generation;
        let weak = Arc::downgrade(self);
        let task = tokio::spawn(async move {
            closed.await;
            if let Some(server) = weak.upgrade() {
                let mut state = server.state.lock().await;
                if state.generation == generation {
                    server.disconnect(&mut state, None).await;
                }
            }
        });
        *self.watcher.lock().expect("MCP watcher lock") = Some(task);
    }

    fn stop_watcher(&self) {
        if let Some(task) = self.watcher.lock().expect("MCP watcher lock").take() {
            task.abort();
        }
    }

    async fn disconnect(&self, state: &mut State, call_id: Option<CallId>) {
        if !matches!(state.connection, Connection::Ready(_)) {
            return;
        }
        let Connection::Ready(mut client) =
            std::mem::replace(&mut state.connection, Connection::Disconnected)
        else {
            unreachable!()
        };
        let active = client.process_isolation();
        self.record(
            state.generation,
            McpLifecycleKind::Disconnected,
            Some(McpFaultKind::Disconnected),
            call_id,
            active,
            false,
        );
        client.close().await; // close 完成后 client 析构才释放 JobGuard
    }

    pub(crate) async fn invoke(
        &self,
        remote: &str,
        registered: &ToolSpec,
        call: &ToolCall,
    ) -> ToolOutcome {
        let mut state = self.state.lock().await;
        if let Connection::Ready(client) = &state.connection
            && client.is_closed() {
                self.disconnect(&mut state, Some(call.id.clone())).await;
            }
        let client = match &state.connection {
            Connection::Ready(client) => client,
            // 短路失败没有发出请求：没有执行边界，也没有结果未知，
            // 只有连接不可用的事实与恢复指引。
            Connection::Disconnected => {
                return self.failure(
                    state.generation,
                    McpError::new(McpFaultKind::Disconnected, false),
                    false,
                    false,
                );
            }
            Connection::Closed => {
                return self.failure(
                    state.generation,
                    McpError::new(McpFaultKind::Closed, false),
                    false,
                    false,
                );
            }
        };
        let active = client.process_isolation();
        if !state.current_tools.iter().any(|spec| spec == registered) {
            return self.failure(
                state.generation,
                McpError::new(McpFaultKind::ToolChanged, false),
                active,
                false,
            );
        }
        let result = match tokio::time::timeout(
            self.timeouts.call,
            client.call_tool(remote, call.arguments.clone()),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => Err(McpError::new(McpFaultKind::CallTimeout, true)),
        };
        match result {
            Ok(text) => ToolOutcome::success(text).with_execution(ExecutionBoundary::mcp(active)),
            Err(mut error) => {
                // 请求已在本连接发出：超时或断连无法证明远端未执行。
                // 下层只做分类，不判断请求是否发出；该事实只有调用点知道。
                if matches!(error.kind, McpFaultKind::CallTimeout | McpFaultKind::Disconnected) {
                    error.result_unknown = true;
                }
                let outcome = self.failure(state.generation, error.clone(), active, true);
                if error.kind == McpFaultKind::Disconnected {
                    self.disconnect(&mut state, Some(call.id.clone())).await;
                }
                outcome
            }
        }
    }

    fn failure(
        &self,
        generation: u64,
        error: McpError,
        active: bool,
        dispatched: bool,
    ) -> ToolOutcome {
        let mut text = format!("MCP Server `{}`：{}", self.server, error);
        if error.result_unknown {
            text.push_str("；原调用已发出，远端可能仍在执行，结果未知。");
        }
        if error.kind == McpFaultKind::Disconnected {
            // 恢复是显式运维动作，不是模型可调用的工具，也不会自动重试。
            // 提示里必须说明这两点。
            text.push_str(&format!(
                "；可在命令行用 `--mcp-reconnect {}` 显式尝试恢复通信，原调用不会自动重放。",
                self.server
            ));
        }
        let mut outcome = if error.kind == McpFaultKind::ToolChanged {
            ToolOutcome::refused(text)
        } else {
            ToolOutcome::failed(text)
        }
        .with_mcp_failure(McpCallFailure {
            server: self.server.clone(),
            generation,
            kind: error.kind,
            result_unknown: error.result_unknown,
            process_isolation: active,
        });
        if dispatched {
            outcome = outcome.with_execution(ExecutionBoundary::mcp(active));
        }
        outcome
    }

    /// 只恢复通信能力，不接收原工具参数，不发送 tools/call，也不重放调用。
    ///
    /// 恢复成功后只重建通信；后续调用仍需通过注册比对、角色授权与审批。
    ///
    /// 该方法只由 lib 内测试显式触发；生产恢复走会话外的新连接流程。
    #[cfg(test)]
    pub(crate) async fn reconnect(
        self: &Arc<Self>,
        call_id: Option<CallId>,
    ) -> Result<bool, McpError> {
        let revision = self.recovery_revision.load(Ordering::SeqCst);
        let mut state = self.state.lock().await;
        // 并发请求在同一次恢复后共享结果（包括失败），不各起一个实例。
        if revision != self.recovery_revision.load(Ordering::SeqCst) {
            return state.last_recovery.clone();
        }
        match &state.connection {
            Connection::Closed => return Err(McpError::new(McpFaultKind::Closed, false)),
            // 连接仍可用，恢复无事可做。前一次恢复已把连接交回 Ready，
            // 串行到达的第二个请求因此不会另起实例。
            Connection::Ready(_) => return Ok(false),
            _ => {}
        }
        self.stop_watcher();
        self.disconnect(&mut state, call_id.clone()).await;
        let Some(connector) = &self.connector else {
            return Err(McpError::new(McpFaultKind::RecoveryUnavailable, false));
        };
        state.generation += 1;
        let started = tokio::time::Instant::now();
        let result = match connector.connect(self.timeouts.connect).await {
            Ok(mut client) => {
                let active = client.process_isolation();
                let remaining = self.timeouts.connect.saturating_sub(started.elapsed());
                match discover_tools(
                    &self.server,
                    client.as_ref(),
                    self.timeouts.discovery.min(remaining),
                )
                .await
                {
                    Ok(tools) => {
                        let changed = tools != self.tools;
                        state.current_tools = tools;
                        state.connection = Connection::Ready(client);
                        self.record(
                            state.generation,
                            McpLifecycleKind::Recovered,
                            None,
                            call_id,
                            active,
                            changed,
                        );
                        Ok(true)
                    }
                    Err(error) => {
                        self.record(
                            state.generation,
                            McpLifecycleKind::RecoveryFailed,
                            Some(error.kind),
                            call_id,
                            active,
                            false,
                        );
                        client.close().await;
                        Err(error)
                    }
                }
            }
            Err(error) => {
                self.record(
                    state.generation,
                    McpLifecycleKind::RecoveryFailed,
                    Some(error.kind),
                    call_id,
                    error.process_isolation,
                    false,
                );
                Err(error)
            }
        };
        #[cfg(test)]
        {
            state.last_recovery = result.clone();
            self.recovery_revision.fetch_add(1, Ordering::SeqCst);
        }
        self.watch(&state);
        result
    }

    /// 当前连接的实际进程保护状态。断连或关闭后没有存活进程，返回 false。
    /// 该值由 lib 内测试用于核对状态机，不参与分类。
    #[cfg(test)]
    pub(crate) async fn process_isolation(&self) -> bool {
        let state = self.state.lock().await;
        match &state.connection {
            Connection::Ready(client) => client.process_isolation(),
            Connection::Disconnected | Connection::Closed => false,
        }
    }

    /// 主动关闭后即使注册表还持有 invoker，也不能继续调用或恢复。
    pub(crate) async fn close(&self) {
        self.stop_watcher();
        let mut state = self.state.lock().await;
        let previous = std::mem::replace(&mut state.connection, Connection::Closed);
        if matches!(previous, Connection::Closed) {
            return;
        }
        let active = if let Connection::Ready(mut client) = previous {
            let active = client.process_isolation();
            client.close().await;
            active
        } else {
            false
        };
        self.record(
            state.generation,
            McpLifecycleKind::Closed,
            None,
            None,
            active,
            false,
        );
    }
}

impl Drop for ManagedServer {
    fn drop(&mut self) {
        self.stop_watcher();
    }
}

impl std::fmt::Debug for ManagedServer {
    // 异步锁内的状态与连接句柄无法同步派生。调试信息包含名字与固定的
    // 工具清单即可定位问题；`Result::expect_err` 等断言要求实现 Debug。
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ManagedServer")
            .field("server", &self.server)
            .field("tools", &self.tools)
            .finish_non_exhaustive()
    }
}

pub(super) async fn discover_tools(
    server: &str,
    client: &dyn McpClientHandle,
    timeout: Duration,
) -> Result<Vec<ToolSpec>, McpError> {
    let tools = tokio::time::timeout(timeout, client.list_tools())
        .await
        .map_err(|_| McpError::new(McpFaultKind::DiscoveryTimeout, false))??;
    Ok(tool_specs(server, tools))
}

fn fact(
    server: &str,
    generation: u64,
    kind: McpLifecycleKind,
    fault: Option<McpFaultKind>,
    call_id: Option<CallId>,
    process_isolation: bool,
    tools_changed: bool,
) -> McpLifecycleFact {
    McpLifecycleFact {
        server: server.to_owned(),
        generation,
        kind,
        fault,
        call_id,
        process_isolation,
        tools_changed,
    }
}

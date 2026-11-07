//! 同一 Server 的连接状态与显式恢复入口；没有自动调用重试队列。
//!
//! 状态机的边界是本文件的全部要点，每条都对应一条不许的承诺：
//!
//! - **调用超时/断连只放弃请求，不承诺取消。** 请求发出后的故障一律
//!   `result_unknown = true`，原调用绝不自动重放（副作用可能已经发生）；
//! - **`isError` 是业务错误，不弄死连接。** 分类里 ServerError 与
//!   Disconnected/Protocol 分开，一条连接的生死只由传输事实决定；
//! - **一个 Server 一条连接，断连只记一次。** 多个工具共享状态机，
//!   断开事件按"连接"落账而不是按"工具"复读；
//! - **恢复是有界的单次尝试。** 并发请求共享同一次结果（包括失败），
//!   不会各起一个实例；恢复只重建通信，不重放、不绕过注册与审批；
//! - **工具清单变化不能悄悄改契约。** 首次注册的规格永不热更新，
//!   重连后的再发现只用于比对；定义变了或消失了，按旧定义拒调。

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
    last_recovery: Result<bool, McpError>,
}

pub(crate) struct ManagedServer {
    pub server: String,
    /// 首次注册的规格永不热更新，审批与调用共用同一份定义。
    pub tools: Vec<ToolSpec>,
    connector: Option<Arc<dyn Connector>>,
    recorder: Option<Arc<McpLifecycleRecorder>>,
    timeouts: McpTimeouts,
    state: Mutex<State>,
    /// 恢复波次代号：一次恢复把结果交给同波次内排队的后来者共享。
    recovery_revision: AtomicU64,
    watcher: StdMutex<Option<JoinHandle<()>>>,
}

impl ManagedServer {
    /// 建立一条全新连接：有界握手 + 发现，握手失败也要留下事实。
    ///
    /// `generation: 0` 专给"连接没能成为任何一代实例"的失败——进程
    /// 连握手都没完成，没有代可数；发现失败的则是第 1 代（握手已完成）。
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
        connector: Option<Arc<dyn Connector>>,
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

    /// 监视器只有 Weak 与通知 Receiver，不持 service/transport/Job，不续命旧连接。
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
        if let Connection::Ready(client) = &state.connection {
            if client.is_closed() {
                self.disconnect(&mut state, Some(call.id.clone())).await;
            }
        }
        let client = match &state.connection {
            Connection::Ready(client) => client,
            // 短路的失败没有发出任何请求：没有执行边界可交代，也没有
            // 结果未知可声张，只有一条"连接没了"的事实与恢复指引。
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
                // 请求确已在本连接上发出：超时或断连都证明不了远端没执行。
                // 下层只做分类、不判"发出过没有"，发出的事实只有调用点知道。
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
            // 恢复是显式的运维动作，不是模型手里能点的工具，也不会有
            // 任何自动重试——指引里必须把这两件事都说死。
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

    /// 只恢复通信能力；不接收原工具参数，不发送 tools/call，也不重放原调用。
    ///
    /// 本轮的触发面是会话外的显式 CLI 动作（README"MCP 生命周期"一节），
    /// 运行中的会话没有自动重试。恢复成功只重建通信：后续调用照常走
    /// 注册比对、角色授权与审批。
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
            // 连接还活着：恢复无事可做。这也是"串行到达的第二个请求
            // 不会多起实例"的兜底——前一次恢复已把连接交回 Ready。
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
        state.last_recovery = result.clone();
        self.recovery_revision.fetch_add(1, Ordering::SeqCst);
        self.watch(&state);
        result
    }

    /// 当前连接的真实进程保护状态。断连/关闭后没有活着的进程可谈保护，
    /// 返回 false；供诚实断言与展示用，不参与任何分类。
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
    // 异步锁里的状态与连接句柄都没法同步派生；调试信息给名字与钉住的
    // 清单就够定位问题（`Result::expect_err` 一类断言要求 Debug）。
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

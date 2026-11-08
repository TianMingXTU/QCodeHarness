//! MCP 生命周期事实：不含进程句柄、SDK 错误文本、参数或环境变量。

use serde::{Deserialize, Serialize};

use super::ids::CallId;

/// SDK 和执行器能证实的故障。连接关闭不等同于进程崩溃。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum McpFaultKind {
    SandboxCreation,
    Spawn,
    Handshake,
    HandshakeTimeout,
    Discovery,
    DiscoveryTimeout,
    Disconnected,
    CallTimeout,
    ServerError,
    Protocol,
    ToolChanged,
    Closed,
    RecoveryUnavailable,
}

impl std::fmt::Display for McpFaultKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::SandboxCreation => "沙箱创建失败（未启动 Server）",
            Self::Spawn => "启动进程失败",
            Self::Handshake => "MCP 握手失败",
            Self::HandshakeTimeout => "MCP 握手超时",
            Self::Discovery => "工具发现失败",
            Self::DiscoveryTimeout => "工具发现超时（检查 Server 的 list_tools 实现）",
            Self::Disconnected => "连接已断开",
            Self::CallTimeout => "工具调用超时",
            Self::ServerError => "Server 返回 isError",
            Self::Protocol => "MCP 协议请求失败",
            Self::ToolChanged => "工具已消失或定义变化，拒绝按旧定义调用；请重新启动任务登记工具",
            Self::Closed => "连接已主动关闭",
            Self::RecoveryUnavailable => "此连接没有重连配置",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum McpLifecycleKind {
    Connected,
    ConnectFailed,
    Disconnected,
    Recovered,
    RecoveryFailed,
    Closed,
}

/// 一条连接级事实。
///
/// generation 区分同名 Server 的实例，避免把多个工具的同一次断连记成
/// 多次退出。call_id 只用于关联触发恢复或断连的调用。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpLifecycleFact {
    pub server: String,
    pub generation: u64,
    pub kind: McpLifecycleKind,
    pub fault: Option<McpFaultKind>,
    pub call_id: Option<CallId>,
    pub process_isolation: bool,
    pub tools_changed: bool,
}

/// 工具调用的失败事实。
///
/// 请求未发出时 result_unknown 必须为 false。请求发出后的超时或断连
/// 只能标记结果未知，不能承诺取消或恰好一次执行。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpCallFailure {
    pub server: String,
    pub generation: u64,
    pub kind: McpFaultKind,
    pub result_unknown: bool,
    pub process_isolation: bool,
}

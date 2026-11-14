//! 本地 Web UI 的应用层端口与静态资源契约。
//!
//! 这里只规定前端能看见什么：绑定本机地址、回放 events.jsonl 的同一份服务
//! 事件、把命令原样交给 TaskService。HTTP、线程与端口都在基础设施层；Web UI
//! 不维护任务状态，避免与常驻服务和 CLI 出现第二套证据语义。

use std::path::PathBuf;

use crate::application::port::BoxFuture;
use crate::application::service::TaskService;
use crate::domain::service::ServiceError;

/// 随服务内嵌提供的静态资源；不请求 CDN 或远程资源。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaticAssets {
    /// GET / 返回的 HTML。
    pub index_html: Vec<u8>,
    /// GET /app.js 返回的原生 JavaScript。
    pub app_js: Vec<u8>,
}

impl StaticAssets {
    /// 校验静态资源非空；页面与脚本缺一不可，否则浏览器只能看到空白壳。
    pub fn validate(&self) -> Result<(), ServiceError> {
        if self.index_html.is_empty() || self.app_js.is_empty() {
            return Err(ServiceError::new(
                "web_ui_assets",
                "Web UI 的 index.html 与 app.js 不能为空",
            ));
        }
        Ok(())
    }
}

/// 单机单用户 Web 服务的启动参数。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebUiConfig {
    /// 监听端口；0 表示由操作系统分配，便于测试和单机多实例。
    pub port: u16,
    /// 只接受本机回环地址；字段显式保留，便于装配处核对。
    pub host: String,
    /// 服务事件日志路径；GET /events 的回放与 CLI 共享此事实来源。
    pub events_path: PathBuf,
    /// 静态页面与脚本。
    pub assets: StaticAssets,
}

impl WebUiConfig {
    /// 校验监听地址确实是 IPv4 回环地址。
    ///
    /// 多用户或远程部署不在本次范围内；拒绝非回环地址比事后依赖防火墙更
    /// 明确，也不让前端意外获得跨主机可达性。
    pub fn validate(&self) -> Result<(), ServiceError> {
        if self.host != "127.0.0.1" {
            return Err(ServiceError::new(
                "web_ui_bind",
                format!("Web UI 只允许绑定 127.0.0.1，收到 {}", self.host),
            ));
        }
        if self.events_path.as_os_str().is_empty() {
            return Err(ServiceError::new("web_ui_events", "事件日志路径不能为空"));
        }
        self.assets.validate()
    }
}

/// 本机 Web 前端生命周期。
///
/// `bind` 返回实际端口；`shutdown` 必须停止监听并等待在途请求，避免测试或
/// 进程退出后端口仍被占用。实现方通过 [`TaskService`] 查询事件与转发命令，
/// 不另建任务状态机。
pub trait WebUiServer: Send + Sync {
    /// 绑定并启动服务，返回实际监听端口。
    fn bind<'a>(&'a self, config: WebUiConfig) -> BoxFuture<'a, Result<u16, ServiceError>>;

    /// 服务绑定的主机地址，供宿主确认未监听外网接口。
    fn bound_host(&self) -> &str;

    /// 停止服务；重复调用必须无副作用。
    fn shutdown(&self) -> BoxFuture<'_, Result<(), ServiceError>>;
}

/// 把服务交给 Web 实现的装配辅助函数。
///
/// 仅约束 trait 对象形态，减少装配处手写 `Arc<dyn TaskService>` 转换的重复。
pub fn web_service(service: std::sync::Arc<dyn TaskService>) -> std::sync::Arc<dyn TaskService> {
    service
}

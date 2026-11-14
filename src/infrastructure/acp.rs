//! ACP 会话客户端：在既有 `SdkClient` 之上提供"跑到完成或首次需要审批"的
//! 编排，供 IDE / CI 这类外部宿主复用同一套 versioned stdio 线格式接入。
//!
//! 会话层不新增线格式，也不决定模型或审批的最终结论：审批结论由外部决定的
//! `allow` 传回任务服务，证据与审批链仍落在服务侧。

use std::sync::Arc;

use crate::application::acp::{AcpApprovalPrompt, AcpRunOutcome, AcpTaskClient};
use crate::application::port::BoxFuture;
use crate::domain::service::{
    ServiceCommand, ServiceError, ServiceEvent, ServiceEventPayload, ServiceReply,
};
use crate::domain::task::TaskKind;
use crate::interface::sdk::SdkClient;

/// 让既有 `SdkClient` 满足 ACP 会话依赖的最小传输面。
impl AcpTaskClient for SdkClient {
    fn call<'a>(
        &'a self,
        id: &'a str,
        command: ServiceCommand,
    ) -> BoxFuture<'a, Result<ServiceReply, ServiceError>> {
        Box::pin(SdkClient::call(self, id, command))
    }

    fn next_event(&self) -> BoxFuture<'_, Result<ServiceEvent, ServiceError>> {
        Box::pin(SdkClient::next_event(self))
    }
}

/// ACP 会话：封装一条 stdio 连接并提供"启动→审批→完成"的控制流。
pub struct AcpSession {
    client: Arc<dyn AcpTaskClient>,
}

impl AcpSession {
    /// 接入一对可双向读写的字节流；握手失败直接返回协议错误。
    pub async fn connect<T>(io: T) -> Result<Self, ServiceError>
    where
        T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        let client = SdkClient::from_io(io).await?;
        Ok(Self {
            client: Arc::new(client),
        })
    }

    /// 对已有客户端再封装；测试注入内存替身。
    pub fn new(client: Arc<dyn AcpTaskClient>) -> Self {
        Self { client }
    }

    /// 启动一个任务并驱动到完成、停止或首次需要审批。
    ///
    /// 若返回 `NeedsApproval`，外部调用 [`AcpSession::decide`] 后继续，直到
    /// 拿到 `Completed` 或 `Stopped`。
    pub async fn run_to_completion(&self, goal: &str) -> AcpRunOutcome {
        let command = ServiceCommand::Create {
            goal: goal.to_owned(),
            kind: TaskKind::Review,
        };
        match self.client.call("acp-create", command).await {
            Ok(ServiceReply::Task { .. }) | Ok(ServiceReply::Accepted { .. }) => self.drive().await,
            Ok(other) => AcpRunOutcome::Error {
                message: format!("Create 未返回任务视图，得到 {other:?}"),
            },
            Err(error) => AcpRunOutcome::Error {
                message: error.message,
            },
        }
    }

    /// 决定一条待审批后继续驱动。
    pub async fn decide(
        &self,
        approval_id: &str,
        task_id: &str,
        execution_id: &str,
        allow: bool,
    ) -> AcpRunOutcome {
        let command = ServiceCommand::Decide {
            approval_id: approval_id.to_owned(),
            task_id: task_id.to_owned(),
            execution_id: execution_id.to_owned(),
            allow,
        };
        if let Err(error) = self.client.call("acp-decide", command).await {
            return AcpRunOutcome::Error {
                message: error.message,
            };
        }
        self.drive().await
    }

    async fn drive(&self) -> AcpRunOutcome {
        loop {
            let event = match self.client.next_event().await {
                Ok(event) => event,
                Err(error) => {
                    return AcpRunOutcome::Error {
                        message: error.message,
                    }
                }
            };
            match event.payload {
                ServiceEventPayload::Completed { answer } => {
                    return AcpRunOutcome::Completed { answer }
                }
                ServiceEventPayload::Stopped { status, error } => {
                    return AcpRunOutcome::Stopped { status, error }
                }
                ServiceEventPayload::ApprovalWaiting { request } => {
                    return AcpRunOutcome::NeedsApproval {
                        prompt: AcpApprovalPrompt { request },
                    }
                }
                _ => continue,
            }
        }
    }
}

//! 服务与事件日志端口。不绑定传输、异步运行时或文件系统。

use super::port::BoxFuture;
use crate::domain::service::{
    EventContext, ServiceCommand, ServiceError, ServiceEvent, ServiceEventPayload, ServiceReply,
};

/// 后台任务服务。调用方提供请求标识，实现方负责命令的幂等边界。
pub trait TaskService: Send + Sync {
    /// 执行命令并返回直接回复；受理长期任务后通过事件报告后续进度。
    fn call<'a>(
        &'a self,
        request_id: &'a str,
        command: ServiceCommand,
    ) -> BoxFuture<'a, Result<ServiceReply, ServiceError>>;

    /// 从排他游标之后回放，再接收实时事件。游标大于当前末尾时返回错误。
    fn subscribe(&self, after: u64) -> Result<Box<dyn EventSubscription>, ServiceError>;
}

/// 按提交顺序交付的订阅。慢消费者收到 `lagged` 后应使用最后成功游标补读。
pub trait EventSubscription: Send {
    /// 等待下一条事件；本方法不执行事件描述的业务动作。
    fn next(&mut self) -> BoxFuture<'_, Result<ServiceEvent, ServiceError>>;
}

/// 全服务共享的事件日志。序号从 1 连续递增，不复用旧 evidence 的序号。
pub trait EventJournal: Send + Sync {
    /// 原子分配序号并持久化，成功后才向订阅方发布。失败不承诺本条已落盘。
    fn append(
        &self,
        context: Option<&EventContext>,
        payload: ServiceEventPayload,
    ) -> Result<ServiceEvent, ServiceError>;

    /// 一次提交一组启动事件：要么全部落盘，要么一条都不落盘。
    ///
    /// 默认实现逐条转发，供只需单条语义的实现复用；有界 JSONL 实现覆盖为
    /// 单次写盘。调用方据此保证一次执行启动时不会留下半组事件。
    fn append_batch(
        &self,
        context: Option<&EventContext>,
        payloads: Vec<ServiceEventPayload>,
    ) -> Result<Vec<ServiceEvent>, ServiceError> {
        payloads
            .into_iter()
            .map(|payload| self.append(context, payload))
            .collect()
    }

    /// 读取 `after` 之后最多 `limit` 条记录，`after = 0` 表示从头开始。
    ///
    /// 游标大于当前末尾时返回错误；`limit = 0` 返回空集合。
    fn read(&self, after: u64, limit: usize) -> Result<Vec<ServiceEvent>, ServiceError>;

    /// 在同一提交边界连接回放与实时流，既不漏事件也不重复事件。
    fn subscribe(&self, after: u64) -> Result<Box<dyn EventSubscription>, ServiceError>;

    /// 最后成功提交的序号；空日志返回 0。
    fn latest(&self) -> Result<u64, ServiceError>;
}

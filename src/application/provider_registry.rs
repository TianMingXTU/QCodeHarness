//! 模型提供方注册表：按 id 登记与查找。
//!
//! 登记顺序即默认顺序：第一个登记的提供方是默认提供方。装配时只登记
//! 一家时，行为与没有注册表时逐字相同——注册表只负责选择，不改变任何
//! 网关行为。

use std::sync::Arc;

use crate::application::port::LlmProvider;

/// 模型提供方注册表。
#[derive(Default)]
pub struct ProviderRegistry {
    providers: Vec<Arc<dyn LlmProvider>>,
}

impl ProviderRegistry {
    /// 空注册表。
    pub fn new() -> Self {
        Self::default()
    }

    /// 登记一家提供方；同一 id 只保留先登记的（后到的忽略并返回 `false`）。
    ///
    /// 重复 id 不覆盖：装配方同时登记两家同名提供方通常是配置错误，
    /// 静默换掉先登记的那家会让"默认是谁"取决于代码顺序。
    pub fn register(&mut self, provider: Arc<dyn LlmProvider>) -> bool {
        if self.providers.iter().any(|known| known.id() == provider.id()) {
            return false;
        }
        self.providers.push(provider);
        true
    }

    /// 按 id 查找提供方。
    pub fn get(&self, id: &str) -> Option<&Arc<dyn LlmProvider>> {
        self.providers.iter().find(|provider| provider.id() == id)
    }

    /// 默认提供方：第一个登记的。
    ///
    /// 空注册表返回 `None` 而不是 panic：是否允许空表是装配方的决定，
    /// 这里只如实回答"没有"。
    pub fn default_provider(&self) -> Option<&Arc<dyn LlmProvider>> {
        self.providers.first()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::port::{BoxFuture, ModelGateway, ModelReply, ModelRequest};
    use crate::application::error::ApplicationError;

    /// 永不调用的网关：注册表测试只关心选择，不关心调用。
    struct UnusedGateway;

    impl ModelGateway for UnusedGateway {
        fn complete<'a>(
            &'a self,
            _request: ModelRequest<'a>,
        ) -> BoxFuture<'a, Result<ModelReply, ApplicationError>> {
            Box::pin(async { unreachable!("注册表测试不该调用网关") })
        }
    }

    /// 只有 id 有意义的提供方。
    struct NamedProvider(&'static str);

    impl LlmProvider for NamedProvider {
        fn id(&self) -> &str {
            self.0
        }

        fn gateway(&self) -> Arc<dyn ModelGateway> {
            Arc::new(UnusedGateway)
        }
    }

    #[test]
    fn the_first_registered_provider_is_the_default() {
        let mut registry = ProviderRegistry::new();
        assert!(registry.register(Arc::new(NamedProvider("first"))));
        assert!(registry.register(Arc::new(NamedProvider("second"))));

        assert_eq!(registry.default_provider().unwrap().id(), "first");
        assert_eq!(registry.get("second").unwrap().id(), "second");
    }

    #[test]
    fn an_empty_registry_has_no_default() {
        let registry = ProviderRegistry::new();
        assert!(registry.default_provider().is_none());
        assert!(registry.get("any").is_none());
    }

    #[test]
    fn a_duplicate_id_does_not_replace_the_first() {
        let mut registry = ProviderRegistry::new();
        assert!(registry.register(Arc::new(NamedProvider("same"))));
        assert!(!registry.register(Arc::new(NamedProvider("same"))));

        assert_eq!(registry.get("same").unwrap().id(), "same");
        assert_eq!(registry.default_provider().unwrap().id(), "same");
    }
}

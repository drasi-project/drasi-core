// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use async_trait::async_trait;
use futures::future::BoxFuture;
use std::sync::Mutex as SyncMutex;

pub(crate) type SourceConstructor = Box<
    dyn FnOnce(
            ComponentId,
        )
            -> BoxFuture<'static, anyhow::Result<(Box<dyn Source>, HashMap<String, String>)>>
        + Send,
>;

pub(super) struct DeferredSource {
    id: ComponentId,
    auto_start: bool,
    constructor: SyncMutex<Option<SourceConstructor>>,
    instance: OnceLock<Arc<dyn Source>>,
    state: super::construction::ConstructionState,
}

impl DeferredSource {
    pub(super) fn configuration_available(&self) -> bool {
        self.state.outcome().is_some_and(|result| result.is_ok())
    }

    pub(super) fn new(
        id: ComponentId,
        auto_start: bool,
        constructor: SourceConstructor,
    ) -> Arc<Self> {
        Arc::new(Self {
            id,
            auto_start,
            constructor: SyncMutex::new(Some(constructor)),
            instance: OnceLock::new(),
            state: super::construction::ConstructionState::new(),
        })
    }

    pub(super) fn metadata(&self) -> Option<&HashMap<String, String>> {
        self.state.metadata.get()
    }

    pub(super) async fn construct(&self) -> anyhow::Result<()> {
        if let Some(result) = self.state.outcome() {
            return result;
        }
        let guard = self.state.guard();
        let result = async {
            let constructor = self
                .constructor
                .lock()
                .map_err(|_| anyhow::anyhow!("source constructor ownership poisoned"))?
                .take()
                .ok_or_else(|| anyhow::anyhow!("source constructor was already consumed"))?;
            let (source, metadata) = constructor(self.id.clone()).await?;
            // Retain the returned owner before validating identity or doing further awaits.
            let _ = self.instance.set(source.into());
            let _ = self.state.metadata.set(metadata);
            anyhow::ensure!(
                self.instance()?.id() == self.id.as_str(),
                "source constructor substituted its assigned UUID"
            );
            Ok(())
        }
        .await;
        guard.finish(result)
    }

    fn instance(&self) -> anyhow::Result<&Arc<dyn Source>> {
        self.instance
            .get()
            .ok_or_else(|| anyhow::anyhow!("source is awaiting construction"))
    }
}

#[async_trait]
impl crate::computation::v1::SourcePreparation for DeferredSource {
    fn is_prepared(&self) -> bool {
        self.configuration_available()
    }
    async fn wait_prepared(&self) -> anyhow::Result<()> {
        self.state.wait().await
    }
}

#[async_trait]
impl Source for DeferredSource {
    fn id(&self) -> &str {
        self.id.as_str()
    }
    fn type_name(&self) -> &str {
        self.instance
            .get()
            .map_or("deferred-source", |source| source.type_name())
    }
    fn properties(&self) -> HashMap<String, serde_json::Value> {
        self.instance
            .get()
            .map_or_else(HashMap::new, |source| source.properties())
    }
    fn auto_start(&self) -> bool {
        self.auto_start
    }
    fn dispatch_mode(&self) -> crate::DispatchMode {
        self.instance
            .get()
            .map_or_else(Default::default, |source| source.dispatch_mode())
    }
    fn supports_replay(&self) -> bool {
        self.instance
            .get()
            .is_some_and(|source| source.supports_replay())
    }
    fn describe_schema(&self) -> Option<crate::schema::SourceSchema> {
        self.instance
            .get()
            .and_then(|source| source.describe_schema())
    }
    async fn start(&self) -> anyhow::Result<()> {
        self.instance()?.start().await
    }
    async fn stop(&self) -> anyhow::Result<()> {
        if let Some(source) = self.instance.get() {
            source.stop().await?;
        }
        Ok(())
    }
    async fn status(&self) -> ComponentStatus {
        match self.instance.get() {
            Some(source) => source.status().await,
            None => ComponentStatus::Added,
        }
    }
    async fn subscribe(
        &self,
        settings: crate::config::SourceSubscriptionSettings,
    ) -> anyhow::Result<crate::SubscriptionResponse> {
        self.state.wait().await?;
        self.instance()?.subscribe(settings).await
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self.instance
            .get()
            .map_or(self as &dyn std::any::Any, |source| source.as_any())
    }
    async fn bind_runtime_context(&self, context: crate::SourceRuntimeContext) {
        if let Some(source) = self.instance.get() {
            source.bind_runtime_context(context).await;
        }
    }
    async fn initialize(&self) -> anyhow::Result<()> {
        self.instance()?.initialize().await
    }
    async fn dispose(&self) -> anyhow::Result<()> {
        if let Some(source) = self.instance.get() {
            source.dispose().await?;
        }
        Ok(())
    }
    async fn pause_processing(&self) -> anyhow::Result<()> {
        self.instance()?.pause_processing().await
    }
    async fn resume_processing(&self) -> anyhow::Result<()> {
        self.instance()?.resume_processing().await
    }
    async fn apply_update(
        &self,
        context: crate::management::OrdinaryComponentUpdateContext,
    ) -> anyhow::Result<()> {
        self.instance()?.apply_update(context).await
    }
    async fn delete_owned_resources(&self) -> anyhow::Result<()> {
        if let Some(source) = self.instance.get() {
            source.delete_owned_resources().await?;
        }
        Ok(())
    }
    async fn set_bootstrap_provider(&self, provider: Box<dyn crate::BootstrapProvider>) {
        if let Some(source) = self.instance.get() {
            source.set_bootstrap_provider(provider).await;
        }
    }
    async fn set_identity_provider(&self, provider: Arc<dyn crate::identity::IdentityProvider>) {
        if let Some(source) = self.instance.get() {
            source.set_identity_provider(provider).await;
        }
    }
    async fn remove_position_handle(&self, query: &str) {
        if let Some(source) = self.instance.get() {
            source.remove_position_handle(query).await;
        }
    }
    async fn on_subscriptions_complete(&self) {
        if let Some(source) = self.instance.get() {
            source.on_subscriptions_complete().await;
        }
    }
}

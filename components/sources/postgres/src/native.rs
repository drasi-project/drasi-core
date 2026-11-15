// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Direct Rust graph bindings for the SDK-native PostgreSQL implementations.
//! Only these host bindings retain the actual local query progress owner.

use anyhow::Result;
use async_trait::async_trait;
use drasi_computation_bootstrap_postgres::NativeBootstrapProvider;
use drasi_computation_plugin_sdk::{lifecycle as native_lifecycle, NativeComponent, NativeSource};
use drasi_computation_source_postgres::PostgresTransactionSource as NativePostgresSource;
use drasi_core::interface::StorageDurability;
use drasi_lib::computation::v1::{
    BootstrapPreparation, BootstrapState, BootstrapWatermark, ComponentDescriptor, ComponentId,
    ComponentRecovery, ComputationBootstrapProvider, ComputationBootstrapSnapshot,
    ComputationComponent, EnvelopeSource, OutputEnvelope, QuerySourceProgress,
    SourceProgressReader, StreamId,
};
use std::sync::Arc;

mod factory;
pub use drasi_computation_source_postgres::PostgresTransactionConfig;
pub use factory::PostgresTransactionSourceFactory;

pub struct PostgresTransactionSource {
    inner: NativePostgresSource,
    stream: StreamId,
    config: PostgresTransactionConfig,
    progress: SourceProgressReader,
    snapshot: Option<Arc<PostgresSnapshot>>,
}

impl PostgresTransactionSource {
    pub fn new(
        id: ComponentId,
        stream: StreamId,
        config: PostgresTransactionConfig,
        progress: impl Into<SourceProgressReader>,
    ) -> Result<Self> {
        let progress = progress.into();
        Ok(Self {
            inner: NativePostgresSource::new(
                id,
                stream.clone(),
                config.clone(),
                progress.read_only(),
                false,
            )?,
            stream,
            config,
            progress,
            snapshot: None,
        })
    }

    pub fn coordinated(
        id: ComponentId,
        stream: StreamId,
        config: PostgresTransactionConfig,
        progress: impl Into<SourceProgressReader>,
    ) -> Result<(Self, Arc<PostgresSnapshot>)> {
        let progress = progress.into();
        let snapshot = Arc::new(PostgresSnapshot {
            inner: drasi_computation_bootstrap_postgres::PostgresSnapshot::new(
                id.clone(),
                stream.clone(),
                config.clone(),
                progress.read_only(),
            )?,
            source: id.clone(),
            stream: stream.clone(),
            config: config.clone(),
            progress: progress.clone(),
        });
        let mut source = Self::new(id, stream, config, progress)?;
        source.attach_snapshot(snapshot.clone())?;
        Ok((source, snapshot))
    }

    fn attach_snapshot(&mut self, snapshot: Arc<PostgresSnapshot>) -> Result<()> {
        snapshot.validate_source(self)?;
        self.inner = NativePostgresSource::new(
            self.inner.descriptor().id().clone(),
            self.stream.clone(),
            self.config.clone(),
            self.progress.read_only(),
            true,
        )?;
        self.snapshot = Some(snapshot);
        Ok(())
    }

    pub fn describe(id: ComponentId) -> Result<ComponentDescriptor> {
        NativePostgresSource::describe(id)
    }
}

#[async_trait]
impl ComputationComponent for PostgresTransactionSource {
    fn descriptor(&self) -> &ComponentDescriptor {
        self.inner.descriptor()
    }
    fn recovery_contract(&self) -> ComponentRecovery {
        ComponentRecovery::admitted(StorageDurability::LOCAL_PROCESS_RESTART)
            .replay_until(self.progress.component_id().clone())
    }
    /// This factory's own shape: the native settings nested under
    /// `settings`, as [`PostgresTransactionSourceFactory`] accepts them.
    fn configuration(&self) -> Result<serde_json::Value> {
        Ok(serde_json::json!({
            "stream": self.stream.as_str(),
            "settings": self.config,
            "coordinated_snapshot": self.snapshot.is_some(),
        }))
    }
    async fn start(&mut self, context: &drasi_lib::computation::v1::StartContext) -> Result<()> {
        let mut native = native_lifecycle::StartContext::new(context.scope().clone(), None);
        Ok(self.inner.start(&mut native).await?)
    }
    async fn stop(&mut self, context: &drasi_lib::computation::v1::StopContext) -> Result<()> {
        let native = native_lifecycle::StopContext::new(context.scope().clone(), context.mode());
        Ok(self.inner.stop(&native).await?)
    }

    async fn initialize(
        &mut self,
        context: &drasi_lib::computation::v1::InitializeContext,
    ) -> Result<()> {
        let native = native_lifecycle::InitializeContext::new(context.scope().clone());
        Ok(self.inner.initialize(&native).await?)
    }
    /// The native source pauses at a transaction boundary while feedback
    /// keeps the replication connection alive.
    async fn pause(&mut self, context: &drasi_lib::computation::v1::PauseContext) -> Result<()> {
        let native = native_lifecycle::PauseContext::new(context.scope().clone());
        Ok(self.inner.pause(&native).await?)
    }
    async fn resume(&mut self, context: &drasi_lib::computation::v1::ResumeContext) -> Result<()> {
        let native = native_lifecycle::ResumeContext::new(context.scope().clone());
        Ok(self.inner.resume(&native).await?)
    }
    /// Erasing disposal drops the managed slots this source owns.
    async fn dispose(
        &mut self,
        context: &drasi_lib::computation::v1::DisposeContext,
    ) -> anyhow::Result<()> {
        let native = native_lifecycle::DisposeContext::new(
            context.scope().clone(),
            context.reason(),
            context.intent(),
            context.attempt(),
        )?;
        Ok(self.inner.dispose(&native).await?)
    }
}

#[async_trait]
impl EnvelopeSource for PostgresTransactionSource {
    fn recovery_reader(&self) -> Option<SourceProgressReader> {
        Some(self.progress.clone())
    }
    fn recovery_progress(&self) -> Option<Arc<QuerySourceProgress>> {
        self.progress.local_owner()
    }
    async fn next(&mut self) -> Result<Option<OutputEnvelope>> {
        self.inner.next().await
    }
}

pub struct PostgresSnapshot {
    inner: drasi_computation_bootstrap_postgres::PostgresSnapshot,
    source: ComponentId,
    stream: StreamId,
    config: PostgresTransactionConfig,
    progress: SourceProgressReader,
}

impl PostgresSnapshot {
    fn validate_source(&self, source: &PostgresTransactionSource) -> Result<()> {
        anyhow::ensure!(
            self.source == *source.descriptor().id()
                && self.stream == source.stream
                && self.progress.same_owner(&source.progress)
                && serde_json::to_value(&self.config)? == serde_json::to_value(&source.config)?,
            "PostgreSQL snapshot resource does not match the source and its actual progress owner"
        );
        Ok(())
    }
}

#[async_trait]
impl ComputationBootstrapProvider for PostgresSnapshot {
    fn recovery_reader(&self) -> Option<SourceProgressReader> {
        Some(self.progress.clone())
    }
    fn validate_query_scope(
        &self,
        instance_id: &str,
        graph_id: &str,
        component_id: &ComponentId,
    ) -> Result<()> {
        self.inner
            .validate_query_scope(instance_id, graph_id, component_id)
    }
    async fn prepare_with_state(&self, state: &dyn BootstrapState) -> Result<BootstrapPreparation> {
        self.inner.prepare_with_state(state).await
    }
    async fn snapshot(&self) -> Result<ComputationBootstrapSnapshot> {
        self.inner.snapshot().await
    }
    async fn snapshot_with_state(
        &self,
        state: &dyn BootstrapState,
    ) -> Result<ComputationBootstrapSnapshot> {
        self.inner.snapshot_with_state(state).await
    }
    async fn complete_snapshot(&self) -> Result<Vec<BootstrapWatermark>> {
        self.inner.complete_snapshot().await
    }
    fn completion_state(&self) -> Result<Option<bytes::Bytes>> {
        self.inner.completion_state()
    }
    async fn stop(&self) -> Result<()> {
        self.inner.stop().await
    }
}

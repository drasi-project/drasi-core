// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Test-only direct bindings of the plugin's whole-commit source and its
//! coordinated snapshot to the drasi-lib component traits. They let the suite
//! drive the native implementation on the actual local query progress owner
//! without a factory; graph tests that prove factory wiring use the shipped
//! plugin factory instead. The description is the plugin's own declaration
//! for `drasi/postgres-transactions`.

use anyhow::Result;
use async_trait::async_trait;
use drasi_computation_plugin_sdk::{
    lifecycle as native_lifecycle, DurableBootstrapProvider, NativeComponent, NativeSource,
};
use drasi_computation_source_postgres::{
    config::Mode, PostgresTransactionConfig,
    PostgresTransactionSource as NativePostgresTransactionSource,
};
use drasi_core::interface::StorageDurability;
use drasi_lib::computation::v1::{
    BootstrapPreparation, BootstrapState, BootstrapWatermark, ComponentDescription,
    ComponentDescriptor, ComponentId, ComponentRecovery, ComputationBootstrapProvider,
    ComputationBootstrapSnapshot, ComputationComponent, DisposeContext, EnvelopeSource,
    InitializeContext, OutputEnvelope, PauseContext, QuerySourceProgress, ResumeContext,
    SourceProgressReader, StartContext, StopContext, StreamId,
};
use std::sync::Arc;

pub struct PostgresTransactionSource {
    inner: NativePostgresTransactionSource,
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
            inner: NativePostgresTransactionSource::new(
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

    /// A source and the snapshot it hands its managed slot over to.
    pub fn coordinated(
        id: ComponentId,
        stream: StreamId,
        config: PostgresTransactionConfig,
        progress: impl Into<SourceProgressReader>,
    ) -> Result<(Self, Arc<PostgresSnapshot>)> {
        let progress = progress.into();
        let snapshot = Arc::new(PostgresSnapshot {
            inner: drasi_computation_source_postgres::bootstrap::PostgresSnapshot::new(
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
        snapshot.validate_source(&source)?;
        source.inner = NativePostgresTransactionSource::new(
            source.inner.descriptor().id().clone(),
            source.stream.clone(),
            source.config.clone(),
            source.progress.read_only(),
            true,
        )?;
        source.snapshot = Some(snapshot.clone());
        Ok((source, snapshot))
    }
}

#[async_trait]
impl ComputationComponent for PostgresTransactionSource {
    /// The loaded plugin's declaration for the same mode: whole commits
    /// whose cursors require durable query progress.
    fn description(&self) -> ComponentDescription {
        drasi_computation_source_postgres::description(
            Mode::Transactions,
            true,
            self.inner.descriptor(),
        )
    }
    fn descriptor(&self) -> &ComponentDescriptor {
        self.inner.descriptor()
    }
    fn recovery_contract(&self) -> ComponentRecovery {
        ComponentRecovery::admitted(StorageDurability::LOCAL_PROCESS_RESTART)
            .replay_until(self.progress.component_id().clone())
    }
    /// The plugin factory's flat configuration for the same source.
    fn configuration(&self) -> Result<serde_json::Value> {
        let mut value = serde_json::to_value(&self.config)?;
        let object = value
            .as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("PostgreSQL settings are not an object"))?;
        object.insert("stream".into(), self.stream.as_str().into());
        object.insert("coordinatedSnapshot".into(), self.snapshot.is_some().into());
        Ok(value)
    }
    async fn start(&mut self, context: &StartContext) -> Result<()> {
        let mut native = native_lifecycle::StartContext::new(context.scope().clone(), None);
        Ok(self.inner.start(&mut native).await?)
    }
    async fn stop(&mut self, context: &StopContext) -> Result<()> {
        let native = native_lifecycle::StopContext::new(context.scope().clone(), context.mode());
        Ok(self.inner.stop(&native).await?)
    }
    async fn initialize(&mut self, context: &InitializeContext) -> Result<()> {
        let native = native_lifecycle::InitializeContext::new(context.scope().clone());
        Ok(self.inner.initialize(&native).await?)
    }
    /// The native source pauses at a transaction boundary while feedback
    /// keeps the replication connection alive.
    async fn pause(&mut self, context: &PauseContext) -> Result<()> {
        let native = native_lifecycle::PauseContext::new(context.scope().clone());
        Ok(self.inner.pause(&native).await?)
    }
    async fn resume(&mut self, context: &ResumeContext) -> Result<()> {
        let native = native_lifecycle::ResumeContext::new(context.scope().clone());
        Ok(self.inner.resume(&native).await?)
    }
    /// Erasing disposal drops the managed slots this source owns.
    async fn dispose(&mut self, context: &DisposeContext) -> Result<()> {
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

/// The plugin's coordinated snapshot as a query bootstrap provider.
pub struct PostgresSnapshot {
    inner: drasi_computation_source_postgres::bootstrap::PostgresSnapshot,
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
        self.inner.prepare(Some(state)).await
    }
    async fn snapshot(&self) -> Result<ComputationBootstrapSnapshot> {
        self.inner.snapshot(None).await
    }
    async fn snapshot_with_state(
        &self,
        state: &dyn BootstrapState,
    ) -> Result<ComputationBootstrapSnapshot> {
        self.inner.snapshot(Some(state)).await
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

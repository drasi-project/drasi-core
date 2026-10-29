// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::{MySqlConfig, MySqlStartPosition};
use anyhow::{Context, Result};
use async_trait::async_trait;
use bytes::Bytes;
use drasi_lib::computation::v1::*;
use drasi_mysql_common::native::MySqlSnapshot as Snapshot;
use std::sync::Arc;

/// The Rust boundary retains the actual query owner; the shared engine only
/// receives its read capability.
pub struct MySqlSnapshot {
    pub(super) inner: Arc<Snapshot>,
    source: ComponentId,
    stream: StreamId,
    config: MySqlConfig,
    progress: SourceProgressReader,
    shared: bool,
}

impl MySqlSnapshot {
    pub fn new(
        source: ComponentId,
        stream: StreamId,
        config: MySqlConfig,
        progress: impl Into<SourceProgressReader>,
    ) -> Result<Self> {
        let progress = progress.into();
        let inner = Arc::new(Snapshot::new(
            source.clone(),
            stream.clone(),
            config.clone(),
            progress.read_only(),
        )?);
        Ok(Self {
            inner,
            source,
            stream,
            config,
            progress,
            shared: false,
        })
    }

    pub fn for_consumer_group(
        source: ComponentId,
        stream: StreamId,
        config: MySqlConfig,
        consumers: &Arc<ConsumerProgressSet>,
        consumer: &ComponentId,
    ) -> Result<Self> {
        anyhow::ensure!(
            consumers.producer_id() == &source,
            "MySQL bootstrap consumer set belongs to another producer"
        );
        let owner = consumers
            .owners()
            .get(consumer)
            .context("MySQL bootstrap consumer is not a member of its producer group")?;
        let progress: SourceProgressReader = owner.clone().into();
        let inner = Arc::new(Snapshot::for_consumer(
            source.clone(),
            stream.clone(),
            config.clone(),
            progress.read_only(),
        )?);
        Ok(Self {
            inner,
            source,
            stream,
            config,
            progress,
            shared: true,
        })
    }

    pub fn consumer_id(&self) -> &ComponentId {
        self.progress.component_id()
    }

    pub fn for_row_consumer(
        source: ComponentId,
        stream: StreamId,
        config: MySqlConfig,
        consumers: &Arc<ConsumerProgressSet>,
        consumer: &ComponentId,
    ) -> Result<Self> {
        anyhow::ensure!(
            consumers.producer_id() == &source,
            "MySQL row bootstrap belongs to another producer"
        );
        let owner = consumers
            .owners()
            .get(consumer)
            .context("MySQL row bootstrap owner is missing")?;
        let progress: SourceProgressReader = owner.clone().into();
        let inner = Arc::new(Snapshot::shared_rows(
            source.clone(),
            stream.clone(),
            config.clone(),
            progress.read_only(),
        )?);
        Ok(Self {
            inner,
            source,
            stream,
            config,
            progress,
            shared: true,
        })
    }

    pub(super) fn validate_source(
        &self,
        source: &ComponentId,
        stream: &StreamId,
        config: &MySqlConfig,
        progress: &SourceProgressReader,
    ) -> Result<()> {
        anyhow::ensure!(
            !self.shared
                && &self.source == source
                && &self.stream == stream
                && self.progress.same_owner(progress)
                && serde_json::to_value(&self.config)? == serde_json::to_value(config)?,
            "MySQL snapshot does not match its source and actual consumer"
        );
        Ok(())
    }

    pub(super) fn validate_consumer_source(
        &self,
        source: &ComponentId,
        stream: &StreamId,
        config: &MySqlConfig,
        consumers: &ConsumerProgressSet,
    ) -> Result<()> {
        anyhow::ensure!(
            self.shared
                && &self.source == source
                && &self.stream == stream
                && consumers.producer_id() == source
                && consumers.graph_id() == self.progress.graph_id()
                && self
                    .progress
                    .local_owner()
                    .is_some_and(|owner| consumers.owns(&owner))
                && serde_json::to_value(&self.config)? == serde_json::to_value(config)?
                && matches!(config.start, MySqlStartPosition::Snapshot { .. }),
            "MySQL snapshot does not match its shared source and actual consumer"
        );
        Ok(())
    }
}

#[async_trait]
impl ComputationBootstrapProvider for MySqlSnapshot {
    fn validate_query_scope(&self, instance: &str, graph: &str, query: &ComponentId) -> Result<()> {
        self.inner.validate_query_scope(instance, graph, query)
    }
    fn recovery_reader(&self) -> Option<SourceProgressReader> {
        Some(self.progress.clone())
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
    fn completion_state(&self) -> Result<Option<Bytes>> {
        self.inner.completion_state()
    }
    async fn stop(&self) -> Result<()> {
        self.inner.stop().await
    }
}

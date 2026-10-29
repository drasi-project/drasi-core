// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use bytes::Bytes;
use drasi_sqlite_common::native::consumer_snapshot::{
    SqliteConsumerSnapshot as SnapshotEngine, SqliteConsumerSnapshots as SnapshotGroup,
};
use tokio::sync::OwnedMutexGuard;
use uuid::Uuid;

pub struct SqliteConsumerSnapshots {
    source: ComponentId,
    stream: StreamId,
    config: SqliteConfig,
    consumers: Arc<ConsumerProgressSet>,
    inner: SnapshotGroup,
}

impl SqliteConsumerSnapshots {
    pub(super) fn new(source: &SqliteSource) -> Result<Self> {
        let consumers = source
            .consumer_progress
            .clone()
            .context("SQLite snapshot requires consumer progress")?;
        let readers = consumers
            .owners()
            .iter()
            .map(|(id, owner)| {
                (
                    id.clone(),
                    SourceProgressReader::from(owner.clone()).read_only(),
                )
            })
            .collect();
        Ok(Self {
            source: source.descriptor.id().clone(),
            stream: source.stream.clone(),
            config: source.config.clone(),
            inner: SnapshotGroup::new(
                source.descriptor.id().clone(),
                source.stream.clone(),
                source.config.clone(),
                consumers.read_only(),
                readers,
            )?,
            consumers,
        })
    }
    pub fn for_consumer(&self, consumer: &ComponentId) -> Result<Arc<SqliteConsumerSnapshot>> {
        let owner = self
            .consumers
            .owners()
            .get(consumer)
            .context("consumer is not in the SQLite snapshot group")?
            .clone();
        Ok(Arc::new(SqliteConsumerSnapshot {
            inner: self.inner.for_consumer(consumer)?,
            owner,
        }))
    }
    pub(super) fn validate_source(&self, source: &SqliteSource) -> Result<()> {
        anyhow::ensure!(
            self.source == *source.descriptor.id()
                && self.stream == source.stream
                && self.config == source.config
                && source
                    .consumer_progress
                    .as_ref()
                    .is_some_and(|owners| Arc::ptr_eq(owners, &self.consumers)),
            "SQLite shared snapshots require their actual source and consumer set"
        );
        Ok(())
    }
    pub(super) fn bind_client(&self, owner: &Arc<tokio::sync::OwnedSemaphorePermit>) -> Result<()> {
        self.inner.bind_client(owner)
    }
    pub(super) fn live_boundary(
        &self,
        progress: &ConsumerProgressSnapshot,
    ) -> Result<(Uuid, OwnedMutexGuard<()>)> {
        self.inner.live_boundary(progress)
    }
}

pub struct SqliteConsumerSnapshot {
    inner: Arc<SnapshotEngine>,
    owner: Arc<QuerySourceProgress>,
}

#[async_trait]
impl ComputationBootstrapProvider for SqliteConsumerSnapshot {
    fn validate_query_scope(
        &self,
        instance: &str,
        graph: &str,
        component: &ComponentId,
    ) -> Result<()> {
        self.inner.validate_query_scope(instance, graph, component)
    }
    fn recovery_reader(&self) -> Option<SourceProgressReader> {
        Some(SourceProgressReader::from(self.owner.clone()))
    }
    async fn prepare_with_state(&self, state: &dyn BootstrapState) -> Result<BootstrapPreparation> {
        self.inner.prepare_with_state(state).await
    }
    async fn snapshot(&self) -> Result<ComputationBootstrapSnapshot> {
        anyhow::bail!("SQLite shared snapshot requires query-owned initialization state")
    }
    async fn snapshot_with_state(
        &self,
        state: &dyn BootstrapState,
    ) -> Result<ComputationBootstrapSnapshot> {
        let reader = self.inner.snapshot_with_state(state).await?;
        Ok(ComputationBootstrapSnapshot {
            changes: Box::pin(futures::stream::try_unfold(
                reader,
                |mut reader| async move { Ok(reader.next().await?.map(|change| (change, reader))) },
            )),
            watermarks: Vec::new(),
        })
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

#[cfg(test)]
mod tests;

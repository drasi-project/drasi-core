// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use bytes::Bytes;
use drasi_sqlite_common::native::snapshot::SqliteSnapshot as SnapshotEngine;
use uuid::Uuid;

pub struct SqliteSnapshot {
    source: ComponentId,
    stream: StreamId,
    config: SqliteConfig,
    progress: SourceProgressReader,
    inner: SnapshotEngine,
}

impl SqliteSnapshot {
    pub(super) fn new(source: &SqliteSource) -> Result<Self> {
        let progress = source
            .progress
            .clone()
            .context("SQLite snapshot requires persistent progress")?;
        Ok(Self {
            source: source.descriptor.id().clone(),
            stream: source.stream.clone(),
            config: source.config.clone(),
            inner: SnapshotEngine::new(
                source.descriptor.id().clone(),
                source.stream.clone(),
                source.config.clone(),
                progress.read_only(),
            )?,
            progress,
        })
    }

    pub(super) fn validate_source(&self, source: &SqliteSource) -> Result<()> {
        anyhow::ensure!(
            self.source == *source.descriptor.id()
                && self.stream == source.stream
                && self.config == source.config
                && source
                    .progress
                    .as_ref()
                    .is_some_and(|owner| owner.same_owner(&self.progress)),
            "native SQLite snapshot does not match its source and actual progress owner"
        );
        Ok(())
    }
    pub(super) fn bind_client(&self, owner: &Arc<tokio::sync::OwnedSemaphorePermit>) -> Result<()> {
        self.inner.bind_client(owner)
    }
    pub(super) fn live_boundary(
        &self,
        progress: &SourceProgressSnapshot,
    ) -> Result<(Uuid, tokio::sync::OwnedMutexGuard<()>)> {
        self.inner.live_boundary(progress)
    }
}

#[async_trait]
impl ComputationBootstrapProvider for SqliteSnapshot {
    async fn prepare_with_state(&self, state: &dyn BootstrapState) -> Result<BootstrapPreparation> {
        self.inner.prepare_with_state(state).await
    }
    async fn snapshot(&self) -> Result<ComputationBootstrapSnapshot> {
        anyhow::bail!("native SQLite snapshot requires query-owned handover state")
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

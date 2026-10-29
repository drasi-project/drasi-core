// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::{MySqlConfig, MySqlOutput, MySqlSnapshot};
use anyhow::{Context, Result};
use async_trait::async_trait;
use drasi_lib::computation::v1::*;
use drasi_mysql_common::native::{
    progress::ConsumerBinding,
    source::{MySqlSource as Source, SourceError as NativeSourceError},
};
use std::sync::Arc;

pub struct MySqlSource {
    inner: Source,
    progress: Option<SourceProgressReader>,
    consumers: Option<Arc<ConsumerProgressSet>>,
}

impl MySqlSource {
    pub fn new(
        id: ComponentId,
        stream: StreamId,
        config: MySqlConfig,
        progress: Option<Arc<QuerySourceProgress>>,
    ) -> Result<Self> {
        Self::new_with_progress_reader(id, stream, config, progress.map(Into::into))
    }
    pub fn new_with_progress_reader(
        id: ComponentId,
        stream: StreamId,
        config: MySqlConfig,
        progress: Option<SourceProgressReader>,
    ) -> Result<Self> {
        let inner = Source::new(
            id,
            stream,
            config,
            progress.as_ref().map(SourceProgressReader::read_only),
        )?;
        Ok(Self {
            inner,
            progress,
            consumers: None,
        })
    }
    pub fn coordinated(
        id: ComponentId,
        stream: StreamId,
        config: MySqlConfig,
        progress: impl Into<SourceProgressReader>,
    ) -> Result<(Self, Arc<MySqlSnapshot>)> {
        let progress = progress.into();
        let snapshot = Arc::new(MySqlSnapshot::new(
            id.clone(),
            stream.clone(),
            config.clone(),
            progress.clone(),
        )?);
        let source = Self::with_snapshot(id, stream, config, progress, snapshot.clone())?;
        Ok((source, snapshot))
    }
    pub fn with_snapshot(
        id: ComponentId,
        stream: StreamId,
        config: MySqlConfig,
        progress: impl Into<SourceProgressReader>,
        snapshot: Arc<MySqlSnapshot>,
    ) -> Result<Self> {
        let progress = progress.into();
        snapshot.validate_source(&id, &stream, &config, &progress)?;
        let inner = Source::with_snapshot(
            id,
            stream,
            config,
            snapshot
                .inner
                .recovery_reader()
                .context("snapshot reader")?,
            snapshot.inner.clone(),
        )?;
        Ok(Self {
            inner,
            progress: Some(progress),
            consumers: None,
        })
    }
    pub fn new_with_consumer_progress(
        id: ComponentId,
        stream: StreamId,
        config: MySqlConfig,
        consumers: Arc<ConsumerProgressSet>,
    ) -> Result<Self> {
        let binding = ConsumerBinding::new(
            &id,
            consumers.read_only(),
            &consumers.owners().keys().cloned().collect::<Vec<_>>(),
        )?;
        let inner = Source::new_with_consumer_progress(id, stream, config, binding)?;
        Ok(Self {
            inner,
            progress: None,
            consumers: Some(consumers),
        })
    }
    pub fn coordinated_consumers(
        id: ComponentId,
        stream: StreamId,
        config: MySqlConfig,
        consumers: Arc<ConsumerProgressSet>,
    ) -> Result<(Self, Vec<Arc<MySqlSnapshot>>)> {
        let snapshots = consumers
            .owners()
            .keys()
            .map(|consumer| {
                MySqlSnapshot::for_consumer_group(
                    id.clone(),
                    stream.clone(),
                    config.clone(),
                    &consumers,
                    consumer,
                )
                .map(Arc::new)
            })
            .collect::<Result<Vec<_>>>()?;
        let source = Self::with_consumer_snapshots(
            id,
            stream,
            config,
            consumers,
            snapshots.iter().cloned(),
        )?;
        Ok((source, snapshots))
    }
    pub fn with_consumer_snapshots(
        id: ComponentId,
        stream: StreamId,
        config: MySqlConfig,
        consumers: Arc<ConsumerProgressSet>,
        snapshots: impl IntoIterator<Item = Arc<MySqlSnapshot>>,
    ) -> Result<Self> {
        let mut providers = Vec::new();
        for snapshot in snapshots {
            snapshot.validate_consumer_source(&id, &stream, &config, &consumers)?;
            providers.push(snapshot.inner.clone());
        }
        let binding = ConsumerBinding::new(
            &id,
            consumers.read_only(),
            &consumers.owners().keys().cloned().collect::<Vec<_>>(),
        )?;
        let inner = Source::with_consumer_snapshots(id, stream, config, binding, providers)?;
        Ok(Self {
            inner,
            progress: None,
            consumers: Some(consumers),
        })
    }
    pub fn describe(id: ComponentId, output: MySqlOutput) -> Result<ComponentDescriptor> {
        Source::describe(id, output)
    }

    pub fn shared_rows(
        id: ComponentId,
        stream: StreamId,
        config: MySqlConfig,
        consumers: Arc<ConsumerProgressSet>,
        persistent: bool,
    ) -> Result<Self> {
        let binding = ConsumerBinding::new(
            &id,
            consumers.read_only(),
            &consumers.owners().keys().cloned().collect::<Vec<_>>(),
        )?;
        let inner = Source::shared_rows(
            id,
            stream,
            config,
            binding,
            drasi_mysql_common::native::rows::Options { persistent },
        )?;
        Ok(Self {
            inner,
            progress: None,
            consumers: Some(consumers),
        })
    }
}

fn source_error(error: anyhow::Error) -> anyhow::Error {
    if let Some(NativeSourceError::PositionUnavailable {
        source_id,
        requested,
        earliest_available,
    }) = error.downcast_ref::<NativeSourceError>()
    {
        let boundary = drasi_lib::sources::SourceError::PositionUnavailable {
            source_id: source_id.clone(),
            requested: requested.clone(),
            earliest_available: earliest_available.clone(),
        };
        error.context(boundary)
    } else {
        error
    }
}

#[async_trait]
impl ComputationComponent for MySqlSource {
    fn descriptor(&self) -> &ComponentDescriptor {
        self.inner.descriptor()
    }
    fn source_schema_reader(&self) -> Option<Arc<dyn SourceSchemaReader>> {
        Some(self.inner.schema_reader())
    }
    fn configuration(&self) -> Result<serde_json::Value> {
        self.inner.configuration()
    }
    fn recovery_contract(&self) -> ComponentRecovery {
        let contract = self
            .inner
            .replay_retention()
            .map(ComponentRecovery::admitted)
            .unwrap_or_default();
        if let Some(consumers) = &self.consumers {
            contract.with_consumer_positions(
                consumers.clone(),
                self.inner
                    .position_policy()
                    .expect("consumer position policy"),
            )
        } else if let (Some(_), Some(progress)) = (self.inner.replay_retention(), &self.progress) {
            contract.replay_until(progress.component_id().clone())
        } else {
            contract
        }
    }
    async fn start(&mut self) -> Result<()> {
        self.inner.start().await.map_err(source_error)
    }
    async fn stop(&mut self) -> Result<()> {
        self.inner.stop().await.map_err(source_error)
    }
}
#[async_trait]
impl EnvelopeSource for MySqlSource {
    fn recovery_reader(&self) -> Option<SourceProgressReader> {
        self.progress.clone()
    }
    fn recovery_progress(&self) -> Option<Arc<QuerySourceProgress>> {
        self.progress
            .as_ref()
            .and_then(SourceProgressReader::local_owner)
    }
    async fn next(&mut self) -> Result<Option<OutputEnvelope>> {
        self.inner.next().await.map_err(source_error)
    }
}

// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Native SQLite execution. The legacy source and its ABI are unchanged.

use anyhow::{Context, Result};
use async_trait::async_trait;
use drasi_lib::computation::v1::*;
use serde::Deserialize;
use std::{
    net::SocketAddr,
    sync::{atomic::AtomicU64, Arc},
};
#[cfg(test)]
use std::{
    num::{NonZeroU64, NonZeroUsize},
    time::Duration,
};
use tokio::sync::{mpsc, RwLock};

mod consumer_snapshot;
mod factory;
mod snapshot;
#[cfg(test)]
mod tests;
pub use consumer_snapshot::{SqliteConsumerSnapshot, SqliteConsumerSnapshots};
use drasi_sqlite_common::native::runtime::{SqliteRuntime, StartContext};
use drasi_sqlite_common::native::{handle, journal, progress};
pub use drasi_sqlite_common::native::{
    SqliteConfig, SqliteOutput, SqliteParam, SqliteReplayConfig, SqliteRow,
};
pub use factory::SqliteSourceFactory;
pub use handle::{SqliteHandle, SqliteTransactionHandle};
pub use snapshot::SqliteSnapshot;

/// Explicit application access to a factory-created source. A client can be bound
/// to only one source owner, including that owner's unfinished database worker.
pub struct SqliteClientResource {
    id: ComponentId,
    config: SqliteConfig,
    commands: Arc<RwLock<Option<mpsc::Sender<handle::Command>>>>,
    owner: Arc<tokio::sync::Semaphore>,
}

impl SqliteClientResource {
    pub fn new(id: ComponentId, config: SqliteConfig) -> Result<Self> {
        config.validate()?;
        Ok(Self {
            id,
            config,
            commands: Arc::new(RwLock::new(None)),
            owner: Arc::new(tokio::sync::Semaphore::new(1)),
        })
    }
    pub fn handle(&self) -> SqliteHandle {
        SqliteHandle::new(self.commands.clone(), &self.config)
    }
}

/// Direct ComputationGraph source with a single owned SQLite connection.
pub struct SqliteSource {
    descriptor: ComponentDescriptor,
    stream: StreamId,
    config: SqliteConfig,
    commands: Arc<RwLock<Option<mpsc::Sender<handle::Command>>>>,
    runtime: SqliteRuntime,
    rest_api: Option<crate::RestApiConfig>,
    cleanup_required: bool,
    sequence: Arc<AtomicU64>,
    progress: Option<SourceProgressReader>,
    consumer_progress: Option<Arc<ConsumerProgressSet>>,
    position_policy: Option<Arc<InputPositionPolicy>>,
    client_owner: Option<Arc<tokio::sync::OwnedSemaphorePermit>>,
    snapshot: Option<Arc<SqliteSnapshot>>,
    consumer_snapshots: Option<Arc<SqliteConsumerSnapshots>>,
}

impl SqliteSource {
    pub fn new(id: ComponentId, stream: StreamId, config: SqliteConfig) -> Result<Self> {
        anyhow::ensure!(
            config.replay.is_none(),
            "SQLite replay requires new_replayable and actual consumer progress"
        );
        Self::build(id, stream, config, None)
    }

    pub fn new_replayable(
        id: ComponentId,
        stream: StreamId,
        config: SqliteConfig,
        progress: impl Into<SourceProgressReader>,
    ) -> Result<Self> {
        anyhow::ensure!(
            config.replay.is_some()
                && config.path.is_some()
                && config.output == SqliteOutput::Transactions,
            "SQLite replay requires a file, journal bounds and complete-transaction output"
        );
        Self::build(id, stream, config, Some(progress.into()))
    }

    /// Replay one journal against every actual persistent consumer. This retains
    /// the immutable owner set without reading progress or opening SQLite.
    pub fn new_replayable_with_consumers(
        id: ComponentId,
        stream: StreamId,
        config: SqliteConfig,
        consumers: Arc<ConsumerProgressSet>,
    ) -> Result<Self> {
        anyhow::ensure!(
            config.replay.is_some()
                && config.path.is_some()
                && config.output == SqliteOutput::Transactions,
            "SQLite group replay requires a file, journal bounds and complete-transaction output"
        );
        anyhow::ensure!(
            consumers.producer_id() == &id,
            "SQLite consumer set belongs to another producer"
        );
        let policy = journal::position_policy(&id)?;
        let mut source = Self::build(id, stream, config, None)?;
        source.consumer_progress = Some(consumers);
        source.position_policy = Some(policy);
        Ok(source)
    }

    /// Initialize existing rows before admitting SQL writes. Attach the returned
    /// provider to this consumer with `with_bootstrap`; it uses query-owned state.
    pub fn coordinated(
        id: ComponentId,
        stream: StreamId,
        config: SqliteConfig,
        progress: impl Into<SourceProgressReader>,
    ) -> Result<(Self, Arc<SqliteSnapshot>)> {
        let mut source = Self::new_replayable(id, stream, config, progress)?;
        let snapshot = Arc::new(SqliteSnapshot::new(&source)?);
        source.snapshot = Some(snapshot.clone());
        Ok((source, snapshot))
    }

    /// Pair each consumer with its own durable initialization, using one shared
    /// snapshot-worker slot before the live SQL worker may admit writes.
    pub fn coordinated_with_consumers(
        id: ComponentId,
        stream: StreamId,
        config: SqliteConfig,
        consumers: Arc<ConsumerProgressSet>,
    ) -> Result<(Self, Arc<SqliteConsumerSnapshots>)> {
        let mut source = Self::new_replayable_with_consumers(id, stream, config, consumers)?;
        let snapshots = Arc::new(SqliteConsumerSnapshots::new(&source)?);
        source.consumer_snapshots = Some(snapshots.clone());
        Ok((source, snapshots))
    }

    fn build(
        id: ComponentId,
        stream: StreamId,
        config: SqliteConfig,
        progress: Option<SourceProgressReader>,
    ) -> Result<Self> {
        config.validate()?;
        let commands = Arc::new(RwLock::new(None));
        let runtime = SqliteRuntime::new(commands.clone(), config.timeout());
        Ok(Self {
            descriptor: Self::describe(id, config.output)?,
            stream,
            config,
            commands,
            runtime,
            rest_api: None,
            cleanup_required: false,
            sequence: Arc::new(AtomicU64::new(0)),
            progress,
            consumer_progress: None,
            position_policy: None,
            client_owner: None,
            snapshot: None,
            consumer_snapshots: None,
        })
    }

    pub fn describe(id: ComponentId, output: SqliteOutput) -> Result<ComponentDescriptor> {
        let schema = match output {
            SqliteOutput::Changes => GraphChangeCodec::schema(),
            SqliteOutput::Transactions => SourceTransactionCodec::schema(),
        };
        Ok(ComponentDescriptor::try_new(
            id,
            vec![PortDescriptor::new(
                PortId::try_new("out")?,
                PortDirection::Output,
                schema.descriptor().clone(),
                PipeRequirements::default(),
            )],
        )?)
    }

    pub fn handle(&self) -> SqliteHandle {
        SqliteHandle::new(self.commands.clone(), &self.config)
    }

    pub fn with_rest_api(mut self, config: crate::RestApiConfig) -> Result<Self> {
        anyhow::ensure!(
            !self.cleanup_required && self.rest_api.is_none(),
            "native SQLite REST must be configured once before startup"
        );
        config.validate()?;
        self.rest_api = Some(config);
        Ok(self)
    }

    pub fn bound_rest_address(&self) -> Option<SocketAddr> {
        self.runtime.bound_rest_address()
    }

    pub fn with_client_resource(mut self, client: Arc<SqliteClientResource>) -> Result<Self> {
        anyhow::ensure!(
            self.client_owner.is_none() && !self.cleanup_required,
            "native SQLite client must be bound before source startup"
        );
        anyhow::ensure!(
            client.id == *self.descriptor.id() && client.config == self.config,
            "native SQLite client source/configuration mismatch"
        );
        self.client_owner =
            Some(Arc::new(client.owner.clone().try_acquire_owned().context(
                "native SQLite client still belongs to another source owner",
            )?));
        if let (Some(snapshot), Some(owner)) = (&self.snapshot, &self.client_owner) {
            snapshot.bind_client(owner)?;
        }
        if let (Some(snapshots), Some(owner)) = (&self.consumer_snapshots, &self.client_owner) {
            snapshots.bind_client(owner)?;
        }
        self.runtime.bind_commands(client.commands.clone())?;
        self.commands = client.commands.clone();
        Ok(self)
    }
}

#[async_trait]
impl ComputationComponent for SqliteSource {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    fn source_schema_reader(&self) -> Option<Arc<dyn SourceSchemaReader>> {
        Some(self.runtime.source_schema_reader())
    }
    fn recovery_contract(&self) -> ComponentRecovery {
        if let Some(consumers) = &self.consumer_progress {
            return ComponentRecovery::admitted(
                drasi_core::interface::StorageDurability::LOCAL_PROCESS_RESTART,
            )
            .with_consumer_positions(
                consumers.clone(),
                self.position_policy.as_ref().expect("group policy").clone(),
            );
        }
        match &self.progress {
            Some(progress) => ComponentRecovery::admitted(
                drasi_core::interface::StorageDurability::LOCAL_PROCESS_RESTART,
            )
            .replay_until(progress.component_id().clone()),
            None => ComponentRecovery::default(),
        }
    }
    fn configuration(&self) -> Result<serde_json::Value> {
        let mut configuration = serde_json::json!({
            "stream": self.stream.as_str(), "settings": self.config,
            "coordinated_snapshot": self.snapshot.is_some() || self.consumer_snapshots.is_some(),
        });
        if let Some(rest) = &self.rest_api {
            configuration["rest_api"] = serde_json::to_value(rest)?;
        }
        Ok(configuration)
    }
    async fn start(&mut self) -> Result<()> {
        anyhow::ensure!(
            !self.cleanup_required,
            "native SQLite source requires awaited cleanup before restart"
        );
        self.cleanup_required = true;
        let mut initialization_lease = None;
        let progress = match &self.progress {
            Some(progress) => {
                let snapshot = progress.wait_ready().await?;
                anyhow::ensure!(
                    snapshot.persistent,
                    "native SQLite replay requires persistent consumer progress"
                );
                self.sequence.fetch_max(
                    snapshot
                        .transport_sequences
                        .get(&self.stream)
                        .copied()
                        .unwrap_or(0),
                    std::sync::atomic::Ordering::Relaxed,
                );
                let bootstrap = if let Some(provider) = &self.snapshot {
                    let (epoch, lease) = provider.live_boundary(&snapshot)?;
                    initialization_lease = Some(lease);
                    Some(epoch)
                } else {
                    None
                };
                Some((
                    progress::Reader::Single(progress.read_only()),
                    progress::Observed::Single(snapshot),
                    bootstrap,
                ))
            }
            None => match &self.consumer_progress {
                Some(consumers) => {
                    let reader = consumers.read_only();
                    let snapshot = reader.wait_ready().await?;
                    for (consumer, state) in &snapshot.consumers {
                        anyhow::ensure!(state.persistent && state.recovered,
                            "native SQLite replay requires persistent recovered progress for consumer {consumer}");
                        self.sequence.fetch_max(
                            state
                                .transport_sequences
                                .get(&self.stream)
                                .copied()
                                .unwrap_or(0),
                            std::sync::atomic::Ordering::Relaxed,
                        );
                    }
                    let bootstrap = if let Some(snapshots) = &self.consumer_snapshots {
                        let (epoch, lease) = snapshots.live_boundary(&snapshot)?;
                        initialization_lease = Some(lease);
                        Some(epoch)
                    } else {
                        None
                    };
                    Some((
                        progress::Reader::Group(reader),
                        progress::Observed::Group(snapshot),
                        bootstrap,
                    ))
                }
                None => None,
            },
        };
        self.runtime
            .start(
                StartContext {
                    source: self.descriptor.id().clone(),
                    stream: self.stream.clone(),
                    config: self.config.clone(),
                    sequence: self.sequence.clone(),
                    rest_api: self.rest_api.clone(),
                    progress,
                },
                Arc::new((self.client_owner.clone(), initialization_lease)),
            )
            .await
    }
    async fn stop(&mut self) -> Result<()> {
        self.runtime.stop().await?;
        self.cleanup_required = false;
        Ok(())
    }
}

#[async_trait]
impl EnvelopeSource for SqliteSource {
    fn recovery_reader(&self) -> Option<SourceProgressReader> {
        self.progress.clone()
    }
    fn recovery_progress(&self) -> Option<Arc<QuerySourceProgress>> {
        self.progress
            .as_ref()
            .and_then(SourceProgressReader::local_owner)
    }
    async fn next(&mut self) -> Result<Option<OutputEnvelope>> {
        self.runtime.next().await
    }
}

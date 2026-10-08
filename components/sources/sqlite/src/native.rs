// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Native SQLite execution. The legacy source and its ABI are unchanged.

use anyhow::{Context, Result};
use async_trait::async_trait;
use drasi_lib::{
    computation::v1::*,
    context::workers::{
        join_owned_worker_gracefully, spawn_owned_blocking_worker, WorkerCompletion,
    },
};
use serde::{Deserialize, Serialize};
use std::{
    num::{NonZeroU64, NonZeroUsize},
    path::PathBuf,
    sync::{atomic::AtomicU64, Arc},
    time::Duration,
};
use tokio::{
    sync::{mpsc, oneshot, watch, RwLock},
    task::JoinHandle,
};

mod factory;
mod handle;
mod journal;
mod snapshot;
#[cfg(test)]
mod tests;
mod worker;
pub use crate::thread::SqliteParam;
pub use factory::SqliteSourceFactory;
pub use handle::{SqliteHandle, SqliteTransactionHandle};
pub use snapshot::SqliteSnapshot;
pub type SqliteRow = serde_json::Map<String, serde_json::Value>;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SqliteOutput {
    /// Ordinary graph changes. No durable source replay or atomic query visibility.
    Changes,
    /// Complete source transactions, for an explicitly transaction-enabled query.
    Transactions,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SqliteReplayConfig {
    pub max_transactions: NonZeroUsize,
    pub max_bytes: NonZeroUsize,
}

/// Explicit bounds apply to SQL admission, captured transactions and read results.
/// Neither output mode enables persistent delivery by itself.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SqliteConfig {
    pub path: Option<PathBuf>,
    /// Empty means all ordinary main-database tables.
    pub tables: Vec<String>,
    pub output: SqliteOutput,
    /// Opt-in source-owned transaction journal. Requires a file and actual
    /// persistent consumer progress, supplied to `new_replayable`.
    pub replay: Option<SqliteReplayConfig>,
    pub transactions: SourceTransactionLimits,
    pub max_sql_bytes: NonZeroUsize,
    pub command_capacity: NonZeroUsize,
    pub output_capacity: NonZeroUsize,
    pub shutdown_timeout_ms: NonZeroU64,
}

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

impl SqliteConfig {
    fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            self.command_capacity.get() <= tokio::sync::Semaphore::MAX_PERMITS
                && self.output_capacity.get() <= tokio::sync::Semaphore::MAX_PERMITS,
            "SQLite channel capacity exceeds Tokio's supported bound"
        );
        anyhow::ensure!(
            self.transactions.max_duration_ms.get() <= i32::MAX as u64,
            "SQLite transaction duration exceeds its busy-timeout bound"
        );
        anyhow::ensure!(
            self.command_capacity.get() >= 2,
            "SQLite command capacity must be at least two"
        );
        anyhow::ensure!(
            self.max_sql_bytes.get() <= i32::MAX as usize
                && self.transactions.max_bytes.get() <= i32::MAX as usize,
            "SQLite byte limits must fit signed 32-bit lengths"
        );
        anyhow::ensure!(
            self.tables.iter().all(|table| !table.is_empty()
                && !table.contains('\0')
                && !table.to_ascii_lowercase().starts_with("__drasi_")),
            "invalid native SQLite table name"
        );
        let unique: std::collections::BTreeSet<_> = self.tables.iter().collect();
        anyhow::ensure!(
            unique.len() == self.tables.len(),
            "duplicate native SQLite table"
        );
        tokio::time::Instant::now()
            .checked_add(self.transactions.duration())
            .context("SQLite transaction deadline is not representable")?;
        tokio::time::Instant::now()
            .checked_add(self.timeout())
            .context("SQLite cleanup deadline is not representable")?;
        Ok(())
    }
    fn timeout(&self) -> Duration {
        Duration::from_millis(self.shutdown_timeout_ms.get())
    }
}

/// Direct ComputationGraph source with a single owned SQLite connection.
pub struct SqliteSource {
    descriptor: ComponentDescriptor,
    stream: StreamId,
    config: SqliteConfig,
    commands: Arc<RwLock<Option<mpsc::Sender<handle::Command>>>>,
    receiver: Option<mpsc::Receiver<OutputEnvelope>>,
    shutdown: watch::Sender<bool>,
    worker: RwLock<Option<JoinHandle<Result<()>>>>,
    cleanup_required: bool,
    sequence: Arc<AtomicU64>,
    progress: Option<SourceProgressReader>,
    client_owner: Option<Arc<tokio::sync::OwnedSemaphorePermit>>,
    snapshot: Option<Arc<SqliteSnapshot>>,
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

    fn build(
        id: ComponentId,
        stream: StreamId,
        config: SqliteConfig,
        progress: Option<SourceProgressReader>,
    ) -> Result<Self> {
        config.validate()?;
        Ok(Self {
            descriptor: Self::describe(id, config.output)?,
            stream,
            config,
            commands: Arc::new(RwLock::new(None)),
            receiver: None,
            shutdown: watch::channel(false).0,
            worker: RwLock::new(None),
            cleanup_required: false,
            sequence: Arc::new(AtomicU64::new(0)),
            progress,
            client_owner: None,
            snapshot: None,
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
        self.commands = client.commands.clone();
        Ok(self)
    }

    async fn join(&mut self) -> Result<()> {
        match join_owned_worker_gracefully(self.worker.get_mut(), self.config.timeout()).await? {
            WorkerCompletion::Completed(result) => result,
            WorkerCompletion::Absent => Ok(()),
            WorkerCompletion::Cancelled => anyhow::bail!("native SQLite worker was cancelled"),
        }
    }
}

#[async_trait]
impl ComputationComponent for SqliteSource {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    fn recovery_contract(&self) -> ComponentRecovery {
        match &self.progress {
            Some(progress) => ComponentRecovery::admitted(
                drasi_core::interface::StorageDurability::LOCAL_PROCESS_RESTART,
            )
            .replay_until(progress.component_id().clone()),
            None => ComponentRecovery::default(),
        }
    }
    fn configuration(&self) -> Result<serde_json::Value> {
        Ok(serde_json::json!({
            "stream": self.stream.as_str(), "settings": self.config,
            "coordinated_snapshot": self.snapshot.is_some(),
        }))
    }
    async fn start(&mut self) -> Result<()> {
        anyhow::ensure!(
            !self.cleanup_required,
            "native SQLite source requires awaited cleanup before restart"
        );
        self.cleanup_required = true;
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
                let bootstrap = self
                    .snapshot
                    .as_ref()
                    .map(|provider| provider.live_epoch(&snapshot))
                    .transpose()?;
                Some((progress.clone(), snapshot, bootstrap))
            }
            None => None,
        };
        self.shutdown.send_replace(false);
        let (commands, receiver) = mpsc::channel(self.config.command_capacity.get());
        let (outputs, output_rx) = mpsc::channel(self.config.output_capacity.get());
        let (ready, readiness) = oneshot::channel();
        let config = self.config.clone();
        let source = self.descriptor.id().clone();
        let stream = self.stream.clone();
        let sequence = self.sequence.clone();
        let shutdown = self.shutdown.subscribe();
        let client_owner = self.client_owner.clone();
        self.receiver = Some(output_rx);
        spawn_owned_blocking_worker(&self.worker, move || {
            let _client_owner = client_owner;
            let result = worker::run(
                config, source, stream, sequence, receiver, outputs, shutdown, ready, progress,
            );
            if let Err(error) = &result {
                log::error!("Native SQLite worker failed: {error:#}");
            }
            result
        })
        .await?;
        if readiness.await.is_err() {
            self.join().await?;
            anyhow::bail!("native SQLite worker exited before readiness");
        }
        *self.commands.write().await = Some(commands);
        Ok(())
    }
    async fn stop(&mut self) -> Result<()> {
        self.commands.write().await.take();
        self.shutdown.send_replace(true);
        self.join().await?;
        self.receiver = None;
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
        let receiver = self
            .receiver
            .as_mut()
            .context("native SQLite source is not running")?;
        if let Some(output) = receiver.recv().await {
            return Ok(Some(output));
        }
        self.join().await?;
        anyhow::bail!("native SQLite worker ended; stop and restart the source")
    }
}

impl Drop for SqliteSource {
    fn drop(&mut self) {
        self.shutdown.send_replace(true);
        if self.worker.get_mut().is_some() {
            log::warn!("Native SQLite source dropped without awaited database cleanup");
        }
    }
}

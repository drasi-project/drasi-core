// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::{
    binlog::{Binlog, Position, Start},
    catalog::{self, field},
    connection::Connection,
    MySqlConfig, MySqlOutput, MySqlRetention, MySqlStartPosition,
};
use anyhow::{Context, Result};
use async_trait::async_trait;
use bytes::Bytes;
use drasi_core::interface::StorageDurability;
use drasi_lib::{
    computation::v1::{
        ComponentDescriptor, ComponentId, ComponentRecovery, ComputationComponent, EnvelopeSource,
        GraphChangeCodec, OutputEnvelope, PipeRequirements, PortDescriptor, PortDirection, PortId,
        QuerySourceProgress, SourceProgressKey, SourceProgressReader, SourceProgressSnapshot,
        SourceProgressUpdates, SourceTransactionCodec, SourceTransactionError, StreamId,
    },
    context::workers::{join_owned_worker_gracefully, spawn_owned_worker, WorkerCompletion},
    error::OperationFailures,
    sources::SourceError,
};
use sha2::{Digest, Sha256};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use tokio::{
    sync::{mpsc, watch, RwLock},
    task::JoinHandle,
};

pub struct MySqlSource {
    descriptor: ComponentDescriptor,
    stream: StreamId,
    config: MySqlConfig,
    progress: Option<SourceProgressReader>,
    receiver: Option<mpsc::Receiver<OutputEnvelope>>,
    shutdown: watch::Sender<bool>,
    worker: RwLock<Option<JoinHandle<Result<()>>>>,
    cleanup_required: bool,
    sequence: Arc<AtomicU64>,
    snapshot: Option<Arc<super::MySqlSnapshot>>,
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
        anyhow::ensure!(
            !matches!(config.start, MySqlStartPosition::Snapshot { .. }),
            "MySQL snapshot mode requires its paired bootstrap provider"
        );
        Self::build(id, stream, config, progress, None)
    }

    pub fn coordinated(
        id: ComponentId,
        stream: StreamId,
        config: MySqlConfig,
        progress: impl Into<SourceProgressReader>,
    ) -> Result<(Self, Arc<super::MySqlSnapshot>)> {
        let progress = progress.into();
        let snapshot = Arc::new(super::MySqlSnapshot::new(
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
        snapshot: Arc<super::MySqlSnapshot>,
    ) -> Result<Self> {
        let progress = progress.into();
        snapshot.validate_source(&id, &stream, &config, &progress)?;
        Self::build(id, stream, config, Some(progress), Some(snapshot))
    }

    fn build(
        id: ComponentId,
        stream: StreamId,
        config: MySqlConfig,
        progress: Option<SourceProgressReader>,
        snapshot: Option<Arc<super::MySqlSnapshot>>,
    ) -> Result<Self> {
        config.validate()?;
        anyhow::ensure!(
            config.replay.is_some() == progress.is_some(),
            "MySQL replay and actual consumer progress must be configured together"
        );
        Ok(Self {
            descriptor: Self::describe(id, config.output)?,
            stream,
            config,
            progress,
            receiver: None,
            shutdown: watch::channel(false).0,
            worker: RwLock::new(None),
            cleanup_required: false,
            sequence: Arc::new(AtomicU64::new(0)),
            snapshot,
        })
    }

    pub fn describe(id: ComponentId, output: MySqlOutput) -> Result<ComponentDescriptor> {
        let schema = match output {
            MySqlOutput::Changes => GraphChangeCodec::schema(),
            MySqlOutput::Transactions => SourceTransactionCodec::schema(),
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

    async fn prepare(&self, snapshot: Arc<SourceProgressSnapshot>) -> Result<Worker> {
        let progress = self
            .progress
            .as_ref()
            .map(SourceProgressReader::subscribe)
            .transpose()?;
        let mut connection = Connection::connect(
            &self.config.connection,
            self.config.max_protocol_bytes.get(),
            self.config.timeout(),
        )
        .await?;
        let prepared = self.prepare_connection(&mut connection, &snapshot).await;
        match prepared {
            Ok((decoder, binding, confirmed)) => Ok(Worker {
                connection,
                decoder,
                binding,
                confirmed,
                source: self.descriptor.id().clone(),
                progress,
                generation: snapshot.reset_generation,
            }),
            Err(error) => match connection.close().await {
                Ok(()) => Err(error),
                Err(cleanup) => Err(OperationFailures::new(
                    "MySQL preparation and socket cleanup failed",
                    vec![error, cleanup],
                )
                .into()),
            },
        }
    }

    async fn prepare_connection(
        &self,
        connection: &mut Connection,
        snapshot: &SourceProgressSnapshot,
    ) -> Result<(Binlog, [u8; 32], u64)> {
        if self.progress.is_some() {
            anyhow::ensure!(
                snapshot.persistent && snapshot.failure.is_none(),
                "MySQL replay requires persistent, unfenced consumer progress"
            );
        }
        let (server, tables) = catalog::inspect(connection, &self.config).await?;
        let binding = source_binding(
            &self.config,
            &server,
            &tables,
            self.descriptor.id(),
            &self.stream,
            self.progress.as_ref(),
        )?;
        let anchor = checkpoint(snapshot, self.descriptor.id(), binding)?;
        if let Some(provider) = &self.snapshot {
            provider.validate_live(snapshot, binding)?;
        }
        anyhow::ensure!(
            self.snapshot.is_some() || anchor.as_ref().is_none_or(|position| position.version == 1),
            "snapshot cursor requires its paired bootstrap provider"
        );
        let confirmed = anchor
            .as_ref()
            .map(Position::sequence)
            .transpose()?
            .unwrap_or(0);
        self.sequence.fetch_max(
            snapshot
                .transport_sequences
                .get(&self.stream)
                .copied()
                .unwrap_or(0),
            Ordering::Relaxed,
        );
        let (file, offset) = match &anchor {
            Some(position) => (
                position.file.clone(),
                if position.version == 2 {
                    position.end
                } else {
                    position.start
                },
            ),
            None => match &self.config.start {
                MySqlStartPosition::Position { file, position } => (file.clone(), *position),
                MySqlStartPosition::End => {
                    let statement = if connection.version >= (8, 4, 0) {
                        "SHOW BINARY LOG STATUS"
                    } else {
                        "SHOW MASTER STATUS"
                    };
                    let rows = connection.query(statement).await?;
                    let [row] = rows.values.as_slice() else {
                        anyhow::bail!("MySQL binary log is unavailable");
                    };
                    (field(row, 0)?.to_string(), field(row, 1)?.parse()?)
                }
                MySqlStartPosition::Snapshot { .. } => {
                    anyhow::bail!("MySQL snapshot has no committed boundary")
                }
            },
        };
        super::file_number(&file)?;
        let rows = connection.query("SHOW BINARY LOGS").await?;
        let mut retained = false;
        for row in &rows.values {
            if field(row, 0)? == file {
                retained = u64::from(offset) <= field(row, 1)?.parse::<u64>()?;
            }
        }
        if !retained {
            return Err(SourceError::PositionUnavailable {
                source_id: self.descriptor.id().as_str().into(),
                requested: Bytes::from(serde_json::to_vec(&MySqlStartPosition::Position {
                    file,
                    position: offset,
                })?),
                earliest_available: None,
            }
            .into());
        }
        connection
            .execute(&format!(
                "SET @master_heartbeat_period={}",
                self.config.connection.heartbeat_interval_ms.get() * 1_000_000
            ))
            .await?;
        let history =
            if let Some(position) = anchor.as_ref().filter(|position| position.version == 2) {
                let (format, digest) =
                    Binlog::snapshot_history(connection, &self.config, &file, offset).await?;
                anyhow::ensure!(
                    digest == position.digest,
                    "MySQL snapshot history has different contents"
                );
                Some(format)
            } else {
                None
            };
        if history.is_none() {
            connection
                .execute("SET @master_binlog_checksum='CRC32'")
                .await?;
            connection
                .start_binlog(self.config.connection.server_id.get(), &file, offset)
                .await?;
        }
        let mut decoder = Binlog::new(Start {
            source: self.descriptor.id().clone(),
            stream: self.stream.clone(),
            config: self.config.clone(),
            tables,
            binding,
            file,
            offset,
            anchor: anchor.filter(|position| position.version == 1),
            sequence: self.sequence.clone(),
        })?;
        if let Some(format) = history {
            decoder.after_snapshot(format, confirmed);
            return Ok((decoder, binding, confirmed));
        }
        // A sent dump request is not evidence that the server accepted the cursor.
        for _ in 0..2 {
            let frame = connection.event().await?;
            anyhow::ensure!(
                decoder.decode(&frame)?.is_empty(),
                "MySQL data arrived before startup completed"
            );
            if decoder.ready() {
                return Ok((decoder, binding, confirmed));
            }
        }
        anyhow::bail!("MySQL did not provide its replication format during startup")
    }

    async fn join(&mut self) -> Result<()> {
        match join_owned_worker_gracefully(self.worker.get_mut(), self.config.timeout()).await? {
            WorkerCompletion::Completed(result) => result,
            WorkerCompletion::Absent => Ok(()),
            WorkerCompletion::Cancelled => {
                anyhow::bail!("native MySQL worker was unexpectedly cancelled")
            }
        }
    }
}

#[async_trait]
impl ComputationComponent for MySqlSource {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    fn recovery_contract(&self) -> ComponentRecovery {
        match (&self.config.replay, &self.progress) {
            (Some(MySqlRetention::UntilProcessed), Some(progress)) => {
                ComponentRecovery::admitted(StorageDurability::LOCAL_PROCESS_RESTART)
                    .replay_until(progress.component_id().clone())
            }
            _ => ComponentRecovery::default(),
        }
    }
    fn configuration(&self) -> Result<serde_json::Value> {
        Ok(serde_json::json!({"stream": self.stream.as_str(), "settings": self.config}))
    }
    async fn start(&mut self) -> Result<()> {
        anyhow::ensure!(
            !self.cleanup_required,
            "MySQL source requires awaited cleanup before restart"
        );
        self.cleanup_required = true;
        let snapshot = match &self.progress {
            Some(progress) => progress.wait_ready().await?,
            None => Arc::new(SourceProgressSnapshot::default()),
        };
        let mut worker = tokio::time::timeout(self.config.timeout(), self.prepare(snapshot))
            .await
            .context("MySQL startup timed out")??;
        let (sender, receiver) = mpsc::channel(1);
        self.receiver = Some(receiver);
        self.shutdown.send_replace(false);
        let mut shutdown = self.shutdown.subscribe();
        spawn_owned_worker(&self.worker, async move {
            let result = tokio::select! {
                biased;
                _ = shutdown.wait_for(|stopped| *stopped) => Ok(()),
                result = worker.run(sender) => result,
            };
            let cleanup = worker.connection.close().await;
            let result = match (result, cleanup) {
                (Ok(()), Ok(())) => Ok(()),
                (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
                (Err(error), Err(cleanup)) => Err(OperationFailures::new(
                    "MySQL replication and socket cleanup failed",
                    vec![error, cleanup],
                )
                .into()),
            };
            if let Err(error) = &result {
                log::error!("Native MySQL replication stopped: {error:#}");
            }
            result
        })
        .await?;
        Ok(())
    }
    async fn stop(&mut self) -> Result<()> {
        self.shutdown.send_replace(true);
        self.join().await?;
        self.receiver = None;
        self.cleanup_required = false;
        Ok(())
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
        let receiver = self
            .receiver
            .as_mut()
            .context("native MySQL source is not running")?;
        if let Some(envelope) = receiver.recv().await {
            return Ok(Some(envelope));
        }
        self.join().await?;
        anyhow::bail!("native MySQL replication ended; stop and restart the source")
    }
}

impl Drop for MySqlSource {
    fn drop(&mut self) {
        self.shutdown.send_replace(true);
        if let Some(worker) = self.worker.get_mut() {
            worker.abort();
            log::warn!("Native MySQL source dropped without awaited worker cleanup");
        }
    }
}

pub(super) fn checkpoint(
    snapshot: &SourceProgressSnapshot,
    source: &ComponentId,
    binding: [u8; 32],
) -> Result<Option<Position>> {
    let Some(checkpoint) = snapshot
        .checkpoints
        .get(&SourceProgressKey::Source(source.as_str().into()))
    else {
        return Ok(None);
    };
    let position: Position = serde_json::from_slice(
        checkpoint
            .source_position
            .as_deref()
            .context("MySQL checkpoint has no cursor")?,
    )?;
    anyhow::ensure!(
        position.binding == binding && position.sequence()? == checkpoint.sequence,
        "MySQL checkpoint binding or sequence mismatch"
    );
    Ok(Some(position))
}

pub(super) fn source_binding(
    config: &MySqlConfig,
    server: &str,
    tables: &catalog::Tables,
    source: &ComponentId,
    stream: &StreamId,
    progress: Option<&SourceProgressReader>,
) -> Result<[u8; 32]> {
    #[derive(serde::Serialize)]
    #[serde(untagged)]
    enum StartBinding<'a> {
        Snapshot { mode: &'static str },
        Configured(&'a MySqlStartPosition),
    }
    let start = match &config.start {
        MySqlStartPosition::Snapshot { .. } => StartBinding::Snapshot { mode: "snapshot" },
        other => StartBinding::Configured(other),
    };
    Ok(Sha256::digest(serde_json::to_vec(&(
        "mysql-native-v1",
        server,
        &config.connection.database,
        tables,
        source.as_str(),
        stream.as_str(),
        start,
        progress.map(|owner| (owner.graph_id(), owner.component_id().as_str())),
    ))?)
    .into())
}

struct Worker {
    connection: Connection,
    decoder: Binlog,
    source: ComponentId,
    binding: [u8; 32],
    progress: Option<Box<dyn SourceProgressUpdates>>,
    generation: u64,
    confirmed: u64,
}

impl Worker {
    fn validate_progress(&mut self) -> Result<()> {
        let Some(progress) = &mut self.progress else {
            return Ok(());
        };
        let snapshot = progress.snapshot()?;
        anyhow::ensure!(
            snapshot.failure.is_none()
                && snapshot.reset_generation == self.generation
                && snapshot.persistent,
            "MySQL consumer progress was fenced, reset or lost persistence"
        );
        if let Some(position) = checkpoint(&snapshot, &self.source, self.binding)? {
            let sequence = position.sequence()?;
            anyhow::ensure!(
                sequence >= self.confirmed && sequence <= self.decoder.admitted,
                "MySQL committed progress moved outside the admitted range"
            );
            self.confirmed = sequence;
        }
        Ok(())
    }

    async fn run(&mut self, sender: mpsc::Sender<OutputEnvelope>) -> Result<()> {
        loop {
            self.validate_progress()?;
            let deadline = self.decoder.deadline();
            tokio::select! {
                biased;
                changed = progress_changed(&mut self.progress) => changed?,
                _ = async {
                    match deadline {
                        Some(deadline) => tokio::time::sleep_until(deadline).await,
                        None => std::future::pending().await,
                    }
                } => return Err(SourceTransactionError::Deadline.into()),
                frame = self.connection.event() => {
                    for envelope in self.decoder.decode(&frame?)? {
                        loop {
                            tokio::select! {
                                biased;
                                changed = progress_changed(&mut self.progress) => { changed?; self.validate_progress()?; }
                                permit = sender.reserve() => {
                                    permit.context("MySQL output receiver closed")?.send(envelope);
                                    break;
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

async fn progress_changed(progress: &mut Option<Box<dyn SourceProgressUpdates>>) -> Result<()> {
    match progress {
        Some(progress) => progress.changed().await,
        None => std::future::pending().await,
    }
}

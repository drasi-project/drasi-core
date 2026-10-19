// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use crate::rows::{RowEmission, RowSourceOptions};
use anyhow::{Context, Result};
use async_trait::async_trait;
use drasi_computation_contracts::GraphChangeCodec;
use drasi_computation_contracts::{
    workers::{join_owned_worker, spawn_owned_worker, WorkerCompletion},
    ComponentDescriptor, ComponentId, OutputEnvelope, PipeRequirements, PortDescriptor,
    PortDirection, PortId, SourceProgressSnapshot, SourceProgressUpdates, SourceProgressView,
    SourceTransactionCodec, SourceTransactionError, StreamId,
};
use drasi_computation_plugin_sdk::{NativeComponent, NativeSource};
use drasi_core_types::interface::StorageDurability;
use drasi_postgres_common::replication::rows::{row_checkpoint, row_live_config, row_position};
use drasi_postgres_common::{
    connection::parse_lsn,
    decoder::PgOutputDecoder,
    protocol::BackendMessage,
    replication::{
        checkpoint, connect, live_config, read_position, source_binding, validate_retention,
        Connection, PostgresTransactionConfig, Transactions,
    },
    types::StandbyStatusUpdate,
};
use std::{
    collections::{HashMap, VecDeque},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{
    sync::{mpsc, watch, RwLock},
    task::JoinHandle,
    time::Instant,
};

/// Emits complete transactions and retains upstream WAL until the bound query's
/// actual committed progress advances. The host supplies a read-only capability.
pub struct PostgresTransactionSource {
    descriptor: ComponentDescriptor,
    stream: StreamId,
    config: PostgresTransactionConfig,
    progress: SourceProgressView,
    coordinated_snapshot: bool,
    receiver: Option<mpsc::Receiver<OutputEnvelope>>,
    shutdown: watch::Sender<bool>,
    worker: RwLock<Option<JoinHandle<Result<()>>>>,
    cleanup_required: bool,
    transport_sequence: Arc<AtomicU64>,
    rows: Option<RowSourceOptions>,
}

impl PostgresTransactionSource {
    pub fn new(
        id: ComponentId,
        stream: StreamId,
        config: PostgresTransactionConfig,
        progress: SourceProgressView,
        coordinated_snapshot: bool,
    ) -> Result<Self> {
        if coordinated_snapshot {
            config.validate_coordinated()?;
        } else {
            config.validate()?;
        }
        Ok(Self {
            descriptor: Self::describe(id)?,
            stream,
            config,
            progress,
            coordinated_snapshot,
            receiver: None,
            shutdown: watch::channel(false).0,
            worker: RwLock::new(None),
            cleanup_required: false,
            transport_sequence: Arc::new(AtomicU64::new(0)),
            rows: None,
        })
    }

    pub fn rows(
        id: ComponentId,
        stream: StreamId,
        config: PostgresTransactionConfig,
        progress: SourceProgressView,
        coordinated_snapshot: bool,
        options: RowSourceOptions,
    ) -> Result<Self> {
        options.validate(coordinated_snapshot)?;
        let mut source = Self::new(id.clone(), stream, config, progress, coordinated_snapshot)?;
        source.descriptor = Self::describe_rows(id)?;
        source.rows = Some(options);
        Ok(source)
    }

    pub fn describe_rows(id: ComponentId) -> Result<ComponentDescriptor> {
        Ok(ComponentDescriptor::try_new(
            id,
            vec![PortDescriptor::new(
                PortId::try_new("out")?,
                PortDirection::Output,
                GraphChangeCodec::schema().descriptor().clone(),
                PipeRequirements::default(),
            )],
        )?)
    }

    pub fn describe(id: ComponentId) -> Result<ComponentDescriptor> {
        Ok(ComponentDescriptor::try_new(
            id,
            vec![PortDescriptor::new(
                PortId::try_new("out")?,
                PortDirection::Output,
                SourceTransactionCodec::schema().descriptor().clone(),
                PipeRequirements::default(),
            )],
        )?)
    }

    async fn prepare(&self, snapshot: Arc<SourceProgressSnapshot>) -> Result<Worker> {
        anyhow::ensure!(
            self.rows.is_some_and(|rows| !rows.persistent) || snapshot.persistent,
            "PostgreSQL transactions require persistent owner progress"
        );
        let config = if self.coordinated_snapshot {
            if self.rows.is_some() {
                row_live_config(&self.config, &snapshot, self.descriptor.id())?
            } else {
                live_config(&self.config, &snapshot, self.descriptor.id())?
            }
        } else {
            anyhow::ensure!(
                if self.rows.is_some() {
                    row_position(&snapshot, self.descriptor.id())?
                        .is_none_or(|position| position.managed_slot.is_none())
                } else {
                    read_position(&snapshot, self.descriptor.id())?
                        .is_none_or(|position| position.managed_slot.is_none())
                },
                "managed PostgreSQL progress requires coordinated snapshot mode"
            );
            self.config.clone()
        };
        let progress = self.progress.subscribe()?;
        let mut connection = connect(&config).await?;
        if self.rows.is_some_and(|rows| rows.auto_create_slot)
            && row_position(&snapshot, self.descriptor.id())?.is_none()
        {
            connection
                .create_replication_slot(&config.connection.slot_name, false)
                .await?;
        }
        validate_retention(&mut connection, &config).await?;
        let (keys, binding) =
            source_binding(&mut connection, &config, self.descriptor.id(), &self.stream).await?;
        let row_resume = if self.rows.is_some() {
            row_checkpoint(&snapshot, self.descriptor.id(), binding)?
        } else {
            None
        };
        self.transport_sequence.fetch_max(
            snapshot
                .transport_sequences
                .get(&self.stream)
                .copied()
                .unwrap_or(0),
            Ordering::Relaxed,
        );
        let slot = connection
            .get_replication_slot_info(&config.connection.slot_name)
            .await?;
        anyhow::ensure!(
            slot.output_plugin == "pgoutput",
            "replication slot is not pgoutput"
        );
        let retained = parse_lsn(
            slot.restart_lsn
                .as_deref()
                .context("slot has no retained WAL")?,
        )?;
        let confirmed = parse_lsn(&slot.consistent_point)?;
        let initial = if self.rows.is_some_and(|rows| rows.start_from_slot) {
            confirmed
        } else {
            parse_lsn(&config.start_lsn)?
        };
        let resume = if self.rows.is_some() {
            row_resume
                .as_ref()
                .map_or(initial, |position| position.commit_lsn)
        } else {
            checkpoint(&snapshot, self.descriptor.id(), binding)?.unwrap_or(initial)
        };
        let feedback = row_resume
            .as_ref()
            .map_or(resume, |position| position.feedback_lsn());
        anyhow::ensure!(
            retained <= resume && confirmed <= feedback,
            "requested PostgreSQL cursor is no longer available; explicit recovery is required"
        );
        let publication = format!(
            "\"{}\"",
            config.connection.publication_name.replace('"', "\"\"")
        );
        connection
            .start_replication(
                &config.connection.slot_name,
                Some(resume),
                HashMap::from([
                    ("proto_version".into(), "1".into()),
                    ("publication_names".into(), publication.replace('\'', "''")),
                ]),
            )
            .await?;
        Ok(Worker {
            connection: Some(connection),
            transactions: Transactions {
                decoder: PgOutputDecoder::strict(config.max_protocol_bytes),
                pending: None,
                keys,
                source: self.descriptor.id().clone(),
                stream: self.stream.clone(),
                limits: config.transactions,
                binding,
                managed_slot: self
                    .coordinated_snapshot
                    .then(|| config.connection.slot_name.clone()),
                resume: if row_resume
                    .as_ref()
                    .is_some_and(|position| !position.complete())
                {
                    resume
                        .checked_sub(1)
                        .context("invalid partial PostgreSQL transaction")?
                } else {
                    resume
                },
                transport_sequence: if self.rows.is_some() {
                    Arc::new(AtomicU64::new(0))
                } else {
                    self.transport_sequence.clone()
                },
                filtered: !config.connection.tables.is_empty(),
            },
            progress,
            reset_generation: snapshot.reset_generation,
            confirmed: feedback,
            next_feedback: Instant::now(),
            config,
            persistent: self.rows.is_none_or(|rows| rows.persistent),
            rows: self
                .rows
                .map(|_| RowEmission::new(row_resume, feedback, self.transport_sequence.clone())),
            reconnect: self.rows,
        })
    }

    async fn join(&mut self) -> Result<()> {
        match join_owned_worker(self.worker.get_mut(), self.config.timeout()).await? {
            WorkerCompletion::Completed(result) => result,
            WorkerCompletion::Cancelled | WorkerCompletion::Absent => Ok(()),
        }
    }
}

#[async_trait]
impl NativeComponent for PostgresTransactionSource {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    fn configuration(&self) -> Result<serde_json::Value> {
        let mut value = serde_json::json!({
            "stream": self.stream.as_str(),
            "settings": self.config,
            "coordinated_snapshot": self.coordinated_snapshot,
        });
        if let Some(rows) = self.rows {
            value["persistent"] = rows.persistent.into();
            value["auto_create_slot"] = rows.auto_create_slot.into();
            value["start_from_slot"] = rows.start_from_slot.into();
            value["reconnect_attempts"] = rows.reconnect_attempts.into();
            value["reconnect_delay_ms"] = rows.reconnect_delay_ms.into();
        }
        Ok(value)
    }
    async fn start(&mut self) -> Result<()> {
        anyhow::ensure!(
            !self.cleanup_required,
            "PostgreSQL source requires awaited cleanup before restart"
        );
        self.cleanup_required = true;
        let progress = self.progress.wait_ready().await?;
        let mut worker = tokio::time::timeout(self.config.timeout(), self.prepare(progress))
            .await
            .context("PostgreSQL startup timed out")??;
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
            if let Err(error) = &result {
                log::error!("Native PostgreSQL replication stopped: {error:#}");
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
impl NativeSource for PostgresTransactionSource {
    fn recovery_reader(&self) -> Option<SourceProgressView> {
        Some(self.progress.clone())
    }
    fn replay_retention(&self) -> Option<(StorageDurability, ComponentId)> {
        if self.rows.is_some_and(|rows| !rows.persistent) {
            return None;
        }
        Some((
            StorageDurability::LOCAL_PROCESS_RESTART,
            self.progress.component_id().clone(),
        ))
    }
    async fn next(&mut self) -> Result<Option<OutputEnvelope>> {
        let receiver = self
            .receiver
            .as_mut()
            .context("PostgreSQL source is not running")?;
        if let Some(envelope) = receiver.recv().await {
            return Ok(Some(envelope));
        }
        self.join().await?;
        anyhow::bail!("PostgreSQL replication ended; stop and restart the source")
    }
}

impl Drop for PostgresTransactionSource {
    fn drop(&mut self) {
        self.shutdown.send_replace(true);
        if let Some(worker) = self.worker.get_mut() {
            worker.abort();
            log::warn!("Native PostgreSQL source dropped without awaited worker cleanup");
        }
    }
}

struct Worker {
    connection: Option<Connection>,
    transactions: Transactions,
    progress: Box<dyn SourceProgressUpdates>,
    reset_generation: u64,
    confirmed: u64,
    next_feedback: Instant,
    config: PostgresTransactionConfig,
    persistent: bool,
    rows: Option<RowEmission>,
    reconnect: Option<RowSourceOptions>,
}

impl Worker {
    async fn feedback(&mut self) -> Result<()> {
        let snapshot = self.progress.snapshot()?;
        anyhow::ensure!(
            snapshot.failure.is_none(),
            "PostgreSQL consumer progress is fenced"
        );
        anyhow::ensure!(
            snapshot.reset_generation == self.reset_generation,
            "PostgreSQL consumer was reset; explicit source reconstruction is required"
        );
        if snapshot.ready {
            anyhow::ensure!(
                !self.persistent || snapshot.persistent,
                "PostgreSQL consumer lost persistent progress"
            );
            let next = if let Some(rows) = &self.rows {
                rows.confirmed(&snapshot, &self.transactions)?
            } else {
                checkpoint(
                    &snapshot,
                    &self.transactions.source,
                    self.transactions.binding,
                )?
            };
            if let Some(next) = next {
                anyhow::ensure!(
                    next >= self.confirmed
                        && next
                            <= self
                                .rows
                                .as_ref()
                                .map_or(self.transactions.resume, RowEmission::admitted_lsn),
                    "PostgreSQL committed cursor moved outside the admitted range"
                );
                self.confirmed = next;
            }
        }
        let io_deadline = Instant::now() + self.config.timeout();
        let assembly_deadline = self
            .transactions
            .pending
            .as_ref()
            .map(|(_, builder)| builder.deadline());
        let deadline = assembly_deadline.map_or(io_deadline, |deadline| deadline.min(io_deadline));
        let result = tokio::time::timeout_at(
            deadline,
            self.connection
                .as_mut()
                .context("PostgreSQL connection is not open")?
                .send_standby_status(StandbyStatusUpdate {
                    write_lsn: self.confirmed,
                    flush_lsn: self.confirmed,
                    apply_lsn: self.confirmed,
                    reply_requested: false,
                }),
        )
        .await;
        if result.is_err() && assembly_deadline.is_some_and(|deadline| deadline <= Instant::now()) {
            return Err(SourceTransactionError::Deadline.into());
        }
        result.context("PostgreSQL feedback timed out")??;
        self.next_feedback =
            Instant::now() + Duration::from_millis(self.config.feedback_interval_ms.get());
        Ok(())
    }

    async fn run(&mut self, sender: mpsc::Sender<OutputEnvelope>) -> Result<()> {
        loop {
            let error = match self.run_connected(sender.clone()).await {
                Ok(()) => return Ok(()),
                Err(error) => error,
            };
            let Some(options) = self
                .reconnect
                .filter(|options| options.reconnect_attempts > 0)
            else {
                return Err(error);
            };
            if !crate::rows::transient(&error) {
                return Err(error);
            }
            let mut failure = error;
            let mut reopened = false;
            self.connection.take();
            for attempt in 1..=options.reconnect_attempts {
                log::warn!("PostgreSQL row connection interrupted; reconnect attempt {attempt}: {failure:#}");
                tokio::time::sleep(Duration::from_millis(options.reconnect_delay_ms)).await;
                match tokio::time::timeout(self.config.timeout(), self.reopen()).await {
                    Ok(Ok(())) => {
                        reopened = true;
                        break;
                    }
                    Ok(Err(error)) => {
                        if !crate::rows::transient(&error) {
                            return Err(error);
                        }
                        failure = error;
                    }
                    Err(error) => {
                        failure =
                            anyhow::Error::new(error).context("PostgreSQL reconnect timed out")
                    }
                }
            }
            if !reopened {
                return Err(failure.context("PostgreSQL reconnect attempts exhausted"));
            }
        }
    }

    async fn reopen(&mut self) -> Result<()> {
        let progress = self.progress.snapshot()?;
        anyhow::ensure!(
            progress.ready
                && progress.failure.is_none()
                && progress.reset_generation == self.reset_generation
                && (!self.persistent || progress.persistent),
            "PostgreSQL row owner is not ready for safe reconnection"
        );
        let position = row_checkpoint(
            &progress,
            &self.transactions.source,
            self.transactions.binding,
        )?;
        let confirmed = position
            .as_ref()
            .map_or(self.confirmed, |position| position.feedback_lsn());
        anyhow::ensure!(
            confirmed >= self.confirmed,
            "PostgreSQL committed row cursor moved backwards"
        );
        let resume = position
            .as_ref()
            .map_or(confirmed, |position| position.commit_lsn);
        let mut connection = connect(&self.config).await?;
        validate_retention(&mut connection, &self.config).await?;
        let (keys, binding) = source_binding(
            &mut connection,
            &self.config,
            &self.transactions.source,
            &self.transactions.stream,
        )
        .await?;
        anyhow::ensure!(
            binding == self.transactions.binding,
            "PostgreSQL binding changed during reconnection"
        );
        let slot = connection
            .get_replication_slot_info(&self.config.connection.slot_name)
            .await?;
        anyhow::ensure!(
            slot.output_plugin == "pgoutput"
                && parse_lsn(
                    slot.restart_lsn
                        .as_deref()
                        .context("slot has no retained WAL")?
                )? <= resume
                && parse_lsn(&slot.consistent_point)? <= confirmed,
            "PostgreSQL committed row cursor is no longer retained"
        );
        let publication = format!(
            "\"{}\"",
            self.config.connection.publication_name.replace('"', "\"\"")
        );
        connection
            .start_replication(
                &self.config.connection.slot_name,
                Some(resume),
                HashMap::from([
                    ("proto_version".into(), "1".into()),
                    ("publication_names".into(), publication.replace('\'', "''")),
                ]),
            )
            .await?;
        self.transactions.decoder = PgOutputDecoder::strict(self.config.max_protocol_bytes);
        self.transactions.pending = None;
        self.transactions.keys = keys;
        self.transactions.resume = if position
            .as_ref()
            .is_some_and(|position| !position.complete())
        {
            resume
                .checked_sub(1)
                .context("invalid partial PostgreSQL row cursor")?
        } else {
            resume
        };
        self.rows
            .as_mut()
            .context("reconnect is only available for row sources")?
            .rewind(position, confirmed);
        self.connection = Some(connection);
        self.confirmed = confirmed;
        self.next_feedback = Instant::now();
        Ok(())
    }

    async fn run_connected(&mut self, sender: mpsc::Sender<OutputEnvelope>) -> Result<()> {
        let mut pending = VecDeque::new();
        loop {
            if let Some((_, builder)) = &mut self.transactions.pending {
                builder.check_deadline()?;
            }
            if !pending.is_empty() {
                tokio::select! {
                    permit = sender.reserve() => {
                        permit.context("PostgreSQL output closed")?
                            .send(pending.pop_front().context("pending PostgreSQL transaction")?);
                    }
                    _ = tokio::time::sleep_until(self.next_feedback) => self.feedback().await?,
                    changed = self.progress.changed() => {
                        changed?;
                        self.feedback().await?;
                    }
                }
                continue;
            }
            let deadline = self
                .transactions
                .pending
                .as_ref()
                .map(|(_, builder)| builder.deadline());
            tokio::select! {
                biased;
                _ = async {
                    match deadline {
                        Some(deadline) => tokio::time::sleep_until(deadline).await,
                        None => std::future::pending().await,
                    }
                } => return Err(SourceTransactionError::Deadline.into()),
                _ = tokio::time::sleep_until(self.next_feedback) => self.feedback().await?,
                changed = self.progress.changed() => {
                    changed?;
                    self.feedback().await?;
                }
                message = self.connection.as_mut().context("PostgreSQL connection is not open")?.read_replication_message() => {
                    match message? {
                        BackendMessage::CopyData(data) => match data.first() {
                            Some(b'w') => {
                                anyhow::ensure!(data.len() > 25, "truncated PostgreSQL XLogData");
                                if let Some(group) = self.transactions.decode(&data[25..])? {
                                    if let Some(rows) = &mut self.rows {
                                        pending = rows.expand(group, &self.transactions)?;
                                    } else {
                                        pending.push_back(group);
                                    }
                                }
                            }
                            Some(b'k') => {
                                anyhow::ensure!(data.len() == 18 && data[17] <= 1, "invalid PostgreSQL keepalive");
                                if data[17] == 1 { self.feedback().await?; }
                            }
                            _ => anyhow::bail!("unknown PostgreSQL CopyData frame"),
                        },
                        BackendMessage::NoticeResponse(notice) => log::info!("PostgreSQL: {}", notice.message),
                        BackendMessage::ErrorResponse(error) => return Err(anyhow::Error::new(error).context("PostgreSQL replication failed")),
                        message => anyhow::bail!("unexpected PostgreSQL replication message: {message:?}"),
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;

// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! The native PostgreSQL source component: construction, lifecycle and
//! management. Replication itself runs in [`crate::worker`].

use crate::config::{Mode, SourceConfig};
use crate::management;
use crate::rows::RowEmission;
use crate::shared::{self, SharedBinding, SharedMode, SharedUpdates, TransactionEmission};
use crate::whole::{Cursors, WholeEmission};
use crate::worker::{decoder_resume, Emission, PauseGate, Worker, WorkerProgress};
use anyhow::{Context, Result};
use async_trait::async_trait;
use drasi_computation_contracts::{
    workers::{
        cancel_owned_worker, join_owned_worker, spawn_owned_worker, WorkerCleanupError,
        WorkerCompletion,
    },
    ComponentDescriptor, ComponentId, ConsumerProgressSnapshot, ConsumerProgressView,
    GraphChangeCodec, InputPositionPolicy, OutputEnvelope, PipeRequirements, PortDescriptor,
    PortDirection, PortId, SourceProgressSnapshot, SourceProgressView, SourceSchemaReader,
    SourceTransactionCodec, StreamId,
};
use drasi_computation_plugin_sdk::lifecycle::{
    ApplyContext, ApplyError, Assessment, DisposeContext, InitializeContext, LifecycleResult,
    PauseContext, ResumeContext, StartContext, StopContext, StopMode, UpdateBoundary,
};
use drasi_computation_plugin_sdk::{
    ActivationCredentials, ApplyUpdateRequest, NativeComponent, NativeConsumerProgress,
    NativeManagement, NativeSource, UpdateRequest,
};
use drasi_core_types::{interface::StorageDurability, schema::SourceSchema};
use drasi_postgres_common::replication::rows::{row_checkpoint, row_live_config, row_position};
use drasi_postgres_common::{
    decoder::PgOutputDecoder,
    replication::{
        checkpoint, connect, drop_owned_slots, ensure_retained, ensure_slot, live_config,
        read_position, schema::describe_schema, slot_positions, source_binding, start_streaming,
        validate_retention, Connection, PostgresTransactionConfig, Transactions,
    },
};
use std::{
    collections::VecDeque,
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

/// What the worker should do when the host stops it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Shutdown {
    Running,
    Drain,
    Abort,
}

/// Owns replication for one of the four modes. Upstream feedback follows the
/// host-bound consumers' actual handled progress.
pub struct PostgresTransactionSource {
    descriptor: ComponentDescriptor,
    mode: Mode,
    stream: StreamId,
    /// The resolved runtime configuration (managed slot bound to this
    /// source). Its serialized form is the accepted configuration.
    config: PostgresTransactionConfig,
    /// The configuration the current activation connects with: `config`
    /// with the host's activation credentials, shared with the schema
    /// reader. Credentials never outlive the activation.
    activation: Arc<std::sync::RwLock<PostgresTransactionConfig>>,
    progress: Option<SourceProgressView>,
    shared: Option<SharedBinding>,
    coordinated_snapshot: bool,
    persistent: bool,
    receiver: Option<mpsc::Receiver<OutputEnvelope>>,
    shutdown: watch::Sender<Shutdown>,
    pause: watch::Sender<bool>,
    paused: Arc<watch::Sender<bool>>,
    worker: RwLock<Option<JoinHandle<Result<()>>>>,
    cleanup_required: bool,
    transport_sequence: Arc<AtomicU64>,
    /// `reconnectBudgetMs`, shared with the running worker so an in-place
    /// update applies to the next transport failure.
    reconnect_budget_ms: Arc<AtomicU64>,
    /// Least quiet time before a progress record.
    idle_progress: Duration,
}

impl PostgresTransactionSource {
    fn build(
        id: ComponentId,
        mode: Mode,
        stream: StreamId,
        mut config: PostgresTransactionConfig,
        graph: &str,
        coordinated_snapshot: bool,
        persistent: bool,
    ) -> Result<Self> {
        config.resolve(graph, &id)?;
        let source = SourceConfig {
            mode,
            stream: stream.as_str().to_owned(),
            coordinated_snapshot,
            persistent,
            settings: config.clone(),
        };
        source.validate()?;
        if coordinated_snapshot {
            config.validate_coordinated()?;
        } else {
            config.validate()?;
        }
        let descriptor = match mode {
            Mode::Transactions | Mode::SharedTransactions => Self::describe(id)?,
            Mode::Rows | Mode::SharedRows => Self::describe_rows(id)?,
        };
        Ok(Self {
            descriptor,
            mode,
            stream,
            activation: Arc::new(std::sync::RwLock::new(config.clone())),
            progress: None,
            shared: None,
            coordinated_snapshot,
            persistent,
            receiver: None,
            shutdown: watch::channel(Shutdown::Running).0,
            pause: watch::channel(false).0,
            paused: Arc::new(watch::channel(false).0),
            worker: RwLock::new(None),
            cleanup_required: false,
            transport_sequence: Arc::new(AtomicU64::new(0)),
            reconnect_budget_ms: Arc::new(AtomicU64::new(config.reconnect_budget_ms)),
            idle_progress: crate::worker::IDLE_PROGRESS,
            config,
        })
    }

    /// Whole commits (`drasi/postgres-transactions`) with query-owned
    /// progress.
    pub fn new(
        id: ComponentId,
        stream: StreamId,
        config: PostgresTransactionConfig,
        progress: SourceProgressView,
        coordinated_snapshot: bool,
    ) -> Result<Self> {
        let mut source = Self::build(
            id,
            Mode::Transactions,
            stream,
            config,
            progress.graph_id(),
            coordinated_snapshot,
            true,
        )?;
        source.progress = Some(progress);
        Ok(source)
    }

    /// One envelope per row change (`drasi.postgres/changes`).
    pub fn rows(
        id: ComponentId,
        stream: StreamId,
        config: PostgresTransactionConfig,
        progress: SourceProgressView,
        coordinated_snapshot: bool,
        persistent: bool,
    ) -> Result<Self> {
        let mut source = Self::build(
            id,
            Mode::Rows,
            stream,
            config,
            progress.graph_id(),
            coordinated_snapshot,
            persistent,
        )?;
        source.progress = Some(progress);
        Ok(source)
    }

    /// Rows with a fixed consumer set (`drasi.postgres/shared-changes`).
    pub fn shared_rows(
        id: ComponentId,
        stream: StreamId,
        config: PostgresTransactionConfig,
        progress: ConsumerProgressView,
        consumers: &[ComponentId],
        persistent: bool,
    ) -> Result<Self> {
        let graph = progress.graph_id().to_owned();
        let mut source = Self::build(
            id.clone(),
            Mode::SharedRows,
            stream,
            config,
            &graph,
            false,
            persistent,
        )?;
        source.shared = Some(SharedBinding::new(&id, progress, consumers)?);
        Ok(source)
    }

    /// Whole commits with a fixed consumer set
    /// (`drasi.postgres/shared-transactions`).
    pub fn shared_transactions(
        id: ComponentId,
        stream: StreamId,
        config: PostgresTransactionConfig,
        progress: NativeConsumerProgress,
    ) -> Result<Self> {
        let graph = progress.reader().graph_id().to_owned();
        let mut source = Self::build(
            id.clone(),
            Mode::SharedTransactions,
            stream,
            config,
            &graph,
            false,
            true,
        )?;
        source.shared = Some(SharedBinding::transactions(&id, progress)?);
        Ok(source)
    }

    pub(crate) fn row_ports() -> Result<Vec<PortDescriptor>> {
        Ok(vec![PortDescriptor::new(
            PortId::try_new("out")?,
            PortDirection::Output,
            GraphChangeCodec::schema().descriptor().clone(),
            PipeRequirements::default(),
        )])
    }

    pub fn describe_rows(id: ComponentId) -> Result<ComponentDescriptor> {
        Ok(ComponentDescriptor::try_new(id, Self::row_ports()?)?)
    }

    pub(crate) fn transaction_ports() -> Result<Vec<PortDescriptor>> {
        Ok(vec![PortDescriptor::new(
            PortId::try_new("out")?,
            PortDirection::Output,
            SourceTransactionCodec::schema().descriptor().clone(),
            PipeRequirements::default(),
        )])
    }

    pub fn describe(id: ComponentId) -> Result<ComponentDescriptor> {
        Ok(ComponentDescriptor::try_new(
            id,
            Self::transaction_ports()?,
        )?)
    }

    fn is_rows(&self) -> bool {
        matches!(self.mode, Mode::Rows | Mode::SharedRows)
    }

    /// The configuration this activation connects with.
    fn activation_config(&self) -> PostgresTransactionConfig {
        self.activation
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Binds the configuration an activation or erasure connects with:
    /// host-supplied credentials replace the configured user and password.
    fn activate(&self, credentials: Option<ActivationCredentials>) {
        let config = match credentials.map(ActivationCredentials::into_auth_pair) {
            Some((user, password)) => self.config.with_credentials(user, password),
            None => self.config.clone(),
        };
        *self
            .activation
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = config;
    }

    /// Sets the least quiet time before a progress record. The default is
    /// 30 seconds; tests shorten it.
    pub fn set_idle_progress(&mut self, interval: Duration) {
        self.idle_progress = interval;
    }

    fn source_config(&self) -> SourceConfig {
        SourceConfig {
            mode: self.mode,
            stream: self.stream.as_str().to_owned(),
            coordinated_snapshot: self.coordinated_snapshot,
            persistent: self.persistent,
            settings: self.config.clone(),
        }
    }

    fn pause_gate(&self) -> PauseGate {
        PauseGate {
            requested: self.pause.subscribe(),
            acknowledged: (*self.paused).clone(),
        }
    }

    fn transactions(
        &self,
        config: &PostgresTransactionConfig,
        keys: drasi_postgres_common::replication::TableKeys,
        binding: [u8; 32],
        managed_slot: Option<String>,
        resume: u64,
    ) -> Transactions {
        Transactions::new(
            PgOutputDecoder::strict(config.max_protocol_bytes),
            keys,
            self.descriptor.id().clone(),
            binding,
            managed_slot,
            resume,
            !config.tables.is_empty(),
        )
    }

    fn worker(
        &self,
        connection: Connection,
        config: PostgresTransactionConfig,
        transactions: Transactions,
        progress: WorkerProgress,
        confirmed: u64,
        emission: Emission,
    ) -> Worker {
        Worker {
            connection: Some(connection),
            stream: self.stream.clone(),
            transactions,
            progress,
            confirmed,
            next_feedback: Instant::now(),
            emission,
            queue: VecDeque::new(),
            holding: false,
            origin: None,
            last_emission: Instant::now(),
            idle_progress: self.idle_progress,
            pause: self.pause_gate(),
            reconnect_budget_ms: self.reconnect_budget_ms.clone(),
            config,
        }
    }

    async fn prepare(&self, snapshot: Arc<SourceProgressSnapshot>) -> Result<Worker> {
        let base = self.activation_config();
        let committed = if self.is_rows() {
            row_position(&snapshot, self.descriptor.id())?.map(|position| position.managed_slot)
        } else {
            read_position(&snapshot, self.descriptor.id())?.map(|position| position.managed_slot)
        };
        let config = if self.coordinated_snapshot {
            if self.is_rows() {
                row_live_config(&base, &snapshot, self.descriptor.id())?
            } else {
                live_config(&base, &snapshot, self.descriptor.id())?
            }
        } else {
            anyhow::ensure!(
                committed.as_ref().is_none_or(Option::is_none),
                "managed PostgreSQL progress requires coordinated snapshot mode"
            );
            base
        };
        let progress = self
            .progress
            .as_ref()
            .context("PostgreSQL single-owner reader is missing")?
            .subscribe()?;
        let mut connection = connect(&config).await?;
        ensure_slot(&mut connection, &config, committed.is_none()).await?;
        validate_retention(&mut connection, &config).await?;
        let (keys, binding) =
            source_binding(&mut connection, &config, self.descriptor.id(), &self.stream).await?;
        let row_resume = if self.is_rows() {
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
        let (retained, confirmed) = slot_positions(&mut connection, &config).await?;
        // The slot's confirmed position (or startLsn) is server-side state,
        // so a fresh single owner's origin is durable without a record.
        let initial = config.start_lsn()?.unwrap_or(confirmed);
        let resume = if self.is_rows() {
            row_resume
                .as_ref()
                .map_or(initial, |position| position.commit_lsn)
        } else {
            checkpoint(&snapshot, self.descriptor.id(), binding)?.unwrap_or(initial)
        };
        let feedback = row_resume
            .as_ref()
            .map_or(resume, |position| position.feedback_lsn());
        ensure_retained(retained, confirmed, resume, feedback)?;
        start_streaming(&mut connection, &config, resume).await?;
        let partial = row_resume
            .as_ref()
            .is_some_and(|position| !position.complete());
        let managed_slot = self
            .coordinated_snapshot
            .then(|| config.slot_name().to_owned());
        let transactions = self.transactions(
            &config,
            keys,
            binding,
            managed_slot,
            decoder_resume(resume, partial)?,
        );
        let emission = if self.is_rows() {
            Emission::Rows(RowEmission::new(
                self.stream.clone(),
                row_resume,
                feedback,
                self.transport_sequence.clone(),
                config.transactions(),
            ))
        } else {
            Emission::Whole(WholeEmission::new(
                self.descriptor.id().clone(),
                self.stream.clone(),
                config.transactions(),
                self.transport_sequence.clone(),
                Cursors::Single,
            ))
        };
        let progress = WorkerProgress::Single {
            updates: progress,
            generation: snapshot.reset_generation,
        };
        Ok(self.worker(
            connection,
            config,
            transactions,
            progress,
            feedback,
            emission,
        ))
    }

    async fn prepare_shared(&self, snapshot: ConsumerProgressSnapshot) -> Result<Worker> {
        let shared = self
            .shared
            .as_ref()
            .context("PostgreSQL consumer binding is missing")?;
        let config = self.activation_config();
        let start = shared::initialization(
            &snapshot,
            &shared.expected,
            self.descriptor.id(),
            &self.stream,
            SharedMode::Rows,
        )?;
        let mut connection = connect(&config).await?;
        ensure_slot(&mut connection, &config, start.fresh).await?;
        validate_retention(&mut connection, &config).await?;
        let (keys, binding) =
            source_binding(&mut connection, &config, self.descriptor.id(), &self.stream).await?;
        let observed = shared::observe(
            &snapshot,
            &shared.expected,
            self.descriptor.id(),
            &self.stream,
            binding,
        )?;
        anyhow::ensure!(observed.ready, "PostgreSQL shared consumers are not ready");
        let (retained, slot_confirmed) = slot_positions(&mut connection, &config).await?;
        let (position, resume, confirmed, previous) = match observed.minimum {
            None => {
                anyhow::ensure!(start.fresh, "PostgreSQL fresh consumer state changed");
                let initial = config.start_lsn()?.unwrap_or(slot_confirmed);
                (None, initial, initial, initial)
            }
            Some(minimum) => {
                anyhow::ensure!(!start.fresh, "PostgreSQL fresh consumer state changed");
                let resume = minimum.position.commit_lsn;
                let previous = minimum.position.feedback_lsn();
                (
                    Some(minimum.position),
                    resume,
                    minimum.feedback_lsn,
                    previous,
                )
            }
        };
        ensure_retained(retained, slot_confirmed, resume, confirmed)?;
        let progress = SharedUpdates::new(shared, &snapshot, self.descriptor.id(), binding)?;
        start_streaming(&mut connection, &config, resume).await?;
        let partial = position
            .as_ref()
            .is_some_and(|position| !position.complete());
        let transactions = self.transactions(
            &config,
            keys,
            binding,
            None,
            decoder_resume(resume, partial)?,
        );
        let mut rows = RowEmission::shared(
            self.stream.clone(),
            position.clone(),
            previous,
            self.transport_sequence.clone(),
            config.transactions(),
            shared.counters.clone(),
            observed.max_counter,
            observed.max_lsn.max(resume),
            observed.boundaries,
        );
        let join = start
            .missing
            .then(|| rows.join(position.as_ref(), resume, &transactions))
            .transpose()?;
        let mut worker = self.worker(
            connection,
            config,
            transactions,
            WorkerProgress::Shared(progress),
            confirmed,
            Emission::Rows(rows),
        );
        worker.origin = start.fresh.then_some(resume);
        worker.holding = join.is_some();
        worker.queue.extend(join);
        Ok(worker)
    }

    async fn prepare_shared_transactions(
        &self,
        snapshot: ConsumerProgressSnapshot,
    ) -> Result<Worker> {
        use drasi_postgres_common::replication::shared_transactions::transaction_binding;
        let shared = self
            .shared
            .as_ref()
            .context("PostgreSQL consumer binding is missing")?;
        anyhow::ensure!(
            shared.mode == SharedMode::Transactions && !self.is_rows(),
            "shared PostgreSQL transaction output mode changed"
        );
        let config = self.activation_config();
        let start = shared::initialization(
            &snapshot,
            &shared.expected,
            self.descriptor.id(),
            &self.stream,
            SharedMode::Transactions,
        )?;
        let mut connection = connect(&config).await?;
        ensure_slot(&mut connection, &config, start.fresh).await?;
        validate_retention(&mut connection, &config).await?;
        let (keys, base) =
            source_binding(&mut connection, &config, self.descriptor.id(), &self.stream).await?;
        let binding = transaction_binding(base);
        let observed = shared::observe_transactions(
            &snapshot,
            &shared.expected,
            self.descriptor.id(),
            &self.stream,
            binding,
        )?;
        anyhow::ensure!(
            observed.ready,
            "PostgreSQL transaction consumers are not ready"
        );
        let (retained, slot_confirmed) = slot_positions(&mut connection, &config).await?;
        let resume = match observed.minimum {
            None => {
                anyhow::ensure!(
                    start.fresh,
                    "PostgreSQL fresh transaction consumers changed"
                );
                config.start_lsn()?.unwrap_or(slot_confirmed)
            }
            Some(minimum) => {
                anyhow::ensure!(
                    !start.fresh,
                    "PostgreSQL fresh transaction consumers changed"
                );
                minimum.position.commit_lsn
            }
        };
        ensure_retained(retained, slot_confirmed, resume, resume)?;
        let progress = SharedUpdates::new(shared, &snapshot, self.descriptor.id(), binding)?;
        let cursors = TransactionEmission::new(
            shared.counters.clone(),
            self.transport_sequence.clone(),
            observed.max_counter,
            observed.max_lsn.max(resume),
        );
        start_streaming(&mut connection, &config, resume).await?;
        let transactions = self.transactions(&config, keys, binding, None, resume);
        let mut whole = WholeEmission::new(
            self.descriptor.id().clone(),
            self.stream.clone(),
            config.transactions(),
            self.transport_sequence.clone(),
            Cursors::Shared(cursors),
        );
        let join = start
            .missing
            .then(|| whole.progress(resume, chrono::Utc::now(), binding, None))
            .transpose()?;
        let mut worker = self.worker(
            connection,
            config,
            transactions,
            WorkerProgress::SharedTransactions(progress),
            resume,
            Emission::Whole(whole),
        );
        worker.origin = start.fresh.then_some(resume);
        worker.holding = join.is_some();
        worker.queue.extend(join);
        Ok(worker)
    }

    /// Waits for the progress owner, then opens replication. Only the
    /// connection phase is bounded by `ioTimeoutMs`: readiness can include a
    /// long coordinated snapshot, bounded only by the host's deadline.
    async fn open(&self) -> Result<Worker> {
        let io = self.config.timeout();
        match (&self.progress, &self.shared) {
            (Some(progress), None) => {
                let snapshot = progress.wait_ready().await?;
                within(io, self.prepare(snapshot)).await
            }
            (None, Some(shared)) => {
                let progress = shared.reader.wait_ready().await?;
                match shared.mode {
                    SharedMode::Rows => within(io, self.prepare_shared(progress)).await,
                    SharedMode::Transactions => {
                        within(io, self.prepare_shared_transactions(progress)).await
                    }
                }
            }
            _ => anyhow::bail!("PostgreSQL source progress binding is invalid"),
        }
    }

    async fn start_activation(&mut self, ctx: &mut StartContext) -> Result<()> {
        anyhow::ensure!(
            !self.cleanup_required,
            "PostgreSQL source requires awaited cleanup before restart"
        );
        self.activate(ctx.take_credentials());
        self.cleanup_required = true;
        let deadline = ctx.deadline();
        let cancellation = ctx.cancellation().clone();
        let opened = tokio::select! {
            biased;
            _ = cancellation.cancelled() => Err(anyhow::anyhow!("PostgreSQL start was cancelled")),
            result = async {
                match deadline {
                    Some(deadline) => tokio::time::timeout_at(deadline, self.open())
                        .await
                        .map_err(|_| anyhow::anyhow!("PostgreSQL startup exceeded its deadline"))?,
                    None => self.open().await,
                }
            } => result,
        };
        // A failed start keeps the cleanup guard: the host stops the
        // component before another start, as for any partial activation.
        let mut worker = opened?;
        let (sender, receiver) = mpsc::channel(1);
        self.receiver = Some(receiver);
        self.shutdown.send_replace(Shutdown::Running);
        self.pause.send_replace(false);
        self.paused.send_replace(false);
        let mut shutdown = self.shutdown.subscribe();
        let paused = self.paused.clone();
        let shared = self.shared.is_some();
        spawn_owned_worker(&self.worker, async move {
            let stopped = tokio::select! {
                biased;
                mode = shutdown.wait_for(|mode| *mode != Shutdown::Running) => {
                    Ok(mode.is_ok_and(|mode| *mode == Shutdown::Drain))
                }
                result = worker.run(sender) => Err(result),
            };
            let result = match stopped {
                Ok(drain) => {
                    if drain {
                        worker.drain().await;
                    }
                    Ok(())
                }
                Err(result) => result,
            };
            // Nothing is read or emitted after exit; a pending pause holds.
            paused.send_replace(true);
            if let Err(error) = &result {
                if shared && consumer_progress_closed(error) {
                    log::debug!(
                        "PostgreSQL consumer progress fenced; reader requires cleanup: {error:#}"
                    );
                } else {
                    log::error!("Native PostgreSQL replication stopped: {error:#}");
                }
            }
            result
        })
        .await?;
        Ok(())
    }

    async fn stop_activation(&mut self, ctx: &StopContext) -> Result<()> {
        let mode = match ctx.mode() {
            StopMode::Abort => Shutdown::Abort,
            _ => Shutdown::Drain,
        };
        self.shutdown.send_replace(mode);
        let timeout = ctx.deadline().map_or(self.config.timeout(), |deadline| {
            deadline.saturating_duration_since(Instant::now())
        });
        let joined = if mode == Shutdown::Abort {
            cancel_owned_worker(self.worker.get_mut(), timeout).await
        } else {
            join_owned_worker(self.worker.get_mut(), timeout).await
        };
        match joined {
            // An earlier replication failure was already reported through
            // `next`; stopping after it succeeds.
            Ok(WorkerCompletion::Completed(Err(error))) => {
                log::debug!("PostgreSQL worker had already failed: {error:#}");
            }
            Ok(_) => {}
            Err(error @ WorkerCleanupError::TimedOut { .. }) => {
                return Err(anyhow::Error::new(error)
                    .context("PostgreSQL replication did not stop within its deadline"))
            }
            Err(error) => return Err(error.into()),
        }
        self.receiver = None;
        self.cleanup_required = false;
        // The activation's credentials end with it.
        self.activate(None);
        Ok(())
    }

    async fn pause_activation(&mut self, ctx: &PauseContext) -> Result<()> {
        if self.worker.get_mut().is_none() {
            return Ok(());
        }
        self.pause.send_replace(true);
        let mut acknowledged = self.paused.subscribe();
        let timeout = ctx.deadline().map_or(self.config.timeout(), |deadline| {
            deadline.saturating_duration_since(Instant::now())
        });
        let cancellation = ctx.cancellation().clone();
        let result = tokio::select! {
            biased;
            _ = cancellation.cancelled() => Err(anyhow::anyhow!("PostgreSQL pause was cancelled")),
            waited = tokio::time::timeout(timeout, acknowledged.wait_for(|paused| *paused)) => match waited {
                Ok(Ok(_)) => Ok(()),
                Ok(Err(_)) => Err(anyhow::anyhow!("PostgreSQL worker closed during pause")),
                Err(_) => Err(anyhow::anyhow!(
                    "PostgreSQL could not reach a transaction boundary before the pause deadline"
                )),
            },
        };
        if result.is_err() {
            self.pause.send_replace(false);
        }
        result
    }

    /// Whether the host's progress shows no committed cursor for this source.
    fn uncommitted(&self) -> bool {
        match (&self.progress, &self.shared) {
            (Some(progress), None) => progress.snapshot().is_ok_and(|snapshot| {
                if self.is_rows() {
                    row_position(&snapshot, self.descriptor.id()).is_ok_and(|p| p.is_none())
                } else {
                    read_position(&snapshot, self.descriptor.id()).is_ok_and(|p| p.is_none())
                }
            }),
            _ => false,
        }
    }

    fn live_assessment(&self, request: &UpdateRequest) -> Result<Assessment> {
        let (current, proposed) = management::configurations(self.mode, request)?;
        Ok(management::plan(&current, &proposed, self.uncommitted()))
    }
}

/// Bounds connection setup by `ioTimeoutMs`.
async fn within(
    io: std::time::Duration,
    future: impl std::future::Future<Output = Result<Worker>>,
) -> Result<Worker> {
    tokio::time::timeout(io, future)
        .await
        .map_err(|_| anyhow::anyhow!("PostgreSQL connection setup exceeded ioTimeoutMs"))?
}

fn refused(reason: impl std::fmt::Display) -> ApplyError {
    ApplyError::Refused {
        now_requires: None,
        reason: reason.to_string(),
    }
}

#[async_trait]
impl NativeComponent for PostgresTransactionSource {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    fn management(&mut self) -> Option<&mut dyn NativeManagement> {
        Some(self)
    }
    fn consumer_progress_reader(&self) -> Option<ConsumerProgressView> {
        self.shared.as_ref().map(|binding| binding.reader.clone())
    }
    fn consumer_replay_retention(&self) -> Option<StorageDurability> {
        (self
            .shared
            .as_ref()
            .is_some_and(|shared| shared.mode == SharedMode::Transactions)
            || (self.shared.is_some() && self.persistent))
            .then_some(StorageDurability::LOCAL_PROCESS_RESTART)
    }
    fn input_position_policy(&self) -> Option<Arc<InputPositionPolicy>> {
        self.shared.as_ref().map(|binding| binding.policy.clone())
    }
    fn configuration(&self) -> Result<serde_json::Value> {
        self.source_config().to_value()
    }
    /// Validates the resolved configuration. No connection is opened.
    async fn initialize(&mut self, _ctx: &InitializeContext) -> LifecycleResult {
        self.config.validate()?;
        Ok(())
    }
    /// Opens replication within the deadline and the configured I/O timeout,
    /// honouring cancellation. Host-supplied activation credentials replace
    /// the configured user and password for this activation.
    async fn start(&mut self, ctx: &mut StartContext) -> LifecycleResult {
        Ok(self.start_activation(ctx).await?)
    }
    /// Stops reading and emitting at the next transaction boundary while
    /// standby feedback continues, so the server keeps the connection. An
    /// open transaction is assembled first; it is held, not emitted.
    async fn pause(&mut self, ctx: &PauseContext) -> LifecycleResult {
        Ok(self.pause_activation(ctx).await?)
    }
    async fn resume(&mut self, _ctx: &ResumeContext) -> LifecycleResult {
        self.pause.send_replace(false);
        Ok(())
    }
    /// `Drain` sends final feedback at the committed cursor and closes the
    /// replication protocol; `Abort` drops the connection. Both honour the
    /// deadline and keep ownership on timeout.
    async fn stop(&mut self, ctx: &StopContext) -> LifecycleResult {
        Ok(self.stop_activation(ctx).await?)
    }
    /// Under `Erase`, drops every slot this source owns (managed ownership
    /// only): its live slot and any coordinated snapshot slots. External
    /// slots are never touched. Idempotent; refuses while an owned slot is
    /// active. Connects with the dispose's activation credentials, so a
    /// source reconstructed after a restart erases without starting.
    async fn dispose(&mut self, ctx: &DisposeContext) -> LifecycleResult {
        if self.worker.get_mut().is_some() {
            return Err(anyhow::anyhow!("dispose requires a stopped PostgreSQL source").into());
        }
        if ctx.erases_owned_state() {
            let config = match ctx.credentials().cloned() {
                Some(credentials) => {
                    let (user, password) = credentials.into_auth_pair();
                    self.config.with_credentials(user, password)
                }
                None => self.config.clone(),
            };
            let dropped = tokio::time::timeout(
                config.timeout().saturating_mul(2),
                drop_owned_slots(&config),
            )
            .await
            .map_err(|_| anyhow::anyhow!("PostgreSQL slot removal timed out"))??;
            if !dropped.is_empty() {
                log::info!("Erased PostgreSQL slots {}", dropped.join(", "));
            }
        }
        Ok(())
    }
}

#[async_trait]
impl NativeManagement for PostgresTransactionSource {
    fn assess(&self, request: &UpdateRequest) -> Result<Assessment> {
        self.live_assessment(request)
    }

    async fn apply(
        &mut self,
        ctx: &ApplyContext,
        update: &ApplyUpdateRequest,
    ) -> Result<(), ApplyError> {
        let (current, proposed) =
            management::configurations(self.mode, &update.request).map_err(refused)?;
        if current.to_value().map_err(refused)?
            != self.source_config().to_value().map_err(refused)?
        {
            return Err(refused(
                "the source configuration changed after the update was assessed",
            ));
        }
        let assessment = self.live_assessment(&update.request).map_err(refused)?;
        let Some(plan) = assessment.plan() else {
            return Err(refused("the update is not supported"));
        };
        if plan != update.plan {
            return Err(ApplyError::Refused {
                now_requires: Some(plan.mode.into()),
                reason: "the update now requires a different boundary".into(),
            });
        }
        let mut settings = proposed.settings;
        settings
            .resolve(
                self.progress
                    .as_ref()
                    .map(|progress| progress.graph_id().to_owned())
                    .or_else(|| {
                        self.shared
                            .as_ref()
                            .map(|shared| shared.reader.graph_id().to_owned())
                    })
                    .as_deref()
                    .unwrap_or_default(),
                self.descriptor.id(),
            )
            .map_err(refused)?;
        settings.validate().map_err(refused)?;
        match ctx.boundary() {
            UpdateBoundary::Pause => {
                if self.worker.get_mut().is_some() && !*self.paused.borrow() {
                    return Err(refused("in-place updates require a completed pause"));
                }
                if !management::same_plan(&assessment, UpdateBoundary::Pause) {
                    return Err(refused("only reconnectBudgetMs can change in place"));
                }
                self.reconnect_budget_ms
                    .store(settings.reconnect_budget_ms, Ordering::Relaxed);
            }
            UpdateBoundary::Stop => {
                if self.worker.get_mut().is_some() {
                    return Err(refused("restart updates require a stopped source"));
                }
                self.reconnect_budget_ms
                    .store(settings.reconnect_budget_ms, Ordering::Relaxed);
            }
            UpdateBoundary::Replace => return Err(refused("Replace is not an instance update")),
        }
        self.config = settings;
        if self.worker.get_mut().is_none() {
            self.activate(None);
        }
        Ok(())
    }
}

#[async_trait]
impl NativeSource for PostgresTransactionSource {
    fn source_schema_reader(&self) -> Option<Arc<dyn SourceSchemaReader>> {
        Some(Arc::new(SchemaReader(self.activation.clone())))
    }
    fn recovery_reader(&self) -> Option<SourceProgressView> {
        self.progress.clone()
    }
    fn replay_retention(&self) -> Option<(StorageDurability, ComponentId)> {
        if self.is_rows() && !self.persistent {
            return None;
        }
        Some((
            StorageDurability::LOCAL_PROCESS_RESTART,
            self.progress.as_ref()?.component_id().clone(),
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
        match join_owned_worker(self.worker.get_mut(), self.config.timeout()).await? {
            WorkerCompletion::Completed(Err(error)) => Err(error),
            _ => anyhow::bail!("PostgreSQL replication ended; stop and restart the source"),
        }
    }
}

/// Describes the selected publication tables on demand, with the current
/// activation's configuration (schema reads run only while started).
struct SchemaReader(Arc<std::sync::RwLock<PostgresTransactionConfig>>);

#[async_trait]
impl SourceSchemaReader for SchemaReader {
    async fn describe_schema(&self) -> Result<Option<SourceSchema>> {
        let config = self
            .0
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let timeout = config.timeout().saturating_mul(2);
        Ok(Some(
            tokio::time::timeout(timeout, describe_schema(&config))
                .await
                .map_err(|_| anyhow::anyhow!("PostgreSQL schema discovery timed out"))??,
        ))
    }
}

impl Drop for PostgresTransactionSource {
    fn drop(&mut self) {
        self.shutdown.send_replace(Shutdown::Abort);
        if let Some(worker) = self.worker.get_mut() {
            worker.abort();
            log::warn!("Native PostgreSQL source dropped without awaited worker cleanup");
        }
    }
}

// The host revokes this capability before invoking native stop. Other failures
// propagate, and CLOSED is only accepted by stop after the worker joins.
fn consumer_progress_closed(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<drasi_computation_plugin_sdk::transport::Failure>()
        .is_some_and(|failure| failure.code == drasi_computation_plugin_sdk::abi::status::CLOSED)
}

#[cfg(test)]
mod tests;

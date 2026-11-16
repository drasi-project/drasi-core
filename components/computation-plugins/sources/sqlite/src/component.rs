// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! The native SQLite source component: construction, lifecycle and
//! management. The database worker and REST listener run in
//! [`drasi_sqlite_common::native::runtime`].

use crate::{
    config::SourceConfig,
    management,
    rest::{self, Tokens},
};
use anyhow::{Context, Result};
use async_trait::async_trait;
use drasi_computation_contracts::{
    lifecycle::{LifecycleError, LifecycleErrorKind},
    ComponentDescriptor, ComponentId, ConsumerProgressView, GraphChangeCodec, InputPositionPolicy,
    OutputEnvelope, PipeRequirements, PortDescriptor, PortDirection, PortId, SourceProgressView,
    SourceSchemaReader, SourceTransactionCodec, StreamId,
};
use drasi_computation_plugin_sdk::lifecycle::{
    ApplyContext, ApplyError, Assessment, DisposeContext, InitializeContext, LifecycleResult,
    PauseContext, ResumeContext, StartContext as LifecycleStartContext, StopContext, StopMode,
    UpdateBoundary,
};
use drasi_computation_plugin_sdk::{
    ApplyUpdateRequest, ControlSender, NativeComponent, NativeConsumerProgress, NativeManagement,
    NativeSource, NativeSourceProgress, UpdateRequest,
};
use drasi_core_types::interface::StorageDurability;
use drasi_sqlite_common::{
    native::{
        handle::Command,
        journal::{self, Erased},
        progress,
        runtime::{SqliteRuntime, StartContext},
        SqliteConfig, SqliteHandle, SqliteOutput,
    },
    RestApiConfig,
};
use std::{
    net::SocketAddr,
    sync::{atomic::AtomicU64, Arc},
};
use tokio::sync::{mpsc, RwLock};

pub(crate) enum Progress {
    None,
    Single(SourceProgressView),
    Shared {
        reader: ConsumerProgressView,
        members: Arc<[ComponentId]>,
    },
}

/// A native SQLite source that owns its database connection.
///
/// The four factories construct it from a flat configuration. A Rust host
/// that links this crate can also construct it directly and use
/// [`Self::handle`] for SQL; a loaded plugin offers only the REST API.
pub struct SqliteSource {
    descriptor: ComponentDescriptor,
    stream: StreamId,
    config: SourceConfig,
    engine: SqliteConfig,
    commands: Arc<RwLock<Option<mpsc::Sender<Command>>>>,
    runtime: SqliteRuntime,
    sequence: Arc<AtomicU64>,
    progress: Progress,
    policy: Option<Arc<InputPositionPolicy>>,
    coordinator: Option<crate::coordinator::SourceBinding>,
    tokens: Tokens,
    control: ControlSender,
}

impl SqliteSource {
    /// A volatile source (`changes` or `transactions` output, no journal).
    pub fn new(id: ComponentId, stream: StreamId, config: SqliteConfig) -> Result<Self> {
        anyhow::ensure!(
            config.replay.is_none(),
            "SQLite replay requires its actual progress binding"
        );
        let source = SourceConfig::from_engine(&stream, &config, false)?;
        Self::build(id, source, Progress::None)
    }

    /// A journal source bound to one persistent consumer's progress.
    pub fn new_replayable(
        id: ComponentId,
        stream: StreamId,
        config: SqliteConfig,
        progress: NativeSourceProgress,
    ) -> Result<Self> {
        Self::validate_replay(&config)?;
        let source = SourceConfig::from_engine(&stream, &config, false)?;
        Self::build(id, source, Progress::Single(progress.reader()))
    }

    /// A journal source shared by a fixed set of persistent consumers.
    pub fn new_replayable_with_consumers(
        id: ComponentId,
        stream: StreamId,
        config: SqliteConfig,
        progress: NativeConsumerProgress,
    ) -> Result<Self> {
        Self::validate_replay(&config)?;
        let source = SourceConfig::from_engine(&stream, &config, true)?;
        Self::with_consumers(id, source, progress)
    }

    pub(crate) fn with_consumers(
        id: ComponentId,
        config: SourceConfig,
        progress: NativeConsumerProgress,
    ) -> Result<Self> {
        anyhow::ensure!(
            progress.reader().producer_id() == &id,
            "SQLite consumer progress belongs to another source"
        );
        Self::build(
            id,
            config,
            Progress::Shared {
                reader: progress.reader(),
                members: progress.consumer_ids().to_vec().into(),
            },
        )
    }

    fn validate_replay(config: &SqliteConfig) -> Result<()> {
        anyhow::ensure!(
            config.replay.is_some()
                && config.path.is_some()
                && config.output == SqliteOutput::Transactions,
            "SQLite replay requires a file, journal bounds and complete-transaction output"
        );
        Ok(())
    }

    pub(crate) fn build(id: ComponentId, config: SourceConfig, progress: Progress) -> Result<Self> {
        let engine = config.sqlite()?;
        engine.validate()?;
        anyhow::ensure!(
            matches!(progress, Progress::None) != config.mode.journal(),
            "SQLite progress binding does not match its mode"
        );
        let stream = StreamId::try_new(config.stream.clone())?;
        let commands = Arc::new(RwLock::new(None));
        let mut runtime = SqliteRuntime::new(commands.clone(), engine.timeout());
        // Drain finishes the request that holds an open transaction.
        runtime.drain_open_transactions(true);
        let policy = matches!(progress, Progress::Shared { .. })
            .then(|| journal::position_policy(&id))
            .transpose()?;
        let tokens = Tokens::default();
        tokens.replace(&config)?;
        Ok(Self {
            descriptor: Self::describe(id, engine.output)?,
            stream,
            config,
            engine,
            commands,
            runtime,
            progress,
            policy,
            sequence: Arc::new(AtomicU64::new(0)),
            coordinator: None,
            tokens,
            control: ControlSender::default(),
        })
    }

    pub(crate) fn with_control(mut self, control: ControlSender) -> Self {
        self.control = control;
        self
    }

    pub(crate) fn with_coordinator(mut self, binding: crate::coordinator::SourceBinding) -> Self {
        self.coordinator = Some(binding);
        self
    }

    pub(crate) fn ports(output: SqliteOutput) -> Result<Vec<PortDescriptor>> {
        let schema = match output {
            SqliteOutput::Changes => GraphChangeCodec::schema(),
            SqliteOutput::Transactions => SourceTransactionCodec::schema(),
        };
        Ok(vec![PortDescriptor::new(
            PortId::try_new("out")?,
            PortDirection::Output,
            schema.descriptor().clone(),
            PipeRequirements::default(),
        )])
    }

    pub fn describe(id: ComponentId, output: SqliteOutput) -> Result<ComponentDescriptor> {
        Ok(ComponentDescriptor::try_new(id, Self::ports(output)?)?)
    }

    /// Rust-only SQL access for a host that links this crate. Writes go
    /// through the source's authorizer and are captured; they are refused
    /// with [`drasi_sqlite_common::native::SqlitePaused`] while paused.
    pub fn handle(&self) -> SqliteHandle {
        SqliteHandle::new(self.commands.clone(), &self.engine)
    }

    /// Rust-only: serve the REST API on `config` (unauthenticated, loopback
    /// only). Factories configure REST through `restApi`, `restHost` and
    /// `restPort` instead.
    pub fn with_rest_api(mut self, config: RestApiConfig) -> Result<Self> {
        anyhow::ensure!(
            !self.runtime.cleanup_required() && !self.config.settings.rest_api,
            "native SQLite REST must be configured once before startup"
        );
        config.validate()?;
        let mut proposed = self.config.clone();
        proposed.settings.rest_api = true;
        proposed.settings.rest_host = config.host;
        proposed.settings.rest_port = config.port;
        proposed.validate()?;
        self.config = proposed;
        Ok(self)
    }

    /// The bound REST address of the running activation (useful with port 0).
    pub fn bound_rest_address(&self) -> Option<SocketAddr> {
        self.runtime.bound_rest_address()
    }

    fn ensure_coordinator(&self) -> Result<()> {
        match &self.coordinator {
            Some(binding) => binding.ensure_active(),
            None => Ok(()),
        }
    }

    async fn start_activation(&mut self) -> Result<()> {
        anyhow::ensure!(
            !self.runtime.cleanup_required(),
            "native SQLite source requires awaited cleanup before restart"
        );
        self.ensure_coordinator()?;
        let prepared = match &self.progress {
            Progress::None => None,
            Progress::Single(reader) => {
                Some(progress::prepare_single(reader.clone(), &self.stream, &self.sequence).await?)
            }
            Progress::Shared { reader, members } => Some(
                progress::prepare_group(
                    reader.clone(),
                    Some(members),
                    &self.stream,
                    &self.sequence,
                )
                .await?,
            ),
        };
        let mut lease = None;
        let progress = match prepared {
            None => None,
            Some((reader, observed)) => {
                let epoch = match &self.coordinator {
                    Some(binding) => {
                        let (epoch, exclusive) = binding.live_boundary(&observed)?;
                        lease = Some(exclusive);
                        Some(epoch)
                    }
                    None => None,
                };
                Some((reader, observed, epoch))
            }
        };
        let owner = self
            .coordinator
            .as_ref()
            .map(|binding| binding.owner.clone());
        let rest = rest::settings(&self.config, &self.tokens)?;
        self.runtime.set_timeout(self.engine.timeout());
        self.runtime
            .start(
                StartContext {
                    source: self.descriptor.id().clone(),
                    stream: self.stream.clone(),
                    config: self.engine.clone(),
                    connection: self.config.connection(),
                    sequence: self.sequence.clone(),
                    rest,
                    progress,
                },
                Arc::new((owner, lease)),
            )
            .await?;
        self.ensure_coordinator()
    }

    fn live_assessment(&self, request: &UpdateRequest) -> Result<Assessment> {
        let (current, proposed) = management::configurations(self.config.mode, request)?;
        Ok(management::plan(&current, &proposed))
    }
}

fn refused(reason: impl std::fmt::Display) -> ApplyError {
    ApplyError::Refused {
        now_requires: None,
        reason: reason.to_string(),
    }
}

#[async_trait]
impl NativeComponent for SqliteSource {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    fn management(&mut self) -> Option<&mut dyn NativeManagement> {
        Some(self)
    }
    fn configuration(&self) -> Result<serde_json::Value> {
        self.config.to_value()
    }
    fn consumer_progress_reader(&self) -> Option<ConsumerProgressView> {
        match &self.progress {
            Progress::Shared { reader, .. } => Some(reader.clone()),
            _ => None,
        }
    }
    fn consumer_replay_retention(&self) -> Option<StorageDurability> {
        matches!(self.progress, Progress::Shared { .. })
            .then_some(StorageDurability::LOCAL_PROCESS_RESTART)
    }
    fn input_position_policy(&self) -> Option<Arc<InputPositionPolicy>> {
        self.policy.clone()
    }
    /// Validates the configuration and that a database file's directory
    /// exists. Opens no connection and binds no listener.
    async fn initialize(&mut self, _ctx: &InitializeContext) -> LifecycleResult {
        self.config.validate()?;
        if let Some(path) = &self.engine.path {
            let parent = path
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty());
            if let Some(parent) = parent {
                if !parent.is_dir() {
                    return Err(anyhow::anyhow!(
                        "SQLite database directory {} does not exist",
                        parent.display()
                    )
                    .into());
                }
            }
        }
        Ok(())
    }
    /// Waits for progress readiness, opens the database, runs `initSql`,
    /// starts the worker and binds the REST listener, all within the host's
    /// deadline and cancellation. A failed or cancelled start signals every
    /// spawned worker to exit; a Stop then joins them.
    async fn start(&mut self, ctx: &mut LifecycleStartContext) -> LifecycleResult {
        let cancellation = ctx.cancellation().clone();
        if cancellation.is_cancelled() {
            return Err(LifecycleError::cancelled());
        }
        if self.runtime.cleanup_required() {
            // Running, or an earlier activation awaits Stop: refuse without
            // touching it.
            return Err(anyhow::anyhow!(
                "native SQLite source requires awaited cleanup before restart"
            )
            .into());
        }
        let deadline = ctx.deadline();
        let result = tokio::select! {
            biased;
            _ = cancellation.cancelled() => Err(LifecycleError::cancelled()),
            result = async {
                match deadline {
                    Some(deadline) => tokio::time::timeout_at(deadline, self.start_activation())
                        .await
                        .map_err(|_| LifecycleError::retryable(anyhow::anyhow!(
                            "native SQLite startup exceeded its deadline"
                        )))?
                        .map_err(LifecycleError::from),
                    None => self.start_activation().await.map_err(LifecycleError::from),
                }
            } => result,
        };
        match result {
            Ok(()) => {
                let _ = self.control.ready();
                Ok(())
            }
            Err(error) => {
                self.runtime.request_abort();
                Err(error)
            }
        }
    }
    /// Refuses REST and handle writes (503 with `Retry-After`, or
    /// [`drasi_sqlite_common::native::SqlitePaused`]) and parks the worker at
    /// its next transaction boundary: no writes, no journal replay and no
    /// output until Resume. Reads are still answered. Returns once the worker
    /// acknowledges, within the deadline; a cancelled or expired Pause leaves
    /// the source running as before.
    async fn pause(&mut self, ctx: &PauseContext) -> LifecycleResult {
        let result = self.runtime.pause(ctx.deadline(), ctx.cancellation()).await;
        match result {
            Ok(()) => Ok(()),
            Err(_) if ctx.cancellation().is_cancelled() => Err(LifecycleError::cancelled()),
            Err(error) => Err(LifecycleError::retryable(error)),
        }
    }
    async fn resume(&mut self, _ctx: &ResumeContext) -> LifecycleResult {
        self.runtime.resume();
        Ok(())
    }
    /// Stops REST admission and the worker together. `Drain` lets the open
    /// transaction and in-flight requests finish; `Abort` rolls back and
    /// drops connections. On expiry the error is retryable and the workers
    /// stay owned until a later Stop joins them.
    async fn stop(&mut self, ctx: &StopContext) -> LifecycleResult {
        let mode = match ctx.mode() {
            StopMode::Abort => StopMode::Abort,
            _ => StopMode::Drain,
        };
        let stopped = self
            .runtime
            .stop(mode, ctx.deadline())
            .await
            .map_err(LifecycleError::retryable)?;
        if stopped.lost_volatile_transactions > 0 {
            drasi_computation_network_common::diag!(
                Warn,
                "SQLite source {} stopped with {} committed volatile transaction(s) undelivered",
                self.descriptor.id(),
                stopped.lost_volatile_transactions
            );
        }
        Ok(())
    }
    /// Under `Erase`, a journal factory drops this source's
    /// `__drasi_native_{outbox,consumers,bootstrap,state}_v1` tables in one
    /// transaction; user tables are never touched. A missing file or journal,
    /// or a journal owned by another source, is success. A locked database is
    /// a retryable failure that changed nothing. Volatile factories own no
    /// state, so their erase is a no-op.
    async fn dispose(&mut self, ctx: &DisposeContext) -> LifecycleResult {
        if self.runtime.cleanup_required() {
            return Err(LifecycleError::retryable(anyhow::anyhow!(
                "dispose requires a stopped SQLite source"
            )));
        }
        if !ctx.erases_owned_state() || !self.config.mode.journal() {
            return Ok(());
        }
        let path = self
            .engine
            .path
            .clone()
            .context("SQLite journal source has no database file")?;
        let source = self.descriptor.id().clone();
        let timeout = ctx
            .deadline()
            .map_or(self.engine.timeout(), |deadline| {
                deadline.saturating_duration_since(tokio::time::Instant::now())
            })
            .min(self.engine.transactions.duration());
        let erased =
            tokio::task::spawn_blocking(move || journal::erase_owned(&path, &source, timeout))
                .await
                .map_err(|error| {
                    LifecycleError::retryable(anyhow::anyhow!("SQLite erase task failed: {error}"))
                })?
                .map_err(|error| LifecycleError::new(LifecycleErrorKind::Retryable, error))?;
        if erased == Erased::NotOwned {
            drasi_computation_network_common::diag!(
                Info,
                "SQLite journal in {} belongs to another source; erase left it in place",
                self.engine
                    .path
                    .as_deref()
                    .unwrap_or(std::path::Path::new(""))
                    .display()
            );
        }
        Ok(())
    }
}

#[async_trait]
impl NativeManagement for SqliteSource {
    fn assess(&self, request: &UpdateRequest) -> Result<Assessment> {
        self.live_assessment(request)
    }

    async fn apply(
        &mut self,
        ctx: &ApplyContext,
        update: &ApplyUpdateRequest,
    ) -> Result<(), ApplyError> {
        let (current, proposed) =
            management::configurations(self.config.mode, &update.request).map_err(refused)?;
        if current.to_value().map_err(refused)? != self.config.to_value().map_err(refused)? {
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
        let engine = proposed.sqlite().map_err(refused)?;
        engine.validate().map_err(refused)?;
        match ctx.boundary() {
            UpdateBoundary::Pause => {
                if self.runtime.cleanup_required() && !self.runtime.paused() {
                    return Err(refused("in-place updates require a completed pause"));
                }
                if !management::supported_at(&assessment, UpdateBoundary::Pause) {
                    return Err(refused(
                        "only authToken and authTokenSecondary change in place",
                    ));
                }
                self.tokens.replace(&proposed).map_err(refused)?;
            }
            UpdateBoundary::Stop => {
                if self.runtime.cleanup_required() {
                    return Err(refused("restart updates require a stopped source"));
                }
                self.tokens.replace(&proposed).map_err(refused)?;
                self.runtime.set_timeout(engine.timeout());
            }
            UpdateBoundary::Replace => return Err(refused("Replace is not an instance update")),
        }
        self.engine = engine;
        self.config = proposed;
        Ok(())
    }
}

#[async_trait]
impl NativeSource for SqliteSource {
    fn source_schema_reader(&self) -> Option<Arc<dyn SourceSchemaReader>> {
        Some(self.runtime.source_schema_reader())
    }
    fn recovery_reader(&self) -> Option<SourceProgressView> {
        match &self.progress {
            Progress::Single(progress) => Some(progress.clone()),
            _ => None,
        }
    }
    fn replay_retention(&self) -> Option<(StorageDurability, ComponentId)> {
        self.recovery_reader().map(|reader| {
            (
                StorageDurability::LOCAL_PROCESS_RESTART,
                reader.component_id().clone(),
            )
        })
    }
    async fn next(&mut self) -> Result<Option<OutputEnvelope>> {
        self.ensure_coordinator()?;
        let output = match self.runtime.next().await {
            Ok(output) => output,
            Err(error) => {
                // A worker or listener failure after start is reported once,
                // here, and withdrawn readiness tells the host the source is
                // down until it is stopped and restarted.
                let _ = self.control.not_ready();
                return Err(error);
            }
        };
        self.ensure_coordinator()?;
        Ok(output)
    }
}

// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! The native SQL Server source component: construction, lifecycle and
//! management. Polling runs in [`crate::worker`].

use crate::{
    config::SourceConfig,
    management,
    progress::Membership,
    worker::{Output, Shared, Shutdown, Worker},
};
use anyhow::{bail, ensure, Context, Result};
use async_trait::async_trait;
use drasi_computation_contracts::pause::PauseControl;
use drasi_computation_contracts::{
    workers::{
        cancel_owned_worker, join_owned_worker, join_owned_worker_gracefully, spawn_owned_worker,
        WorkerCleanupError, WorkerCompletion,
    },
    ComponentDescriptor, ComponentId, ConsumerProgressView, GraphChangeCodec, InputPositionPolicy,
    OutputEnvelope, PipeRequirements, PortDescriptor, PortDirection, PortId, SourceProgressKey,
    SourceSchemaReader, StreamId,
};
use drasi_computation_plugin_sdk::lifecycle::{
    ApplyContext, ApplyError, Assessment, CancellationToken, DisposeContext, InitializeContext,
    LifecycleError, LifecycleResult, PauseContext, ResumeContext, StartContext, StopContext,
    StopMode, UpdateBoundary,
};
use drasi_computation_plugin_sdk::{
    ActivationCredentials, ApplyUpdateRequest, NativeComponent, NativeManagement, NativeSource,
    UpdateRequest,
};
use drasi_core_types::{interface::StorageDurability, schema::SourceSchema};
use drasi_mssql_common::{
    client::Authentication,
    native::{
        self, error::is_transient, markers, CdcConnector, CursorComparator, NativeSettings, Poller,
        TiberiusConnector,
    },
};
use std::{
    collections::{BTreeSet, VecDeque},
    future::Future,
    sync::{atomic::Ordering, Arc, Mutex, RwLock, Weak},
    time::Duration,
};
use tokio::{
    sync::{mpsc, watch, RwLock as AsyncRwLock},
    task::JoinHandle,
    time::Instant,
};

/// Opens connectors for a configuration and authentication. Production uses
/// Tiberius; tests inject an in-memory database.
pub type ConnectorFactory =
    Arc<dyn Fn(&NativeSettings, &Authentication) -> Arc<dyn CdcConnector> + Send + Sync>;

/// The production connector factory.
pub fn tiberius_connectors() -> ConnectorFactory {
    Arc::new(|settings, authentication| {
        Arc::new(TiberiusConnector::new(settings, authentication)) as Arc<dyn CdcConnector>
    })
}

/// Bound on a graceful session close when the host gives no deadline.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(5);

/// State the schema reader may use while the source exists.
struct ReaderState {
    shared: Arc<Shared>,
    connectors: ConnectorFactory,
    authentication: Mutex<Option<Authentication>>,
    identity_bound: bool,
}

impl ReaderState {
    fn authentication(&self, settings: &NativeSettings) -> Result<Authentication> {
        if let Some(authentication) = self
            .authentication
            .lock()
            .map_err(|_| anyhow::anyhow!("SQL Server credential lock poisoned"))?
            .clone()
        {
            return Ok(authentication);
        }
        ensure!(
            !self.identity_bound,
            "this SQL Server source authenticates with its activation identity, which is \
             available only after a start in this process"
        );
        Ok(settings.literal_authentication())
    }
}

/// The native SQL Server CDC source (`drasi.mssql/changes`).
pub struct MsSqlSource {
    descriptor: ComponentDescriptor,
    stream: StreamId,
    graph: String,
    progress: ConsumerProgressView,
    members: BTreeSet<ComponentId>,
    policy: Arc<InputPositionPolicy>,
    state: Arc<ReaderState>,
    schema_reader: Arc<SchemaReader>,
    /// Least time between idle progress records.
    idle_progress: Duration,
    receiver: Option<mpsc::Receiver<OutputEnvelope>>,
    shutdown: watch::Sender<Shutdown>,
    pause: PauseControl,
    worker: AsyncRwLock<Option<JoinHandle<Result<()>>>>,
    cleanup_required: bool,
}

impl MsSqlSource {
    pub(crate) fn ports() -> Result<Vec<PortDescriptor>> {
        Ok(vec![PortDescriptor::new(
            PortId::try_new("out")?,
            PortDirection::Output,
            GraphChangeCodec::schema().descriptor().clone(),
            PipeRequirements::default(),
        )])
    }

    pub fn describe(id: ComponentId) -> Result<ComponentDescriptor> {
        Ok(ComponentDescriptor::try_new(id, Self::ports()?)?)
    }

    /// Configuration-only construction; no I/O.
    pub fn new(
        id: ComponentId,
        config: SourceConfig,
        progress: ConsumerProgressView,
        members: &[ComponentId],
        identity_bound: bool,
        connectors: ConnectorFactory,
    ) -> Result<Self> {
        config.validate()?;
        config.settings.validate_credentials(identity_bound)?;
        let unique: BTreeSet<_> = members.iter().cloned().collect();
        ensure!(
            progress.producer_id() == &id
                && !unique.is_empty()
                && unique.len() == members.len()
                && !unique.contains(&id),
            "invalid SQL Server consumer membership"
        );
        let state = Arc::new(ReaderState {
            shared: Arc::new(Shared {
                settings: RwLock::new(config.settings),
                sequence: Default::default(),
            }),
            connectors,
            authentication: Mutex::new(None),
            identity_bound,
        });
        Ok(Self {
            policy: Arc::new(InputPositionPolicy::new(
                SourceProgressKey::Source(id.clone()),
                Arc::new(CursorComparator),
            )?),
            descriptor: Self::describe(id)?,
            stream: StreamId::try_new(config.stream)?,
            graph: progress.graph_id().to_owned(),
            progress,
            members: unique,
            schema_reader: Arc::new(SchemaReader(Arc::downgrade(&state))),
            state,
            idle_progress: crate::worker::IDLE_PROGRESS,
            receiver: None,
            shutdown: watch::channel(Shutdown::Running).0,
            pause: PauseControl::new(),
            worker: AsyncRwLock::new(None),
            cleanup_required: false,
        })
    }

    fn settings(&self) -> Result<NativeSettings> {
        self.state.shared.settings()
    }

    fn source_config(&self) -> Result<SourceConfig> {
        Ok(SourceConfig {
            stream: self.stream.as_str().to_owned(),
            settings: self.settings()?,
        })
    }

    /// The authentication for an activation: provider credentials when an
    /// identity is bound (never falling back to literals), else the literal
    /// SQL credentials. A token is a Microsoft Entra ID access token.
    fn authentication(
        &self,
        settings: &NativeSettings,
        credentials: Option<ActivationCredentials>,
    ) -> Result<Authentication> {
        match credentials {
            Some(credentials) => {
                ensure!(
                    self.state.identity_bound,
                    "SQL Server activation credentials were not bound by the host"
                );
                credentials.validate()?;
                match credentials {
                    ActivationCredentials::UsernamePassword { username, password } => {
                        ensure!(
                            !username.is_empty() && !password.is_empty(),
                            "the identity provider returned an empty SQL login or password"
                        );
                        Ok(Authentication::Sql {
                            user: username,
                            password,
                        })
                    }
                    ActivationCredentials::Token { token, .. } => {
                        ensure!(
                            !token.is_empty(),
                            "the identity provider returned an empty access token"
                        );
                        Ok(Authentication::AccessToken { token })
                    }
                }
            }
            None => {
                ensure!(
                    !self.state.identity_bound,
                    "a provider-bound SQL Server source cannot fall back to literal credentials"
                );
                Ok(settings.literal_authentication())
            }
        }
    }

    /// Waits for the consumers, connects, reads the catalog, checks the
    /// committed cursor and builds the worker. Bounded by the caller.
    async fn open(
        &mut self,
        settings: NativeSettings,
        authentication: &Authentication,
    ) -> Result<Worker> {
        let snapshot = self.progress.wait_ready().await?;
        let mut membership = Membership::new(
            self.descriptor.id().clone(),
            self.stream.clone(),
            self.members.clone(),
        );
        let observation = membership.observe(&snapshot)?;
        self.state
            .shared
            .sequence
            .fetch_max(observation.sequence, Ordering::AcqRel);
        let connector = (self.state.connectors)(&settings, authentication);
        let mut session = connector.connect().await?;
        let poller =
            Poller::open(session.as_mut(), &settings, self.descriptor.id().as_str()).await?;
        // Fresh members of a group with committed cursors join at its
        // minimum. Only a group with no cursor at all resolves an origin;
        // `current` is then committed by every member before any change is
        // published, while `beginning` is the retained minimum and needs no
        // record.
        let (cursor, origin) = match observation.minimum.clone() {
            Some(minimum) => {
                poller.check_binding(&minimum)?;
                (Some(minimum), None)
            }
            None => {
                let origin = poller.origin(session.as_mut(), &settings).await?;
                (origin.clone(), origin)
            }
        };
        let mut queue = VecDeque::new();
        if let Some(origin) = origin {
            queue.push_back(Output::Progress(origin));
        }
        Ok(Worker {
            source: self.descriptor.id().clone(),
            stream: self.stream.clone(),
            connector,
            session: Some(session),
            poller,
            shared: self.state.shared.clone(),
            progress: self.progress.clone(),
            membership,
            holding: !queue.is_empty(),
            emitted: cursor.clone(),
            cursor,
            queue,
            last_emission: Instant::now(),
            idle_progress: self.idle_progress,
            pause: self.pause.gate(),
            shutdown: self.shutdown.subscribe(),
            port: PortId::try_new("out")?,
        })
    }

    async fn start_activation(&mut self, ctx: &mut StartContext) -> LifecycleResult {
        if ctx.cancellation().is_cancelled() {
            return Err(LifecycleError::cancelled());
        }
        if self.cleanup_required || self.worker.get_mut().is_some() {
            return Err(anyhow::anyhow!(
                "SQL Server source requires a completed stop before it starts again"
            )
            .into());
        }
        let settings = self.settings()?;
        let authentication = self.authentication(&settings, ctx.take_credentials())?;
        settings
            .connection()
            .warn_about_transport("Native SQL Server source");
        // A failed start keeps this guard: the host stops the component
        // before another start, as for any partial activation.
        self.cleanup_required = true;
        let cancellation = ctx.cancellation().clone();
        let deadline = ctx.deadline();
        let opened = bounded(
            &cancellation,
            deadline,
            self.open(settings.clone(), &authentication),
        )
        .await;
        let mut worker = opened.map_err(lifecycle_error)?;
        *self
            .state
            .authentication
            .lock()
            .map_err(|_| anyhow::anyhow!("SQL Server credential lock poisoned"))? =
            Some(authentication);
        let (sender, receiver) = mpsc::channel(1);
        self.receiver = Some(receiver);
        self.shutdown.send_replace(Shutdown::Running);
        self.pause.resume();
        let close_timeout = settings.command_timeout().min(CLOSE_TIMEOUT);
        spawn_owned_worker(&self.worker, async move {
            let result = worker.run(sender).await;
            match &result {
                Ok(()) => worker.close(close_timeout).await,
                Err(error) => log::error!("Native SQL Server source stopped: {error:#}"),
            }
            result
        })
        .await
        .map_err(anyhow::Error::from)?;
        Ok(())
    }

    async fn pause_activation(&mut self, ctx: &PauseContext) -> LifecycleResult {
        if self.worker.get_mut().is_none() {
            return Ok(());
        }
        self.pause
            .pause(ctx.deadline(), ctx.cancellation())
            .await
            .map_err(|error| {
                if error.is_retryable() {
                    LifecycleError::retryable(anyhow::anyhow!(
                        "SQL Server could not finish its in-flight page before the pause deadline"
                    ))
                } else {
                    error
                }
            })
    }

    async fn stop_activation(&mut self, ctx: &StopContext) -> LifecycleResult {
        let mode = match ctx.mode() {
            StopMode::Abort => Shutdown::Abort,
            _ => Shutdown::Drain,
        };
        self.shutdown.send_replace(mode);
        let timeout = ctx.deadline().map_or(
            self.settings()?.command_timeout().saturating_mul(2),
            |deadline| deadline.saturating_duration_since(Instant::now()),
        );
        let joined = if mode == Shutdown::Abort {
            cancel_owned_worker(self.worker.get_mut(), timeout).await
        } else {
            join_owned_worker_gracefully(self.worker.get_mut(), timeout).await
        };
        match joined {
            // A polling failure was already reported through `next`.
            Ok(WorkerCompletion::Completed(Err(error))) => {
                log::debug!("SQL Server worker had already failed: {error:#}");
            }
            Ok(_) => {}
            Err(error @ WorkerCleanupError::TimedOut { .. }) => return Err(
                LifecycleError::retryable(anyhow::Error::new(error).context(
                    "the SQL Server worker did not stop within the deadline; its in-flight query \
                     is still owned and a later stop completes it",
                )),
            ),
            Err(error) => return Err(anyhow::Error::from(error).into()),
        }
        self.receiver = None;
        self.cleanup_required = false;
        Ok(())
    }

    async fn erase(&mut self, ctx: &DisposeContext) -> Result<()> {
        let settings = self.settings()?;
        let Some(barrier) = settings.barrier()? else {
            log::debug!("SQL Server source owns no barrier markers; nothing to erase");
            return Ok(());
        };
        let authentication = self.state.authentication(&settings).with_context(|| {
            format!(
                "erasing this source's barrier markers needs credentials; delete them manually \
                 with DELETE FROM {} WHERE UPPER(LEFT(CONVERT(char(36), token), 18)) = '{}'",
                barrier.sql(),
                markers::marker_prefix(&self.graph, self.descriptor.id().as_str())
            )
        })?;
        let prefix = markers::marker_prefix(&self.graph, self.descriptor.id().as_str());
        let connector = (self.state.connectors)(&settings, &authentication);
        let erased = bounded(ctx.cancellation(), ctx.deadline(), async {
            let mut session = connector.connect().await?;
            let erased = session.erase_markers(&barrier, &prefix).await?;
            let _ = tokio::time::timeout(CLOSE_TIMEOUT, session.close()).await;
            Ok(erased)
        })
        .await?;
        log::info!(
            "SQL Server source erased {erased} orphaned barrier marker(s) from {}",
            barrier.sql()
        );
        Ok(())
    }

    fn live_assessment(&self, request: &UpdateRequest) -> Result<Assessment> {
        let (current, proposed) = management::configurations(request)?;
        Ok(management::plan(&current, &proposed))
    }

    #[cfg(test)]
    pub(crate) fn set_idle_progress(&mut self, interval: Duration) {
        self.idle_progress = interval;
    }

    #[cfg(test)]
    pub(crate) fn sequence(&self) -> u64 {
        self.state.shared.sequence.load(Ordering::Acquire)
    }
}

/// Runs `work` until it finishes, the token is cancelled or the deadline
/// passes. Dropping `work` drops any connection it owns.
async fn bounded<T>(
    cancellation: &CancellationToken,
    deadline: Option<Instant>,
    work: impl Future<Output = Result<T>>,
) -> Result<T> {
    let deadline = async {
        match deadline {
            Some(deadline) => tokio::time::sleep_until(deadline).await,
            None => std::future::pending().await,
        }
    };
    tokio::select! {
        biased;
        _ = cancellation.cancelled() => Err(anyhow::Error::new(Cancelled)),
        _ = deadline => Err(anyhow::Error::new(DeadlineExpired)),
        result = work => result,
    }
}

#[derive(Debug)]
struct Cancelled;
impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the SQL Server lifecycle operation was cancelled")
    }
}
impl std::error::Error for Cancelled {}

#[derive(Debug)]
struct DeadlineExpired;
impl std::fmt::Display for DeadlineExpired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the SQL Server lifecycle deadline expired")
    }
}
impl std::error::Error for DeadlineExpired {}

fn lifecycle_error(error: anyhow::Error) -> LifecycleError {
    if error.downcast_ref::<Cancelled>().is_some() {
        LifecycleError::cancelled()
    } else if error.downcast_ref::<DeadlineExpired>().is_some() || is_transient(&error) {
        LifecycleError::retryable(error)
    } else {
        error.into()
    }
}

fn refused(reason: impl std::fmt::Display) -> ApplyError {
    ApplyError::Refused {
        now_requires: None,
        reason: reason.to_string(),
    }
}

#[async_trait]
impl NativeComponent for MsSqlSource {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    fn management(&mut self) -> Option<&mut dyn NativeManagement> {
        Some(self)
    }
    fn configuration(&self) -> Result<serde_json::Value> {
        self.source_config()?.to_value()
    }
    fn consumer_progress_reader(&self) -> Option<ConsumerProgressView> {
        Some(self.progress.clone())
    }
    /// Decision MS-21: replay is claimed across a process restart only. CDC
    /// change tables survive power loss, but the cleanup job purges them by
    /// age, so the source cannot promise a stronger replay. A fresh group's
    /// origin is committed by every member before any change is published.
    fn consumer_replay_retention(&self) -> Option<StorageDurability> {
        Some(StorageDurability::LOCAL_PROCESS_RESTART)
    }
    fn input_position_policy(&self) -> Option<Arc<InputPositionPolicy>> {
        Some(self.policy.clone())
    }
    /// Validates the configuration. No connection is opened.
    async fn initialize(&mut self, _ctx: &InitializeContext) -> LifecycleResult {
        self.settings()?.validate()?;
        Ok(())
    }
    /// Waits for the consumers, connects, reads and checks the catalog and
    /// the committed cursor, then starts the worker, all within the host's
    /// deadline and abandoned on cancellation.
    async fn start(&mut self, ctx: &mut StartContext) -> LifecycleResult {
        self.start_activation(ctx).await
    }
    /// Stops at the next page boundary: an in-flight query finishes, queued
    /// changes are held, nothing more is read or emitted. Returns once the
    /// worker acknowledges, within the deadline.
    async fn pause(&mut self, ctx: &PauseContext) -> LifecycleResult {
        self.pause_activation(ctx).await
    }
    async fn resume(&mut self, _ctx: &ResumeContext) -> LifecycleResult {
        self.pause.resume();
        Ok(())
    }
    /// `Drain` finishes the in-flight query and closes the session; `Abort`
    /// drops the connection. Both honour the deadline and keep ownership of
    /// an unfinished worker, so a later stop completes it.
    async fn stop(&mut self, ctx: &StopContext) -> LifecycleResult {
        self.stop_activation(ctx).await
    }
    /// Requires a stopped source. `Erase` deletes this source's orphaned
    /// barrier markers (decision D-1), idempotently; without `barrierTable`
    /// it owns nothing and succeeds.
    async fn dispose(&mut self, ctx: &DisposeContext) -> LifecycleResult {
        if self.worker.get_mut().is_some() {
            return Err(anyhow::anyhow!("dispose requires a stopped SQL Server source").into());
        }
        if ctx.erases_owned_state() {
            self.erase(ctx).await.map_err(lifecycle_error)?;
        }
        Ok(())
    }
}

#[async_trait]
impl NativeManagement for MsSqlSource {
    fn assess(&self, request: &UpdateRequest) -> Result<Assessment> {
        self.live_assessment(request)
    }

    async fn apply(
        &mut self,
        ctx: &ApplyContext,
        update: &ApplyUpdateRequest,
    ) -> Result<(), ApplyError> {
        let (current, proposed) = management::configurations(&update.request).map_err(refused)?;
        if current != self.source_config().map_err(refused)? {
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
        proposed.validate().map_err(refused)?;
        proposed
            .settings
            .validate_credentials(self.state.identity_bound)
            .map_err(refused)?;
        match ctx.boundary() {
            UpdateBoundary::Pause => {
                if self.worker.get_mut().is_some() && !self.pause.is_paused() {
                    return Err(refused("in-place updates require a completed pause"));
                }
                if !management::same_plan(&assessment, UpdateBoundary::Pause) {
                    return Err(refused(
                        "only pollIntervalMs, maxRows, maxBytes and reconnectBudgetMs change in place",
                    ));
                }
            }
            UpdateBoundary::Stop => {
                if self.worker.get_mut().is_some() {
                    return Err(refused("restart updates require a stopped source"));
                }
            }
            UpdateBoundary::Replace => return Err(refused("Replace is not an instance update")),
        }
        *self
            .state
            .shared
            .settings
            .write()
            .map_err(|_| refused("SQL Server settings lock poisoned"))? = proposed.settings;
        Ok(())
    }
}

#[async_trait]
impl NativeSource for MsSqlSource {
    fn source_schema_reader(&self) -> Option<Arc<dyn SourceSchemaReader>> {
        Some(self.schema_reader.clone())
    }
    async fn next(&mut self) -> Result<Option<OutputEnvelope>> {
        let receiver = self
            .receiver
            .as_mut()
            .context("SQL Server source is not running")?;
        if let Some(envelope) = receiver.recv().await {
            return Ok(Some(envelope));
        }
        let timeout = self.settings()?.command_timeout();
        match join_owned_worker(self.worker.get_mut(), timeout).await? {
            WorkerCompletion::Completed(Err(error)) => Err(error),
            _ => bail!("SQL Server polling ended; stop and restart the source"),
        }
    }
}

/// Describes the selected tables on demand, with the source's current
/// settings and credentials. Fenced once the source is dropped.
struct SchemaReader(Weak<ReaderState>);

#[async_trait]
impl SourceSchemaReader for SchemaReader {
    async fn describe_schema(&self) -> Result<Option<SourceSchema>> {
        let state = self
            .0
            .upgrade()
            .context("SQL Server schema reader belongs to a retired source")?;
        let settings = state.shared.settings()?;
        let authentication = state.authentication(&settings)?;
        let connector = (state.connectors)(&settings, &authentication);
        drop(state);
        let timeout = settings
            .connect_timeout()
            .saturating_add(settings.command_timeout().saturating_mul(4));
        let schema = tokio::time::timeout(timeout, async {
            let mut session = connector.connect().await?;
            let schema = native::describe_schema(session.as_mut(), &settings).await?;
            let _ = tokio::time::timeout(CLOSE_TIMEOUT, session.close()).await;
            Ok::<_, anyhow::Error>(schema)
        })
        .await
        .map_err(|_| anyhow::anyhow!("SQL Server schema discovery timed out"))??;
        Ok(Some(schema))
    }
}

impl Drop for MsSqlSource {
    fn drop(&mut self) {
        self.shutdown.send_replace(Shutdown::Abort);
        if let Some(worker) = self.worker.get_mut() {
            worker.abort();
            log::warn!("Native SQL Server source dropped without awaited worker cleanup");
        }
    }
}

#[cfg(test)]
#[path = "component_tests.rs"]
mod tests;

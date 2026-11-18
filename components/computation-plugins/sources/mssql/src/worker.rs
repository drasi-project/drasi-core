// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! The polling worker: one per activation. It owns one SQL Server session
//! (reused across polls), reads bounded CDC pages after its read cursor and
//! emits their changes, one envelope at a time, into a capacity-one channel
//! that `next` drains.
//!
//! - **Pause** is honoured at a page boundary: never while a query is in
//!   flight. The worker then acknowledges, holds any queued changes and reads
//!   nothing until Resume. A reconnecting worker counts as paused.
//! - **Stop** is observed at the same boundaries: `Drain` lets the in-flight
//!   query finish, then closes the session; `Abort` cancels the task, which
//!   drops the connection (Tiberius cannot send a TDS ATTENTION, so SQL
//!   Server ends the request when it notices the closed connection).
//! - **Reconnect:** a transport failure (by type or SQL Server error number)
//!   drops the session and reconnects with full-jitter backoff until
//!   `reconnectBudgetMs` is spent, revalidating the binding, then resumes at
//!   the read cursor. Everything before that cursor is already queued in
//!   this worker, so nothing is lost or replayed; after a restart the source
//!   resumes at the members' committed minimum instead. Typed permanent
//!   failures never reconnect.
//! - **Progress records:** a start that finds a member without a cursor
//!   first emits a progress-only record at the group's origin and holds
//!   every change until each member has committed a cursor, so the origin is
//!   durable before anything is published. While the selected tables are
//!   idle, an empty page's boundary is emitted as a progress record at most
//!   once per `idle_progress` interval, so committed cursors follow CDC
//!   retention instead of stranding at the last change.

use crate::progress::Membership;
use anyhow::{Context, Result};
use drasi_computation_contracts::pause::{PauseGate, PauseState};
use drasi_computation_contracts::{
    exchange::{SourceEvent, SourceEventWrapper},
    reconnect::BackoffPolicy,
    ChangeOperation, ChangeSet, ComponentId, ConsumerProgressView, GraphChangeCodec,
    OutputEnvelope, PortId, StreamId,
};
use drasi_mssql_common::native::{
    error::is_transient, CdcConnector, CdcSession, Change, Cursor, NativeSettings, Poller,
};
use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, RwLock,
    },
    time::Duration,
};
use tokio::sync::{mpsc, watch};

/// First reconnect backoff ceiling; doubles per attempt.
pub(crate) const RECONNECT_BASE: Duration = Duration::from_millis(250);
/// Largest reconnect backoff ceiling.
pub(crate) const RECONNECT_MAX: Duration = Duration::from_secs(30);

/// What the worker should do when the host stops it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Shutdown {
    Running,
    Drain,
    Abort,
}

/// Least time between two idle progress records.
pub(crate) const IDLE_PROGRESS: Duration = Duration::from_secs(30);

/// State shared by the component, its worker and its schema reader.
pub(crate) struct Shared {
    /// The live settings; in-place updates change the polling fields.
    pub settings: RwLock<NativeSettings>,
    /// The last logical sequence allocated, across activations.
    pub sequence: AtomicU64,
}

/// One queued emission.
pub(crate) enum Output {
    Change(Change),
    /// A progress-only record at a position that holds no further change.
    Progress(Cursor),
}

impl Shared {
    pub fn settings(&self) -> Result<NativeSettings> {
        Ok(self
            .settings
            .read()
            .map_err(|_| anyhow::anyhow!("SQL Server settings lock poisoned"))?
            .clone())
    }
}

pub(crate) struct Worker {
    pub source: ComponentId,
    pub stream: StreamId,
    pub connector: Arc<dyn CdcConnector>,
    pub session: Option<Box<dyn CdcSession>>,
    pub poller: Poller,
    pub shared: Arc<Shared>,
    pub progress: ConsumerProgressView,
    pub membership: Membership,
    /// The read cursor: everything at or before it is queued or emitted.
    /// `None` reads from each capture instance's retained minimum.
    pub cursor: Option<Cursor>,
    pub queue: VecDeque<Output>,
    /// Holds polling until every member has committed a cursor (after an
    /// origin record).
    pub holding: bool,
    /// The last position emitted, change or progress record.
    pub emitted: Option<Cursor>,
    pub last_emission: tokio::time::Instant,
    pub idle_progress: Duration,
    pub pause: PauseGate,
    pub shutdown: watch::Receiver<Shutdown>,
    pub port: PortId,
}

impl Worker {
    fn stopping(&self) -> bool {
        *self.shutdown.borrow() != Shutdown::Running
    }

    /// Runs until Stop (`Ok`) or a permanent failure.
    pub async fn run(&mut self, sender: mpsc::Sender<OutputEnvelope>) -> Result<()> {
        let mut waiting_for_members = false;
        loop {
            if self.stopping() {
                return Ok(());
            }
            // The loop top is a page boundary: acknowledge any pause here.
            if self.pause.sync() == PauseState::Paused {
                tokio::select! {
                    _ = self.pause.changed() => {}
                    _ = self.shutdown.changed() => {}
                }
                continue;
            }
            if !self.queue.is_empty() {
                tokio::select! {
                    biased;
                    _ = self.shutdown.changed() => {}
                    _ = self.pause.pause_requested() => {}
                    permit = sender.reserve() => {
                        let permit = permit.context("SQL Server output closed")?;
                        let output = self.queue.pop_front().context("queued SQL Server change")?;
                        permit.send(self.encode(output)?);
                    }
                }
                continue;
            }
            let observation = self.membership.observe(&self.progress.snapshot()?)?;
            self.shared
                .sequence
                .fetch_max(observation.sequence, Ordering::AcqRel);
            let settings = self.shared.settings()?;
            if !observation.ready {
                if !waiting_for_members {
                    log::info!("SQL Server source holds polling until every consumer is ready");
                    waiting_for_members = true;
                }
                self.idle(settings.poll_interval()).await;
                continue;
            }
            waiting_for_members = false;
            if self.holding {
                if observation.missing > 0 {
                    self.idle(settings.poll_interval()).await;
                    continue;
                }
                self.holding = false;
                log::info!("SQL Server consumers committed the origin; streaming changes");
            }
            match self.poll(&settings).await {
                Ok(page) => {
                    if page.changes.is_empty() {
                        if self.idle_progress_due(&page.boundary)? {
                            self.queue
                                .push_back(Output::Progress(page.boundary.clone()));
                        } else {
                            self.idle(settings.poll_interval()).await;
                        }
                    } else {
                        self.queue
                            .extend(page.changes.into_iter().map(Output::Change));
                    }
                    self.cursor = Some(page.boundary);
                }
                Err(error) if is_transient(&error) => self.reconnect(error).await?,
                Err(error) => return Err(error),
            }
        }
    }

    async fn poll(
        &mut self,
        settings: &NativeSettings,
    ) -> Result<drasi_mssql_common::native::Poll> {
        if self.session.is_none() {
            self.session = Some(self.connector.connect().await?);
        }
        let session = self
            .session
            .as_mut()
            .context("SQL Server session is not open")?;
        self.poller
            .poll(session.as_mut(), settings, self.cursor.as_ref())
            .await
    }

    /// Sleeps for `delay`, returning early on a pause request or Stop.
    async fn idle(&mut self, delay: Duration) {
        tokio::select! {
            _ = tokio::time::sleep(delay) => {}
            _ = self.pause.pause_requested() => {}
            _ = self.shutdown.changed() => {}
        }
    }

    /// Sleeps without a session, acknowledging any pause meanwhile: nothing
    /// is read or emitted while reconnecting. Returns early on Stop.
    async fn idle_disconnected(&mut self, delay: Duration) {
        let until = tokio::time::Instant::now() + delay;
        loop {
            self.pause.sync();
            tokio::select! {
                _ = tokio::time::sleep_until(until) => return,
                _ = self.shutdown.changed() => return,
                _ = self.pause.changed() => {}
            }
        }
    }

    /// Reconnects after a transport failure, within `reconnectBudgetMs`.
    async fn reconnect(&mut self, error: anyhow::Error) -> Result<()> {
        // Dropping the session closes a connection whose state is unknown.
        self.session = None;
        let budget = self.shared.settings()?.reconnect_budget();
        if budget.is_zero() {
            return Err(error.context("SQL Server connection failed and reconnectBudgetMs is 0"));
        }
        let mut backoff = BackoffPolicy::with_budget(RECONNECT_BASE, RECONNECT_MAX, budget).start();
        let mut failure = error;
        loop {
            let Some(delay) = backoff.next_delay() else {
                return Err(failure.context("SQL Server reconnect budget exhausted"));
            };
            let attempt = backoff.attempt();
            log::warn!(
                "SQL Server connection interrupted; reconnect attempt {attempt} in {delay:?}: {failure:#}"
            );
            self.idle_disconnected(delay).await;
            if self.stopping() {
                return Ok(());
            }
            match self.reopen().await {
                Ok(()) => {
                    log::info!("SQL Server source reconnected after {attempt} attempt(s)");
                    return Ok(());
                }
                Err(error) if is_transient(&error) => failure = error,
                Err(error) => return Err(error),
            }
        }
    }

    /// Opens a session and revalidates the catalog binding.
    async fn reopen(&mut self) -> Result<()> {
        let settings = self.shared.settings()?;
        let mut session = self.connector.connect().await?;
        self.poller
            .refresh(session.as_mut(), &settings, true)
            .await?;
        self.session = Some(session);
        Ok(())
    }

    /// Closes the session gracefully after a Drain stop.
    pub async fn close(&mut self, timeout: Duration) {
        if let Some(session) = self.session.take() {
            match tokio::time::timeout(timeout, session.close()).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => log::debug!("SQL Server session close failed: {error:#}"),
                Err(_) => log::debug!("SQL Server session close timed out"),
            }
        }
    }

    /// Whether an empty page's `boundary` should be committed: it is past
    /// the last emitted position and the selected tables have been quiet for
    /// at least `idle_progress`.
    fn idle_progress_due(&self, boundary: &Cursor) -> Result<bool> {
        if self.last_emission.elapsed() < self.idle_progress {
            return Ok(false);
        }
        Ok(match &self.emitted {
            Some(emitted) => boundary.compare(emitted)? == std::cmp::Ordering::Greater,
            None => true,
        })
    }

    fn encode(&mut self, output: Output) -> Result<OutputEnvelope> {
        let sequence = self
            .shared
            .sequence
            .fetch_add(1, Ordering::AcqRel)
            .checked_add(1)
            .context("SQL Server sequence exhausted")?;
        self.last_emission = tokio::time::Instant::now();
        match output {
            Output::Change(change) => {
                self.emitted = Some(change.position.clone());
                envelope(&self.source, &self.stream, &self.port, sequence, change)
            }
            Output::Progress(cursor) => {
                let envelope =
                    progress_envelope(&self.source, &self.stream, &self.port, sequence, &cursor)?;
                self.emitted = Some(cursor);
                Ok(envelope)
            }
        }
    }
}

/// Encodes a progress-only record at `cursor`.
pub(crate) fn progress_envelope(
    source: &ComponentId,
    stream: &StreamId,
    port: &PortId,
    sequence: u64,
    cursor: &Cursor,
) -> Result<OutputEnvelope> {
    Ok(OutputEnvelope {
        execution_context: drasi_computation_contracts::telemetry::ExecutionContext::current(),
        port: port.clone(),
        envelope: GraphChangeCodec::encode_source_progress(
            source,
            stream.clone(),
            sequence,
            cursor.encode()?.into(),
            chrono::Utc::now(),
        )?,
    })
}

/// Encodes one change with its CDC position and, for updates and deletes,
/// its full before image.
pub(crate) fn envelope(
    source: &ComponentId,
    stream: &StreamId,
    port: &PortId,
    sequence: u64,
    change: Change,
) -> Result<OutputEnvelope> {
    let event = SourceEventWrapper::new(
        source.clone(),
        SourceEvent::Change(change.change),
        chrono::Utc::now(),
        sequence,
    )
    .with_source_position(change.position.encode()?.into());
    let mut envelope = GraphChangeCodec::encode_source_event(
        Arc::new(event),
        source,
        stream.clone(),
        sequence,
        None,
    )?;
    if let Some(before) = change.before {
        let before = GraphChangeCodec::encode_change(
            drasi_core_types::models::SourceChange::Insert { element: before },
            stream.clone(),
            sequence,
            None,
        )?;
        let ChangeOperation::Added { after: before, .. } = &before.changes().operations()[0] else {
            anyhow::bail!("invalid SQL Server before image");
        };
        let mut operation = envelope.changes().operations()[0].clone();
        match &mut operation {
            ChangeOperation::Updated { before: image, .. }
            | ChangeOperation::Deleted { before: image, .. } => *image = Some(before.clone()),
            _ => anyhow::bail!("unexpected SQL Server before-image operation"),
        }
        let changes = ChangeSet::try_new(
            envelope.changes().id().clone(),
            envelope.changes().schema().clone(),
            vec![operation],
        )?;
        envelope = envelope.derive(
            envelope.id().clone(),
            changes,
            envelope.system().as_ref().clone(),
        );
    }
    Ok(OutputEnvelope {
        execution_context: drasi_computation_contracts::telemetry::ExecutionContext::current(),
        port: port.clone(),
        envelope,
    })
}

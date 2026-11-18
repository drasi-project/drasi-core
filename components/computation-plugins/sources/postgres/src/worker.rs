// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! The replication worker: reads pgoutput, emits whole commits (or their
//! rows), and sends standby feedback that follows the host's committed
//! progress.
//!
//! The worker also implements pause (stop reading and emitting at a
//! transaction boundary while feedback continues), reconnection after a
//! transport failure in every mode, and the final feedback of a Drain stop.
//!
//! **Progress records.** Standby feedback only follows committed cursors, so
//! a publication whose selected tables are quiet would make the server
//! retain WAL without bound. While nothing is open or queued and no output
//! has been emitted for `idle_progress`, the worker emits a progress record
//! (an empty transaction for whole-transaction outputs) at a boundary proven
//! to hold no unhandled change: a commit without selected changes, or the
//! WAL end of a primary keepalive minus one. Every transaction committing at
//! or after a keepalive's WAL end is still to be sent, and the decoder skips
//! only commits at or below its cursor. Once consumers commit the record,
//! feedback and the slot advance with it.
//!
//! **Consumer groups.** A start that finds a group member without a cursor
//! first emits a record at the group's join point (the fresh origin, or the
//! slowest member's cursor) and reads nothing further until every member
//! has committed a cursor, so the join point is durable before anything is
//! published.

use crate::rows::RowEmission;
use crate::shared::SharedUpdates;
use crate::whole::WholeEmission;
use anyhow::{Context, Result};
use drasi_computation_contracts::pause::{PauseGate, PauseState};
use drasi_computation_contracts::reconnect::BackoffPolicy;
use drasi_computation_contracts::{
    OutputEnvelope, SourceProgressUpdates, SourceTransactionError, StreamId,
};
use drasi_postgres_common::{
    decoder::PgOutputDecoder,
    protocol::BackendMessage,
    replication::{
        checkpoint, connect, ensure_binding, ensure_retained, is_transient, rows::row_checkpoint,
        slot_positions, source_binding, start_streaming, validate_retention, Connection, Decoded,
        PostgresTransactionConfig, Transactions,
    },
    types::StandbyStatusUpdate,
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
    sync::{mpsc, watch},
    time::Instant,
};

/// Offset of the WAL payload in an XLogData (`w`) message: the tag plus
/// start, end and send-time fields.
const XLOG_DATA_HEADER: usize = 25;
/// Length of a primary keepalive (`k`) message: the tag, WAL end, send time
/// and reply-requested flag.
const KEEPALIVE_LEN: usize = 18;
/// Offset of the keepalive's reply-requested flag.
const KEEPALIVE_REPLY: usize = 17;
/// First reconnect backoff ceiling; doubles per attempt.
const RECONNECT_BASE: Duration = Duration::from_millis(250);
/// Largest reconnect backoff ceiling.
const RECONNECT_MAX: Duration = Duration::from_secs(30);
/// Least quiet time before a progress record, as for the MySQL and SQL
/// Server sources.
pub(crate) const IDLE_PROGRESS: Duration = Duration::from_secs(30);

pub(crate) enum WorkerProgress {
    Single {
        updates: Box<dyn SourceProgressUpdates>,
        generation: u64,
    },
    Shared(SharedUpdates),
    SharedTransactions(SharedUpdates),
}

impl WorkerProgress {
    async fn changed(&mut self) -> Result<()> {
        match self {
            Self::Single { updates, .. } => updates.changed().await,
            Self::Shared(progress) | Self::SharedTransactions(progress) => {
                progress.updates.changed().await
            }
        }
    }
}

/// The output a worker produces.
pub(crate) enum Emission {
    Rows(RowEmission),
    Whole(WholeEmission),
}

pub(crate) struct Worker {
    pub connection: Option<Connection>,
    pub stream: StreamId,
    pub transactions: Transactions,
    pub progress: WorkerProgress,
    pub confirmed: u64,
    pub next_feedback: Instant,
    pub config: PostgresTransactionConfig,
    pub emission: Emission,
    /// Outputs waiting for the channel, in order.
    pub queue: VecDeque<OutputEnvelope>,
    /// Reads nothing until every member of the consumer group has committed
    /// a cursor (after the join record).
    pub holding: bool,
    /// A fresh group's origin, the resume point while no member has a
    /// cursor.
    pub origin: Option<u64>,
    pub last_emission: Instant,
    pub idle_progress: Duration,
    pub pause: PauseGate,
    /// Live `reconnectBudgetMs`; `0` disables reconnection.
    pub reconnect_budget_ms: Arc<AtomicU64>,
}

impl Worker {
    fn paused(&self) -> bool {
        self.pause.is_pause_requested()
    }

    /// Whether reading may stop here: between transactions, or while a row
    /// transaction streams.
    fn at_boundary(&self) -> bool {
        !self.transactions.in_transaction()
            || matches!(&self.emission, Emission::Rows(rows) if rows.at_boundary())
    }

    fn assembly_deadline(&self) -> Option<Instant> {
        match &self.emission {
            Emission::Rows(rows) => rows.deadline(),
            Emission::Whole(whole) => whole.deadline(),
        }
    }

    /// Whether a progress record is due: nothing is queued or held and no
    /// output has been emitted for `idle_progress`.
    fn idle_due(&self) -> bool {
        !self.holding && self.queue.is_empty() && self.last_emission.elapsed() >= self.idle_progress
    }

    pub async fn feedback(&mut self) -> Result<()> {
        let next = match &mut self.progress {
            WorkerProgress::Single {
                updates,
                generation,
            } => {
                let snapshot = updates.snapshot()?;
                anyhow::ensure!(
                    snapshot.failure.is_none(),
                    "PostgreSQL consumer progress is fenced"
                );
                anyhow::ensure!(
                    snapshot.reset_generation == *generation,
                    "PostgreSQL consumer was reset; explicit source reconstruction is required"
                );
                if snapshot.ready {
                    match &self.emission {
                        Emission::Rows(rows) => rows.confirmed(&snapshot, &self.transactions)?,
                        Emission::Whole(_) => checkpoint(
                            &snapshot,
                            &self.transactions.source,
                            self.transactions.binding,
                        )?,
                    }
                } else {
                    None
                }
            }
            WorkerProgress::Shared(progress) => {
                let observed = progress.snapshot(
                    &self.transactions.source,
                    &self.stream,
                    self.transactions.binding,
                )?;
                let Emission::Rows(rows) = &self.emission else {
                    anyhow::bail!("shared source has no row emissions");
                };
                rows.validate_shared_observation(observed.max_counter, observed.max_lsn)?;
                self.release(observed.ready, observed.missing);
                if observed.ready {
                    observed.minimum.map(|minimum| minimum.feedback_lsn)
                } else {
                    None
                }
            }
            WorkerProgress::SharedTransactions(progress) => {
                let observed = progress.transaction_snapshot(
                    &self.transactions.source,
                    &self.stream,
                    self.transactions.binding,
                )?;
                let Emission::Whole(whole) = &self.emission else {
                    anyhow::bail!("shared source has no whole-transaction emissions");
                };
                whole
                    .shared()
                    .context("shared source has no shared transaction cursors")?
                    .validate_observation(observed.max_counter, observed.max_lsn)?;
                self.release(observed.ready, observed.missing);
                if observed.ready {
                    observed.minimum.map(|minimum| minimum.position.commit_lsn)
                } else {
                    None
                }
            }
        };
        if let Some(next) = next {
            anyhow::ensure!(
                next >= self.confirmed && next <= self.admitted_lsn(),
                "PostgreSQL committed cursor moved outside the admitted range"
            );
            self.confirmed = next;
        }
        let io_deadline = Instant::now() + self.config.timeout();
        let assembly_deadline = self.assembly_deadline();
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
        self.next_feedback = Instant::now() + self.config.feedback_interval();
        Ok(())
    }

    /// Ends the hold once every group member has committed a cursor.
    fn release(&mut self, ready: bool, missing: bool) {
        if self.holding && ready && !missing {
            self.holding = false;
            log::info!("PostgreSQL consumers committed their join point; streaming changes");
        }
    }

    fn admitted_lsn(&self) -> u64 {
        match &self.emission {
            Emission::Rows(rows) => rows.admitted_lsn(),
            Emission::Whole(whole) => whole.admitted_lsn(self.transactions.resume),
        }
    }

    /// Streams until a permanent failure. A transport failure reconnects at
    /// the committed cursor with jittered exponential backoff until
    /// `reconnectBudgetMs` is spent.
    pub async fn run(&mut self, sender: mpsc::Sender<OutputEnvelope>) -> Result<()> {
        loop {
            let error = match self.run_connected(sender.clone()).await {
                Ok(()) => return Ok(()),
                Err(error) => error,
            };
            let budget = match self.reconnect_budget_ms.load(Ordering::Relaxed) {
                0 => return Err(error),
                millis => Duration::from_millis(millis),
            };
            if !is_transient(&error) {
                return Err(error);
            }
            self.connection.take();
            let mut backoff =
                BackoffPolicy::with_budget(RECONNECT_BASE, RECONNECT_MAX, budget).start();
            let mut failure = error;
            loop {
                let Some(delay) = backoff.next_delay() else {
                    return Err(failure.context("PostgreSQL reconnect budget exhausted"));
                };
                let attempt = backoff.attempt();
                log::warn!(
                    "PostgreSQL connection interrupted; reconnect attempt {attempt} in {delay:?}: {failure:#}"
                );
                self.idle(delay).await;
                match tokio::time::timeout(self.config.timeout(), self.reopen()).await {
                    Ok(Ok(())) => {
                        log::info!("PostgreSQL replication reconnected after {attempt} attempt(s)");
                        break;
                    }
                    Ok(Err(error)) if is_transient(&error) => failure = error,
                    Ok(Err(error)) => return Err(error),
                    Err(elapsed) => {
                        failure =
                            anyhow::Error::new(elapsed).context("PostgreSQL reconnect timed out")
                    }
                }
            }
        }
    }

    /// Sleeps without a connection, acknowledging any pause meanwhile:
    /// nothing is read or emitted while reconnecting.
    async fn idle(&mut self, delay: Duration) {
        let until = Instant::now() + delay;
        loop {
            self.pause.sync();
            tokio::select! {
                _ = tokio::time::sleep_until(until) => return,
                _ = self.pause.changed() => {}
            }
        }
    }

    /// Reopens replication at the committed cursor, after a transport
    /// failure. Output after the cursor is replayed; consumers deduplicate by
    /// their committed positions, as after a restart. Queued output is
    /// dropped; a group still holding gets its join record again.
    async fn reopen(&mut self) -> Result<()> {
        use drasi_postgres_common::replication::rows::RowPosition;
        enum Rewind {
            Single {
                position: Option<RowPosition>,
                previous: u64,
            },
            Shared {
                position: Option<RowPosition>,
                previous: u64,
                floor: (u64, u64, std::collections::BTreeSet<u64>),
            },
            Whole {
                floor: (u64, u64),
            },
            Transactions,
        }
        let (resume, confirmed, rewind) = match &mut self.progress {
            WorkerProgress::Single {
                updates,
                generation,
            } => {
                let progress = updates.snapshot()?;
                anyhow::ensure!(
                    progress.ready
                        && progress.failure.is_none()
                        && progress.reset_generation == *generation,
                    "PostgreSQL progress owner is not ready for safe reconnection"
                );
                if matches!(self.emission, Emission::Rows(_)) {
                    let position = row_checkpoint(
                        &progress,
                        &self.transactions.source,
                        self.transactions.binding,
                    )?;
                    let confirmed = position
                        .as_ref()
                        .map_or(self.confirmed, RowPosition::feedback_lsn);
                    let resume = position
                        .as_ref()
                        .map_or(confirmed, |position| position.commit_lsn);
                    (
                        resume,
                        confirmed,
                        Rewind::Single {
                            position,
                            previous: confirmed,
                        },
                    )
                } else {
                    let committed = checkpoint(
                        &progress,
                        &self.transactions.source,
                        self.transactions.binding,
                    )?
                    .unwrap_or(self.confirmed);
                    (committed, committed, Rewind::Transactions)
                }
            }
            WorkerProgress::Shared(progress) => {
                let observed = progress.snapshot(
                    &self.transactions.source,
                    &self.stream,
                    self.transactions.binding,
                )?;
                anyhow::ensure!(
                    observed.ready,
                    "PostgreSQL shared consumers are not ready for safe reconnection"
                );
                let Emission::Rows(rows) = &self.emission else {
                    anyhow::bail!("shared source has no row emissions");
                };
                rows.validate_shared_observation(observed.max_counter, observed.max_lsn)?;
                let (position, confirmed, previous) = match observed.minimum {
                    Some(minimum) => {
                        let previous = minimum.position.feedback_lsn();
                        (Some(minimum.position), minimum.feedback_lsn, previous)
                    }
                    None => {
                        let origin = retained_origin(self.origin, self.confirmed)?;
                        (None, origin, origin)
                    }
                };
                let resume = position
                    .as_ref()
                    .map_or(confirmed, |position| position.commit_lsn);
                (
                    resume,
                    confirmed,
                    Rewind::Shared {
                        position,
                        previous,
                        floor: (observed.max_counter, observed.max_lsn, observed.boundaries),
                    },
                )
            }
            WorkerProgress::SharedTransactions(progress) => {
                let observed = progress.transaction_snapshot(
                    &self.transactions.source,
                    &self.stream,
                    self.transactions.binding,
                )?;
                anyhow::ensure!(
                    observed.ready,
                    "PostgreSQL transaction consumers are not ready for safe reconnection"
                );
                let Emission::Whole(whole) = &self.emission else {
                    anyhow::bail!("shared source has no whole-transaction emissions");
                };
                whole
                    .shared()
                    .context("shared source has no shared transaction cursors")?
                    .validate_observation(observed.max_counter, observed.max_lsn)?;
                let resume = match observed.minimum {
                    Some(minimum) => minimum.position.commit_lsn,
                    None => retained_origin(self.origin, self.confirmed)?,
                };
                (
                    resume,
                    resume,
                    Rewind::Whole {
                        floor: (observed.max_counter, observed.max_lsn),
                    },
                )
            }
        };
        anyhow::ensure!(
            confirmed >= self.confirmed,
            "PostgreSQL committed cursor moved backwards"
        );
        let mut connection = connect(&self.config).await?;
        validate_retention(&mut connection, &self.config).await?;
        let (keys, binding) = source_binding(
            &mut connection,
            &self.config,
            &self.transactions.source,
            &self.stream,
        )
        .await?;
        ensure_binding(self.transactions.binding, binding)?;
        let (retained, slot_confirmed) = slot_positions(&mut connection, &self.config).await?;
        ensure_retained(retained, slot_confirmed, resume, confirmed)?;
        start_streaming(&mut connection, &self.config, resume).await?;
        let decoder = PgOutputDecoder::strict(self.config.max_protocol_bytes);
        let partial = |position: &Option<RowPosition>| {
            position
                .as_ref()
                .is_some_and(|position| !position.complete())
        };
        self.queue.clear();
        match (rewind, &mut self.emission) {
            (Rewind::Single { position, previous }, Emission::Rows(rows)) => {
                self.transactions.restart(
                    decoder,
                    keys,
                    decoder_resume(resume, partial(&position))?,
                );
                rows.rewind(position, previous);
            }
            (
                Rewind::Shared {
                    position,
                    previous,
                    floor,
                },
                Emission::Rows(rows),
            ) => {
                self.transactions.restart(
                    decoder,
                    keys,
                    decoder_resume(resume, partial(&position))?,
                );
                rows.rewind_shared(position.clone(), previous, floor.0, floor.1, floor.2)?;
                if self.holding {
                    self.queue.push_back(rows.join(
                        position.as_ref(),
                        resume,
                        &self.transactions,
                    )?);
                }
            }
            (Rewind::Whole { floor }, Emission::Whole(whole)) => {
                self.transactions.restart(decoder, keys, resume);
                whole.rewind();
                whole
                    .shared()
                    .context("shared source has no shared transaction cursors")?
                    .raise(floor.0, floor.1);
                if self.holding {
                    self.queue.push_back(whole.progress(
                        resume,
                        chrono::Utc::now(),
                        self.transactions.binding,
                        None,
                    )?);
                }
            }
            (Rewind::Transactions, Emission::Whole(whole)) => {
                self.transactions.restart(decoder, keys, resume);
                whole.rewind();
            }
            _ => anyhow::bail!("PostgreSQL progress and output modes disagree"),
        }
        self.connection = Some(connection);
        self.confirmed = confirmed;
        self.next_feedback = Instant::now();
        Ok(())
    }

    /// Final feedback for a Drain stop, then an orderly protocol close.
    /// Failures are logged: the cursor is already committed by the host.
    pub async fn drain(&mut self) {
        let timeout = self.config.timeout();
        if self.connection.is_none() {
            return;
        }
        if let Err(error) = tokio::time::timeout(timeout, self.feedback())
            .await
            .map_err(anyhow::Error::from)
            .and_then(|result| result)
        {
            log::debug!("PostgreSQL final feedback was not sent: {error:#}");
        }
        if let Some(connection) = self.connection.take() {
            let _ = tokio::time::timeout(timeout, connection.close()).await;
        }
    }

    /// Applies one pgoutput message.
    fn handle(&mut self, payload: &[u8]) -> Result<()> {
        let Some(decoded) = self.transactions.decode(payload)? else {
            return Ok(());
        };
        let idle = self.idle_due();
        let transactions = &self.transactions;
        match (decoded, &mut self.emission) {
            (Decoded::Begin(info), Emission::Rows(rows)) => rows.begin(&info)?,
            (Decoded::Begin(_), Emission::Whole(whole)) => whole.begin()?,
            (Decoded::Changes(changes), Emission::Rows(rows)) => {
                rows.push(changes, transactions, &mut self.queue)?
            }
            (Decoded::Changes(changes), Emission::Whole(whole)) => whole.push(changes)?,
            (Decoded::Commit(info), Emission::Rows(rows)) => {
                rows.commit(&info, idle, transactions, &mut self.queue)?
            }
            (Decoded::Commit(info), Emission::Whole(whole)) => {
                if let Some(envelope) = whole.commit(
                    info.commit_lsn,
                    info.commit_timestamp,
                    idle,
                    transactions.binding,
                    transactions.managed_slot.as_deref(),
                )? {
                    self.queue.push_back(envelope);
                }
            }
        }
        Ok(())
    }

    /// A keepalive reports the server's WAL end: every commit before it has
    /// been sent. While idle, commit the range up to it as a progress record.
    fn keepalive(&mut self, wal_end: u64) -> Result<()> {
        let Some(lsn) = wal_end.checked_sub(1) else {
            return Ok(());
        };
        if self.transactions.in_transaction() || lsn <= self.transactions.resume || !self.idle_due()
        {
            return Ok(());
        }
        let timestamp = chrono::Utc::now();
        let envelope = match &mut self.emission {
            Emission::Rows(rows) => rows.progress(lsn, timestamp, &self.transactions)?,
            Emission::Whole(whole) => whole.progress(
                lsn,
                timestamp,
                self.transactions.binding,
                self.transactions.managed_slot.as_deref(),
            )?,
        };
        self.transactions.advance(lsn);
        self.queue.push_back(envelope);
        Ok(())
    }

    async fn run_connected(&mut self, sender: mpsc::Sender<OutputEnvelope>) -> Result<()> {
        loop {
            if let Emission::Whole(whole) = &mut self.emission {
                whole.check_deadline()?;
            }
            if self
                .assembly_deadline()
                .is_some_and(|deadline| deadline <= Instant::now())
            {
                return Err(SourceTransactionError::Deadline.into());
            }
            let boundary = self.at_boundary();
            // Only a boundary acknowledges a pause; elsewhere the worker
            // finishes the open transaction first.
            let paused = if boundary {
                self.pause.sync() == PauseState::Paused
            } else {
                self.paused()
            };
            // Park at a boundary while paused, or while a group holds for
            // its join point. Feedback continues so the server's
            // wal_sender_timeout does not disconnect us.
            if boundary && (paused || (self.holding && self.queue.is_empty())) {
                tokio::select! {
                    _ = tokio::time::sleep_until(self.next_feedback) => self.feedback().await?,
                    changed = self.progress.changed() => {
                        changed?;
                        self.feedback().await?;
                    }
                    _ = self.pause.changed() => {}
                }
                continue;
            }
            if !self.queue.is_empty() && !paused {
                tokio::select! {
                    permit = sender.reserve() => {
                        permit.context("PostgreSQL output closed")?
                            .send(self.queue.pop_front().context("queued PostgreSQL output")?);
                        self.last_emission = Instant::now();
                    }
                    _ = tokio::time::sleep_until(self.next_feedback) => self.feedback().await?,
                    changed = self.progress.changed() => {
                        changed?;
                        self.feedback().await?;
                    }
                    _ = self.pause.changed() => {}
                }
                continue;
            }
            let deadline = self.assembly_deadline();
            // At a boundary, a pause request interrupts the read so the next
            // loop parks without waiting for WAL or feedback.
            tokio::select! {
                biased;
                _ = async {
                    match deadline {
                        Some(deadline) => tokio::time::sleep_until(deadline).await,
                        None => std::future::pending().await,
                    }
                } => return Err(SourceTransactionError::Deadline.into()),
                _ = self.pause.pause_requested(), if boundary => {}
                _ = tokio::time::sleep_until(self.next_feedback) => self.feedback().await?,
                changed = self.progress.changed() => {
                    changed?;
                    self.feedback().await?;
                }
                message = self.connection.as_mut().context("PostgreSQL connection is not open")?.read_replication_message() => {
                    match message? {
                        BackendMessage::CopyData(data) => match data.first() {
                            Some(b'w') => {
                                anyhow::ensure!(data.len() > XLOG_DATA_HEADER, "truncated PostgreSQL XLogData");
                                self.handle(&data[XLOG_DATA_HEADER..])?;
                            }
                            Some(b'k') => {
                                anyhow::ensure!(data.len() == KEEPALIVE_LEN && data[KEEPALIVE_REPLY] <= 1, "invalid PostgreSQL keepalive");
                                let mut wal_end = [0; 8];
                                wal_end.copy_from_slice(&data[1..9]);
                                self.keepalive(u64::from_be_bytes(wal_end))?;
                                if data[KEEPALIVE_REPLY] == 1 { self.feedback().await?; }
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

/// The resume point of a group in which no member has a cursor yet: its
/// fresh origin, still the confirmed position.
fn retained_origin(origin: Option<u64>, confirmed: u64) -> Result<u64> {
    let origin = origin.context("PostgreSQL consumer group has no cursor and no origin")?;
    anyhow::ensure!(
        confirmed == origin,
        "PostgreSQL shared streaming origin is no longer retained"
    );
    Ok(origin)
}

/// The decoder skips commits at or below its resume position; a partial
/// row cursor must replay its own commit.
pub(crate) fn decoder_resume(resume: u64, partial: bool) -> Result<u64> {
    if partial {
        resume
            .checked_sub(1)
            .context("invalid partial PostgreSQL cursor")
    } else {
        Ok(resume)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_cursors_replay_their_commit() {
        assert_eq!(decoder_resume(10, false).unwrap(), 10);
        assert_eq!(decoder_resume(10, true).unwrap(), 9);
        assert!(decoder_resume(0, true).is_err());
    }
}

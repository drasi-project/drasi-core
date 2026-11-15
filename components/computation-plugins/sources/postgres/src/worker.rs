// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! The replication worker: reads pgoutput, assembles complete commits,
//! emits them (or their rows), and sends standby feedback that follows the
//! host's committed progress.
//!
//! The worker also implements pause (stop reading and emitting at a
//! transaction boundary while feedback continues), reconnection after a
//! transport failure in every mode, and the final feedback of a Drain stop.

use crate::rows::RowEmission;
use crate::shared::{SharedUpdates, TransactionEmission};
use anyhow::{Context, Result};
use drasi_computation_contracts::{OutputEnvelope, SourceProgressUpdates, SourceTransactionError};
use drasi_postgres_common::{
    decoder::PgOutputDecoder,
    protocol::BackendMessage,
    replication::{
        checkpoint, connect, ensure_binding, ensure_retained, is_transient, rows::row_checkpoint,
        slot_positions, source_binding, start_streaming, validate_retention, Connection,
        PostgresTransactionConfig, Transactions,
    },
    types::StandbyStatusUpdate,
};
use std::{
    collections::{hash_map::RandomState, VecDeque},
    hash::{BuildHasher, Hasher},
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
/// Length of a primary keepalive (`k`) message.
const KEEPALIVE_LEN: usize = 18;
/// Offset of the keepalive's reply-requested flag.
const KEEPALIVE_REPLY: usize = 17;
/// First reconnect backoff ceiling; doubles per attempt.
const RECONNECT_BASE: Duration = Duration::from_millis(250);
/// Largest reconnect backoff ceiling.
const RECONNECT_MAX: Duration = Duration::from_secs(30);

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

/// Pause request and acknowledgement shared with the component.
pub(crate) struct PauseGate {
    /// `true` while the host has paused the source.
    pub requested: watch::Receiver<bool>,
    /// Set `true` once the worker has stopped reading and emitting.
    pub acknowledged: watch::Sender<bool>,
}

pub(crate) struct Worker {
    pub connection: Option<Connection>,
    pub transactions: Transactions,
    pub progress: WorkerProgress,
    pub confirmed: u64,
    pub next_feedback: Instant,
    pub config: PostgresTransactionConfig,
    pub persistent: bool,
    pub rows: Option<RowEmission>,
    pub whole: Option<TransactionEmission>,
    pub pause: PauseGate,
    /// Live `reconnectBudgetMs`; `0` disables reconnection.
    pub reconnect_budget_ms: Arc<AtomicU64>,
}

impl Worker {
    fn paused(&self) -> bool {
        *self.pause.requested.borrow()
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
                    anyhow::ensure!(
                        !self.persistent || snapshot.persistent,
                        "PostgreSQL consumer lost persistent progress"
                    );
                    if let Some(rows) = &self.rows {
                        rows.confirmed(&snapshot, &self.transactions)?
                    } else {
                        checkpoint(
                            &snapshot,
                            &self.transactions.source,
                            self.transactions.binding,
                        )?
                    }
                } else {
                    None
                }
            }
            WorkerProgress::Shared(progress) => {
                let observed = progress.snapshot(
                    &self.transactions.source,
                    &self.transactions.stream,
                    self.transactions.binding,
                    self.persistent,
                )?;
                let rows = self
                    .rows
                    .as_ref()
                    .context("shared source has no row emissions")?;
                rows.validate_shared_observation(observed.max_counter, observed.max_lsn)?;
                if observed.ready {
                    observed.minimum.map(|minimum| minimum.feedback_lsn)
                } else {
                    None
                }
            }
            WorkerProgress::SharedTransactions(progress) => {
                let observed = progress.transaction_snapshot(
                    &self.transactions.source,
                    &self.transactions.stream,
                    self.transactions.binding,
                )?;
                self.whole
                    .as_ref()
                    .context("shared source has no whole-transaction emissions")?
                    .validate_observation(observed.max_counter, observed.max_lsn)?;
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
        self.next_feedback = Instant::now() + self.config.feedback_interval();
        Ok(())
    }

    fn admitted_lsn(&self) -> u64 {
        self.rows.as_ref().map_or_else(
            || {
                self.whole
                    .as_ref()
                    .map_or(self.transactions.resume, TransactionEmission::admitted_lsn)
            },
            RowEmission::admitted_lsn,
        )
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
            let started = Instant::now();
            let mut failure = error;
            let mut attempt = 0u32;
            loop {
                let ceiling = RECONNECT_BASE
                    .saturating_mul(1u32 << attempt.min(16))
                    .min(RECONNECT_MAX);
                let delay = jitter(ceiling);
                let Some(remaining) = budget.checked_sub(started.elapsed()) else {
                    return Err(failure.context("PostgreSQL reconnect budget exhausted"));
                };
                if delay >= remaining {
                    return Err(failure.context("PostgreSQL reconnect budget exhausted"));
                }
                attempt += 1;
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
            let paused = self.paused();
            self.pause.acknowledged.send_replace(paused);
            tokio::select! {
                _ = tokio::time::sleep_until(until) => return,
                changed = self.pause.requested.changed() => {
                    if changed.is_err() {
                        tokio::time::sleep_until(until).await;
                        return;
                    }
                }
            }
        }
    }

    /// Reopens replication at the committed cursor, after a transport
    /// failure. Output after the cursor is replayed; consumers deduplicate by
    /// their committed positions, as after a restart.
    async fn reopen(&mut self) -> Result<()> {
        enum Rewind {
            Single {
                position: Option<drasi_postgres_common::replication::rows::RowPosition>,
                previous: u64,
            },
            Shared {
                position: Option<drasi_postgres_common::replication::rows::RowPosition>,
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
                        && progress.reset_generation == *generation
                        && (!self.persistent || progress.persistent),
                    "PostgreSQL progress owner is not ready for safe reconnection"
                );
                if self.rows.is_some() {
                    let position = row_checkpoint(
                        &progress,
                        &self.transactions.source,
                        self.transactions.binding,
                    )?;
                    let confirmed = position
                        .as_ref()
                        .map_or(self.confirmed, |position| position.feedback_lsn());
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
                    &self.transactions.stream,
                    self.transactions.binding,
                    self.persistent,
                )?;
                anyhow::ensure!(
                    observed.ready,
                    "PostgreSQL shared consumers are not ready for safe reconnection"
                );
                self.rows
                    .as_ref()
                    .context("shared source has no row emissions")?
                    .validate_shared_observation(observed.max_counter, observed.max_lsn)?;
                let (position, confirmed, previous) = match observed.minimum {
                    Some(minimum) => {
                        let previous = minimum.position.feedback_lsn();
                        (Some(minimum.position), minimum.feedback_lsn, previous)
                    }
                    None => {
                        anyhow::ensure!(observed.missing, "PostgreSQL shared minimum is fenced");
                        let origin = progress.initial_lsn()?;
                        anyhow::ensure!(
                            self.confirmed == origin,
                            "PostgreSQL shared streaming origin is no longer retained"
                        );
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
                    &self.transactions.stream,
                    self.transactions.binding,
                )?;
                anyhow::ensure!(
                    observed.ready,
                    "PostgreSQL transaction consumers are not ready for safe reconnection"
                );
                self.whole
                    .as_ref()
                    .context("shared source has no whole-transaction emissions")?
                    .validate_observation(observed.max_counter, observed.max_lsn)?;
                let resume = match observed.minimum {
                    Some(minimum) => minimum.position.commit_lsn,
                    None => {
                        anyhow::ensure!(
                            observed.missing,
                            "PostgreSQL shared transaction minimum is fenced"
                        );
                        let origin = progress.initial_lsn()?;
                        anyhow::ensure!(
                            self.confirmed == origin,
                            "PostgreSQL shared streaming origin is no longer retained"
                        );
                        origin
                    }
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
            &self.transactions.stream,
        )
        .await?;
        ensure_binding(self.transactions.binding, binding)?;
        let (retained, slot_confirmed) = slot_positions(&mut connection, &self.config).await?;
        ensure_retained(retained, slot_confirmed, resume, confirmed)?;
        start_streaming(&mut connection, &self.config, resume).await?;
        self.transactions.decoder = PgOutputDecoder::strict(self.config.max_protocol_bytes);
        self.transactions.pending = None;
        self.transactions.keys = keys;
        let partial = |position: &Option<drasi_postgres_common::replication::rows::RowPosition>| {
            position
                .as_ref()
                .is_some_and(|position| !position.complete())
        };
        match rewind {
            Rewind::Single { position, previous } => {
                self.transactions.resume = decoder_resume(resume, partial(&position))?;
                self.rows
                    .as_mut()
                    .context("row emissions are missing")?
                    .rewind(position, previous);
            }
            Rewind::Shared {
                position,
                previous,
                floor,
            } => {
                self.transactions.resume = decoder_resume(resume, partial(&position))?;
                self.rows
                    .as_mut()
                    .context("row emissions are missing")?
                    .rewind_shared(position, previous, floor.0, floor.1, floor.2)?;
            }
            Rewind::Whole { floor } => {
                self.transactions.resume = resume;
                self.whole
                    .as_ref()
                    .context("whole-transaction emissions are missing")?
                    .raise(floor.0, floor.1);
            }
            Rewind::Transactions => self.transactions.resume = resume,
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

    async fn run_connected(&mut self, sender: mpsc::Sender<OutputEnvelope>) -> Result<()> {
        let mut pending = VecDeque::new();
        loop {
            if let Some((_, builder)) = &mut self.transactions.pending {
                builder.check_deadline()?;
            }
            // Pause at a transaction boundary: finish assembling an open
            // commit, then stop reading and emitting. Feedback continues so
            // the server's wal_sender_timeout does not disconnect us.
            if self.paused() && self.transactions.pending.is_none() {
                self.pause.acknowledged.send_replace(true);
                tokio::select! {
                    _ = tokio::time::sleep_until(self.next_feedback) => self.feedback().await?,
                    changed = self.progress.changed() => {
                        changed?;
                        self.feedback().await?;
                    }
                    changed = self.pause.requested.changed() => {
                        if changed.is_err() {
                            return Ok(());
                        }
                    }
                }
                continue;
            }
            if !self.paused() {
                self.pause.acknowledged.send_replace(false);
            }
            if !pending.is_empty() && !self.paused() {
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
                    _ = self.pause.requested.changed(), if self.pause.requested.has_changed().is_ok() => {}
                }
                continue;
            }
            let deadline = self
                .transactions
                .pending
                .as_ref()
                .map(|(_, builder)| builder.deadline());
            // Between transactions, a pause request interrupts the read so
            // the next loop parks without waiting for WAL or feedback.
            let between = deadline.is_none();
            tokio::select! {
                biased;
                _ = async {
                    match deadline {
                        Some(deadline) => tokio::time::sleep_until(deadline).await,
                        None => std::future::pending().await,
                    }
                } => return Err(SourceTransactionError::Deadline.into()),
                _ = self.pause.requested.changed(), if between && self.pause.requested.has_changed().is_ok() => {}
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
                                let payload = &data[XLOG_DATA_HEADER..];
                                let group = match &self.whole {
                                    Some(whole) => self.transactions.decode_with_position(payload, |lsn, binding, managed| whole.position(lsn, binding, managed))?,
                                    None => self.transactions.decode(payload)?,
                                };
                                if let Some(group) = group {
                                    if let Some(whole) = &self.whole { whole.admit(self.transactions.resume); }
                                    if let Some(rows) = &mut self.rows {
                                        pending = rows.expand(group, &self.transactions)?;
                                    } else {
                                        pending.push_back(group);
                                    }
                                }
                            }
                            Some(b'k') => {
                                anyhow::ensure!(data.len() == KEEPALIVE_LEN && data[KEEPALIVE_REPLY] <= 1, "invalid PostgreSQL keepalive");
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

/// A uniformly random duration in `[0, ceiling]`, so sources reconnecting to
/// one recovering server spread out.
fn jitter(ceiling: Duration) -> Duration {
    let random = RandomState::new().build_hasher().finish();
    let nanos = u64::try_from(ceiling.as_nanos()).unwrap_or(u64::MAX);
    Duration::from_nanos(random % nanos.saturating_add(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jitter_stays_within_its_ceiling() {
        for _ in 0..100 {
            assert!(jitter(Duration::from_millis(10)) <= Duration::from_millis(10));
        }
        assert_eq!(jitter(Duration::ZERO), Duration::ZERO);
    }

    #[test]
    fn partial_cursors_replay_their_commit() {
        assert_eq!(decoder_resume(10, false).unwrap(), 10);
        assert_eq!(decoder_resume(10, true).unwrap(), 9);
        assert!(decoder_resume(0, true).is_err());
    }
}

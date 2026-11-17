// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! The delivery driver: one transaction per operation over a database
//! [`Session`], identical for every stored-procedure sink.
//!
//! An operation's transaction locks the stream's cursor row, decides with
//! [`decide`], writes the cursor, marks the effect pending, runs the rendered
//! command and advances the cursor, then commits. The effect and the cursor
//! commit together, so a retried or replayed operation is recognised.
//!
//! **One retry model.** The driver makes one attempt per call and never
//! sleeps: a transient failure is returned as a transient
//! [`DeliveryFailure`] and the graph retries the same input with its bounded
//! backoff. The only immediate retry is a reconnect when a reused idle
//! session turned out to be dead before its transaction started.
//!
//! **Commit outcome.** A commit that the database definitely refused (a
//! deferred constraint, a serialization failure at commit) is classified like
//! any statement failure. Only a commit whose reply was lost is "outcome
//! unknown": the session is discarded and the failure is transient, and the
//! next attempt resolves it by re-reading the cursor (a committed operation
//! is then a duplicate).
//!
//! **Transaction control.** A command that commits, rolls back or otherwise
//! ends the delivery transaction is detected by the session
//! ([`TransactionEnded`]) and fails permanently; the cursor is never advanced
//! outside the delivery transaction. If the effect escaped (committed with
//! the `pending` mark), every later attempt refuses to run it again.
//!
//! **Permanent failures** follow the sink's declared
//! [`PermanentFailurePolicy`]. Under `skip`, a failure caused by the
//! operation's content (the procedure or its commit rejected it, the template
//! did not render, the command ended the transaction) is settled: a second
//! transaction records the operation and its error in the dead-letter table
//! and advances the cursor past it, atomically, and the call reports
//! [`DeliveryFailure::skipped`]. A replay then finds the operation settled, so
//! the skip happens exactly once. A cursor rejection is never settled.

use crate::cursor::{decide, CursorError, CursorRow, CursorWrite, Decision, Position};
use anyhow::Result;
use async_trait::async_trait;
use drasi_computation_contracts::{DeliveryFailure, PermanentFailurePolicy};
use std::time::Duration;

/// How a database error affects the operation and the session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorClass {
    /// The same operation may succeed later. `reconnect` discards the session
    /// (it is unusable, or its state is unknown).
    Transient { reconnect: bool },
    /// Retrying the same operation gives the same answer.
    Permanent,
}

/// A session's report that the delivery transaction is no longer the one it
/// began: the command committed, rolled back or ended it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error(
    "the command ended the delivery transaction (COMMIT, ROLLBACK or an implicit commit); \
     procedures must not control the transaction, so the operation is refused and the cursor \
     is not advanced"
)]
pub struct TransactionEnded;

/// The outcome of a COMMIT that did not succeed.
#[derive(Debug)]
pub enum CommitError {
    /// The database answered with a definite error and rolled back.
    Rejected(anyhow::Error),
    /// The reply was lost: the commit may or may not have happened.
    Unknown(anyhow::Error),
}

/// The database operations of the protocol, all on one connection, bound to
/// one owner and one progress table. Every method between `begin` and
/// `commit`/`rollback` runs in that transaction.
#[async_trait]
pub trait Session: Send {
    /// A rendered procedure command.
    type Command: Send + Sync;

    /// Classifies an error from this session, by type, error code or I/O
    /// kind, never by message text.
    fn classify(error: &anyhow::Error) -> ErrorClass;

    /// Creates the progress and dead-letter tables when missing and checks
    /// their shape and this login's access. Idempotent.
    async fn prepare(&mut self) -> Result<()>;
    async fn begin(&mut self) -> Result<()>;
    /// Reads and locks the stream's row until the transaction ends.
    async fn read_cursor(&mut self, stream: &str) -> Result<Option<CursorRow>>;
    /// Serialises this owner's first-stream admission until the transaction
    /// ends.
    async fn lock_admission(&mut self) -> Result<()>;
    async fn count_streams(&mut self) -> Result<i64>;
    /// Inserts the stream's row for `position`'s batch with `pending` set to
    /// operation 0.
    async fn insert_cursor(&mut self, position: &Position) -> Result<()>;
    /// Replaces the row's batch with `position`'s, with `pending` set to
    /// operation 0.
    async fn rollover_cursor(&mut self, position: &Position) -> Result<()>;
    /// Marks `operation` (the completed prefix) as pending.
    async fn mark_pending(&mut self, stream: &str, operation: i64) -> Result<()>;
    /// Runs the command and drains its results. Fails with
    /// [`TransactionEnded`] when the command ended the transaction.
    async fn execute(&mut self, command: &Self::Command) -> Result<()>;
    /// Records a skipped operation and its error in the dead-letter table.
    async fn record_skip(&mut self, position: &Position, error: &str) -> Result<()>;
    /// Increments `completed` from `completed` and clears `pending`; returns
    /// the rows changed. Fails with [`TransactionEnded`] when the transaction
    /// is no longer the delivery transaction.
    async fn advance_cursor(&mut self, stream: &str, completed: i64) -> Result<u64>;
    async fn commit(&mut self) -> std::result::Result<(), CommitError>;
    async fn rollback(&mut self) -> Result<()>;
    /// Deletes this owner's cursor and dead-letter rows; succeeds when the
    /// tables do not exist. Returns the cursor rows deleted.
    async fn erase(&mut self) -> Result<u64>;
    /// Whether the session can carry another operation.
    fn healthy(&self) -> bool {
        true
    }
    /// Closes the connection gracefully.
    async fn close(self) -> Result<()>
    where
        Self: Sized;
}

/// Opens and retires sessions.
#[async_trait]
pub trait Connector: Send {
    type Session: Session;
    /// One connection attempt, bounded by the connect timeout.
    async fn connect(&mut self) -> Result<Self::Session>;
    /// Retires a session that may still run a command or hold an open
    /// transaction. The default drops it: the server rolls the transaction
    /// back when the connection closes.
    async fn discard(&mut self, session: Self::Session) -> Result<()> {
        drop(session);
        Ok(())
    }
}

/// The result of one delivered operation.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Outcome {
    Applied,
    /// Already settled earlier (applied or skipped).
    Duplicate,
    /// Settled as skipped now.
    Skipped,
}

/// Why one attempt failed.
#[derive(Debug)]
enum Fault {
    /// The cursor refused the operation.
    Cursor(CursorError),
    /// An earlier attempt's effect escaped its transaction.
    Escaped { operation: i64 },
    /// The command ended the transaction.
    Ended(anyhow::Error),
    /// The template did not render.
    Render(anyhow::Error),
    /// The procedure command failed.
    Effect(anyhow::Error),
    /// A cursor statement failed.
    Statement(anyhow::Error),
    /// COMMIT was definitely refused.
    CommitRejected(anyhow::Error),
    /// COMMIT's outcome is unknown.
    CommitUnknown(anyhow::Error),
}

impl Fault {
    fn statement(error: anyhow::Error) -> Self {
        if error.downcast_ref::<TransactionEnded>().is_some() {
            return Self::Ended(error);
        }
        match error.downcast::<CursorError>() {
            Ok(cursor) => Self::Cursor(cursor),
            Err(error) => Self::Statement(error),
        }
    }

    fn class<S: Session>(&self) -> ErrorClass {
        match self {
            Self::Cursor(_) | Self::Escaped { .. } | Self::Render(_) => ErrorClass::Permanent,
            // The session's transaction state is no longer what it began.
            Self::Ended(_) => ErrorClass::Permanent,
            Self::CommitUnknown(_) => ErrorClass::Transient { reconnect: true },
            Self::Effect(error) | Self::Statement(error) | Self::CommitRejected(error) => {
                S::classify(error)
            }
        }
    }

    /// Whether a permanent fault is caused by the operation's content and so
    /// may be settled as skipped.
    fn skippable(&self) -> bool {
        matches!(
            self,
            Self::Escaped { .. }
                | Self::Ended(_)
                | Self::Render(_)
                | Self::Effect(_)
                | Self::CommitRejected(_)
        )
    }

    fn into_error(self) -> anyhow::Error {
        match self {
            Self::Cursor(error) => error.into(),
            Self::Escaped { operation } => anyhow::Error::new(EscapedEffect { operation }),
            Self::Render(error) => error.context("the command template failed to render"),
            Self::Ended(error) => error,
            Self::Effect(error) => error.context("the procedure command failed"),
            Self::Statement(error) => error.context("a destination cursor statement failed"),
            Self::CommitRejected(error) => {
                error.context("the database rejected the delivery commit")
            }
            Self::CommitUnknown(error) => error.context(
                "the delivery commit's outcome is unknown; the next attempt resolves it from the \
                 destination cursor",
            ),
        }
    }
}

/// An earlier attempt's effect committed outside its delivery transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error(
    "an earlier attempt of operation {operation} ran a command that ended the delivery \
     transaction, so its effect may already be committed; the operation is not run again"
)]
pub struct EscapedEffect {
    pub operation: i64,
}

/// Bounds dead-letter error text.
fn error_text(error: &anyhow::Error) -> String {
    let text = format!("{error:#}");
    match text.char_indices().nth(crate::cursor::MAX_ERROR_CHARS) {
        Some((end, _)) => text[..end].to_owned(),
        None => text,
    }
}

/// Runs one operation in one transaction on `session`.
async fn apply<S: Session + ?Sized>(
    session: &mut S,
    max_streams: u32,
    position: &Position,
    render: &mut (dyn FnMut() -> Result<Option<S::Command>> + Send),
) -> std::result::Result<Outcome, Fault> {
    session.begin().await.map_err(Fault::statement)?;
    let result = async {
        let Some((completed, write, escaped)) = admit(session, max_streams, position).await? else {
            return Ok(Outcome::Duplicate);
        };
        if escaped {
            return Err(Fault::Escaped {
                operation: position.operation,
            });
        }
        if let Some(command) = render().map_err(Fault::Render)? {
            if write == CursorWrite::Keep {
                session
                    .mark_pending(&position.stream, completed)
                    .await
                    .map_err(Fault::statement)?;
            }
            session.execute(&command).await.map_err(|error| {
                if error.downcast_ref::<TransactionEnded>().is_some() {
                    Fault::Ended(error)
                } else {
                    Fault::Effect(error)
                }
            })?;
        }
        advance(session, &position.stream, completed).await?;
        Ok(Outcome::Applied)
    }
    .await;
    finish(session, result).await
}

/// Settles a permanently failed operation as skipped in its own transaction.
async fn settle<S: Session + ?Sized>(
    session: &mut S,
    max_streams: u32,
    position: &Position,
    error: &str,
) -> std::result::Result<Outcome, Fault> {
    session.begin().await.map_err(Fault::statement)?;
    let result = async {
        let Some((completed, _, _)) = admit(session, max_streams, position).await? else {
            return Ok(Outcome::Duplicate);
        };
        session
            .record_skip(position, error)
            .await
            .map_err(Fault::statement)?;
        advance(session, &position.stream, completed).await?;
        Ok(Outcome::Skipped)
    }
    .await;
    // A settle failure is never itself settled.
    finish(session, result).await.map_err(|fault| match fault {
        Fault::CommitRejected(error) | Fault::Ended(error) => Fault::Statement(error),
        fault => fault,
    })
}

/// Locks and decides the cursor and writes the stream's row.
async fn admit<S: Session + ?Sized>(
    session: &mut S,
    max_streams: u32,
    position: &Position,
) -> std::result::Result<Option<(i64, CursorWrite, bool)>, Fault> {
    let mut row = session
        .read_cursor(&position.stream)
        .await
        .map_err(Fault::statement)?;
    if row.is_none() {
        // Serialise only first-stream admission; existing streams use their
        // own row locks.
        session.lock_admission().await.map_err(Fault::statement)?;
        row = session
            .read_cursor(&position.stream)
            .await
            .map_err(Fault::statement)?;
    }
    let (completed, write, escaped) = match decide(row.as_ref(), position).map_err(Fault::Cursor)? {
        Decision::Duplicate => return Ok(None),
        Decision::Apply {
            completed,
            write,
            escaped,
        } => (completed, write, escaped),
    };
    match write {
        CursorWrite::Insert => {
            if session.count_streams().await.map_err(Fault::statement)? >= i64::from(max_streams) {
                return Err(Fault::Cursor(CursorError::StreamCapacity {
                    max: max_streams,
                }));
            }
            session
                .insert_cursor(position)
                .await
                .map_err(Fault::statement)?;
        }
        CursorWrite::Rollover => session
            .rollover_cursor(position)
            .await
            .map_err(Fault::statement)?,
        CursorWrite::Keep => {}
    }
    Ok(Some((completed, write, escaped)))
}

async fn advance<S: Session + ?Sized>(
    session: &mut S,
    stream: &str,
    completed: i64,
) -> std::result::Result<(), Fault> {
    match session.advance_cursor(stream, completed).await {
        Ok(1) => Ok(()),
        Ok(_) => Err(Fault::Cursor(CursorError::CursorChanged)),
        Err(error) => Err(Fault::statement(error)),
    }
}

/// Commits an applied or settled operation, or rolls back.
async fn finish<S: Session + ?Sized>(
    session: &mut S,
    result: std::result::Result<Outcome, Fault>,
) -> std::result::Result<Outcome, Fault> {
    match result {
        Ok(Outcome::Duplicate) => {
            // Release the row lock taken by the read.
            session
                .rollback()
                .await
                .map_err(|error| Fault::Statement(error.context(SessionLost)))?;
            Ok(Outcome::Duplicate)
        }
        Ok(outcome) => match session.commit().await {
            Ok(()) => Ok(outcome),
            Err(CommitError::Rejected(error)) => Err(Fault::CommitRejected(error)),
            Err(CommitError::Unknown(error)) => Err(Fault::CommitUnknown(error)),
        },
        Err(fault) => match session.rollback().await {
            Ok(()) => Err(fault),
            Err(rollback) => {
                log::warn!("stored-procedure delivery rollback failed: {rollback:#}");
                Err(RollbackFailed::mark(fault))
            }
        },
    }
}

/// Marker for a fault whose rollback also failed: the session is discarded.
struct RollbackFailed;

impl RollbackFailed {
    fn mark(fault: Fault) -> Fault {
        match fault {
            Fault::Statement(error) => Fault::Statement(error.context(SessionLost)),
            Fault::Effect(error) => Fault::Effect(error.context(SessionLost)),
            Fault::Render(error) => Fault::Render(error.context(SessionLost)),
            Fault::Ended(error) => Fault::Ended(error.context(SessionLost)),
            Fault::CommitRejected(error) => Fault::CommitRejected(error.context(SessionLost)),
            Fault::CommitUnknown(error) => Fault::CommitUnknown(error),
            fault @ (Fault::Cursor(_) | Fault::Escaped { .. }) => fault,
        }
    }
}

/// Context marking an error after which the session must be discarded.
#[derive(Debug, Clone, Copy, thiserror::Error)]
#[error("the database session was lost and is discarded")]
pub struct SessionLost;

fn session_lost(fault: &Fault) -> bool {
    match fault {
        Fault::Statement(error)
        | Fault::Effect(error)
        | Fault::Render(error)
        | Fault::Ended(error)
        | Fault::CommitRejected(error) => error.downcast_ref::<SessionLost>().is_some(),
        Fault::CommitUnknown(_) => true,
        Fault::Cursor(_) | Fault::Escaped { .. } => false,
    }
}

/// A session held by the driver. `busy` is set while an operation runs on it,
/// so a call the host dropped mid-operation leaves it set and the session is
/// discarded before reuse.
struct Held<S> {
    session: S,
    busy: bool,
    /// Used before (idle since its last operation).
    reused: bool,
}

/// The delivery engine of one sink activation: one session, reconnected on
/// demand, and the sink's declared settings read per operation.
pub struct Delivery<C: Connector> {
    connector: C,
    held: Option<Held<C::Session>>,
    max_streams: u32,
    policy: PermanentFailurePolicy,
}

impl<C: Connector> Delivery<C> {
    pub fn new(connector: C, max_streams: u32, policy: PermanentFailurePolicy) -> Self {
        Self {
            connector,
            held: None,
            max_streams,
            policy,
        }
    }

    pub fn connector(&self) -> &C {
        &self.connector
    }

    pub fn connector_mut(&mut self) -> &mut C {
        &mut self.connector
    }

    pub fn connected(&self) -> bool {
        self.held.is_some()
    }

    /// Settings read per operation (in-place updates).
    pub fn set_max_streams(&mut self, max_streams: u32) {
        self.max_streams = max_streams;
    }

    /// Connects and prepares the tables: one attempt, bounded by the
    /// connector's timeouts. The caller bounds the call with the start
    /// deadline and cancellation.
    pub async fn prepare(&mut self) -> Result<()> {
        let mut session = self.session().await?;
        match session.session.prepare().await {
            Ok(()) => {
                self.held = Some(session);
                Ok(())
            }
            Err(error) => {
                if let Err(discard) = self.connector.discard(session.session).await {
                    log::warn!("discarding the stored-procedure session failed: {discard:#}");
                }
                Err(error)
            }
        }
    }

    async fn session(&mut self) -> Result<Held<C::Session>> {
        match self.held.take() {
            Some(held) if !held.busy && held.session.healthy() => Ok(Held {
                reused: true,
                ..held
            }),
            Some(held) => {
                self.connector.discard(held.session).await?;
                self.fresh().await
            }
            None => self.fresh().await,
        }
    }

    async fn fresh(&mut self) -> Result<Held<C::Session>> {
        Ok(Held {
            session: self.connector.connect().await?,
            busy: false,
            reused: false,
        })
    }

    /// Delivers one operation: one attempt (see the module documentation).
    /// The error carries a [`DeliveryFailure`] classification: transient,
    /// permanent, or skipped when the operation was settled under `skip`.
    pub async fn deliver(
        &mut self,
        position: &Position,
        render: &mut (dyn FnMut() -> Result<Option<<C::Session as Session>::Command>> + Send),
    ) -> Result<Outcome> {
        let mut first = true;
        loop {
            let mut held = self
                .session()
                .await
                .map_err(|error| classified::<C::Session>(error, None))?;
            held.busy = true;
            self.held = Some(held);
            let session = &mut self.held.as_mut().expect("held session").session;
            let result = apply(session, self.max_streams, position, render).await;
            let fault = match result {
                Ok(outcome) => {
                    self.release_busy();
                    return Ok(outcome);
                }
                Err(fault) => fault,
            };
            let class = fault.class::<C::Session>();
            let lost =
                session_lost(&fault) || matches!(class, ErrorClass::Transient { reconnect: true });
            let reused = self.held.as_ref().is_some_and(|held| held.reused);
            if lost {
                self.discard_held().await?;
            } else {
                self.release_busy();
            }
            // A reused idle session that was already dead: nothing ran, so
            // reconnect once without involving the graph's retry.
            if first && reused && lost && matches!(fault, Fault::Statement(_)) {
                first = false;
                log::info!("stored-procedure session was gone; reconnecting: {fault:?}");
                continue;
            }
            if class == ErrorClass::Permanent
                && fault.skippable()
                && self.policy == PermanentFailurePolicy::Skip
            {
                return self.skip(position, fault).await;
            }
            return Err(classified::<C::Session>(fault.into_error(), Some(class)));
        }
    }

    async fn skip(&mut self, position: &Position, fault: Fault) -> Result<Outcome> {
        let error = fault.into_error();
        let text = error_text(&error);
        let mut held = self
            .session()
            .await
            .map_err(|error| classified::<C::Session>(error, None))?;
        held.busy = true;
        self.held = Some(held);
        let session = &mut self.held.as_mut().expect("held session").session;
        match settle(session, self.max_streams, position, &text).await {
            Ok(Outcome::Skipped) => {
                self.release_busy();
                log::warn!(
                    "stored-procedure delivery skipped operation {} after a permanent failure: \
                     {text}",
                    position.delivery_id
                );
                Err(DeliveryFailure::skipped(error).into())
            }
            Ok(outcome) => {
                self.release_busy();
                Ok(outcome)
            }
            Err(settle_fault) => {
                let class = settle_fault.class::<C::Session>();
                if session_lost(&settle_fault)
                    || matches!(class, ErrorClass::Transient { reconnect: true })
                {
                    self.discard_held().await?;
                } else {
                    self.release_busy();
                }
                let settle_error = settle_fault.into_error().context(format!(
                    "the operation failed permanently ({text}) and could not be settled as skipped"
                ));
                Err(classified::<C::Session>(settle_error, Some(class)))
            }
        }
    }

    fn release_busy(&mut self) {
        if let Some(held) = self.held.as_mut() {
            held.busy = false;
        }
    }

    async fn discard_held(&mut self) -> Result<()> {
        if let Some(held) = self.held.take() {
            self.connector.discard(held.session).await.map_err(|error| {
                // The old session may still run: nothing new may start.
                DeliveryFailure::transient(error.context("retiring the failed session")).into()
            })
        } else {
            Ok(())
        }
    }

    /// Deletes this owner's rows on the current or a fresh session.
    pub async fn erase(&mut self) -> Result<u64> {
        let mut held = self.session().await?;
        let erased = held.session.erase().await;
        self.held = Some(held);
        erased
    }

    /// Releases the session: a session left mid-operation by a dropped call
    /// is discarded, an idle one is closed gracefully within `timeout`.
    pub async fn close(&mut self, timeout: Duration) -> Result<()> {
        match self.held.take() {
            Some(held) if held.busy || !held.session.healthy() => {
                self.connector.discard(held.session).await
            }
            Some(held) => match tokio::time::timeout(timeout, held.session.close()).await {
                Ok(result) => result,
                Err(_) => anyhow::bail!("closing the database session timed out after {timeout:?}"),
            },
            None => Ok(()),
        }
    }

    /// Whether a session was left mid-operation by a dropped call.
    pub fn abandoned(&self) -> bool {
        self.held.as_ref().is_some_and(|held| held.busy)
    }
}

/// Wraps `error` in its [`DeliveryFailure`] class.
fn classified<S: Session>(error: anyhow::Error, class: Option<ErrorClass>) -> anyhow::Error {
    if drasi_computation_contracts::delivery_failure_class(&error).is_some() {
        return error;
    }
    match class.unwrap_or_else(|| S::classify(&error)) {
        ErrorClass::Permanent => DeliveryFailure::permanent(error).into(),
        ErrorClass::Transient { .. } => DeliveryFailure::transient(error).into(),
    }
}

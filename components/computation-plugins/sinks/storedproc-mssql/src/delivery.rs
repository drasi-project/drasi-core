// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! The destination cursor protocol.
//!
//! One SQL Server transaction per operation commits the procedure's effect
//! and this sink's cursor row together. The cursor row is keyed by
//! `(owner, stream_key)`: `owner` identifies this sink in its graph and
//! `stream_key` is the host's ordered delivery stream. The row holds the
//! batch in progress (`sequence`, `digest`, `operations`) and the completed
//! operation prefix (`completed`), so a repeated operation is recognised and
//! skipped, a gap or an older batch is refused, and the table holds at most
//! one row per stream instead of one per operation.
//!
//! The protocol runs against the [`Session`] trait, so its decisions and its
//! reconnect/retry behaviour are tested without SQL Server. `session.rs`
//! implements `Session` over Tiberius.

use anyhow::Result;
use async_trait::async_trait;
use drasi_computation_contracts::DeliveryItem;
use drasi_mssql_common::sql::RenderedCommand;
use sha2::{Digest, Sha256};
use std::{
    hash::{BuildHasher, Hasher},
    time::Duration,
};
use tokio::time::Instant;

/// The shared cursor-table format (identical across the stored-procedure
/// sinks).
pub const PROGRESS_FORMAT_VERSION: i16 = 2;
/// Domain separator of the owner digest.
pub const OWNER_DOMAIN: &[u8] = b"drasi.storedproc/owner/v1\0";

/// The cursor-table owner of one sink: lowercase hex SHA-256 of
/// `"drasi.storedproc/owner/v1\0" + graph_id + "\0" + component_id`.
pub fn owner(graph_id: &str, component_id: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(OWNER_DOMAIN);
    digest.update(graph_id.as_bytes());
    digest.update([0]);
    digest.update(component_id.as_bytes());
    format!("{:x}", digest.finalize())
}

/// Protocol failures with a fixed meaning. Every variant except
/// [`DeliveryError::CommitUnknown`] and [`DeliveryError::Rollback`] is
/// permanent: retrying the same operation gives the same answer.
#[derive(Debug)]
pub enum DeliveryError {
    /// The operation belongs to a batch older than the cursor's batch.
    ReceiptExpired { sequence: u64, cursor: u64 },
    /// The cursor's batch has the same sequence but different content.
    PayloadConflict,
    /// A new batch arrived while the previous one is unfinished, or it does
    /// not start at operation 0.
    UnfinishedBatch,
    /// The operation is not the next one in the batch.
    OperationGap { expected: i64, actual: i64 },
    /// A stream's first delivery does not start at operation 0.
    MissingFirstOperation,
    /// The stored cursor row is not a valid format-2 row.
    InvalidProgress(&'static str),
    /// This sink already holds `maxStreams` streams.
    StreamCapacity { max: u32 },
    /// The cursor did not advance by exactly one row.
    CursorChanged,
    /// The procedure committed, rolled back or opened a transaction, so its
    /// effect is no longer atomic with the cursor.
    TransactionStateChanged { depth: i64 },
    /// The command failed to render (D-5: permanent per-change failure).
    Render(anyhow::Error),
    /// COMMIT was sent but its outcome is unknown. Retrying after reconnect
    /// is safe: the cursor recognises a committed operation as a duplicate.
    CommitUnknown(anyhow::Error),
    /// The operation failed and the rollback also failed; the connection is
    /// dropped, which makes SQL Server roll the transaction back.
    Rollback {
        failure: anyhow::Error,
        rollback: anyhow::Error,
    },
}

impl std::fmt::Display for DeliveryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ReceiptExpired { sequence, cursor } => write!(
                f,
                "destination delivery receipt expired: batch {sequence} is older than the \
                 cursor's batch {cursor}"
            ),
            Self::PayloadConflict => {
                f.write_str("destination delivery payload conflicts with its receipt")
            }
            Self::UnfinishedBatch => f.write_str(
                "destination has unfinished operations or the batch is missing its prefix",
            ),
            Self::OperationGap { expected, actual } => write!(
                f,
                "destination delivery has an operation gap: expected operation {expected}, \
                 got {actual}"
            ),
            Self::MissingFirstOperation => {
                f.write_str("destination delivery is missing its first operation")
            }
            Self::InvalidProgress(detail) => {
                write!(f, "invalid destination delivery progress: {detail}")
            }
            Self::StreamCapacity { max } => write!(
                f,
                "destination delivery stream capacity ({max}) is exhausted for this sink; \
                 raise maxStreams or erase stale streams"
            ),
            Self::CursorChanged => {
                f.write_str("destination delivery cursor changed during its effect")
            }
            Self::TransactionStateChanged { depth } => write!(
                f,
                "the procedure changed the delivery transaction (transaction depth {depth} \
                 after the command, expected 1); procedures must not COMMIT, ROLLBACK or \
                 leave transactions open, so the effect may have been committed without \
                 the cursor"
            ),
            Self::Render(error) => write!(f, "failed to render the command: {error:#}"),
            Self::CommitUnknown(error) => {
                write!(
                    f,
                    "SQL Server delivery commit outcome is unknown: {error:#}"
                )
            }
            Self::Rollback { failure, rollback } => write!(
                f,
                "SQL Server delivery failed: {failure:#}; rollback also failed: {rollback:#}"
            ),
        }
    }
}

impl std::error::Error for DeliveryError {}

/// A raw cursor row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CursorRow {
    pub version: i16,
    pub sequence: String,
    pub digest: Vec<u8>,
    pub operations: i64,
    pub completed: i64,
}

struct Cursor {
    sequence: u64,
    digest: Vec<u8>,
    operations: i64,
    completed: i64,
}

impl CursorRow {
    fn parse(&self) -> Result<Cursor, DeliveryError> {
        if self.version != PROGRESS_FORMAT_VERSION {
            return Err(DeliveryError::InvalidProgress("unsupported format version"));
        }
        let sequence = self
            .sequence
            .parse::<u64>()
            .ok()
            .filter(|sequence| *sequence > 0 && sequence.to_string() == self.sequence)
            .ok_or(DeliveryError::InvalidProgress(
                "sequence is not a positive decimal",
            ))?;
        if self.digest.len() != 32
            || self.operations <= 0
            || self.completed < 0
            || self.completed > self.operations
        {
            return Err(DeliveryError::InvalidProgress(
                "digest, operations or completed out of range",
            ));
        }
        Ok(Cursor {
            sequence,
            digest: self.digest.clone(),
            operations: self.operations,
            completed: self.completed,
        })
    }
}

/// The ordered position of one operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Position {
    pub stream: String,
    pub sequence: u64,
    pub operation: i64,
    pub operations: i64,
    pub digest: [u8; 32],
}

impl Position {
    pub fn from_item(item: &DeliveryItem<'_>) -> Result<Self> {
        item.position.validate()?;
        Ok(Self {
            stream: item.position.stream().to_owned(),
            sequence: item.position.sequence(),
            operation: i64::try_from(item.position.operation())?,
            operations: i64::try_from(item.position.operations())?,
            digest: item.content_digest,
        })
    }
}

/// What the cursor says about an operation.
#[derive(Debug, PartialEq, Eq)]
pub enum Decision {
    /// Already committed: skip it.
    Duplicate,
    /// Apply it, writing the cursor as stated, then advance from `completed`.
    Apply { completed: i64, write: CursorWrite },
}

/// The cursor write that precedes the effect.
#[derive(Debug, PartialEq, Eq)]
pub enum CursorWrite {
    /// The batch is the cursor's batch.
    Keep,
    /// The stream's first batch: insert a row.
    Insert,
    /// A new batch after a completed one: replace the batch.
    Rollover,
}

/// The pure cursor decision for one operation.
pub fn decide(
    previous: Option<&CursorRow>,
    position: &Position,
) -> Result<Decision, DeliveryError> {
    let Some(previous) = previous else {
        if position.operation != 0 {
            return Err(DeliveryError::MissingFirstOperation);
        }
        return Ok(Decision::Apply {
            completed: 0,
            write: CursorWrite::Insert,
        });
    };
    let cursor = previous.parse()?;
    if position.sequence < cursor.sequence {
        return Err(DeliveryError::ReceiptExpired {
            sequence: position.sequence,
            cursor: cursor.sequence,
        });
    }
    if position.sequence == cursor.sequence {
        if cursor.digest.as_slice() != position.digest.as_slice()
            || cursor.operations != position.operations
        {
            return Err(DeliveryError::PayloadConflict);
        }
        if position.operation < cursor.completed {
            return Ok(Decision::Duplicate);
        }
        if position.operation != cursor.completed {
            return Err(DeliveryError::OperationGap {
                expected: cursor.completed,
                actual: position.operation,
            });
        }
        return Ok(Decision::Apply {
            completed: cursor.completed,
            write: CursorWrite::Keep,
        });
    }
    if cursor.completed != cursor.operations || position.operation != 0 {
        return Err(DeliveryError::UnfinishedBatch);
    }
    Ok(Decision::Apply {
        completed: 0,
        write: CursorWrite::Rollover,
    })
}

/// The database operations of the protocol, all on one connection. Every
/// method between `begin` and `commit`/`rollback` runs in that transaction.
#[async_trait]
pub trait Session: Send {
    /// Creates the progress table when missing and checks its shape and this
    /// login's access to it. Retry-safe and idempotent.
    async fn prepare(&mut self) -> Result<()>;
    async fn begin(&mut self) -> Result<()>;
    /// Reads and locks (`UPDLOCK, HOLDLOCK`) the stream's row.
    async fn read_cursor(&mut self, owner: &str, stream: &str) -> Result<Option<CursorRow>>;
    /// Serialises first-stream admission for this owner until the
    /// transaction ends.
    async fn lock_admission(&mut self, owner: &str) -> Result<()>;
    async fn count_streams(&mut self, owner: &str) -> Result<i64>;
    async fn insert_cursor(&mut self, owner: &str, position: &Position) -> Result<()>;
    async fn rollover_cursor(&mut self, owner: &str, position: &Position) -> Result<()>;
    /// Runs the procedure command and drains every result set.
    async fn execute(&mut self, command: &RenderedCommand) -> Result<()>;
    /// Fails with [`DeliveryError::TransactionStateChanged`] when the
    /// transaction depth is not 1; otherwise increments `completed` from
    /// `completed` and returns the rows changed.
    async fn advance_cursor(&mut self, owner: &str, stream: &str, completed: i64) -> Result<u64>;
    async fn commit(&mut self) -> Result<()>;
    async fn rollback(&mut self) -> Result<()>;
    /// Deletes this owner's rows; succeeds when the table does not exist.
    async fn erase(&mut self, owner: &str) -> Result<u64>;
    /// Closes the connection gracefully.
    async fn close(self) -> Result<()>
    where
        Self: Sized;
}

/// Opens sessions.
#[async_trait]
pub trait Connector: Send + Sync {
    type Session: Session;
    async fn connect(&self) -> Result<Self::Session>;
}

/// The result of one delivered operation.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Outcome {
    Applied,
    Duplicate,
}

/// Runs one operation in one transaction. `render` is called only after the
/// cursor admits the operation, so a duplicate never re-renders.
pub async fn apply_operation<S: Session + ?Sized>(
    session: &mut S,
    owner: &str,
    max_streams: u32,
    position: &Position,
    render: &mut (dyn FnMut() -> Result<Option<RenderedCommand>> + Send),
) -> Result<Outcome> {
    session.begin().await?;
    let result = async {
        let mut row = session.read_cursor(owner, &position.stream).await?;
        if row.is_none() {
            // Serialise only first-stream admission; existing streams use
            // their own row locks.
            session.lock_admission(owner).await?;
            row = session.read_cursor(owner, &position.stream).await?;
        }
        let (completed, write) = match decide(row.as_ref(), position)? {
            Decision::Duplicate => return Ok(Outcome::Duplicate),
            Decision::Apply { completed, write } => (completed, write),
        };
        match write {
            CursorWrite::Insert => {
                if session.count_streams(owner).await? >= i64::from(max_streams) {
                    return Err(DeliveryError::StreamCapacity { max: max_streams }.into());
                }
                session.insert_cursor(owner, position).await?;
            }
            CursorWrite::Rollover => session.rollover_cursor(owner, position).await?,
            CursorWrite::Keep => {}
        }
        if let Some(command) = render().map_err(DeliveryError::Render)? {
            session.execute(&command).await?;
        }
        if session
            .advance_cursor(owner, &position.stream, completed)
            .await?
            != 1
        {
            return Err(DeliveryError::CursorChanged.into());
        }
        Ok(Outcome::Applied)
    }
    .await;
    match result {
        Ok(Outcome::Applied) => match session.commit().await {
            Ok(()) => Ok(Outcome::Applied),
            Err(error) => Err(DeliveryError::CommitUnknown(error).into()),
        },
        // Release the row lock taken by the read.
        Ok(Outcome::Duplicate) => match session.rollback().await {
            Ok(()) => Ok(Outcome::Duplicate),
            Err(rollback) => Err(DeliveryError::Rollback {
                failure: anyhow::anyhow!("releasing a duplicate operation's cursor lock"),
                rollback,
            }
            .into()),
        },
        Err(failure) => match session.rollback().await {
            Ok(()) => Err(failure),
            Err(rollback) => Err(DeliveryError::Rollback { failure, rollback }.into()),
        },
    }
}

/// How a failure is handled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureClass {
    /// The same operation may succeed later. `reconnect` drops the
    /// connection first (it is broken or its state is unknown).
    Retryable { reconnect: bool },
    /// Retrying gives the same answer.
    Permanent,
}

impl FailureClass {
    pub fn is_retryable(self) -> bool {
        matches!(self, Self::Retryable { .. })
    }
}

/// SQL Server errors after which the transaction was rolled back but the
/// connection stays usable.
const TRANSIENT_SERVER_ERRORS: [u32; 8] = [
    1205,  // deadlock victim
    1222,  // lock request timeout
    3960,  // snapshot isolation update conflict
    3961,  // snapshot isolation: object changed by DDL
    41301, // in-memory OLTP dependency failure
    41302, // in-memory OLTP update conflict
    41305, // in-memory OLTP repeatable-read validation
    41325, // in-memory OLTP serializable validation
];

/// SQL Server and Azure SQL errors after which the connection is closed or
/// should be replaced (failover, throttling, service busy, network).
const TRANSIENT_CONNECTION_ERRORS: [u32; 12] = [
    233, 64, 4221, 10053, 10054, 10060, 10928, 10929, 40197, 40501, 40613, 49918,
];

/// Classifies by type, SQL Server error number or I/O kind, never by text.
pub fn classify(error: &anyhow::Error) -> FailureClass {
    for cause in error.chain() {
        if let Some(error) = cause.downcast_ref::<DeliveryError>() {
            return match error {
                DeliveryError::CommitUnknown(_) | DeliveryError::Rollback { .. } => {
                    FailureClass::Retryable { reconnect: true }
                }
                _ => FailureClass::Permanent,
            };
        }
        if let Some(error) = cause.downcast_ref::<tiberius::error::Error>() {
            return classify_tiberius(error);
        }
        if cause.downcast_ref::<std::io::Error>().is_some()
            || cause
                .downcast_ref::<tokio::time::error::Elapsed>()
                .is_some()
        {
            return FailureClass::Retryable { reconnect: true };
        }
        if cause.downcast_ref::<ConnectTimeout>().is_some() {
            return FailureClass::Retryable { reconnect: true };
        }
    }
    FailureClass::Permanent
}

fn classify_tiberius(error: &tiberius::error::Error) -> FailureClass {
    use tiberius::error::Error;
    match error {
        Error::Server(token) => classify_server_code(token.code()),
        // A broken or desynchronised connection: the server rolls back the
        // open transaction when it closes, so a retry after reconnect is safe.
        Error::Io { .. } | Error::Protocol(_) | Error::Routing { .. } => {
            FailureClass::Retryable { reconnect: true }
        }
        _ => FailureClass::Permanent,
    }
}

/// Classifies a SQL Server error number. Everything not listed (procedure
/// `THROW`/`RAISERROR`, constraint violations, permissions, login failures,
/// syntax) is permanent.
pub fn classify_server_code(code: u32) -> FailureClass {
    if TRANSIENT_SERVER_ERRORS.contains(&code) || code == crate::session::ADMISSION_LOCK_ERROR {
        FailureClass::Retryable { reconnect: false }
    } else if TRANSIENT_CONNECTION_ERRORS.contains(&code) || (49919..=49920).contains(&code) {
        FailureClass::Retryable { reconnect: true }
    } else {
        FailureClass::Permanent
    }
}

/// A connection attempt exceeded `connectTimeoutMs`.
#[derive(Debug)]
pub struct ConnectTimeout(pub Duration);

impl std::fmt::Display for ConnectTimeout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SQL Server connection timed out after {:?}", self.0)
    }
}

impl std::error::Error for ConnectTimeout {}

/// First backoff ceiling; doubles per attempt up to [`MAX_BACKOFF`].
pub const BASE_BACKOFF: Duration = Duration::from_millis(100);
/// Largest backoff ceiling between attempts.
pub const MAX_BACKOFF: Duration = Duration::from_secs(5);

/// Bounded, full-jitter exponential backoff within a total budget.
pub struct Backoff {
    started: Instant,
    budget: Duration,
    attempt: u32,
}

impl Backoff {
    pub fn new(budget: Duration) -> Self {
        Self {
            started: Instant::now(),
            budget,
            attempt: 0,
        }
    }

    /// The next delay, or `None` when the budget would be exceeded.
    pub fn next_delay(&mut self) -> Option<Duration> {
        let ceiling = BASE_BACKOFF
            .saturating_mul(1u32.checked_shl(self.attempt.min(16)).unwrap_or(u32::MAX))
            .min(MAX_BACKOFF);
        let delay = jitter(ceiling);
        let remaining = self.budget.checked_sub(self.started.elapsed())?;
        if delay >= remaining {
            return None;
        }
        self.attempt += 1;
        Some(delay)
    }
}

fn jitter(ceiling: Duration) -> Duration {
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u128(ceiling.as_nanos());
    let nanos = u64::try_from(ceiling.as_nanos()).unwrap_or(u64::MAX);
    Duration::from_nanos(hasher.finish() % nanos.saturating_add(1))
}

/// The delivery engine: one persistent session, reconnected on demand.
///
/// The session is moved into the running future while an operation is in
/// flight. If the host drops `deliver` (Abort, a deadline), the session is
/// dropped with it, which closes the socket and makes SQL Server roll the
/// open transaction back; the next call reconnects. Nothing is latched.
pub struct Delivery<C: Connector> {
    connector: C,
    session: Option<C::Session>,
    owner: String,
    max_streams: u32,
    retry_budget: Duration,
}

impl<C: Connector> Delivery<C> {
    pub fn new(connector: C, owner: String, max_streams: u32, retry_budget: Duration) -> Self {
        Self {
            connector,
            session: None,
            owner,
            max_streams,
            retry_budget,
        }
    }

    pub fn owner(&self) -> &str {
        &self.owner
    }

    pub fn connected(&self) -> bool {
        self.session.is_some()
    }

    pub fn connector(&self) -> &C {
        &self.connector
    }

    /// Settings read per operation (in-place updates).
    pub fn set_options(&mut self, max_streams: u32, retry_budget: Duration) {
        self.max_streams = max_streams;
        self.retry_budget = retry_budget;
    }

    /// Drops the session without waiting.
    pub fn disconnect(&mut self) {
        self.session = None;
    }

    /// Replaces the connector (new timeouts) and drops the session, so the
    /// next operation reconnects with the new settings.
    pub fn replace_connector(&mut self, connector: C) {
        self.connector = connector;
        self.session = None;
    }

    /// Closes the session gracefully, within `timeout`.
    pub async fn close(&mut self, timeout: Duration) -> Result<()> {
        match self.session.take() {
            Some(session) => tokio::time::timeout(timeout, session.close())
                .await
                .map_err(|_| anyhow::anyhow!("closing the SQL Server connection timed out"))?,
            None => Ok(()),
        }
    }

    /// Connects and prepares the progress table, retrying retryable failures
    /// within the budget. The caller bounds the call with the start deadline
    /// and cancellation.
    pub async fn prepare(&mut self) -> Result<()> {
        let mut backoff = Backoff::new(self.retry_budget);
        loop {
            let attempt = async {
                let mut session = match self.session.take() {
                    Some(session) => session,
                    None => self.connector.connect().await?,
                };
                session.prepare().await?;
                Ok::<_, anyhow::Error>(session)
            }
            .await;
            match attempt {
                Ok(session) => {
                    self.session = Some(session);
                    return Ok(());
                }
                Err(error) => self.wait_or_fail(error, &mut backoff).await?,
            }
        }
    }

    /// Delivers one operation, reconnecting and retrying retryable failures
    /// within the budget. A permanent failure returns immediately.
    pub async fn deliver(
        &mut self,
        position: &Position,
        render: &mut (dyn FnMut() -> Result<Option<RenderedCommand>> + Send),
    ) -> Result<Outcome> {
        let mut backoff = Backoff::new(self.retry_budget);
        loop {
            let mut session = match self.session.take() {
                Some(session) => session,
                None => match self.connector.connect().await {
                    Ok(session) => session,
                    Err(error) => {
                        self.wait_or_fail(error, &mut backoff).await?;
                        continue;
                    }
                },
            };
            let result = apply_operation(
                &mut session,
                &self.owner,
                self.max_streams,
                position,
                render,
            )
            .await;
            let class = result.as_ref().err().map(classify);
            if !matches!(class, Some(FailureClass::Retryable { reconnect: true })) {
                self.session = Some(session);
            }
            match result {
                Ok(outcome) => return Ok(outcome),
                Err(error) => self.wait_or_fail(error, &mut backoff).await?,
            }
        }
    }

    /// Deletes this owner's rows on a fresh or current session.
    pub async fn erase(&mut self) -> Result<u64> {
        let mut session = match self.session.take() {
            Some(session) => session,
            None => self.connector.connect().await?,
        };
        let erased = session.erase(&self.owner).await?;
        self.session = Some(session);
        Ok(erased)
    }

    async fn wait_or_fail(&mut self, error: anyhow::Error, backoff: &mut Backoff) -> Result<()> {
        match classify(&error) {
            FailureClass::Permanent => Err(error),
            FailureClass::Retryable { reconnect } => {
                if reconnect {
                    self.session = None;
                }
                match backoff.next_delay() {
                    Some(delay) => {
                        log::warn!("SQL Server sink retrying after a retryable failure: {error:#}");
                        tokio::time::sleep(delay).await;
                        Ok(())
                    }
                    None => Err(error),
                }
            }
        }
    }
}

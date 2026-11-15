// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Format-2 destination delivery cursor, owned per consumer.
//!
//! One PostgreSQL transaction commits an operation's effect and the advance of
//! its destination cursor together, so a retried operation whose earlier
//! attempt committed is recognised and not repeated. Format 2 differs from the
//! legacy format 1 (`public.drasi_delivery_stream_v1`, see
//! [`super::PostgresDeliveryHandler`]) in three ways:
//!
//! * the table is configurable ([`ProgressTable`]);
//! * every row carries an `owner` ([`DeliveryOwner`]), the hash of the graph and
//!   consumer identifiers, so several consumers share a table without sharing
//!   a stream limit, and erasing one consumer deletes only its rows;
//! * the table is prepared by an explicit, privilege-checked step
//!   ([`ProgressStore::ensure_table`]), not by every handler start.
//!
//! The cursor protocol is otherwise unchanged: one row per stable
//! producer/consumer stream holds the latest batch's sequence, content digest,
//! operation count and completed prefix. A retry of a completed operation is
//! acknowledged without an effect; an older batch, a conflicting payload, an
//! operation gap or an unfinished prefix is rejected.

use async_trait::async_trait;
use drasi_computation_contracts::DeliveryItem;
use sha2::{Digest, Sha256};
use std::{fmt, future::Future, time::Duration};
use tokio_postgres::{Client, IsolationLevel, Row, Statement, Transaction};

/// The cursor row format written by [`ProgressStore`].
pub const PROGRESS_FORMAT_VERSION: i16 = 2;
/// The default progress table.
pub const DEFAULT_PROGRESS_TABLE: &str = "public.drasi_delivery_progress";
/// The default number of streams one owner may keep.
pub const DEFAULT_MAX_STREAMS: u32 = 4096;
/// The largest configurable number of streams per owner.
pub const MAX_STREAMS_LIMIT: u32 = 1_000_000;
/// Domain separator of the owner hash.
pub const OWNER_DOMAIN: &str = "drasi.storedproc/owner/v1";
/// Longest PostgreSQL identifier (`NAMEDATALEN - 1`).
pub const MAX_IDENTIFIER_BYTES: usize = 63;

/// Advisory lock class shared with the format-1 handler.
const LOCK_CLASS: i32 = 1_146_241_363;
/// Advisory lock object serialising progress-table creation.
const TABLE_LOCK: i32 = 2;

/// A validated, schema-qualified progress table name.
///
/// Both parts are lowercase unquoted PostgreSQL identifiers
/// (`[a-z_][a-z0-9_]*`, at most 63 bytes), so the written name and the
/// catalog name are the same and quoting cannot change its meaning.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProgressTable {
    schema: String,
    table: String,
}

impl ProgressTable {
    /// Parses `schema.table`.
    pub fn parse(name: &str) -> anyhow::Result<Self> {
        let (schema, table) = name.split_once('.').ok_or_else(|| {
            anyhow::anyhow!(
                "progressTable must be schema-qualified, for example {DEFAULT_PROGRESS_TABLE}"
            )
        })?;
        for (part, value) in [("schema", schema), ("table", table)] {
            validate_identifier(part, value)?;
        }
        Ok(Self {
            schema: schema.to_owned(),
            table: table.to_owned(),
        })
    }

    pub fn schema(&self) -> &str {
        &self.schema
    }

    pub fn table(&self) -> &str {
        &self.table
    }

    /// The quoted, schema-qualified name for SQL text.
    pub fn qualified(&self) -> String {
        format!("\"{}\".\"{}\"", self.schema, self.table)
    }
}

impl fmt::Display for ProgressTable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.schema, self.table)
    }
}

fn validate_identifier(part: &str, value: &str) -> anyhow::Result<()> {
    let mut characters = value.chars();
    anyhow::ensure!(
        !value.is_empty()
            && value.len() <= MAX_IDENTIFIER_BYTES
            && characters
                .next()
                .is_some_and(|first| first == '_' || first.is_ascii_lowercase())
            && characters.all(|next| {
                next == '_' || next.is_ascii_lowercase() || next.is_ascii_digit()
            }),
        "progressTable {part} '{value}' must be a lowercase identifier ([a-z_][a-z0-9_]*, at most {MAX_IDENTIFIER_BYTES} bytes)"
    );
    Ok(())
}

/// The `CREATE TABLE IF NOT EXISTS` statement for a format-2 progress table.
/// Operators who pre-create the table for a least-privilege role run exactly
/// this statement.
pub fn create_table_sql(table: &ProgressTable) -> String {
    format!(
        "CREATE TABLE IF NOT EXISTS {} (
    owner TEXT NOT NULL CHECK (owner ~ '^[0-9a-f]{{64}}$'),
    stream_key TEXT NOT NULL,
    version SMALLINT NOT NULL CHECK (version = 2),
    sequence TEXT NOT NULL CHECK (sequence ~ '^[1-9][0-9]{{0,19}}$'),
    digest BYTEA NOT NULL CHECK (octet_length(digest) = 32),
    operations BIGINT NOT NULL CHECK (operations > 0),
    completed BIGINT NOT NULL CHECK (completed >= 0 AND completed <= operations),
    PRIMARY KEY (owner, stream_key)
)",
        table.qualified()
    )
}

/// The consumer that owns cursor rows: lowercase hex SHA-256 of
/// `"drasi.storedproc/owner/v1\0" + graph_id + "\0" + component_id`.
///
/// It is stable across restarts, processes and configuration updates of the
/// same component, and distinct across graphs and components.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeliveryOwner(String);

impl DeliveryOwner {
    pub fn derive(graph_id: &str, component_id: &str) -> Self {
        let mut hash = Sha256::new();
        hash.update(OWNER_DOMAIN.as_bytes());
        hash.update(b"\0");
        hash.update(graph_id.as_bytes());
        hash.update(b"\0");
        hash.update(component_id.as_bytes());
        Self(
            hash.finalize()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect(),
        )
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Advisory lock object serialising this owner's stream admission.
    fn lock_key(&self) -> i32 {
        let bytes = self.0.as_bytes();
        let prefix = std::str::from_utf8(&bytes[..8]).unwrap_or("00000000");
        u32::from_str_radix(prefix, 16).unwrap_or_default() as i32
    }
}

impl fmt::Display for DeliveryOwner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// What a caller should do after a failed delivery attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Disposition {
    /// The transaction was rolled back and the connection is usable; the same
    /// operation may be retried.
    Retry,
    /// The connection is unusable or the commit outcome is unknown. Discard
    /// the connection, reconnect and retry: the destination cursor makes the
    /// retry idempotent.
    Reconnect,
    /// Deterministic failure; retrying the same input cannot succeed until the
    /// configuration, the destination or the data changes.
    Permanent,
}

/// Classifies a PostgreSQL error by SQLSTATE, never by message text.
pub fn disposition_of(error: &tokio_postgres::Error) -> Disposition {
    if error.is_closed() {
        return Disposition::Reconnect;
    }
    match error.code() {
        Some(code) => disposition_of_code(code.code()),
        // No SQLSTATE: an I/O, protocol or closed-connection failure has an
        // `io::Error` source; anything else (parameter conversion) is
        // deterministic.
        None if has_io_source(error) => Disposition::Reconnect,
        None => Disposition::Permanent,
    }
}

fn has_io_source(error: &tokio_postgres::Error) -> bool {
    let mut source = std::error::Error::source(error);
    while let Some(cause) = source {
        if cause.is::<std::io::Error>() {
            return true;
        }
        source = cause.source();
    }
    false
}

/// The SQLSTATE classification behind [`disposition_of`].
pub fn disposition_of_code(code: &str) -> Disposition {
    match code {
        // serialization_failure, deadlock_detected, lock_not_available,
        // query_canceled (statement_timeout): rolled back, safe to retry.
        "40001" | "40P01" | "55P03" | "57014" => Disposition::Retry,
        // admin_shutdown (pg_terminate_backend), crash_shutdown,
        // cannot_connect_now, too_many_connections.
        "57P01" | "57P02" | "57P03" | "53300" => Disposition::Reconnect,
        // connection_exception class.
        code if code.starts_with("08") => Disposition::Reconnect,
        // insufficient_resources class (disk full, out of memory).
        code if code.starts_with("53") => Disposition::Retry,
        _ => Disposition::Permanent,
    }
}

/// A failed delivery attempt, classified by [`DeliveryError::disposition`].
#[derive(Debug, thiserror::Error)]
pub enum DeliveryError {
    #[error("PostgreSQL delivery failed: {0}")]
    Database(#[source] tokio_postgres::Error),
    #[error(
        "PostgreSQL did not answer a delivery command within {0:?}; the connection is discarded"
    )]
    Timeout(Duration),
    #[error(
        "PostgreSQL delivery commit outcome is unknown ({0}); retrying after reconnecting is safe \
         because the destination cursor recognises a committed operation"
    )]
    CommitUnknown(#[source] tokio_postgres::Error),
    #[error(
        "PostgreSQL did not answer the delivery commit within {0:?}; the outcome is unknown and \
         retrying after reconnecting is safe"
    )]
    CommitTimeout(Duration),
    #[error("{failure}; rolling back also failed: {rollback}")]
    Rollback {
        #[source]
        failure: Box<DeliveryError>,
        rollback: String,
    },
    #[error(
        "delivery stream capacity is exhausted: owner {owner} already has {limit} streams in \
         {table}; erase unused consumers or raise maxStreams"
    )]
    Capacity {
        owner: String,
        table: String,
        limit: u32,
    },
    #[error("destination delivery cursor rejected the operation: {0}")]
    Rejected(String),
    #[error("delivery effect failed: {0:#}")]
    Effect(anyhow::Error),
}

impl DeliveryError {
    pub fn disposition(&self) -> Disposition {
        match self {
            Self::Database(error) => disposition_of(error),
            Self::Timeout(_) | Self::CommitUnknown(_) | Self::CommitTimeout(_) => {
                Disposition::Reconnect
            }
            Self::Rollback { failure, .. } => match failure.disposition() {
                Disposition::Permanent => Disposition::Permanent,
                _ => Disposition::Reconnect,
            },
            Self::Capacity { .. } | Self::Rejected(_) | Self::Effect(_) => Disposition::Permanent,
        }
    }

    /// Whether the attempt may be repeated (possibly after reconnecting).
    pub fn retryable(&self) -> bool {
        self.disposition() != Disposition::Permanent
    }

    /// Whether the connection must be discarded before another attempt.
    pub fn discards_connection(&self) -> bool {
        match self {
            Self::Rollback { .. } => true,
            other => other.disposition() == Disposition::Reconnect,
        }
    }
}

/// A failure to prepare the progress table at start.
#[derive(Debug, thiserror::Error)]
pub enum ProgressTableError {
    #[error("progress table schema \"{schema}\" does not exist; create it or choose another progressTable")]
    MissingSchema { schema: String },
    #[error("role \"{role}\" lacks {privilege} on {object}; {hint}")]
    MissingPrivilege {
        role: String,
        privilege: String,
        object: String,
        hint: &'static str,
    },
    #[error("progress table {table} exists but does not have the format-2 layout: {source}")]
    Incompatible {
        table: String,
        #[source]
        source: tokio_postgres::Error,
    },
    #[error("preparing progress table {table} failed: {source}")]
    Database {
        table: String,
        #[source]
        source: tokio_postgres::Error,
    },
    #[error("preparing progress table {table} did not finish within {timeout:?}")]
    Timeout { table: String, timeout: Duration },
}

impl ProgressTableError {
    /// Whether retrying the start can succeed without operator action.
    pub fn retryable(&self) -> bool {
        match self {
            Self::Database { source, .. } => disposition_of(source) != Disposition::Permanent,
            Self::Timeout { .. } => true,
            _ => false,
        }
    }
}

/// The destination-side effect of one operation. It may use only the borrowed
/// transaction: no transaction control, no other connection, no external
/// effects. The store owns commit and rollback.
#[async_trait]
pub trait ProgressEffect: Send + Sync {
    async fn apply(
        &self,
        transaction: &Transaction<'_>,
        item: &DeliveryItem<'_>,
    ) -> anyhow::Result<()>;
}

/// Cursor statements prepared on one connection.
pub struct PreparedProgress {
    read: Statement,
    lock: Statement,
    count: Statement,
    insert: Statement,
    rollover: Statement,
    advance: Statement,
}

/// One consumer's view of a format-2 progress table.
#[derive(Clone, Debug)]
pub struct ProgressStore {
    table: ProgressTable,
    owner: DeliveryOwner,
    max_streams: u32,
}

impl ProgressStore {
    pub fn new(
        table: ProgressTable,
        owner: DeliveryOwner,
        max_streams: u32,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            (1..=MAX_STREAMS_LIMIT).contains(&max_streams),
            "maxStreams must be in 1..={MAX_STREAMS_LIMIT}"
        );
        Ok(Self {
            table,
            owner,
            max_streams,
        })
    }

    pub fn table(&self) -> &ProgressTable {
        &self.table
    }

    pub fn owner(&self) -> &DeliveryOwner {
        &self.owner
    }

    pub fn max_streams(&self) -> u32 {
        self.max_streams
    }

    /// Verifies that the progress table exists with the format-2 layout and
    /// that the current role can use it, creating it under an advisory lock
    /// when it is absent and the role may create it. Safe to repeat and to run
    /// concurrently. `limit` bounds each statement.
    pub async fn ensure_table(
        &self,
        client: &mut Client,
        limit: Option<Duration>,
    ) -> Result<(), ProgressTableError> {
        let table = self.table.to_string();
        let error = |source: DeliveryError| match source {
            DeliveryError::Database(source) => ProgressTableError::Database {
                table: table.clone(),
                source,
            },
            _ => ProgressTableError::Timeout {
                table: table.clone(),
                timeout: limit.unwrap_or_default(),
            },
        };
        let transaction = timed(
            limit,
            client
                .build_transaction()
                .isolation_level(IsolationLevel::ReadCommitted)
                .start(),
        )
        .await
        .map_err(error)?;
        timed(
            limit,
            transaction.execute(
                "SELECT pg_advisory_xact_lock($1, $2)",
                &[&LOCK_CLASS, &TABLE_LOCK],
            ),
        )
        .await
        .map_err(error)?;
        let row = timed(
            limit,
            transaction.query_one(
                "SELECT current_user::text AS role,
                        n.oid IS NOT NULL AS schema_exists,
                        COALESCE(has_schema_privilege(n.oid, 'USAGE'), false) AS can_use,
                        COALESCE(has_schema_privilege(n.oid, 'CREATE'), false) AS can_create,
                        c.oid IS NOT NULL AS table_exists,
                        COALESCE(has_table_privilege(c.oid, 'SELECT'), false) AS can_select,
                        COALESCE(has_table_privilege(c.oid, 'INSERT'), false) AS can_insert,
                        COALESCE(has_table_privilege(c.oid, 'UPDATE'), false) AS can_update,
                        COALESCE(has_table_privilege(c.oid, 'DELETE'), false) AS can_delete
                 FROM (SELECT 1) AS one
                 LEFT JOIN pg_namespace n ON n.nspname = $1
                 LEFT JOIN pg_class c ON c.relnamespace = n.oid AND c.relname = $2
                     AND c.relkind IN ('r', 'p')",
                &[&self.table.schema, &self.table.table],
            ),
        )
        .await
        .map_err(error)?;
        let flag = |name: &str| row.try_get::<_, bool>(name);
        let read = || -> Result<_, tokio_postgres::Error> {
            Ok((
                row.try_get::<_, String>("role")?,
                flag("schema_exists")?,
                flag("can_use")?,
                flag("can_create")?,
                flag("table_exists")?,
                [
                    ("SELECT", flag("can_select")?),
                    ("INSERT", flag("can_insert")?),
                    ("UPDATE", flag("can_update")?),
                    ("DELETE", flag("can_delete")?),
                ],
            ))
        };
        let (role, schema_exists, can_use, can_create, table_exists, privileges) =
            read().map_err(|source| error(DeliveryError::Database(source)))?;
        let schema = format!("schema \"{}\"", self.table.schema);
        if !schema_exists {
            return Err(ProgressTableError::MissingSchema {
                schema: self.table.schema.clone(),
            });
        }
        if !can_use {
            return Err(ProgressTableError::MissingPrivilege {
                role,
                privilege: "USAGE".into(),
                object: schema,
                hint: "grant USAGE on the progress table's schema",
            });
        }
        if table_exists {
            let missing = privileges
                .iter()
                .filter(|(_, granted)| !granted)
                .map(|(privilege, _)| *privilege)
                .collect::<Vec<_>>();
            if !missing.is_empty() {
                return Err(ProgressTableError::MissingPrivilege {
                    role,
                    privilege: missing.join(", "),
                    object: format!("table {}", self.table),
                    hint: "grant SELECT, INSERT, UPDATE and DELETE on the progress table",
                });
            }
            transaction
                .execute(
                    &format!(
                        "SELECT owner, stream_key, version, sequence, digest, operations, completed \
                         FROM {} WHERE false",
                        self.table.qualified()
                    ),
                    &[],
                )
                .await
                .map_err(|source| ProgressTableError::Incompatible {
                    table: table.clone(),
                    source,
                })?;
        } else {
            if !can_create {
                return Err(ProgressTableError::MissingPrivilege {
                    role,
                    privilege: "CREATE".into(),
                    object: schema,
                    hint: "the progress table does not exist: pre-create it with the documented \
                           statement and grant SELECT, INSERT, UPDATE and DELETE, or grant CREATE \
                           on the schema (PostgreSQL 15 and later no longer grant CREATE on \
                           public to every role)",
                });
            }
            timed(
                limit,
                transaction.batch_execute(&create_table_sql(&self.table)),
            )
            .await
            .map_err(error)?;
        }
        timed(limit, transaction.commit()).await.map_err(error)?;
        Ok(())
    }

    /// Prepares the cursor statements on `client`. The table must exist.
    pub async fn prepare(
        &self,
        client: &Client,
        limit: Option<Duration>,
    ) -> Result<PreparedProgress, DeliveryError> {
        let table = self.table.qualified();
        Ok(PreparedProgress {
            read: timed(
                limit,
                client.prepare(&format!(
                    "SELECT version, sequence, digest, operations, completed FROM {table}
                     WHERE owner = $1 AND stream_key = $2 FOR UPDATE"
                )),
            )
            .await?,
            lock: timed(
                limit,
                client.prepare("SELECT pg_advisory_xact_lock($1, $2)"),
            )
            .await?,
            count: timed(
                limit,
                client.prepare(&format!("SELECT count(*) FROM {table} WHERE owner = $1")),
            )
            .await?,
            insert: timed(
                limit,
                client.prepare(&format!(
                    "INSERT INTO {table}
                     (owner, stream_key, version, sequence, digest, operations, completed)
                     VALUES ($1, $2, {PROGRESS_FORMAT_VERSION}, $3, $4, $5, 0)"
                )),
            )
            .await?,
            rollover: timed(
                limit,
                client.prepare(&format!(
                    "UPDATE {table} SET sequence = $3, digest = $4, operations = $5, completed = 0
                     WHERE owner = $1 AND stream_key = $2"
                )),
            )
            .await?,
            advance: timed(
                limit,
                client.prepare(&format!(
                    "UPDATE {table} SET completed = completed + 1
                     WHERE owner = $1 AND stream_key = $2 AND completed = $3"
                )),
            )
            .await?,
        })
    }

    /// Deletes every cursor row of this owner and nothing else. Idempotent: a
    /// missing table or an owner without rows deletes nothing. Returns the
    /// number of deleted rows.
    pub async fn erase(
        &self,
        client: &Client,
        limit: Option<Duration>,
    ) -> Result<u64, DeliveryError> {
        let qualified = self.table.qualified();
        let exists: bool = timed(
            limit,
            client.query_one("SELECT to_regclass($1) IS NOT NULL", &[&qualified]),
        )
        .await?
        .try_get(0)
        .map_err(DeliveryError::Database)?;
        if !exists {
            return Ok(0);
        }
        match timed(
            limit,
            client.execute(
                &format!("DELETE FROM {qualified} WHERE owner = $1"),
                &[&self.owner.as_str()],
            ),
        )
        .await
        {
            Err(DeliveryError::Database(error))
                if error.code() == Some(&tokio_postgres::error::SqlState::UNDEFINED_TABLE) =>
            {
                Ok(0)
            }
            other => other,
        }
    }

    /// Applies one operation and advances its cursor in one transaction.
    /// `limit` bounds every individual command, never the whole input.
    pub async fn deliver<E: ProgressEffect + ?Sized>(
        &self,
        client: &mut Client,
        prepared: &PreparedProgress,
        effect: &E,
        item: &DeliveryItem<'_>,
        limit: Option<Duration>,
    ) -> Result<(), DeliveryError> {
        item.position
            .validate()
            .map_err(|error| DeliveryError::Rejected(format!("{error:#}")))?;
        let transaction = timed(
            limit,
            client
                .build_transaction()
                .isolation_level(IsolationLevel::ReadCommitted)
                .start(),
        )
        .await?;
        match self
            .apply(&transaction, prepared, effect, item, limit)
            .await
        {
            Ok(()) => match limit {
                Some(limit) => match tokio::time::timeout(limit, transaction.commit()).await {
                    Ok(result) => result.map_err(DeliveryError::CommitUnknown),
                    Err(_) => Err(DeliveryError::CommitTimeout(limit)),
                },
                None => transaction
                    .commit()
                    .await
                    .map_err(DeliveryError::CommitUnknown),
            },
            // Dropping the transaction requests a rollback; the caller
            // discards this connection anyway.
            Err(failure) if failure.discards_connection() => Err(failure),
            Err(failure) => match timed(limit, transaction.rollback()).await {
                Ok(()) => Err(failure),
                Err(rollback) => Err(DeliveryError::Rollback {
                    failure: Box::new(failure),
                    rollback: rollback.to_string(),
                }),
            },
        }
    }

    async fn apply<E: ProgressEffect + ?Sized>(
        &self,
        transaction: &Transaction<'_>,
        prepared: &PreparedProgress,
        effect: &E,
        item: &DeliveryItem<'_>,
        limit: Option<Duration>,
    ) -> Result<(), DeliveryError> {
        let rejected = |reason: &str| DeliveryError::Rejected(reason.to_owned());
        let owner = self.owner.as_str();
        let stream = item.position.stream();
        let sequence = item.position.sequence();
        let operations = i64::try_from(item.position.operations())
            .map_err(|_| rejected("operation count exceeds the cursor range"))?;
        let operation = i64::try_from(item.position.operation())
            .map_err(|_| rejected("operation index exceeds the cursor range"))?;
        let digest = item.content_digest.as_slice();
        let mut row = timed(
            limit,
            transaction.query_opt(&prepared.read, &[&owner, &stream]),
        )
        .await?;
        if row.is_none() {
            // Serialize only this owner's first-stream admission; existing
            // streams use their own row locks.
            timed(
                limit,
                transaction.execute(&prepared.lock, &[&LOCK_CLASS, &self.owner.lock_key()]),
            )
            .await?;
            row = timed(
                limit,
                transaction.query_opt(&prepared.read, &[&owner, &stream]),
            )
            .await?;
        }
        let completed = match row {
            Some(row) => {
                let previous = Cursor::read(&row)?;
                if sequence < previous.sequence {
                    return Err(rejected("destination delivery receipt expired"));
                }
                if sequence == previous.sequence {
                    if previous.digest.as_slice() != digest || previous.operations != operations {
                        return Err(rejected(
                            "destination delivery payload conflicts with its receipt",
                        ));
                    }
                    if operation < previous.completed {
                        return Ok(());
                    }
                    previous.completed
                } else {
                    if previous.completed != previous.operations || operation != 0 {
                        return Err(rejected(
                            "destination has unfinished operations or a missing batch prefix",
                        ));
                    }
                    timed(
                        limit,
                        transaction.execute(
                            &prepared.rollover,
                            &[&owner, &stream, &sequence.to_string(), &digest, &operations],
                        ),
                    )
                    .await?;
                    0
                }
            }
            None => {
                if operation != 0 {
                    return Err(rejected(
                        "destination delivery is missing its first operation",
                    ));
                }
                let count: i64 = timed(limit, transaction.query_one(&prepared.count, &[&owner]))
                    .await?
                    .try_get(0)
                    .map_err(DeliveryError::Database)?;
                if count >= i64::from(self.max_streams) {
                    return Err(DeliveryError::Capacity {
                        owner: owner.to_owned(),
                        table: self.table.to_string(),
                        limit: self.max_streams,
                    });
                }
                timed(
                    limit,
                    transaction.execute(
                        &prepared.insert,
                        &[&owner, &stream, &sequence.to_string(), &digest, &operations],
                    ),
                )
                .await?;
                0
            }
        };
        if operation != completed {
            return Err(rejected("destination delivery has an operation gap"));
        }
        let applied = effect.apply(transaction, item);
        let result = match limit {
            Some(limit) => tokio::time::timeout(limit, applied)
                .await
                .map_err(|_| DeliveryError::Timeout(limit))?,
            None => applied.await,
        };
        result.map_err(|error| match error.downcast::<tokio_postgres::Error>() {
            Ok(error) => DeliveryError::Database(error),
            Err(error) => DeliveryError::Effect(error),
        })?;
        timed(
            limit,
            transaction.batch_execute("SET LOCAL synchronous_commit = on"),
        )
        .await?;
        let changed = timed(
            limit,
            transaction.execute(&prepared.advance, &[&owner, &stream, &completed]),
        )
        .await?;
        if changed != 1 {
            return Err(rejected(
                "destination delivery cursor changed during its effect",
            ));
        }
        Ok(())
    }
}

async fn timed<T>(
    limit: Option<Duration>,
    future: impl Future<Output = Result<T, tokio_postgres::Error>>,
) -> Result<T, DeliveryError> {
    match limit {
        None => future.await.map_err(DeliveryError::Database),
        Some(limit) => match tokio::time::timeout(limit, future).await {
            Ok(result) => result.map_err(DeliveryError::Database),
            Err(_) => Err(DeliveryError::Timeout(limit)),
        },
    }
}

struct Cursor {
    sequence: u64,
    digest: Vec<u8>,
    operations: i64,
    completed: i64,
}

impl Cursor {
    fn read(row: &Row) -> Result<Self, DeliveryError> {
        let invalid = || DeliveryError::Rejected("invalid destination delivery progress".into());
        let version: i16 = row.try_get("version").map_err(DeliveryError::Database)?;
        let encoded: String = row.try_get("sequence").map_err(DeliveryError::Database)?;
        let sequence = encoded.parse::<u64>().map_err(|_| invalid())?;
        let cursor = Self {
            sequence,
            digest: row.try_get("digest").map_err(DeliveryError::Database)?,
            operations: row.try_get("operations").map_err(DeliveryError::Database)?,
            completed: row.try_get("completed").map_err(DeliveryError::Database)?,
        };
        if version == PROGRESS_FORMAT_VERSION
            && sequence > 0
            && sequence.to_string() == encoded
            && cursor.digest.len() == 32
            && cursor.operations > 0
            && cursor.completed >= 0
            && cursor.completed <= cursor.operations
        {
            Ok(cursor)
        } else {
            Err(invalid())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owner_is_the_domain_separated_sha256_of_graph_and_component() {
        let owner = DeliveryOwner::derive("graph", "component");
        let mut hash = Sha256::new();
        hash.update(b"drasi.storedproc/owner/v1\0graph\0component");
        let expected = hash
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        assert_eq!(owner.as_str(), expected);
        assert_eq!(owner.as_str().len(), 64);
        assert!(owner
            .as_str()
            .chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)));
        assert_eq!(owner, DeliveryOwner::derive("graph", "component"));
        // The separator keeps (graph, component) boundaries unambiguous.
        for other in [
            DeliveryOwner::derive("graph2", "component"),
            DeliveryOwner::derive("graph", "component2"),
            DeliveryOwner::derive("graphc", "omponent"),
            DeliveryOwner::derive("component", "graph"),
        ] {
            assert_ne!(owner, other);
        }
        assert_eq!(
            owner.lock_key(),
            u32::from_str_radix(&owner.as_str()[..8], 16).unwrap() as i32
        );
    }

    #[test]
    fn progress_table_names_are_schema_qualified_lowercase_identifiers() {
        let table = ProgressTable::parse(DEFAULT_PROGRESS_TABLE).unwrap();
        assert_eq!(table.schema(), "public");
        assert_eq!(table.table(), "drasi_delivery_progress");
        assert_eq!(table.qualified(), "\"public\".\"drasi_delivery_progress\"");
        assert_eq!(table.to_string(), DEFAULT_PROGRESS_TABLE);
        assert!(ProgressTable::parse("_s1.t_2").is_ok());
        let long = "a".repeat(MAX_IDENTIFIER_BYTES);
        assert!(ProgressTable::parse(&format!("{long}.{long}")).is_ok());
        for invalid in [
            "",
            "progress",
            ".t",
            "s.",
            "a.b.c",
            "Public.t",
            "s.T",
            "1s.t",
            "s.t-1",
            "s.\"t\"",
            "s.t; DROP TABLE x",
            "s.t\0",
            &format!("s.{long}a"),
        ] {
            assert!(ProgressTable::parse(invalid).is_err(), "{invalid:?}");
        }
        let sql = create_table_sql(&table);
        assert!(
            sql.starts_with("CREATE TABLE IF NOT EXISTS \"public\".\"drasi_delivery_progress\"")
        );
        assert!(sql.contains("PRIMARY KEY (owner, stream_key)"));
        assert!(sql.contains("CHECK (version = 2)"));
    }

    #[test]
    fn sqlstates_classify_rollbacks_connections_and_permanent_failures() {
        for code in ["40001", "40P01", "55P03", "57014", "53100", "53200"] {
            assert_eq!(disposition_of_code(code), Disposition::Retry, "{code}");
        }
        for code in [
            "08000", "08003", "08006", "57P01", "57P02", "57P03", "53300",
        ] {
            assert_eq!(disposition_of_code(code), Disposition::Reconnect, "{code}");
        }
        for code in [
            "P0001", "23505", "42501", "42883", "22003", "28P01", "3D000", "42P01",
        ] {
            assert_eq!(disposition_of_code(code), Disposition::Permanent, "{code}");
        }
    }

    #[test]
    fn delivery_errors_classify_commit_uncertainty_as_reconnect_and_protocol_as_permanent() {
        let timeout = DeliveryError::Timeout(Duration::from_secs(1));
        assert_eq!(timeout.disposition(), Disposition::Reconnect);
        assert!(timeout.retryable() && timeout.discards_connection());
        let commit = DeliveryError::CommitTimeout(Duration::from_secs(1));
        assert_eq!(commit.disposition(), Disposition::Reconnect);
        for permanent in [
            DeliveryError::Rejected("gap".into()),
            DeliveryError::Effect(anyhow::anyhow!("render")),
            DeliveryError::Capacity {
                owner: "o".into(),
                table: "t".into(),
                limit: 1,
            },
        ] {
            assert_eq!(permanent.disposition(), Disposition::Permanent);
            assert!(!permanent.retryable() && !permanent.discards_connection());
        }
        let rollback = DeliveryError::Rollback {
            failure: Box::new(DeliveryError::Timeout(Duration::from_secs(1))),
            rollback: "closed".into(),
        };
        assert_eq!(rollback.disposition(), Disposition::Reconnect);
        assert!(rollback.discards_connection());
        let permanent_rollback = DeliveryError::Rollback {
            failure: Box::new(DeliveryError::Rejected("gap".into())),
            rollback: "closed".into(),
        };
        assert_eq!(permanent_rollback.disposition(), Disposition::Permanent);
        assert!(permanent_rollback.discards_connection());
    }

    #[test]
    fn stream_limits_are_bounded() {
        let table = ProgressTable::parse(DEFAULT_PROGRESS_TABLE).unwrap();
        let owner = DeliveryOwner::derive("g", "c");
        assert!(ProgressStore::new(table.clone(), owner.clone(), 0).is_err());
        assert!(ProgressStore::new(table.clone(), owner.clone(), MAX_STREAMS_LIMIT + 1).is_err());
        let store = ProgressStore::new(table, owner, DEFAULT_MAX_STREAMS).unwrap();
        assert_eq!(store.max_streams(), DEFAULT_MAX_STREAMS);
    }
}

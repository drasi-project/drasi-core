// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! The PostgreSQL [`Session`] of the shared stored-procedure delivery
//! protocol (`drasi_computation_storedproc_common`).
//!
//! The protocol, its decisions and its failure rules live in the shared
//! crate; this module supplies only PostgreSQL SQL and SQLSTATE
//! classification. The cursor (format 4) and dead-letter tables are
//! configurable ([`ProgressTable`]) and prepared by an explicit,
//! privilege-checked step ([`PgSession::prepare`]). Every row carries the
//! owner digest, so several consumers share a table without sharing a stream
//! limit, and erasing one consumer deletes only its rows. Earlier formats are
//! refused, not migrated.
//!
//! The legacy format-1 destination ([`super::PostgresDeliveryHandler`]) is a
//! separate, opt-in Rust API used only by the legacy reaction crate's
//! `delivery` module.

use async_trait::async_trait;
pub use drasi_computation_storedproc_common::cursor::{
    ProgressTable, DEFAULT_MAX_STREAMS, DEFAULT_PROGRESS_TABLE, MAX_STREAMS_LIMIT,
    PROGRESS_FORMAT_VERSION,
};
use drasi_computation_storedproc_common::{
    cursor::{valid_owner, CursorRow, Position},
    driver::{CommitError, ErrorClass, Session, TransactionEnded},
};
use std::{fmt, future::Future, time::Duration};
use tokio::task::JoinHandle;
use tokio_postgres::{Client, Statement};

/// The default schema of an unqualified progress table.
pub const DEFAULT_SCHEMA: &str = "public";
/// Advisory lock class shared with the format-1 handler.
const LOCK_CLASS: i32 = 1_146_241_363;
/// Advisory lock object serialising progress-table creation.
const TABLE_LOCK: i32 = 2;

/// The PostgreSQL connection driver task.
pub type Driver = JoinHandle<Result<(), tokio_postgres::Error>>;

fn quote(part: &str) -> String {
    format!("\"{part}\"")
}

/// The quoted, schema-qualified name of `table`.
pub fn qualified(table: &ProgressTable) -> String {
    table.quoted(DEFAULT_SCHEMA, quote)
}

/// The statements that create the format-4 cursor table and its dead-letter
/// table. Operators who pre-create the tables for a least-privilege role run
/// exactly these.
pub fn create_table_sql(table: &ProgressTable) -> String {
    format!(
        "CREATE TABLE IF NOT EXISTS {progress} (
    owner TEXT NOT NULL CHECK (owner ~ '^[0-9a-f]{{64}}$'),
    stream_key TEXT NOT NULL,
    version SMALLINT NOT NULL CHECK (version = {PROGRESS_FORMAT_VERSION}),
    sequence TEXT NOT NULL CHECK (sequence ~ '^[1-9][0-9]{{0,19}}$'),
    digest BYTEA NOT NULL CHECK (octet_length(digest) = 32),
    operations BIGINT NOT NULL CHECK (operations > 0),
    completed BIGINT NOT NULL CHECK (completed >= 0 AND completed <= operations),
    epoch TEXT NOT NULL CHECK (epoch = '' OR epoch ~ '^[0-9a-f]{{64}}$'),
    generation BIGINT NOT NULL CHECK (generation >= 0),
    pending BIGINT NOT NULL CHECK (pending >= -1 AND pending < operations),
    PRIMARY KEY (owner, stream_key)
);
CREATE TABLE IF NOT EXISTS {dead_letter} (
    owner TEXT NOT NULL CHECK (owner ~ '^[0-9a-f]{{64}}$'),
    delivery_id TEXT NOT NULL,
    stream_key TEXT NOT NULL,
    sequence TEXT NOT NULL,
    operation BIGINT NOT NULL,
    epoch TEXT NOT NULL,
    generation BIGINT NOT NULL,
    error TEXT NOT NULL,
    recorded_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (owner, delivery_id)
)",
        progress = qualified(table),
        dead_letter = qualified(&table.dead_letter())
    )
}

/// What a caller should do after a failed statement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Disposition {
    /// Rolled back; the connection is usable and the operation may be retried.
    Retry,
    /// The connection is unusable: discard it and reconnect.
    Reconnect,
    /// Deterministic: retrying the same operation cannot succeed.
    Permanent,
}

impl From<Disposition> for ErrorClass {
    fn from(disposition: Disposition) -> Self {
        match disposition {
            Disposition::Retry => ErrorClass::Transient { reconnect: false },
            Disposition::Reconnect => ErrorClass::Transient { reconnect: true },
            Disposition::Permanent => ErrorClass::Permanent,
        }
    }
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

/// A command the client stopped waiting for; the connection is discarded.
#[derive(Debug, thiserror::Error)]
#[error("PostgreSQL did not answer a delivery command within {0:?}; the connection is discarded")]
pub struct CommandTimeout(pub Duration);

/// A failed connection attempt.
#[derive(Debug, thiserror::Error)]
#[error("connecting to PostgreSQL failed: {source:#}")]
pub struct ConnectError {
    permanent: bool,
    source: anyhow::Error,
}

impl ConnectError {
    pub fn permanent(source: impl Into<anyhow::Error>) -> Self {
        Self {
            permanent: true,
            source: source.into(),
        }
    }

    pub fn transient(source: impl Into<anyhow::Error>) -> Self {
        Self {
            permanent: false,
            source: source.into(),
        }
    }

    /// Authentication, a missing database and missing privileges are
    /// permanent; everything else (refused, unreachable, starting up, too
    /// many connections, TLS I/O) is transient.
    pub fn classify(error: tokio_postgres::Error) -> Self {
        match error.code().map(|code| code.code()) {
            Some(code) if code.starts_with("28") || code == "3D000" || code == "42501" => {
                Self::permanent(error)
            }
            _ => Self::transient(error),
        }
    }

    /// Whether another attempt can succeed without configuration changes.
    pub fn retryable(&self) -> bool {
        !self.permanent
    }
}

/// A failure to prepare the progress tables at start.
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
    #[error(
        "{table} exists but does not have the format-{PROGRESS_FORMAT_VERSION} layout (earlier \
         formats are not migrated; drop it or choose another progressTable): {source}"
    )]
    Incompatible {
        table: String,
        #[source]
        source: tokio_postgres::Error,
    },
}

/// Classifies any error a PostgreSQL session returns.
pub fn classify(error: &anyhow::Error) -> ErrorClass {
    for cause in error.chain() {
        if let Some(error) = cause.downcast_ref::<tokio_postgres::Error>() {
            return disposition_of(error).into();
        }
        if cause.is::<CommandTimeout>() {
            return ErrorClass::Transient { reconnect: true };
        }
        if let Some(error) = cause.downcast_ref::<ConnectError>() {
            return if error.retryable() {
                ErrorClass::Transient { reconnect: true }
            } else {
                ErrorClass::Permanent
            };
        }
        if cause.is::<ProgressTableError>() {
            return ErrorClass::Permanent;
        }
    }
    ErrorClass::Permanent
}

/// Runs a statement under the client-side bound.
async fn timed<T>(
    limit: Duration,
    future: impl Future<Output = Result<T, tokio_postgres::Error>>,
) -> anyhow::Result<T> {
    match tokio::time::timeout(limit, future).await {
        Ok(result) => Ok(result?),
        Err(_) => Err(CommandTimeout(limit).into()),
    }
}

struct Statements {
    read: Statement,
    count: Statement,
    insert: Statement,
    rollover: Statement,
    pending: Statement,
    skip: Statement,
    advance: Statement,
}

/// One connection, bound to one owner and one progress table. Dropping it
/// aborts the connection driver, which closes the socket and makes the
/// server roll back an open transaction.
pub struct PgSession {
    client: Client,
    driver: Option<Driver>,
    owner: String,
    table: ProgressTable,
    limit: Duration,
    statements: Option<Statements>,
    /// The delivery transaction's ID, read at `begin`.
    transaction: Option<String>,
}

impl fmt::Debug for PgSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PgSession")
            .field("owner", &self.owner)
            .field("table", &self.table)
            .finish_non_exhaustive()
    }
}

impl PgSession {
    /// `limit` bounds every statement on the client side (the server's
    /// `statement_timeout` plus a grace).
    pub fn new(
        client: Client,
        driver: Driver,
        owner: String,
        table: ProgressTable,
        limit: Duration,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(valid_owner(&owner), "invalid destination cursor owner");
        Ok(Self {
            client,
            driver: Some(driver),
            owner,
            table,
            limit,
            statements: None,
            transaction: None,
        })
    }

    pub fn client(&self) -> &Client {
        &self.client
    }

    /// Applies an in-place change of the statement bounds to this session.
    pub async fn set_command_timeout(
        &mut self,
        server: Duration,
        client: Duration,
    ) -> anyhow::Result<()> {
        self.limit = client;
        let statement = format!("SET statement_timeout = {}", server.as_millis());
        timed(self.limit, self.client.batch_execute(&statement)).await
    }

    fn statements(&self) -> anyhow::Result<&Statements> {
        self.statements
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("the cursor statements are not prepared"))
    }

    async fn ensure_tables(&mut self) -> anyhow::Result<()> {
        let limit = self.limit;
        timed(
            limit,
            self.client
                .batch_execute("BEGIN ISOLATION LEVEL READ COMMITTED"),
        )
        .await?;
        let result = async {
            timed(
                limit,
                self.client.execute(
                    "SELECT pg_advisory_xact_lock($1, $2)",
                    &[&LOCK_CLASS, &TABLE_LOCK],
                ),
            )
            .await?;
            let schema = self.table.schema().unwrap_or(DEFAULT_SCHEMA).to_owned();
            let mut missing_tables = Vec::new();
            for table in [self.table.clone(), self.table.dead_letter()] {
                let row = timed(
                    limit,
                    self.client.query_one(
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
                        &[&schema, &table.table()],
                    ),
                )
                .await?;
                let role: String = row.try_get("role")?;
                let flag = |name: &str| row.try_get::<_, bool>(name);
                if !flag("schema_exists")? {
                    return Err(ProgressTableError::MissingSchema {
                        schema: schema.clone(),
                    }
                    .into());
                }
                if !flag("can_use")? {
                    return Err(ProgressTableError::MissingPrivilege {
                        role,
                        privilege: "USAGE".into(),
                        object: format!("schema \"{schema}\""),
                        hint: "grant USAGE on the progress table's schema",
                    }
                    .into());
                }
                if flag("table_exists")? {
                    let mut missing = Vec::new();
                    for (privilege, column) in [
                        ("SELECT", "can_select"),
                        ("INSERT", "can_insert"),
                        ("UPDATE", "can_update"),
                        ("DELETE", "can_delete"),
                    ] {
                        if !flag(column)? {
                            missing.push(privilege);
                        }
                    }
                    if !missing.is_empty() {
                        return Err(ProgressTableError::MissingPrivilege {
                            role,
                            privilege: missing.join(", "),
                            object: format!("table {table}"),
                            hint: "grant SELECT, INSERT, UPDATE and DELETE on the progress and \
                                   dead-letter tables",
                        }
                        .into());
                    }
                } else if !flag("can_create")? {
                    return Err(ProgressTableError::MissingPrivilege {
                        role,
                        privilege: "CREATE".into(),
                        object: format!("schema \"{schema}\""),
                        hint: "the progress tables do not exist: pre-create them with the \
                               documented statements and grant SELECT, INSERT, UPDATE and DELETE, \
                               or grant CREATE on the schema (PostgreSQL 15 and later no longer \
                               grant CREATE on public to every role)",
                    }
                    .into());
                } else {
                    missing_tables.push(table);
                }
            }
            if !missing_tables.is_empty() {
                timed(limit, self.client.batch_execute(&create_table_sql(&self.table))).await?;
            }
            for (table, columns) in [
                (
                    &self.table,
                    "owner, stream_key, version, sequence, digest, operations, completed, epoch, \
                     generation, pending",
                ),
                (
                    &self.table.dead_letter(),
                    "owner, delivery_id, stream_key, sequence, operation, epoch, generation, \
                     error, recorded_at",
                ),
            ] {
                if let Err(source) = self
                    .client
                    .execute(
                        &format!("SELECT {columns} FROM {} WHERE false", qualified(table)),
                        &[],
                    )
                    .await
                {
                    return Err(ProgressTableError::Incompatible {
                        table: table.to_string(),
                        source,
                    }
                    .into());
                }
            }
            anyhow::Ok(())
        }
        .await;
        match result {
            Ok(()) => timed(limit, self.client.batch_execute("COMMIT")).await,
            Err(error) => {
                let _ = timed(limit, self.client.batch_execute("ROLLBACK")).await;
                Err(error)
            }
        }
    }

    async fn prepare_statements(&mut self) -> anyhow::Result<()> {
        let table = qualified(&self.table);
        let dead_letter = qualified(&self.table.dead_letter());
        let limit = self.limit;
        let client = &self.client;
        let prepare = |sql: String| async move { timed(limit, client.prepare(&sql)).await };
        let statements = Statements {
            read: prepare(format!(
                "SELECT version, sequence, digest, operations, completed, epoch, generation, \
                 pending FROM {table} WHERE owner = $1 AND stream_key = $2 FOR UPDATE"
            ))
            .await?,
            count: prepare(format!("SELECT count(*) FROM {table} WHERE owner = $1")).await?,
            insert: prepare(format!(
                "INSERT INTO {table} (owner, stream_key, version, sequence, digest, operations, \
                 completed, epoch, generation, pending) VALUES ($1, $2, \
                 {PROGRESS_FORMAT_VERSION}, $3, $4, $5, 0, $6, $7, 0)"
            ))
            .await?,
            rollover: prepare(format!(
                "UPDATE {table} SET version = {PROGRESS_FORMAT_VERSION}, sequence = $3, \
                 digest = $4, operations = $5, completed = 0, epoch = $6, generation = $7, \
                 pending = 0 WHERE owner = $1 AND stream_key = $2"
            ))
            .await?,
            pending: prepare(format!(
                "UPDATE {table} SET pending = $3 WHERE owner = $1 AND stream_key = $2"
            ))
            .await?,
            skip: prepare(format!(
                "INSERT INTO {dead_letter} (owner, delivery_id, stream_key, sequence, operation, \
                 epoch, generation, error) VALUES ($1, $2, $3, $4, $5, $6, $7, $8) \
                 ON CONFLICT (owner, delivery_id) DO UPDATE SET error = EXCLUDED.error, \
                 recorded_at = now()"
            ))
            .await?,
            advance: prepare(format!(
                "UPDATE {table} SET completed = completed + 1, pending = -1 \
                 WHERE owner = $1 AND stream_key = $2 AND completed = $3"
            ))
            .await?,
        };
        self.statements = Some(statements);
        Ok(())
    }

    /// Fails with [`TransactionEnded`] when the session no longer runs the
    /// delivery transaction it began.
    async fn check_transaction(&self) -> anyhow::Result<()> {
        let current: String = timed(
            self.limit,
            self.client.query_one("SELECT txid_current()::text", &[]),
        )
        .await?
        .try_get(0)?;
        anyhow::ensure!(
            self.transaction.as_deref() == Some(current.as_str()),
            TransactionEnded
        );
        Ok(())
    }
}

impl Drop for PgSession {
    fn drop(&mut self) {
        if let Some(driver) = self.driver.take() {
            driver.abort();
        }
    }
}

#[async_trait]
impl Session for PgSession {
    /// The rendered SQL and its positional parameters.
    type Command = (String, Vec<serde_json::Value>);

    fn classify(error: &anyhow::Error) -> ErrorClass {
        classify(error)
    }

    async fn prepare(&mut self) -> anyhow::Result<()> {
        self.ensure_tables().await?;
        self.prepare_statements().await
    }

    async fn begin(&mut self) -> anyhow::Result<()> {
        if self.statements.is_none() {
            self.prepare_statements().await?;
        }
        timed(
            self.limit,
            self.client.batch_execute(
                "BEGIN ISOLATION LEVEL READ COMMITTED; SET LOCAL synchronous_commit = on",
            ),
        )
        .await?;
        self.transaction = Some(
            timed(
                self.limit,
                self.client.query_one("SELECT txid_current()::text", &[]),
            )
            .await?
            .try_get(0)?,
        );
        Ok(())
    }

    async fn read_cursor(&mut self, stream: &str) -> anyhow::Result<Option<CursorRow>> {
        let row = timed(
            self.limit,
            self.client
                .query_opt(&self.statements()?.read, &[&self.owner, &stream]),
        )
        .await?;
        row.map(|row| {
            Ok(CursorRow {
                version: row.try_get(0)?,
                sequence: row.try_get(1)?,
                digest: row.try_get(2)?,
                operations: row.try_get(3)?,
                completed: row.try_get(4)?,
                epoch: row.try_get(5)?,
                generation: row.try_get(6)?,
                pending: row.try_get(7)?,
            })
        })
        .transpose()
    }

    async fn lock_admission(&mut self) -> anyhow::Result<()> {
        // 32 bits of the owner digest: a collision only serialises two
        // owners' admissions.
        let key = i32::from_str_radix(&self.owner[..7], 16)?;
        timed(
            self.limit,
            self.client
                .execute("SELECT pg_advisory_xact_lock($1, $2)", &[&LOCK_CLASS, &key]),
        )
        .await?;
        Ok(())
    }

    async fn count_streams(&mut self) -> anyhow::Result<i64> {
        Ok(timed(
            self.limit,
            self.client
                .query_one(&self.statements()?.count, &[&self.owner]),
        )
        .await?
        .try_get(0)?)
    }

    async fn insert_cursor(&mut self, position: &Position) -> anyhow::Result<()> {
        timed(
            self.limit,
            self.client.execute(
                &self.statements()?.insert,
                &[
                    &self.owner,
                    &position.stream,
                    &position.sequence.to_string(),
                    &position.digest.as_slice(),
                    &position.operations,
                    &position.epoch,
                    &position.generation,
                ],
            ),
        )
        .await?;
        Ok(())
    }

    async fn rollover_cursor(&mut self, position: &Position) -> anyhow::Result<()> {
        timed(
            self.limit,
            self.client.execute(
                &self.statements()?.rollover,
                &[
                    &self.owner,
                    &position.stream,
                    &position.sequence.to_string(),
                    &position.digest.as_slice(),
                    &position.operations,
                    &position.epoch,
                    &position.generation,
                ],
            ),
        )
        .await?;
        Ok(())
    }

    async fn mark_pending(&mut self, stream: &str, operation: i64) -> anyhow::Result<()> {
        timed(
            self.limit,
            self.client.execute(
                &self.statements()?.pending,
                &[&self.owner, &stream, &operation],
            ),
        )
        .await?;
        Ok(())
    }

    async fn execute(&mut self, (sql, parameters): &Self::Command) -> anyhow::Result<()> {
        let result = match tokio::time::timeout(
            self.limit,
            crate::sql::execute_parameters(&self.client, sql, parameters.clone()),
        )
        .await
        {
            Ok(result) => result.map(drop),
            Err(_) => Err(CommandTimeout(self.limit).into()),
        };
        if let Err(error) = result {
            // invalid_transaction_termination: a procedure CALLed inside the
            // delivery transaction ran COMMIT or ROLLBACK.
            let ended = error.chain().any(|cause| {
                cause
                    .downcast_ref::<tokio_postgres::Error>()
                    .and_then(tokio_postgres::Error::code)
                    .is_some_and(|code| code.code() == "2D000")
            });
            return Err(if ended {
                error.context(TransactionEnded)
            } else {
                error
            });
        }
        // A template that ran COMMIT itself leaves the session outside the
        // delivery transaction.
        self.check_transaction().await
    }

    async fn record_skip(&mut self, position: &Position, error: &str) -> anyhow::Result<()> {
        timed(
            self.limit,
            self.client.execute(
                &self.statements()?.skip,
                &[
                    &self.owner,
                    &position.delivery_id,
                    &position.stream,
                    &position.sequence.to_string(),
                    &position.operation,
                    &position.epoch,
                    &position.generation,
                    &error,
                ],
            ),
        )
        .await?;
        Ok(())
    }

    async fn advance_cursor(&mut self, stream: &str, completed: i64) -> anyhow::Result<u64> {
        timed(
            self.limit,
            self.client.execute(
                &self.statements()?.advance,
                &[&self.owner, &stream, &completed],
            ),
        )
        .await
    }

    async fn commit(&mut self) -> Result<(), CommitError> {
        self.transaction = None;
        match tokio::time::timeout(self.limit, self.client.batch_execute("COMMIT")).await {
            Ok(Ok(())) => Ok(()),
            // A SQLSTATE on a live connection (a deferred constraint, a
            // serialization failure at commit) is a definite rollback.
            Ok(Err(error)) if error.code().is_some() && !error.is_closed() => {
                Err(CommitError::Rejected(error.into()))
            }
            Ok(Err(error)) => Err(CommitError::Unknown(error.into())),
            Err(_) => Err(CommitError::Unknown(CommandTimeout(self.limit).into())),
        }
    }

    async fn rollback(&mut self) -> anyhow::Result<()> {
        self.transaction = None;
        timed(self.limit, self.client.batch_execute("ROLLBACK")).await
    }

    async fn erase(&mut self) -> anyhow::Result<u64> {
        let mut erased = 0;
        for (index, table) in [self.table.clone(), self.table.dead_letter()]
            .iter()
            .enumerate()
        {
            let name = qualified(table);
            let exists: bool = timed(
                self.limit,
                self.client
                    .query_one("SELECT to_regclass($1) IS NOT NULL", &[&name]),
            )
            .await?
            .try_get(0)?;
            if !exists {
                continue;
            }
            match timed(
                self.limit,
                self.client.execute(
                    &format!("DELETE FROM {name} WHERE owner = $1"),
                    &[&self.owner],
                ),
            )
            .await
            {
                Ok(rows) if index == 0 => erased = rows,
                Ok(_) => {}
                Err(error)
                    if error
                        .downcast_ref::<tokio_postgres::Error>()
                        .and_then(tokio_postgres::Error::code)
                        == Some(&tokio_postgres::error::SqlState::UNDEFINED_TABLE) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(erased)
    }

    fn healthy(&self) -> bool {
        !self.client.is_closed()
            && self
                .driver
                .as_ref()
                .is_some_and(|driver| !driver.is_finished())
    }

    /// Drops the client, which closes the connection once the server has
    /// answered any command still running, and joins the driver.
    async fn close(mut self) -> anyhow::Result<()> {
        let driver = self.driver.take();
        drop(self);
        if let Some(driver) = driver {
            match driver.await {
                Ok(Ok(())) => {}
                // An earlier connection failure is not a close failure.
                Ok(Err(error)) => log::debug!("PostgreSQL sink connection closed: {error}"),
                Err(error) if error.is_cancelled() => {}
                Err(error) => log::warn!("PostgreSQL sink connection driver failed: {error}"),
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sqlstates_are_classified_without_message_text() {
        for (code, expected) in [
            ("40001", Disposition::Retry),
            ("40P01", Disposition::Retry),
            ("57014", Disposition::Retry),
            ("53100", Disposition::Retry),
            ("57P01", Disposition::Reconnect),
            ("08006", Disposition::Reconnect),
            ("23505", Disposition::Permanent),
            ("23503", Disposition::Permanent),
            ("P0001", Disposition::Permanent),
            ("2D000", Disposition::Permanent),
        ] {
            assert_eq!(disposition_of_code(code), expected, "{code}");
        }
        assert_eq!(
            classify(&CommandTimeout(Duration::from_secs(1)).into()),
            ErrorClass::Transient { reconnect: true }
        );
        assert_eq!(
            classify(&ConnectError::permanent(anyhow::anyhow!("auth")).into()),
            ErrorClass::Permanent
        );
    }

    #[test]
    fn tables_are_quoted_in_the_default_schema() {
        let table = ProgressTable::parse(DEFAULT_PROGRESS_TABLE).unwrap();
        assert_eq!(qualified(&table), "\"public\".\"drasi_delivery_progress\"");
        let sql = create_table_sql(&ProgressTable::parse("app.progress").unwrap());
        assert!(sql.contains("\"app\".\"progress\""));
        assert!(sql.contains("\"app\".\"progress_dead_letter\""));
        assert!(sql.contains("CHECK (version = 4)"));
    }
}

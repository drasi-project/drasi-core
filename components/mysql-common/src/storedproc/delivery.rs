// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! The MySQL [`Session`] of the shared stored-procedure delivery protocol
//! (`drasi_computation_storedproc_common`).
//!
//! The protocol, its decisions and its failure rules live in the shared
//! crate; this module supplies only MySQL SQL, the session KILL proof and
//! error-code classification. The format-4 cursor table and its dead-letter
//! table are InnoDB tables (`table` in the configured database, or
//! `schema.table`). Every cursor write is a prepared statement whose reply
//! reports whether the delivery transaction is still open, so a procedure
//! that commits or rolls back is detected ([`TransactionEnded`]) and the
//! cursor is never advanced in autocommit. First-stream admission for an
//! owner is serialised with a named lock (`GET_LOCK`), released when the
//! transaction ends.

use super::session::{self, ConnectFailure, Endpoint, Live, Sessions};
use crate::native::{catalog::field, MySqlServerError};
use anyhow::{ensure, Context, Result};
use async_trait::async_trait;
pub use drasi_computation_storedproc_common::cursor::{
    ProgressTable, DEFAULT_MAX_STREAMS, DEFAULT_PROGRESS_TABLE, PROGRESS_FORMAT_VERSION,
};
use drasi_computation_storedproc_common::{
    cursor::{valid_owner, CursorRow, Position},
    driver::{CommitError, Connector, ErrorClass, Session, TransactionEnded},
};
use mysql_common::Value;
use sha2::{Digest, Sha256};
use std::{fmt, future::Future, time::Duration};

/// Server error codes that end the session.
const FATAL_SERVER_CODES: [u16; 4] = [1053, 1927, 3169, 4031];

/// Marks a command failure that left the session unusable: a transport,
/// protocol or timeout failure (the reply was not fully read), or a server
/// error that ends the session. Ordinary server errors leave it usable.
#[derive(Debug)]
pub struct SessionFault;

impl fmt::Display for SessionFault {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("the MySQL session is no longer usable")
    }
}

impl std::error::Error for SessionFault {}

/// Another first-stream admission for this owner held the admission lock.
#[derive(Debug, thiserror::Error)]
#[error("another delivery stream admission for this component holds the admission lock")]
pub struct AdmissionBusy;

/// A retired session could not yet be proven gone; nothing new runs until
/// cleanup completes.
#[derive(Debug, thiserror::Error)]
#[error("a previous MySQL session is not yet proven gone; retry after cleanup: {0:#}")]
pub struct CleanupPending(pub anyhow::Error);

/// Whether `error` came from a command that left the session unusable.
pub fn breaks_session(error: &anyhow::Error) -> bool {
    error.downcast_ref::<SessionFault>().is_some()
        || server_error(error).is_some_and(|error| FATAL_SERVER_CODES.contains(&error.code))
}

/// The first MySQL server error in the chain.
pub fn server_error(error: &anyhow::Error) -> Option<&MySqlServerError> {
    error
        .chain()
        .take(64)
        .find_map(|cause| cause.downcast_ref::<MySqlServerError>())
}

/// Classifies by type and server error code, never by message text:
/// deadlock (1213) and lock wait timeout (1205) are transient on the same
/// session (InnoDB rolled the statement or transaction back); a broken
/// session, an unproven cleanup and a retryable connection failure are
/// transient after reconnecting; a busy admission lock is transient.
pub fn classify(error: &anyhow::Error) -> ErrorClass {
    for cause in error.chain().take(64) {
        if let Some(failure) = cause.downcast_ref::<ConnectFailure>() {
            return if failure.retryable {
                ErrorClass::Transient { reconnect: true }
            } else {
                ErrorClass::Permanent
            };
        }
        if cause.is::<CleanupPending>() {
            return ErrorClass::Transient { reconnect: true };
        }
        if cause.is::<AdmissionBusy>() {
            return ErrorClass::Transient { reconnect: false };
        }
    }
    if breaks_session(error) {
        return ErrorClass::Transient { reconnect: true };
    }
    match server_error(error) {
        Some(server) if matches!(server.code, 1205 | 1213) => {
            ErrorClass::Transient { reconnect: false }
        }
        _ => ErrorClass::Permanent,
    }
}

/// Runs one statement under `timeout`. A timeout, transport or protocol
/// failure leaves the session mid-command; such failures carry
/// [`SessionFault`] so [`breaks_session`] reports them.
pub async fn bounded<T>(timeout: Duration, work: impl Future<Output = Result<T>>) -> Result<T> {
    match tokio::time::timeout(timeout, work).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(error)) => match server_error(&error) {
            Some(server) if !FATAL_SERVER_CODES.contains(&server.code) => Err(error),
            _ => Err(error.context(SessionFault)),
        },
        Err(elapsed) => Err(anyhow::Error::new(elapsed)
            .context("MySQL command exceeded commandTimeoutMs")
            .context(SessionFault)),
    }
}

/// The quoted name of `table` in `database` (or its own schema).
pub fn quoted(table: &ProgressTable, database: &str) -> String {
    table.quoted(database, crate::quote_identifier)
}

/// The `CREATE TABLE` statements of the cursor and dead-letter tables.
/// Unnamed CHECK constraints avoid MySQL's schema-wide constraint-name
/// namespace; the protocol validates every row it reads.
pub fn create_statements(table: &ProgressTable, database: &str) -> [String; 2] {
    [
        format!(
            "CREATE TABLE IF NOT EXISTS {} (\
             owner CHAR(64) CHARACTER SET ascii COLLATE ascii_bin NOT NULL, \
             stream_key VARCHAR(128) CHARACTER SET ascii COLLATE ascii_bin NOT NULL, \
             version SMALLINT NOT NULL, \
             sequence VARCHAR(20) CHARACTER SET ascii COLLATE ascii_bin NOT NULL, \
             digest BINARY(32) NOT NULL, \
             operations BIGINT NOT NULL, \
             completed BIGINT NOT NULL, \
             epoch VARCHAR(64) CHARACTER SET ascii COLLATE ascii_bin NOT NULL, \
             generation BIGINT NOT NULL, \
             pending BIGINT NOT NULL, \
             PRIMARY KEY (owner, stream_key), \
             CHECK (version = {PROGRESS_FORMAT_VERSION}), \
             CHECK (operations > 0), \
             CHECK (completed >= 0 AND completed <= operations), \
             CHECK (pending >= -1 AND pending < operations)\
             ) ENGINE=InnoDB",
            quoted(table, database)
        ),
        format!(
            "CREATE TABLE IF NOT EXISTS {} (\
             owner CHAR(64) CHARACTER SET ascii COLLATE ascii_bin NOT NULL, \
             delivery_id VARCHAR(128) CHARACTER SET ascii COLLATE ascii_bin NOT NULL, \
             stream_key VARCHAR(128) CHARACTER SET ascii COLLATE ascii_bin NOT NULL, \
             sequence VARCHAR(20) CHARACTER SET ascii COLLATE ascii_bin NOT NULL, \
             operation BIGINT NOT NULL, \
             epoch VARCHAR(64) CHARACTER SET ascii COLLATE ascii_bin NOT NULL, \
             generation BIGINT NOT NULL, \
             error TEXT NOT NULL, \
             recorded_at TIMESTAMP(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6), \
             PRIMARY KEY (owner, delivery_id)\
             ) ENGINE=InnoDB",
            quoted(&table.dead_letter(), database)
        ),
    ]
}

const CURSOR_COLUMNS: [(&str, &str); 10] = [
    ("owner", "char"),
    ("stream_key", "varchar"),
    ("version", "smallint"),
    ("sequence", "varchar"),
    ("digest", "binary"),
    ("operations", "bigint"),
    ("completed", "bigint"),
    ("epoch", "varchar"),
    ("generation", "bigint"),
    ("pending", "bigint"),
];

const DEAD_LETTER_COLUMNS: [(&str, &str); 9] = [
    ("owner", "char"),
    ("delivery_id", "varchar"),
    ("stream_key", "varchar"),
    ("sequence", "varchar"),
    ("operation", "bigint"),
    ("epoch", "varchar"),
    ("generation", "bigint"),
    ("error", "text"),
    ("recorded_at", "timestamp"),
];

fn literal(value: &str) -> Result<String> {
    ensure!(
        value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_'),
        "invalid delivery identifier"
    );
    Ok(format!("'{value}'"))
}

/// One MySQL session, bound to one owner and one progress table.
pub struct MySqlSession {
    live: Live,
    owner: String,
    database: String,
    table: ProgressTable,
    timeout: Duration,
    /// The admission lock held until the transaction ends.
    admission: Option<String>,
    /// Releasing the admission lock failed: the session is retired.
    broken: bool,
}

impl MySqlSession {
    fn cursor(&self) -> String {
        quoted(&self.table, &self.database)
    }

    async fn query(&mut self, sql: &str) -> Result<Vec<Vec<Option<Vec<u8>>>>> {
        Ok(bounded(self.timeout, self.live.connection.query(sql))
            .await?
            .values)
    }

    async fn run(&mut self, sql: &str) -> Result<()> {
        bounded(self.timeout, self.live.connection.execute(sql)).await
    }

    /// A prepared statement inside the delivery transaction; fails with
    /// [`TransactionEnded`] when the transaction is no longer open after it.
    async fn write(&mut self, sql: &str, parameters: &[Value]) -> Result<()> {
        let open = bounded(
            self.timeout,
            self.live
                .connection
                .execute_parameters_in_transaction(sql, parameters),
        )
        .await?;
        ensure!(open, TransactionEnded);
        Ok(())
    }

    /// Releases the admission lock after the transaction ended.
    async fn release(&mut self) {
        let Some(name) = self.admission.take() else {
            return;
        };
        if let Err(error) = self.run(&format!("DO RELEASE_LOCK('{name}')")).await {
            log::warn!("MySQL admission lock release failed; retiring the session: {error:#}");
            self.broken = true;
        }
    }

    /// The first-stream admission lock of this owner (at most 64 bytes).
    fn lock_name(&self) -> String {
        let mut digest = Sha256::new();
        digest.update(b"drasi.storedproc/admission/v1\0");
        digest.update(self.cursor().as_bytes());
        digest.update([0]);
        digest.update(self.owner.as_bytes());
        format!("drasi_dp_{:x}", digest.finalize())[..49].to_owned()
    }

    async fn verify(&mut self, table: &ProgressTable, expected: &[(&str, &str)]) -> Result<()> {
        let name = quoted(table, &self.database);
        let probe = format!(
            "SELECT {} FROM {name} LIMIT 0",
            expected
                .iter()
                .map(|(column, _)| *column)
                .collect::<Vec<_>>()
                .join(", ")
        );
        match self.query(&probe).await {
            Ok(_) => {}
            Err(error) if server_error(&error).is_some_and(|error| error.code == 1146) => {
                let [cursor, dead_letter] = create_statements(&self.table, &self.database);
                for statement in [cursor, dead_letter] {
                    if let Err(error) = self.run(&statement).await {
                        if server_error(&error)
                            .is_some_and(|error| matches!(error.code, 1142 | 1044))
                        {
                            return Err(error.context(format!(
                                "the MySQL user may not create the delivery progress tables; \
                                 grant CREATE on the database or pre-create {name} and its \
                                 dead-letter table (see CONFIGURATION.md)"
                            )));
                        }
                        return Err(error.context("creating the delivery progress tables"));
                    }
                }
            }
            Err(error)
                if server_error(&error)
                    .is_some_and(|error| matches!(error.code, 1142 | 1143 | 1044)) =>
            {
                return Err(error.context(format!(
                    "the MySQL user lacks SELECT, INSERT, UPDATE and DELETE on {name}"
                )));
            }
            Err(error) if server_error(&error).is_some_and(|error| error.code == 1054) => {
                return Err(error.context(format!(
                    "{name} exists but does not have the format-{PROGRESS_FORMAT_VERSION} layout \
                     (earlier formats are not migrated; drop the table or configure another \
                     progressTable)"
                )));
            }
            Err(error) => return Err(error.context(format!("reading {name}"))),
        }
        let schema = literal(table.schema().unwrap_or(&self.database))?;
        let relation = literal(table.table())?;
        let engine = self
            .query(&format!(
                "SELECT ENGINE FROM information_schema.TABLES WHERE TABLE_SCHEMA={schema} AND \
                 TABLE_NAME={relation}"
            ))
            .await?;
        ensure!(
            matches!(engine.as_slice(), [row] if field(row, 0).is_ok_and(|engine| engine.eq_ignore_ascii_case("InnoDB"))),
            "{name} must be an InnoDB table so the cursor commits with the procedure effect"
        );
        let columns = self
            .query(&format!(
                "SELECT COLUMN_NAME, DATA_TYPE FROM information_schema.COLUMNS \
                 WHERE TABLE_SCHEMA={schema} AND TABLE_NAME={relation} ORDER BY ORDINAL_POSITION"
            ))
            .await?;
        let actual = columns
            .iter()
            .map(|row| {
                Ok((
                    field(row, 0)?.to_ascii_lowercase(),
                    field(row, 1)?.to_ascii_lowercase(),
                ))
            })
            .collect::<Result<Vec<_>>>()?;
        ensure!(
            actual.len() == expected.len()
                && actual.iter().zip(expected).all(
                    |((name, kind), (expected_name, expected_kind))| name == expected_name
                        && kind == expected_kind
                ),
            "{name} does not have the format-{PROGRESS_FORMAT_VERSION} delivery progress columns"
        );
        Ok(())
    }

    fn row(row: &[Option<Vec<u8>>]) -> Result<CursorRow> {
        ensure!(row.len() == 8, "invalid destination delivery progress row");
        Ok(CursorRow {
            version: field(row, 0)?.parse()?,
            sequence: field(row, 1)?.to_owned(),
            digest: row[2].clone().context("missing destination digest")?,
            operations: field(row, 3)?.parse()?,
            completed: field(row, 4)?.parse()?,
            epoch: match row[5].as_deref() {
                Some(epoch) => std::str::from_utf8(epoch)?.to_owned(),
                None => anyhow::bail!("missing destination delivery epoch"),
            },
            generation: field(row, 6)?.parse()?,
            pending: field(row, 7)?.parse()?,
        })
    }
}

/// A rendered procedure command: SQL with `?` placeholders and its values.
#[derive(Debug, Clone)]
pub struct Effect {
    pub sql: String,
    pub parameters: Vec<Value>,
}

#[async_trait]
impl Session for MySqlSession {
    type Command = Effect;

    fn classify(error: &anyhow::Error) -> ErrorClass {
        classify(error)
    }

    async fn prepare(&mut self) -> Result<()> {
        let table = self.table.clone();
        self.verify(&table, &CURSOR_COLUMNS).await?;
        self.verify(&table.dead_letter(), &DEAD_LETTER_COLUMNS)
            .await
    }

    async fn begin(&mut self) -> Result<()> {
        self.run("START TRANSACTION").await
    }

    async fn read_cursor(&mut self, stream: &str) -> Result<Option<CursorRow>> {
        let sql = format!(
            "SELECT version, sequence, digest, operations, completed, epoch, generation, pending \
             FROM {} WHERE owner={} AND stream_key={} FOR UPDATE",
            self.cursor(),
            literal(&self.owner)?,
            literal(stream)?
        );
        match self.query(&sql).await?.as_slice() {
            [] => Ok(None),
            [row] => Ok(Some(Self::row(row)?)),
            _ => anyhow::bail!("ambiguous destination delivery progress"),
        }
    }

    async fn lock_admission(&mut self) -> Result<()> {
        let name = self.lock_name();
        let wait = (self.timeout.as_secs() / 2).max(1);
        let acquired = self
            .query(&format!("SELECT GET_LOCK('{name}', {wait})"))
            .await?;
        match acquired.as_slice() {
            [row] if row.first().and_then(Option::as_deref) == Some(b"1") => {
                self.admission = Some(name);
                Ok(())
            }
            [row] if row.first().and_then(Option::as_deref) == Some(b"0") => {
                Err(AdmissionBusy.into())
            }
            _ => anyhow::bail!("MySQL GET_LOCK failed for delivery stream admission"),
        }
    }

    async fn count_streams(&mut self) -> Result<i64> {
        let sql = format!(
            "SELECT COUNT(*) FROM {} WHERE owner={}",
            self.cursor(),
            literal(&self.owner)?
        );
        let rows = self.query(&sql).await?;
        let [row] = rows.as_slice() else {
            anyhow::bail!("invalid destination stream count");
        };
        Ok(field(row, 0)?.parse()?)
    }

    async fn insert_cursor(&mut self, position: &Position) -> Result<()> {
        let sql = format!(
            "INSERT INTO {} (owner, stream_key, version, sequence, digest, operations, completed, \
             epoch, generation, pending) VALUES (?, ?, {PROGRESS_FORMAT_VERSION}, ?, ?, ?, 0, ?, ?, 0)",
            self.cursor()
        );
        let parameters = [
            Value::from(self.owner.as_str()),
            Value::from(position.stream.as_str()),
            Value::from(position.sequence.to_string()),
            Value::Bytes(position.digest.to_vec()),
            Value::from(position.operations),
            Value::from(position.epoch.as_str()),
            Value::from(position.generation),
        ];
        self.write(&sql, &parameters).await
    }

    async fn rollover_cursor(&mut self, position: &Position) -> Result<()> {
        let sql = format!(
            "UPDATE {} SET version={PROGRESS_FORMAT_VERSION}, sequence=?, digest=?, operations=?, \
             completed=0, epoch=?, generation=?, pending=0 WHERE owner=? AND stream_key=?",
            self.cursor()
        );
        let parameters = [
            Value::from(position.sequence.to_string()),
            Value::Bytes(position.digest.to_vec()),
            Value::from(position.operations),
            Value::from(position.epoch.as_str()),
            Value::from(position.generation),
            Value::from(self.owner.as_str()),
            Value::from(position.stream.as_str()),
        ];
        self.write(&sql, &parameters).await
    }

    async fn mark_pending(&mut self, stream: &str, operation: i64) -> Result<()> {
        let sql = format!(
            "UPDATE {} SET pending=? WHERE owner=? AND stream_key=?",
            self.cursor()
        );
        let parameters = [
            Value::from(operation),
            Value::from(self.owner.as_str()),
            Value::from(stream),
        ];
        self.write(&sql, &parameters).await
    }

    async fn execute(&mut self, effect: &Effect) -> Result<()> {
        self.write(&effect.sql, &effect.parameters).await
    }

    async fn record_skip(&mut self, position: &Position, error: &str) -> Result<()> {
        let sql = format!(
            "INSERT INTO {} (owner, delivery_id, stream_key, sequence, operation, epoch, \
             generation, error) VALUES (?, ?, ?, ?, ?, ?, ?, ?) ON DUPLICATE KEY UPDATE \
             error=VALUES(error), recorded_at=CURRENT_TIMESTAMP(6)",
            quoted(&self.table.dead_letter(), &self.database)
        );
        let parameters = [
            Value::from(self.owner.as_str()),
            Value::from(position.delivery_id.as_str()),
            Value::from(position.stream.as_str()),
            Value::from(position.sequence.to_string()),
            Value::from(position.operation),
            Value::from(position.epoch.as_str()),
            Value::from(position.generation),
            Value::from(error),
        ];
        self.write(&sql, &parameters).await
    }

    async fn advance_cursor(&mut self, stream: &str, completed: i64) -> Result<u64> {
        let sql = format!(
            "UPDATE {} SET completed=completed+1, pending=-1 WHERE owner=? AND stream_key=? AND \
             completed=?",
            self.cursor()
        );
        let parameters = [
            Value::from(self.owner.as_str()),
            Value::from(stream),
            Value::from(completed),
        ];
        self.write(&sql, &parameters).await?;
        let changed = self.query("SELECT ROW_COUNT()").await?;
        let [row] = changed.as_slice() else {
            anyhow::bail!("invalid MySQL row count");
        };
        Ok(field(row, 0)?.parse()?)
    }

    async fn commit(&mut self) -> std::result::Result<(), CommitError> {
        let result = self.run("COMMIT").await;
        match result {
            Ok(()) => {
                self.release().await;
                Ok(())
            }
            // A server error that leaves the session usable is a definite
            // rollback.
            Err(error) if server_error(&error).is_some() && !breaks_session(&error) => {
                self.release().await;
                Err(CommitError::Rejected(error))
            }
            Err(error) => Err(CommitError::Unknown(error)),
        }
    }

    async fn rollback(&mut self) -> Result<()> {
        self.run("ROLLBACK").await?;
        self.release().await;
        Ok(())
    }

    async fn erase(&mut self) -> Result<u64> {
        let owner = literal(&self.owner)?;
        let mut erased = 0;
        for (index, table) in [self.table.clone(), self.table.dead_letter()]
            .iter()
            .enumerate()
        {
            let sql = format!(
                "DELETE FROM {} WHERE owner={owner}",
                quoted(table, &self.database)
            );
            match self.run(&sql).await {
                Ok(()) if index == 0 => {
                    let changed = self.query("SELECT ROW_COUNT()").await?;
                    erased = changed
                        .first()
                        .map(|row| field(row, 0).and_then(|count| Ok(count.parse()?)))
                        .transpose()?
                        .unwrap_or(0);
                }
                Ok(()) => {}
                Err(error) if server_error(&error).is_some_and(|error| error.code == 1146) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(erased)
    }

    fn healthy(&self) -> bool {
        !self.broken
    }

    async fn close(self) -> Result<()> {
        self.live.connection.close().await
    }
}

/// Opens [`MySqlSession`]s for one activation and retires failed ones with
/// the session KILL proof: a session that may still run a command is killed
/// and proven gone before any replacement runs, so an abandoned transaction
/// can never commit or hold locks behind a retry.
pub struct MySqlConnector {
    endpoint: Endpoint,
    owner: String,
    database: String,
    table: ProgressTable,
    connect_timeout: Duration,
    command_timeout: Duration,
    shutdown_timeout: Duration,
    retiring: Sessions,
}

impl MySqlConnector {
    pub fn new(
        endpoint: Endpoint,
        owner: String,
        database: String,
        table: ProgressTable,
        timeouts: Timeouts,
    ) -> Result<Self> {
        ensure!(valid_owner(&owner), "invalid destination cursor owner");
        Ok(Self {
            endpoint,
            owner,
            database,
            table,
            connect_timeout: timeouts.connect,
            command_timeout: timeouts.command,
            shutdown_timeout: timeouts.shutdown,
            retiring: Sessions::default(),
        })
    }

    /// Timeouts read per session (in-place updates).
    pub fn set_timeouts(&mut self, timeouts: Timeouts) {
        self.connect_timeout = timeouts.connect;
        self.command_timeout = timeouts.command;
        self.shutdown_timeout = timeouts.shutdown;
    }

    /// Whether retired sessions still await proof that they are gone.
    pub fn cleanup_pending(&self) -> bool {
        self.retiring.cleanup_pending()
    }

    /// Proves every retired session gone within `budget`.
    pub async fn retire(&mut self, budget: Duration) -> Result<()> {
        self.retiring
            .retire(&self.endpoint, budget)
            .await
            .map_err(|error| CleanupPending(error).into())
    }
}

/// The per-session timeouts of a [`MySqlConnector`].
#[derive(Debug, Clone, Copy)]
pub struct Timeouts {
    pub connect: Duration,
    pub command: Duration,
    pub shutdown: Duration,
}

#[async_trait]
impl Connector for MySqlConnector {
    type Session = MySqlSession;

    async fn connect(&mut self) -> Result<MySqlSession> {
        if self.retiring.cleanup_pending() {
            self.retire(self.shutdown_timeout).await?;
        }
        let live = session::connect(&self.endpoint, self.connect_timeout, None, None).await?;
        Ok(MySqlSession {
            live,
            owner: self.owner.clone(),
            database: self.database.clone(),
            table: self.table.clone(),
            timeout: self.command_timeout,
            admission: None,
            broken: false,
        })
    }

    async fn discard(&mut self, session: MySqlSession) -> Result<()> {
        self.retiring.retire_session(session.live);
        self.retire(self.shutdown_timeout).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server(code: u16) -> anyhow::Error {
        MySqlServerError {
            code,
            sql_state: None,
            message: "x".into(),
        }
        .into()
    }

    #[test]
    fn classification_uses_codes_and_typed_errors() {
        let transient = ErrorClass::Transient { reconnect: false };
        let reconnect = ErrorClass::Transient { reconnect: true };
        assert_eq!(classify(&server(1213)), transient);
        assert_eq!(classify(&server(1205)), transient);
        assert_eq!(classify(&server(1213).context("wrapped")), transient);
        for code in [1045, 1146, 1644, 1062, 1142] {
            assert_eq!(classify(&server(code)), ErrorClass::Permanent, "{code}");
        }
        assert_eq!(classify(&server(1927)), reconnect);
        assert_eq!(classify(&AdmissionBusy.into()), transient);
        let io = || anyhow::Error::from(std::io::Error::other("reset"));
        assert_eq!(classify(&io().context(SessionFault)), reconnect);
        assert_eq!(classify(&CleanupPending(io()).into()), reconnect);
        assert_eq!(
            classify(
                &ConnectFailure {
                    retryable: false,
                    source: server(1045)
                }
                .into()
            ),
            ErrorClass::Permanent
        );
        assert_eq!(classify(&io()), ErrorClass::Permanent);
    }

    #[test]
    fn tables_are_quoted_in_the_database_and_created_in_format_four() {
        let table = ProgressTable::parse(DEFAULT_PROGRESS_TABLE).unwrap();
        assert_eq!(quoted(&table, "app"), "`app`.`drasi_delivery_progress`");
        let [cursor, dead_letter] = create_statements(&table, "app");
        assert!(cursor.contains("CHECK (version = 4)") && cursor.contains("ENGINE=InnoDB"));
        assert!(dead_letter.contains("`app`.`drasi_delivery_progress_dead_letter`"));
        let other = ProgressTable::parse("audit.progress").unwrap();
        assert_eq!(quoted(&other, "app"), "`audit`.`progress`");
        assert!(literal("x'; DROP").is_err());
    }
}

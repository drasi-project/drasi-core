// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Destination cursor shared by the stored-procedure sinks (format version 2).
//!
//! One MySQL transaction commits an operation's procedure effect together
//! with the operation's row in the cursor table:
//!
//! ```text
//! owner      CHAR(64)      lowercase hex SHA-256("drasi.storedproc/owner/v1\0" graph "\0" component)
//! stream_key VARCHAR(128)  the host DeliveryPosition stream
//! version    SMALLINT      2
//! sequence   VARCHAR(20)   decimal producer sequence of the current batch
//! digest     BINARY(32)    canonical batch digest
//! operations BIGINT        operations in the current batch
//! completed  BIGINT        completed operation prefix of the current batch
//! PRIMARY KEY (owner, stream_key), InnoDB
//! ```
//!
//! The protocol mirrors the PostgreSQL destination: lock the stream row
//! (`FOR UPDATE`); reject an expired receipt (older sequence) or a conflicting
//! one (same sequence, other digest or operation count); skip an operation
//! inside the completed prefix; require a complete previous batch and
//! operation zero before a new batch replaces the row; reject gaps; run the
//! effect; and increment `completed` in the same transaction. First-stream
//! admission for an owner is serialized with a named lock, which also
//! enforces the per-owner stream capacity.

use crate::native::{catalog::field, connection::Connection, MySqlServerError};
use anyhow::{ensure, Context, Result};
use drasi_computation_contracts::DeliveryItem;
use sha2::{Digest, Sha256};
use std::{fmt, future::Future, time::Duration};

/// Cursor table format version written to every row.
pub const PROGRESS_FORMAT_VERSION: i64 = 2;
/// Default cursor table, in the configured database.
pub const DEFAULT_PROGRESS_TABLE: &str = "drasi_delivery_progress";
/// Default per-owner stream capacity.
pub const DEFAULT_MAX_STREAMS: u32 = 4096;
const OWNER_DOMAIN: &[u8] = b"drasi.storedproc/owner/v1\0";

/// The cursor owner for a component in a graph: lowercase hex SHA-256 of
/// `"drasi.storedproc/owner/v1\0" + graph_id + "\0" + component_id`.
pub fn owner_id(graph_id: &str, component_id: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(OWNER_DOMAIN);
    digest.update(graph_id.as_bytes());
    digest.update([0]);
    digest.update(component_id.as_bytes());
    hex(&digest.finalize())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// A validated cursor table name in a validated database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgressTable {
    database: String,
    table: String,
}

impl ProgressTable {
    pub fn new(database: &str, table: &str) -> Result<Self> {
        for (name, value) in [("database", database), ("progressTable", table)] {
            ensure!(
                crate::is_valid_identifier(value) && value.len() <= 64,
                "{name} must be a MySQL identifier of letters, digits and underscores (at most 64)"
            );
        }
        Ok(Self {
            database: database.into(),
            table: table.into(),
        })
    }

    pub fn database(&self) -> &str {
        &self.database
    }

    pub fn table(&self) -> &str {
        &self.table
    }

    /// `` `database`.`table` ``; both parts are validated identifiers.
    pub fn quoted(&self) -> String {
        format!(
            "{}.{}",
            crate::quote_identifier(&self.database),
            crate::quote_identifier(&self.table)
        )
    }

    /// The table definition. Unnamed CHECK constraints avoid MySQL's
    /// schema-wide constraint-name namespace; MySQL 5.7 parses and ignores
    /// them, and the protocol validates every row it reads.
    pub fn create_statement(&self) -> String {
        format!(
            "CREATE TABLE IF NOT EXISTS {} (\
             owner CHAR(64) CHARACTER SET ascii COLLATE ascii_bin NOT NULL, \
             stream_key VARCHAR(128) CHARACTER SET ascii COLLATE ascii_bin NOT NULL, \
             version SMALLINT NOT NULL, \
             sequence VARCHAR(20) CHARACTER SET ascii COLLATE ascii_bin NOT NULL, \
             digest BINARY(32) NOT NULL, \
             operations BIGINT NOT NULL, \
             completed BIGINT NOT NULL, \
             PRIMARY KEY (owner, stream_key), \
             CHECK (version = 2), \
             CHECK (operations > 0), \
             CHECK (completed >= 0 AND completed <= operations)\
             ) ENGINE=InnoDB",
            self.quoted()
        )
    }

    /// The first-stream admission lock for an owner (at most 64 bytes).
    fn lock_name(&self, owner: &str) -> String {
        let mut digest = Sha256::new();
        digest.update(b"drasi.storedproc/admission/v1\0");
        digest.update(self.database.as_bytes());
        digest.update([0]);
        digest.update(self.table.as_bytes());
        digest.update([0]);
        digest.update(owner.as_bytes());
        format!("drasi_dp_{}", &hex(&digest.finalize())[..40])
    }
}

/// Typed delivery failures. [`retryable`] classifies them.
#[derive(Debug)]
pub enum MySqlDeliveryError {
    /// The session failed before COMMIT was sent; its owned session has been
    /// retired, so the server rolled the transaction back.
    ConnectionLost(anyhow::Error),
    /// COMMIT was sent but its outcome is unknown. The owned session has been
    /// retired; the cursor makes a retry idempotent.
    CommitUnknown(anyhow::Error),
    /// A statement exceeded `commandTimeoutMs`; the owned session has been
    /// retired and the transaction rolled back.
    CommandTimeout(anyhow::Error),
    /// Another first-stream admission for this owner held the lock.
    AdmissionBusy,
    /// The owner already holds `maxStreams` streams.
    StreamCapacity(u32),
    /// The template could not be rendered for this change (permanent).
    Render(anyhow::Error),
    /// The cursor row disagrees with the operation (permanent).
    Receipt(&'static str),
    /// A retired session could not yet be proven gone; nothing new runs until
    /// cleanup completes.
    CleanupPending(anyhow::Error),
}

impl fmt::Display for MySqlDeliveryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ConnectionLost(error) => write!(
                formatter,
                "MySQL session failed before commit; the transaction rolled back: {error:#}"
            ),
            Self::CommitUnknown(error) => write!(
                formatter,
                "MySQL commit outcome is unknown; the destination cursor makes the retry idempotent: {error:#}"
            ),
            Self::CommandTimeout(error) => write!(
                formatter,
                "MySQL command exceeded commandTimeoutMs; the session was killed and the transaction rolled back: {error:#}"
            ),
            Self::AdmissionBusy => formatter.write_str(
                "another delivery stream admission for this component holds the admission lock",
            ),
            Self::StreamCapacity(max) => write!(
                formatter,
                "destination delivery stream capacity ({max} streams, maxStreams) is exhausted for this component"
            ),
            Self::Render(error) => {
                write!(formatter, "MySQL command template failed to render: {error:#}")
            }
            Self::Receipt(reason) => formatter.write_str(reason),
            Self::CleanupPending(error) => write!(
                formatter,
                "a previous MySQL session is not yet proven gone; retry after cleanup: {error:#}"
            ),
        }
    }
}

impl std::error::Error for MySqlDeliveryError {}

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

/// Whether `error` came from a command that left the session unusable.
pub fn breaks_session(error: &anyhow::Error) -> bool {
    error.downcast_ref::<SessionFault>().is_some()
        || server_error(error).is_some_and(|error| FATAL_SERVER_CODES.contains(&error.code))
}

/// Whether `error` is a `commandTimeoutMs` expiry.
pub fn timed_out(error: &anyhow::Error) -> bool {
    error.chain().take(64).any(|cause| {
        cause
            .downcast_ref::<tokio::time::error::Elapsed>()
            .is_some()
    })
}

/// The first MySQL server error in the chain.
pub fn server_error(error: &anyhow::Error) -> Option<&MySqlServerError> {
    error
        .chain()
        .take(64)
        .find_map(|cause| cause.downcast_ref::<MySqlServerError>())
}

/// Host retry classification: deadlock (1213) and lock wait timeout (1205)
/// after rollback; connection loss before commit and an unknown commit
/// outcome after the owned session was retired; a busy admission lock.
pub fn retryable(error: &anyhow::Error) -> bool {
    for cause in error.chain().take(64) {
        if let Some(error) = cause.downcast_ref::<MySqlDeliveryError>() {
            return matches!(
                error,
                MySqlDeliveryError::ConnectionLost(_)
                    | MySqlDeliveryError::CommitUnknown(_)
                    | MySqlDeliveryError::AdmissionBusy
            );
        }
        if let Some(error) = cause.downcast_ref::<MySqlServerError>() {
            return matches!(error.code, 1205 | 1213);
        }
    }
    false
}

/// A failure inside one operation, tagged with the protocol stage reached.
pub enum Failure {
    /// `START TRANSACTION` failed to execute: nothing ran.
    NotStarted(anyhow::Error),
    /// A statement inside the transaction failed; COMMIT was not sent.
    InTransaction(anyhow::Error),
    /// COMMIT was sent and failed or did not answer.
    Commit(anyhow::Error),
}

/// The rendered effect of one operation; `None` advances the cursor only.
pub struct Effect {
    pub sql: String,
    pub parameters: Vec<mysql_common::Value>,
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

struct Cursor {
    sequence: u64,
    digest: Vec<u8>,
    operations: i64,
    completed: i64,
}

impl Cursor {
    fn read(row: &[Option<Vec<u8>>]) -> Result<Self> {
        ensure!(row.len() == 5, "invalid destination delivery progress row");
        let version: i64 = field(row, 0)?.parse()?;
        let encoded = field(row, 1)?;
        let sequence = encoded
            .parse::<u64>()
            .context("invalid destination delivery sequence")?;
        let cursor = Self {
            sequence,
            digest: row[2].clone().context("missing destination digest")?,
            operations: field(row, 3)?.parse()?,
            completed: field(row, 4)?.parse()?,
        };
        ensure!(
            version == PROGRESS_FORMAT_VERSION
                && sequence > 0
                && sequence.to_string() == encoded
                && cursor.digest.len() == 32
                && cursor.operations > 0
                && cursor.completed >= 0
                && cursor.completed <= cursor.operations,
            "invalid destination delivery progress (expected format version 2)"
        );
        Ok(cursor)
    }
}

/// Per-operation protocol inputs.
pub struct Operation<'a> {
    pub table: &'a ProgressTable,
    pub owner: &'a str,
    pub max_streams: u32,
    pub command_timeout: Duration,
}

/// Validated literals: owner and stream are lowercase hex or the fixed
/// stream prefix, the sequence is decimal and the digest is hex-encoded.
struct Keys {
    filter: String,
    stream: String,
    sequence: u64,
    digest: String,
    operations: i64,
    operation: i64,
}

impl Keys {
    fn new(owner: &str, item: &DeliveryItem<'_>) -> Result<Self> {
        item.position.validate()?;
        ensure!(
            owner.len() == 64
                && owner
                    .bytes()
                    .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f')),
            "invalid destination cursor owner"
        );
        let stream = item.position.stream();
        ensure!(
            stream.len() <= 128
                && stream
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-'),
            "invalid delivery stream key"
        );
        Ok(Self {
            filter: format!("owner='{owner}' AND stream_key='{stream}'"),
            stream: stream.to_owned(),
            sequence: item.position.sequence(),
            digest: hex(&item.content_digest),
            operations: i64::try_from(item.position.operations())?,
            operation: i64::try_from(item.position.operation())?,
        })
    }
}

impl Operation<'_> {
    async fn query(
        &self,
        connection: &mut Connection,
        sql: &str,
    ) -> Result<Vec<Vec<Option<Vec<u8>>>>> {
        Ok(bounded(self.command_timeout, connection.query(sql))
            .await?
            .values)
    }

    async fn execute(&self, connection: &mut Connection, sql: &str) -> Result<()> {
        bounded(self.command_timeout, connection.execute(sql)).await
    }

    async fn read(&self, connection: &mut Connection, keys: &Keys) -> Result<Option<Cursor>> {
        let rows = self
            .query(
                connection,
                &format!(
                    "SELECT version, sequence, digest, operations, completed FROM {} WHERE {} FOR UPDATE",
                    self.table.quoted(),
                    keys.filter
                ),
            )
            .await?;
        match rows.as_slice() {
            [] => Ok(None),
            [row] => Ok(Some(Cursor::read(row)?)),
            _ => anyhow::bail!("ambiguous destination delivery progress"),
        }
    }

    /// Runs one operation in its own transaction. `admission` reports
    /// whether the admission lock was taken, so the caller releases it after
    /// COMMIT or ROLLBACK. Returns whether the effect kept the transaction.
    pub async fn run(
        &self,
        connection: &mut Connection,
        item: &DeliveryItem<'_>,
        effect: Option<&Effect>,
        admission: &mut Option<String>,
    ) -> std::result::Result<bool, Failure> {
        let keys = Keys::new(self.owner, item).map_err(Failure::NotStarted)?;
        self.execute(connection, "START TRANSACTION")
            .await
            .map_err(Failure::NotStarted)?;
        let kept = self
            .apply(connection, &keys, effect, admission)
            .await
            .map_err(Failure::InTransaction)?;
        self.execute(connection, "COMMIT")
            .await
            .map_err(|error| match server_error(&error) {
                Some(_) if !breaks_session(&error) => Failure::InTransaction(error),
                _ => Failure::Commit(error),
            })?;
        Ok(kept)
    }

    async fn apply(
        &self,
        connection: &mut Connection,
        keys: &Keys,
        effect: Option<&Effect>,
        admission: &mut Option<String>,
    ) -> Result<bool> {
        let table = self.table.quoted();
        let mut row = self.read(connection, keys).await?;
        if row.is_none() {
            // Serialize only this owner's first-stream admission; existing
            // streams use their own row lock.
            let name = self.table.lock_name(self.owner);
            let wait = (self.command_timeout.as_secs() / 2).max(1);
            let acquired = self
                .query(connection, &format!("SELECT GET_LOCK('{name}', {wait})"))
                .await?;
            match acquired.as_slice() {
                [row] if row.first().and_then(Option::as_deref) == Some(b"1") => {
                    *admission = Some(name)
                }
                [row] if row.first().and_then(Option::as_deref) == Some(b"0") => {
                    return Err(MySqlDeliveryError::AdmissionBusy.into())
                }
                _ => anyhow::bail!("MySQL GET_LOCK failed for delivery stream admission"),
            }
            row = self.read(connection, keys).await?;
        }
        let completed = match row {
            Some(previous) => {
                ensure!(
                    keys.sequence >= previous.sequence,
                    MySqlDeliveryError::Receipt("destination delivery receipt expired")
                );
                if keys.sequence == previous.sequence {
                    ensure!(
                        previous.digest == hex_decode(&keys.digest)
                            && previous.operations == keys.operations,
                        MySqlDeliveryError::Receipt(
                            "destination delivery payload conflicts with its receipt"
                        )
                    );
                    if keys.operation < previous.completed {
                        // Already committed with its effect: nothing to repeat.
                        return Ok(true);
                    }
                    previous.completed
                } else {
                    ensure!(
                        previous.completed == previous.operations && keys.operation == 0,
                        MySqlDeliveryError::Receipt(
                            "destination has unfinished operations or a missing batch prefix"
                        )
                    );
                    self.execute(
                        connection,
                        &format!(
                            "UPDATE {table} SET sequence='{}', digest=X'{}', operations={}, completed=0 WHERE {}",
                            keys.sequence, keys.digest, keys.operations, keys.filter
                        ),
                    )
                    .await?;
                    0
                }
            }
            None => {
                ensure!(
                    keys.operation == 0,
                    MySqlDeliveryError::Receipt(
                        "destination delivery is missing its first operation"
                    )
                );
                let count = self
                    .query(
                        connection,
                        &format!("SELECT COUNT(*) FROM {table} WHERE owner='{}'", self.owner),
                    )
                    .await?;
                let [row] = count.as_slice() else {
                    anyhow::bail!("invalid destination stream count");
                };
                let count: u64 = field(row, 0)?.parse()?;
                ensure!(
                    count < u64::from(self.max_streams),
                    MySqlDeliveryError::StreamCapacity(self.max_streams)
                );
                self.execute(
                    connection,
                    &format!(
                        "INSERT INTO {table} (owner, stream_key, version, sequence, digest, operations, completed) \
                         VALUES ('{}', '{}', {PROGRESS_FORMAT_VERSION}, '{}', X'{}', {}, 0)",
                        self.owner, keys.stream, keys.sequence, keys.digest, keys.operations
                    ),
                )
                .await?;
                0
            }
        };
        ensure!(
            keys.operation == completed,
            MySqlDeliveryError::Receipt("destination delivery has an operation gap")
        );
        let mut kept = true;
        if let Some(effect) = effect {
            kept = bounded(
                self.command_timeout,
                connection.execute_parameters_in_transaction(&effect.sql, &effect.parameters),
            )
            .await?;
            if !kept {
                log::warn!(
                    "MySQL procedure ended the delivery transaction (COMMIT, ROLLBACK or an implicit \
                     commit); this operation is at-least-once, not exactly-once"
                );
            }
        }
        self.execute(
            connection,
            &format!(
                "UPDATE {table} SET completed=completed+1 WHERE {} AND completed={completed}",
                keys.filter
            ),
        )
        .await?;
        let changed = self.query(connection, "SELECT ROW_COUNT()").await?;
        ensure!(
            matches!(changed.as_slice(), [row] if field(row, 0)? == "1"),
            MySqlDeliveryError::Receipt("destination delivery cursor changed during its effect")
        );
        Ok(kept)
    }
}

fn hex_decode(value: &str) -> Vec<u8> {
    (0..value.len())
        .step_by(2)
        .filter_map(|index| u8::from_str_radix(&value[index..index + 2], 16).ok())
        .collect()
}

/// Creates (when absent) and verifies the cursor table. Retry-safe: the
/// creation is `IF NOT EXISTS` and verification reads only the catalog.
pub async fn ensure_table(
    connection: &mut Connection,
    table: &ProgressTable,
    timeout: Duration,
) -> Result<()> {
    let probe = format!(
        "SELECT owner, stream_key, version, sequence, digest, operations, completed FROM {} LIMIT 0",
        table.quoted()
    );
    match bounded(timeout, connection.query(&probe)).await {
        Ok(_) => {}
        Err(error) if server_error(&error).is_some_and(|error| error.code == 1146) => {
            if let Err(error) =
                bounded(timeout, connection.execute(&table.create_statement())).await
            {
                if server_error(&error).is_some_and(|error| matches!(error.code, 1142 | 1044)) {
                    return Err(error.context(format!(
                        "the MySQL user may not create the delivery progress table {}; \
                         grant CREATE on the database or pre-create the table (see CONFIGURATION.md)",
                        table.quoted()
                    )));
                }
                return Err(error.context("creating the delivery progress table"));
            }
        }
        Err(error)
            if server_error(&error)
                .is_some_and(|error| matches!(error.code, 1142 | 1143 | 1044)) =>
        {
            return Err(error.context(format!(
                "the MySQL user lacks SELECT, INSERT, UPDATE and DELETE on the delivery progress table {}",
                table.quoted()
            )));
        }
        Err(error) if server_error(&error).is_some_and(|error| error.code == 1054) => {
            return Err(error.context(format!(
                "{} exists but is not a format version 2 delivery progress table",
                table.quoted()
            )));
        }
        Err(error) => return Err(error.context("reading the delivery progress table")),
    }
    let literal = |value: &str| format!("'{value}'");
    let engine = bounded(
        timeout,
        connection.query(&format!(
            "SELECT ENGINE FROM information_schema.TABLES WHERE TABLE_SCHEMA={} AND TABLE_NAME={}",
            literal(table.database()),
            literal(table.table())
        )),
    )
    .await?;
    ensure!(
        matches!(engine.values.as_slice(), [row] if field(row, 0).is_ok_and(|engine| engine.eq_ignore_ascii_case("InnoDB"))),
        "{} must be an InnoDB table so the cursor commits with the procedure effect",
        table.quoted()
    );
    let columns = bounded(
        timeout,
        connection.query(&format!(
            "SELECT COLUMN_NAME, DATA_TYPE FROM information_schema.COLUMNS \
             WHERE TABLE_SCHEMA={} AND TABLE_NAME={} ORDER BY ORDINAL_POSITION",
            literal(table.database()),
            literal(table.table())
        )),
    )
    .await?;
    let actual = columns
        .values
        .iter()
        .map(|row| {
            Ok((
                field(row, 0)?.to_ascii_lowercase(),
                field(row, 1)?.to_ascii_lowercase(),
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    let expected = [
        ("owner", "char"),
        ("stream_key", "varchar"),
        ("version", "smallint"),
        ("sequence", "varchar"),
        ("digest", "binary"),
        ("operations", "bigint"),
        ("completed", "bigint"),
    ];
    ensure!(
        actual.len() == expected.len()
            && actual.iter().zip(expected).all(
                |((name, kind), (expected_name, expected_kind))| name == expected_name
                    && kind == expected_kind
            ),
        "{} does not have the format version 2 delivery progress columns",
        table.quoted()
    );
    let key = bounded(
        timeout,
        connection.query(&format!(
            "SELECT COLUMN_NAME FROM information_schema.KEY_COLUMN_USAGE \
             WHERE TABLE_SCHEMA={} AND TABLE_NAME={} AND CONSTRAINT_NAME='PRIMARY' \
             ORDER BY ORDINAL_POSITION",
            literal(table.database()),
            literal(table.table())
        )),
    )
    .await?;
    let key = key
        .values
        .iter()
        .map(|row| Ok(field(row, 0)?.to_ascii_lowercase()))
        .collect::<Result<Vec<_>>>()?;
    ensure!(
        key == ["owner", "stream_key"],
        "{} must have PRIMARY KEY (owner, stream_key)",
        table.quoted()
    );
    Ok(())
}

/// Deletes only `owner`'s rows. Idempotent; an absent table has nothing to
/// erase.
pub async fn erase(
    connection: &mut Connection,
    table: &ProgressTable,
    owner: &str,
    timeout: Duration,
) -> Result<()> {
    ensure!(
        owner.len() == 64
            && owner
                .bytes()
                .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f')),
        "invalid destination cursor owner"
    );
    match bounded(
        timeout,
        connection.execute(&format!(
            "DELETE FROM {} WHERE owner='{owner}'",
            table.quoted()
        )),
    )
    .await
    {
        Err(error) if server_error(&error).is_some_and(|error| error.code == 1146) => Ok(()),
        result => result,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owner_is_domain_separated_lowercase_hex() {
        let owner = owner_id("graph", "component");
        assert_eq!(owner.len(), 64);
        assert!(owner
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f')));
        let mut expected = Sha256::new();
        expected.update(b"drasi.storedproc/owner/v1\0graph\0component");
        assert_eq!(owner, hex(&expected.finalize()));
        assert_ne!(owner, owner_id("graphc", "omponent"));
        assert_ne!(owner, owner_id("component", "graph"));
        assert_eq!(owner, owner_id("graph", "component"));
    }

    #[test]
    fn table_names_are_validated_and_quoted() -> Result<()> {
        let table = ProgressTable::new("app", "drasi_delivery_progress")?;
        assert_eq!(table.quoted(), "`app`.`drasi_delivery_progress`");
        assert!(table
            .create_statement()
            .contains("PRIMARY KEY (owner, stream_key)"));
        assert!(table.create_statement().contains("ENGINE=InnoDB"));
        assert!(table.lock_name(&owner_id("g", "c")).len() <= 64);
        for bad in ["", "a b", "a`b", "a.b", "x;DROP", &"x".repeat(65)] {
            assert!(ProgressTable::new("app", bad).is_err(), "{bad}");
            assert!(ProgressTable::new(bad, "t").is_err(), "{bad}");
        }
        Ok(())
    }

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
        assert!(retryable(&server(1213)));
        assert!(retryable(&server(1205)));
        assert!(retryable(&server(1213).context("wrapped")));
        for code in [1045, 1146, 1644, 1062, 1142] {
            assert!(!retryable(&server(code)), "{code}");
        }
        let io = || anyhow::Error::from(std::io::Error::other("reset"));
        assert!(retryable(&MySqlDeliveryError::ConnectionLost(io()).into()));
        assert!(retryable(&MySqlDeliveryError::CommitUnknown(io()).into()));
        assert!(retryable(&MySqlDeliveryError::AdmissionBusy.into()));
        for error in [
            MySqlDeliveryError::CommandTimeout(io()),
            MySqlDeliveryError::StreamCapacity(4),
            MySqlDeliveryError::Render(io()),
            MySqlDeliveryError::Receipt("gap"),
            MySqlDeliveryError::CleanupPending(io()),
        ] {
            assert!(!retryable(&error.into()));
        }
        assert!(!retryable(&io()));
        assert!(breaks_session(&io().context(SessionFault)));
        assert!(!breaks_session(&anyhow::Error::new(
            MySqlDeliveryError::Receipt("gap")
        )));
        assert!(breaks_session(&server(1927)));
        assert!(!breaks_session(&server(1213)));
    }

    #[test]
    fn cursor_rows_are_validated() -> Result<()> {
        let row =
            |version: &str, sequence: &str, digest: usize, operations: &str, completed: &str| {
                vec![
                    Some(version.as_bytes().to_vec()),
                    Some(sequence.as_bytes().to_vec()),
                    Some(vec![7; digest]),
                    Some(operations.as_bytes().to_vec()),
                    Some(completed.as_bytes().to_vec()),
                ]
            };
        let cursor = Cursor::read(&row("2", "17", 32, "3", "1"))?;
        assert_eq!(
            (cursor.sequence, cursor.operations, cursor.completed),
            (17, 3, 1)
        );
        for bad in [
            row("1", "17", 32, "3", "1"),
            row("2", "017", 32, "3", "1"),
            row("2", "0", 32, "3", "1"),
            row("2", "17", 31, "3", "1"),
            row("2", "17", 32, "0", "0"),
            row("2", "17", 32, "3", "4"),
            row("2", "17", 32, "3", "-1"),
        ] {
            assert!(Cursor::read(&bad).is_err());
        }
        assert_eq!(hex_decode(&hex(&[0, 1, 254, 255])), [0, 1, 254, 255]);
        Ok(())
    }
}

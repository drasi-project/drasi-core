// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Factory configuration: one flat camelCase object per component.
//!
//! The database, capture, limit and REST fields are shared by the four
//! factories. Each factory adds its mode fields at the same level: `stream`
//! for all of them, and `coordinatedSnapshot`, `maxJournalTransactions` and
//! `maxJournalBytes` for the two journal (replay) factories. Fields that do
//! not belong to the factory's mode are rejected.

use anyhow::{Context, Result};
use drasi_computation_contracts::{SourceTransactionLimits, StreamId};
use drasi_computation_network_common::config::{SecretString, SecuritySettings};
use drasi_computation_plugin_sdk::{ConfigDefinition, ConfigOption, ConfigType};
use drasi_sqlite_common::{
    native::{SqliteConfig, SqliteConnectionOptions, SqliteOutput, SqliteReplayConfig},
    TableKeyConfig,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    net::IpAddr,
    num::{NonZeroU64, NonZeroUsize},
    path::PathBuf,
    time::Duration,
};

pub const DEFAULT_MAX_TRANSACTION_CHANGES: usize = 10_000;
pub const DEFAULT_MAX_TRANSACTION_BYTES: usize = 16 * 1024 * 1024;
pub const DEFAULT_MAX_TRANSACTION_DURATION_MS: u64 = 30_000;
pub const DEFAULT_MAX_SQL_BYTES: usize = 1024 * 1024;
pub const DEFAULT_COMMAND_CAPACITY: usize = 64;
pub const DEFAULT_OUTPUT_CAPACITY: usize = 16;
pub const DEFAULT_SHUTDOWN_TIMEOUT_MS: u64 = 10_000;
pub const DEFAULT_REST_HOST: &str = "127.0.0.1";
pub const DEFAULT_REST_PORT: u16 = 8080;
pub const DEFAULT_MAX_REQUEST_BYTES: usize = 1024 * 1024;
pub const DEFAULT_REQUEST_TIMEOUT_MS: u64 = 30_000;
pub const DEFAULT_MAX_CONNECTIONS: usize = 128;
pub const DEFAULT_PAGE_SIZE: usize = 1_000;
pub const DEFAULT_MAX_JOURNAL_TRANSACTIONS: usize = 10_000;
pub const DEFAULT_MAX_JOURNAL_BYTES: usize = 256 * 1024 * 1024;
/// Upper bound on every channel capacity (Tokio's semaphore bound is larger).
pub const MAX_CAPACITY: usize = 1 << 20;
pub const MAX_CONNECTIONS: usize = 65_536;
pub const MAX_REQUEST_BYTES: usize = 64 * 1024 * 1024;

/// Which of the four factories a configuration belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// `drasi.sqlite/changes`: one graph-change envelope per committed
    /// transaction; no journal.
    Changes,
    /// `drasi.sqlite/transactions`: complete source transactions; no journal.
    Transactions,
    /// `drasi.sqlite/replay-transactions`: complete transactions recorded in a
    /// source-owned journal, replayed from the query's committed cursor.
    Replay,
    /// `drasi.sqlite/shared-transactions`: the journal shared by a fixed set
    /// of consumers.
    Shared,
}

impl Mode {
    /// Whether the mode owns journal tables in the database file.
    pub fn journal(self) -> bool {
        matches!(self, Self::Replay | Self::Shared)
    }

    pub fn output(self) -> SqliteOutput {
        match self {
            Self::Changes => SqliteOutput::Changes,
            _ => SqliteOutput::Transactions,
        }
    }
}

/// An explicit element key for a table.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TableKey {
    pub table: String,
    pub key_columns: Vec<String>,
}

fn default_max_transaction_changes() -> NonZeroUsize {
    NonZeroUsize::new(DEFAULT_MAX_TRANSACTION_CHANGES).expect("nonzero constant")
}
fn default_max_transaction_bytes() -> NonZeroUsize {
    NonZeroUsize::new(DEFAULT_MAX_TRANSACTION_BYTES).expect("nonzero constant")
}
fn default_max_transaction_duration_ms() -> NonZeroU64 {
    NonZeroU64::new(DEFAULT_MAX_TRANSACTION_DURATION_MS).expect("nonzero constant")
}
fn default_max_sql_bytes() -> NonZeroUsize {
    NonZeroUsize::new(DEFAULT_MAX_SQL_BYTES).expect("nonzero constant")
}
fn default_command_capacity() -> NonZeroUsize {
    NonZeroUsize::new(DEFAULT_COMMAND_CAPACITY).expect("nonzero constant")
}
fn default_output_capacity() -> NonZeroUsize {
    NonZeroUsize::new(DEFAULT_OUTPUT_CAPACITY).expect("nonzero constant")
}
fn default_shutdown_timeout_ms() -> NonZeroU64 {
    NonZeroU64::new(DEFAULT_SHUTDOWN_TIMEOUT_MS).expect("nonzero constant")
}
fn default_rest_host() -> String {
    DEFAULT_REST_HOST.into()
}
fn default_rest_port() -> u16 {
    DEFAULT_REST_PORT
}
fn default_max_request_bytes() -> usize {
    DEFAULT_MAX_REQUEST_BYTES
}
fn default_request_timeout_ms() -> NonZeroU64 {
    NonZeroU64::new(DEFAULT_REQUEST_TIMEOUT_MS).expect("nonzero constant")
}
fn default_max_connections() -> usize {
    DEFAULT_MAX_CONNECTIONS
}
fn default_page_size() -> NonZeroUsize {
    NonZeroUsize::new(DEFAULT_PAGE_SIZE).expect("nonzero constant")
}
fn default_max_journal_transactions() -> NonZeroUsize {
    NonZeroUsize::new(DEFAULT_MAX_JOURNAL_TRANSACTIONS).expect("nonzero constant")
}
fn default_max_journal_bytes() -> NonZeroUsize {
    NonZeroUsize::new(DEFAULT_MAX_JOURNAL_BYTES).expect("nonzero constant")
}

/// The fields every factory shares, as they appear in the configuration.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Settings {
    /// The database file. Absent opens a private in-memory
    /// database, which requires `initSql` or `sqlApi`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    /// Tables to capture; empty captures every ordinary table.
    #[serde(default)]
    pub tables: Vec<String>,
    /// Explicit element keys; otherwise the primary key or rowid.
    #[serde(default)]
    pub table_keys: Vec<TableKey>,
    /// Row changes per transaction and rows per read.
    #[serde(default = "default_max_transaction_changes")]
    pub max_transaction_changes: NonZeroUsize,
    /// Encoded bytes per transaction and per read result.
    #[serde(default = "default_max_transaction_bytes")]
    pub max_transaction_bytes: NonZeroUsize,
    /// Deadline per transaction or read; also the busy timeout.
    #[serde(default = "default_max_transaction_duration_ms")]
    pub max_transaction_duration_ms: NonZeroU64,
    /// SQL text plus parameters per statement, and the `initSql` size.
    #[serde(default = "default_max_sql_bytes")]
    pub max_sql_bytes: NonZeroUsize,
    /// Commands queued for the database worker.
    #[serde(default = "default_command_capacity")]
    pub command_capacity: NonZeroUsize,
    /// Committed envelopes buffered for the host.
    #[serde(default = "default_output_capacity")]
    pub output_capacity: NonZeroUsize,
    /// Pause, Stop and erase bound when the host gives no deadline.
    #[serde(default = "default_shutdown_timeout_ms")]
    pub shutdown_timeout_ms: NonZeroU64,
    /// `journal_mode=WAL` for a file database without a journal.
    #[serde(default)]
    pub wal_mode: bool,
    /// Script run on every connection before the authorizer applies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub init_sql: Option<String>,
    /// Serve the REST API.
    #[serde(default)]
    pub rest_api: bool,
    /// Literal IP address the listener binds.
    #[serde(default = "default_rest_host")]
    pub rest_host: String,
    /// Listener port; 0 picks a free port.
    #[serde(default = "default_rest_port")]
    pub rest_port: u16,
    /// Bearer token for every route except `/health` (secret).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_token: Option<SecretString>,
    /// A second accepted token, for rotation (secret).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_token_secondary: Option<SecretString>,
    /// Permit an unauthenticated non-loopback listener.
    #[serde(default)]
    pub allow_unauthenticated: bool,
    /// TLS certificate chain (PEM).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_certificate_pem: Option<String>,
    /// TLS private key (PEM, secret).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_private_key_pem: Option<SecretString>,
    /// CA for client certificates (mutual TLS).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_client_ca_pem: Option<String>,
    /// Largest request body (413 above it).
    #[serde(default = "default_max_request_bytes")]
    pub max_request_bytes: usize,
    /// Deadline per request (504 when it expires).
    #[serde(default = "default_request_timeout_ms")]
    pub request_timeout_ms: NonZeroU64,
    /// Simultaneously open connections.
    #[serde(default = "default_max_connections")]
    pub max_connections: usize,
    /// Largest page of `GET /api/tables/{table}`.
    #[serde(default = "default_page_size")]
    pub page_size: NonZeroUsize,
    /// Enable the authenticated `POST /api/sql` route.
    #[serde(default)]
    pub sql_api: bool,
}

impl std::fmt::Debug for Settings {
    /// Secrets print as `<redacted>` through [`SecretString`]; `initSql` is
    /// summarised by length because it may embed literal data.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Settings")
            .field("path", &self.path)
            .field("tables", &self.tables)
            .field("table_keys", &self.table_keys)
            .field("max_transaction_changes", &self.max_transaction_changes)
            .field("max_transaction_bytes", &self.max_transaction_bytes)
            .field(
                "max_transaction_duration_ms",
                &self.max_transaction_duration_ms,
            )
            .field("max_sql_bytes", &self.max_sql_bytes)
            .field("command_capacity", &self.command_capacity)
            .field("output_capacity", &self.output_capacity)
            .field("shutdown_timeout_ms", &self.shutdown_timeout_ms)
            .field("wal_mode", &self.wal_mode)
            .field("init_sql_bytes", &self.init_sql.as_ref().map(String::len))
            .field("rest_api", &self.rest_api)
            .field("rest_host", &self.rest_host)
            .field("rest_port", &self.rest_port)
            .field("auth_token", &self.auth_token)
            .field("auth_token_secondary", &self.auth_token_secondary)
            .field("allow_unauthenticated", &self.allow_unauthenticated)
            .field("tls", &self.tls_certificate_pem.is_some())
            .field("tls_private_key_pem", &self.tls_private_key_pem)
            .field("max_request_bytes", &self.max_request_bytes)
            .field("request_timeout_ms", &self.request_timeout_ms)
            .field("max_connections", &self.max_connections)
            .field("page_size", &self.page_size)
            .field("sql_api", &self.sql_api)
            .finish()
    }
}

/// A parsed factory configuration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceConfig {
    pub mode: Mode,
    /// Output stream identifier.
    pub stream: String,
    /// Journal factories: initialize from the paired provider's snapshot.
    pub coordinated_snapshot: bool,
    /// Journal factories: retained transactions.
    pub max_journal_transactions: NonZeroUsize,
    /// Journal factories: retained bytes.
    pub max_journal_bytes: NonZeroUsize,
    /// The fields every factory shares.
    pub settings: Settings,
}

const STREAM: &str = "stream";
const COORDINATED: &str = "coordinatedSnapshot";
const JOURNAL_TRANSACTIONS: &str = "maxJournalTransactions";
const JOURNAL_BYTES: &str = "maxJournalBytes";

impl SourceConfig {
    /// Parses and statically validates `value` for `mode`. Unknown fields,
    /// including another mode's fields, are rejected. No I/O.
    pub fn from_value(mode: Mode, value: &Value) -> Result<Self> {
        let mut object = value
            .as_object()
            .context("SQLite source configuration must be an object")?
            .clone();
        // An explicit null path means "in memory", like an absent one.
        if object.get("path").is_some_and(Value::is_null) {
            object.remove("path");
        }
        let stream = match object.remove(STREAM) {
            Some(Value::String(stream)) => stream,
            Some(_) => anyhow::bail!("stream must be a string"),
            None => anyhow::bail!("stream is required"),
        };
        let journal_only =
            |name: &str| format!("{name} is only supported by the journal (replay) factories");
        let coordinated_snapshot = match object.remove(COORDINATED) {
            None => false,
            Some(Value::Bool(value)) if mode.journal() => value,
            Some(Value::Bool(_)) => anyhow::bail!(journal_only(COORDINATED)),
            Some(_) => anyhow::bail!("{COORDINATED} must be a boolean"),
        };
        let mut bound = |name: &str, default: NonZeroUsize| -> Result<NonZeroUsize> {
            match object.remove(name) {
                None => Ok(default),
                Some(_) if !mode.journal() => anyhow::bail!(journal_only(name)),
                Some(value) => serde_json::from_value(value)
                    .with_context(|| format!("{name} must be a positive integer")),
            }
        };
        let max_journal_transactions =
            bound(JOURNAL_TRANSACTIONS, default_max_journal_transactions())?;
        let max_journal_bytes = bound(JOURNAL_BYTES, default_max_journal_bytes())?;
        let settings: Settings =
            serde_json::from_value(Value::Object(object)).context("invalid SQLite settings")?;
        let config = Self {
            mode,
            stream,
            coordinated_snapshot,
            max_journal_transactions,
            max_journal_bytes,
            settings,
        };
        config.validate()?;
        // A loaded plugin has no Rust handle, so an in-memory database is
        // usable only when the source can create its own tables.
        anyhow::ensure!(
            config.settings.path.is_some()
                || config.settings.init_sql.is_some()
                || config.settings.sql_api,
            "an in-memory SQLite source (no path) needs initSql or sqlApi to create its tables; \
             set path to use a database file"
        );
        Ok(config)
    }

    /// The configuration of a source constructed in Rust from an engine
    /// configuration (no factory). REST is off; [`crate::SqliteSource::with_rest_api`]
    /// enables it. Unlike [`Self::from_value`], an in-memory database without
    /// `initSql` is allowed: a Rust caller creates tables through
    /// [`crate::SqliteSource::handle`].
    pub fn from_engine(stream: &StreamId, engine: &SqliteConfig, shared: bool) -> Result<Self> {
        let mode = match (engine.output, &engine.replay, shared) {
            (SqliteOutput::Changes, None, false) => Mode::Changes,
            (SqliteOutput::Transactions, None, false) => Mode::Transactions,
            (SqliteOutput::Transactions, Some(_), false) => Mode::Replay,
            (SqliteOutput::Transactions, Some(_), true) => Mode::Shared,
            _ => anyhow::bail!("SQLite output, replay and consumer settings do not name a mode"),
        };
        let mut settings: Settings = serde_json::from_value(json!({}))?;
        settings.path = engine.path.clone();
        settings.tables = engine.tables.clone();
        settings.table_keys = engine
            .table_keys
            .iter()
            .map(|key| TableKey {
                table: key.table.clone(),
                key_columns: key.key_columns.clone(),
            })
            .collect();
        settings.max_transaction_changes = engine.transactions.max_changes;
        settings.max_transaction_bytes = engine.transactions.max_bytes;
        settings.max_transaction_duration_ms = engine.transactions.max_duration_ms;
        settings.max_sql_bytes = engine.max_sql_bytes;
        settings.command_capacity = engine.command_capacity;
        settings.output_capacity = engine.output_capacity;
        settings.shutdown_timeout_ms = engine.shutdown_timeout_ms;
        settings.page_size = settings.page_size.min(engine.transactions.max_changes);
        let config = Self {
            mode,
            stream: stream.as_str().to_owned(),
            coordinated_snapshot: false,
            max_journal_transactions: engine
                .replay
                .as_ref()
                .map_or_else(default_max_journal_transactions, |replay| {
                    replay.max_transactions
                }),
            max_journal_bytes: engine
                .replay
                .as_ref()
                .map_or_else(default_max_journal_bytes, |replay| replay.max_bytes),
            settings,
        };
        config.validate()?;
        Ok(config)
    }

    /// The configuration as the factory accepts it, with defaults filled.
    pub fn to_value(&self) -> Result<Value> {
        let Value::Object(mut object) = serde_json::to_value(&self.settings)? else {
            anyhow::bail!("SQLite settings did not serialize as an object");
        };
        object.insert(STREAM.into(), Value::String(self.stream.clone()));
        if self.mode.journal() {
            object.insert(COORDINATED.into(), Value::Bool(self.coordinated_snapshot));
            object.insert(
                JOURNAL_TRANSACTIONS.into(),
                json!(self.max_journal_transactions.get()),
            );
            object.insert(JOURNAL_BYTES.into(), json!(self.max_journal_bytes.get()));
        }
        Ok(Value::Object(object))
    }

    /// Static validation: no I/O and no owner identity.
    pub fn validate(&self) -> Result<()> {
        StreamId::try_new(self.stream.clone())?;
        let settings = &self.settings;
        let sqlite = self.sqlite()?;
        sqlite.validate()?;
        if let Some(path) = &settings.path {
            anyhow::ensure!(
                !path.as_os_str().is_empty(),
                "path must not be empty; omit it for an in-memory database"
            );
        } else {
            anyhow::ensure!(
                !self.mode.journal(),
                "the journal (replay) factories require a database file path"
            );
        }
        anyhow::ensure!(
            settings.command_capacity.get() >= 2
                && settings.command_capacity.get() <= MAX_CAPACITY
                && settings.output_capacity.get() <= MAX_CAPACITY,
            "commandCapacity must be in 2..=1048576 and outputCapacity in 1..=1048576"
        );
        anyhow::ensure!(
            !settings.wal_mode || (settings.path.is_some() && !self.mode.journal()),
            "walMode requires a database file and a factory without a journal \
             (journal factories use journal_mode=DELETE with an exclusive lock)"
        );
        self.connection().validate(&sqlite)?;
        anyhow::ensure!(
            !self.coordinated_snapshot || self.mode.journal(),
            "coordinatedSnapshot requires a journal (replay) factory"
        );
        // The REST fields are validated whether or not the listener is enabled,
        // so enabling it later is a restart, never a surprise rejection.
        let security = self.security()?;
        let ip: IpAddr = settings
            .rest_host
            .parse()
            .map_err(|_| anyhow::anyhow!("restHost must be a literal IP address"))?;
        anyhow::ensure!(
            ip.is_loopback() || !security.auth_tokens.is_empty() || settings.allow_unauthenticated,
            "restHost {} is not a loopback address: set authToken, or allowUnauthenticated: true behind an authenticating proxy",
            settings.rest_host
        );
        anyhow::ensure!(
            (1..=MAX_REQUEST_BYTES).contains(&settings.max_request_bytes),
            "maxRequestBytes must be in 1..=67108864"
        );
        anyhow::ensure!(
            (1..=MAX_CONNECTIONS).contains(&settings.max_connections),
            "maxConnections must be in 1..=65536"
        );
        anyhow::ensure!(
            settings.page_size <= settings.max_transaction_changes,
            "pageSize must not exceed maxTransactionChanges (the read row bound)"
        );
        tokio::time::Instant::now()
            .checked_add(self.request_timeout())
            .context("requestTimeoutMs is not representable")?;
        if settings.sql_api {
            anyhow::ensure!(settings.rest_api, "sqlApi requires restApi: true");
            anyhow::ensure!(
                !security.auth_tokens.is_empty(),
                "sqlApi requires authToken: /api/sql is never unauthenticated"
            );
        }
        Ok(())
    }

    /// Validated REST security: tokens and TLS material.
    pub fn security(&self) -> Result<drasi_computation_network_common::config::IngressSecurity> {
        let settings = &self.settings;
        SecuritySettings {
            auth_token: settings.auth_token.as_ref(),
            auth_token_secondary: settings.auth_token_secondary.as_ref(),
            allow_unauthenticated: settings.allow_unauthenticated,
            rate_limit_per_second: None,
            rate_limit_burst: None,
            tls_certificate_pem: settings.tls_certificate_pem.as_deref(),
            tls_private_key_pem: settings.tls_private_key_pem.as_ref(),
            tls_client_ca_pem: settings.tls_client_ca_pem.as_deref(),
        }
        .build()
    }

    pub fn request_timeout(&self) -> Duration {
        Duration::from_millis(self.settings.request_timeout_ms.get())
    }

    /// The engine configuration.
    pub fn sqlite(&self) -> Result<SqliteConfig> {
        let settings = &self.settings;
        Ok(SqliteConfig {
            path: settings.path.clone(),
            tables: settings.tables.clone(),
            table_keys: settings
                .table_keys
                .iter()
                .map(|key| TableKeyConfig {
                    table: key.table.clone(),
                    key_columns: key.key_columns.clone(),
                })
                .collect(),
            output: self.mode.output(),
            replay: self.mode.journal().then_some(SqliteReplayConfig {
                max_transactions: self.max_journal_transactions,
                max_bytes: self.max_journal_bytes,
            }),
            transactions: SourceTransactionLimits {
                max_changes: settings.max_transaction_changes,
                max_bytes: settings.max_transaction_bytes,
                max_duration_ms: settings.max_transaction_duration_ms,
            },
            max_sql_bytes: settings.max_sql_bytes,
            command_capacity: settings.command_capacity,
            output_capacity: settings.output_capacity,
            shutdown_timeout_ms: settings.shutdown_timeout_ms,
        })
    }

    /// Per-connection options (`initSql`, `walMode`).
    pub fn connection(&self) -> SqliteConnectionOptions {
        SqliteConnectionOptions {
            init_sql: self.settings.init_sql.clone(),
            wal: self.settings.wal_mode,
        }
    }
}

fn integer(name: &str, description: &str, default: u64, minimum: i64) -> ConfigOption {
    ConfigOption::new(name, ConfigType::Integer, false, description)
        .default(default)
        .minimum(minimum)
}

fn string(name: &str, description: &str) -> ConfigOption {
    ConfigOption::new(name, ConfigType::String, false, description)
}

fn boolean(name: &str, description: &str) -> ConfigOption {
    ConfigOption::new(name, ConfigType::Boolean, false, description).default(false)
}

/// The published configuration options for `mode`.
pub fn options(mode: Mode) -> Vec<ConfigOption> {
    let mut options = vec![
        ConfigOption::new(STREAM, ConfigType::String, true, "Output stream identifier carried by every emitted change."),
        string("path", "SQLite database file. The source opens it read-write and creates it if missing. Omit for a private in-memory database, which then needs initSql or sqlApi. Required by the journal factories.")
            .default_description("in-memory database (requires initSql or sqlApi)"),
        ConfigOption::new("tables", ConfigType::Array, false, "Tables to capture and expose through REST. Empty captures every ordinary table except sqlite_* and __drasi_* tables.")
            .default(json!([])),
        ConfigOption::new("tableKeys", ConfigType::Array, false, "Explicit element keys, as {\"table\": \"name\", \"keyColumns\": [..]}, for tables whose primary key or rowid should not identify rows.")
            .default(json!([])),
        integer("maxTransactionChanges", "Largest number of row changes in one transaction, and the largest number of rows one read returns. A larger write fails and rolls back.", DEFAULT_MAX_TRANSACTION_CHANGES as u64, 1),
        integer("maxTransactionBytes", "Largest encoded size of one transaction and of one read result, in bytes. Also bounds SQLite's maximum value length.", DEFAULT_MAX_TRANSACTION_BYTES as u64, 1)
            .maximum(i32::MAX as u64),
        integer("maxTransactionDurationMs", "Deadline for one transaction or read, and SQLite's busy timeout.", DEFAULT_MAX_TRANSACTION_DURATION_MS, 1)
            .maximum(i32::MAX as u64),
        integer("maxSqlBytes", "Largest SQL text plus parameters of one statement, and the largest initSql script, in bytes.", DEFAULT_MAX_SQL_BYTES as u64, 1)
            .maximum(i32::MAX as u64),
        integer("commandCapacity", "Commands (REST requests, handle calls) queued for the database worker.", DEFAULT_COMMAND_CAPACITY as u64, 2)
            .maximum(MAX_CAPACITY as u64),
        integer("outputCapacity", "Committed envelopes buffered before writes wait for the host to take output.", DEFAULT_OUTPUT_CAPACITY as u64, 1)
            .maximum(MAX_CAPACITY as u64),
        integer("shutdownTimeoutMs", "Fallback bound for Pause, Stop and erase when the host supplies no deadline.", DEFAULT_SHUTDOWN_TIMEOUT_MS, 1),
        boolean("walMode", "Set journal_mode=WAL on a file database without a journal, so other processes can read while the source writes. Rejected by the journal factories."),
        string("initSql", "SQL script run on every connection the source opens, before its authorizer applies: create tables, indexes, views or triggers. It may not touch __drasi_* objects, attach databases or load extensions, and its changes are not captured. Make it idempotent (CREATE TABLE IF NOT EXISTS) for file databases."),
        boolean("restApi", "Serve the REST API (table CRUD, batch and, with sqlApi, /api/sql)."),
        string("restHost", "Literal IP address the REST listener binds. A non-loopback address requires authToken, or allowUnauthenticated: true.")
            .default(DEFAULT_REST_HOST),
        ConfigOption::new("restPort", ConfigType::Integer, false, "REST listener port; 0 picks a free port.")
            .default(DEFAULT_REST_PORT)
            .minimum(0)
            .maximum(u16::MAX as u64),
        string("authToken", "Bearer token required on every REST route except /health (Authorization: Bearer <token>). 16 to 4096 visible ASCII characters.").secret(),
        string("authTokenSecondary", "A second accepted bearer token, for rotation without downtime. Requires authToken.").secret(),
        boolean("allowUnauthenticated", "Permit an unauthenticated REST listener on a non-loopback address, for deployments behind an authenticating proxy."),
        string("tlsCertificatePem", "Serve REST over TLS with this PEM certificate chain. Requires tlsPrivateKeyPem."),
        string("tlsPrivateKeyPem", "PEM private key for tlsCertificatePem.").secret(),
        string("tlsClientCaPem", "Require client certificates issued by this PEM CA (mutual TLS)."),
        integer("maxRequestBytes", "Largest REST request body, in bytes; larger bodies get 413.", DEFAULT_MAX_REQUEST_BYTES as u64, 1)
            .maximum(MAX_REQUEST_BYTES as u64),
        integer("requestTimeoutMs", "Deadline for one REST request; it gets 504 when it expires. A write whose commit was already submitted may still complete.", DEFAULT_REQUEST_TIMEOUT_MS, 1),
        integer("maxConnections", "Simultaneously open REST connections; further connections are closed.", DEFAULT_MAX_CONNECTIONS as u64, 1)
            .maximum(MAX_CONNECTIONS as u64),
        integer("pageSize", "Largest page GET /api/tables/{table} returns (?limit=&offset=); X-Next-Offset names the next page. At most maxTransactionChanges.", DEFAULT_PAGE_SIZE as u64, 1),
        boolean("sqlApi", "Enable POST /api/sql, which runs parameterized SQL through the source's connection and authorizer. Requires restApi and authToken."),
    ];
    if mode.journal() {
        options.push(boolean(COORDINATED, "Initialize each query from a snapshot taken by the paired drasi.sqlite coordinated (or shared) bootstrap provider, before live writes are admitted. Required when the tables already contain rows."));
        options.push(integer(JOURNAL_TRANSACTIONS, "Largest number of committed transactions retained in the journal until every consumer has handled them. Writes fail while it is full.", DEFAULT_MAX_JOURNAL_TRANSACTIONS as u64, 1));
        options.push(integer(
            JOURNAL_BYTES,
            "Largest total size of retained journal transactions, in bytes.",
            DEFAULT_MAX_JOURNAL_BYTES as u64,
            1,
        ));
    }
    options
}

/// The published configuration for `mode`.
pub fn definition(mode: Mode) -> ConfigDefinition {
    ConfigDefinition::try_new(options(mode)).expect("constant SQLite source configuration")
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [Mode; 4] = [
        Mode::Changes,
        Mode::Transactions,
        Mode::Replay,
        Mode::Shared,
    ];

    fn minimal() -> Value {
        json!({"stream": "sqlite/out", "path": "app.db", "tables": ["items"]})
    }

    #[test]
    fn definitions_construct_and_publish_every_field() {
        for mode in ALL {
            let definition = definition(mode);
            let config = SourceConfig::from_value(mode, &minimal()).unwrap();
            let value = config.to_value().unwrap();
            let schema = definition.schema();
            schema.validate(&value).unwrap();
            for key in value.as_object().unwrap().keys() {
                assert!(
                    schema.fields.contains_key(key),
                    "{mode:?} {key} unpublished"
                );
            }
            assert_eq!(
                definition.details().fields.len(),
                schema.fields.len(),
                "{mode:?}"
            );
        }
    }

    #[test]
    fn minimal_configuration_round_trips_with_published_defaults() {
        for mode in ALL {
            let config = SourceConfig::from_value(mode, &minimal()).unwrap();
            let value = config.to_value().unwrap();
            assert_eq!(SourceConfig::from_value(mode, &value).unwrap(), config);
            // Every published literal default equals the serde default.
            let details = definition(mode).details();
            for (name, field) in &details.fields {
                if minimal().get(name.as_str()).is_some() {
                    continue;
                }
                if let Some(default) = &field.default {
                    assert_eq!(&value[name.as_str()], default, "{mode:?} {name}");
                }
            }
            assert!(details
                .equivalent_with_defaults(&definition(mode).schema(), &minimal(), &value)
                .unwrap());
        }
    }

    #[test]
    fn foreign_unknown_and_invalid_fields_are_rejected() {
        let cases: Vec<(Mode, &str, Value)> = vec![
            (Mode::Changes, "settings", json!({})),
            (Mode::Changes, "rest_api", json!({})),
            (Mode::Changes, "coordinatedSnapshot", json!(true)),
            (Mode::Transactions, "maxJournalBytes", json!(1024)),
            (Mode::Replay, "maxJournalBytes", json!(0)),
            (Mode::Changes, "maxTransactionChanges", json!(0)),
            (Mode::Changes, "commandCapacity", json!(1)),
            (Mode::Changes, "restHost", json!("localhost")),
            (Mode::Changes, "restHost", json!("0.0.0.0")),
            (Mode::Changes, "authToken", json!("short")),
            (
                Mode::Changes,
                "authTokenSecondary",
                json!("0123456789abcdef"),
            ),
            (Mode::Changes, "sqlApi", json!(true)),
            (Mode::Changes, "pageSize", json!(20_000)),
            (Mode::Changes, "maxRequestBytes", json!(0)),
            (Mode::Changes, "initSql", json!("")),
            (Mode::Changes, "path", json!("")),
            (Mode::Replay, "walMode", json!(true)),
            (Mode::Changes, "tables", json!(["__drasi_native_state_v1"])),
            (Mode::Changes, "tlsCertificatePem", json!("not pem")),
        ];
        for (mode, field, value) in cases {
            let mut config = minimal();
            config[field] = value.clone();
            assert!(
                SourceConfig::from_value(mode, &config).is_err(),
                "{mode:?} accepted {field} = {value}"
            );
        }
        assert!(SourceConfig::from_value(Mode::Changes, &json!({"path": "a.db"})).is_err());
    }

    #[test]
    fn in_memory_requires_init_sql_or_sql_api_and_no_journal() {
        let memory = json!({"stream": "sqlite/out"});
        let error = SourceConfig::from_value(Mode::Changes, &memory).unwrap_err();
        assert!(
            format!("{error:#}").contains("initSql or sqlApi"),
            "{error:#}"
        );
        let mut init = memory.clone();
        init["initSql"] = json!("CREATE TABLE items(id INTEGER PRIMARY KEY)");
        SourceConfig::from_value(Mode::Changes, &init).unwrap();
        SourceConfig::from_value(Mode::Transactions, &init).unwrap();
        assert!(SourceConfig::from_value(Mode::Replay, &init).is_err());
        let mut sql = memory.clone();
        sql["sqlApi"] = json!(true);
        assert!(
            SourceConfig::from_value(Mode::Changes, &sql).is_err(),
            "needs REST and a token"
        );
        sql["restApi"] = json!(true);
        sql["authToken"] = json!("0123456789abcdef");
        SourceConfig::from_value(Mode::Changes, &sql).unwrap();
    }

    #[test]
    fn explicit_null_path_gets_the_in_memory_rule() {
        let error = SourceConfig::from_value(Mode::Changes, &json!({"stream": "s", "path": null}))
            .unwrap_err();
        assert!(
            format!("{error:#}").contains("initSql or sqlApi"),
            "{error:#}"
        );
        let config = SourceConfig::from_value(
            Mode::Changes,
            &json!({"stream": "s", "path": null, "initSql": "CREATE TABLE t(id)"}),
        )
        .unwrap();
        assert!(config.to_value().unwrap().get("path").is_none());
    }

    #[test]
    fn non_loopback_rest_requires_a_token_or_explicit_opt_in() {
        let mut config = minimal();
        config["restApi"] = json!(true);
        config["restHost"] = json!("0.0.0.0");
        assert!(SourceConfig::from_value(Mode::Changes, &config).is_err());
        config["allowUnauthenticated"] = json!(true);
        SourceConfig::from_value(Mode::Changes, &config).unwrap();
        config["allowUnauthenticated"] = json!(false);
        config["authToken"] = json!("0123456789abcdef");
        SourceConfig::from_value(Mode::Changes, &config).unwrap();
    }

    #[test]
    fn secrets_are_top_level_and_redacted() {
        let schema = definition(Mode::Replay).schema();
        let secrets: Vec<_> = schema
            .fields
            .iter()
            .filter(|(_, field)| field.secret)
            .map(|(name, _)| name.to_string())
            .collect();
        assert_eq!(
            secrets,
            ["authToken", "authTokenSecondary", "tlsPrivateKeyPem"]
        );
        let mut value = minimal();
        value["authToken"] = json!("0123456789abcdef-TOKENVALUE");
        value["initSql"] = json!("INSERT INTO t VALUES('INITVALUE')");
        let config = SourceConfig::from_value(Mode::Changes, &value).unwrap();
        let printed = format!("{config:?}");
        assert!(
            !printed.contains("TOKENVALUE") && !printed.contains("INITVALUE"),
            "{printed}"
        );
        // configuration() must return the actual value (the persistence hook).
        assert_eq!(
            config.to_value().unwrap()["authToken"],
            "0123456789abcdef-TOKENVALUE"
        );
    }

    #[test]
    fn engine_configuration_follows_the_mode() {
        let replay = SourceConfig::from_value(Mode::Replay, &minimal()).unwrap();
        let sqlite = replay.sqlite().unwrap();
        assert_eq!(sqlite.output, SqliteOutput::Transactions);
        assert_eq!(
            sqlite.replay.unwrap().max_transactions.get(),
            DEFAULT_MAX_JOURNAL_TRANSACTIONS
        );
        let changes = SourceConfig::from_value(Mode::Changes, &minimal()).unwrap();
        assert_eq!(changes.sqlite().unwrap().output, SqliteOutput::Changes);
        assert!(changes.sqlite().unwrap().replay.is_none());
    }
}

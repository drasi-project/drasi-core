// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! The flat native MySQL configuration shared by the native source factories
//! and the native MySQL bootstrap providers.
//!
//! The configuration is one object of camelCase fields. `password` and
//! `tlsClientKeyPem` are top-level fields so the host can resolve them from
//! secret references; nothing else is secret. Each factory adds its own mode
//! fields (`stream`, `retention`, `persistent`, `source`, `labels`) at the same
//! level and converts the shared part into the engine's [`MySqlConfig`] with
//! [`MySqlSourceSettings::engine_config`], so a source and its paired
//! bootstrap provider always compute the same binding.

use super::{
    MySqlConfig, MySqlConnectionConfig, MySqlOutput, MySqlRetention, MySqlStartPosition,
    MySqlTlsMode,
};
use anyhow::{Context, Result};
use drasi_computation_contracts::{ComponentId, SourceTransactionLimits};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fmt,
    num::{NonZeroU32, NonZeroU64, NonZeroUsize},
};

/// Default MySQL port.
pub const DEFAULT_PORT: u16 = 3306;
/// Default replication heartbeat period (the legacy source's 30 s).
pub const DEFAULT_HEARTBEAT_INTERVAL_MS: u64 = 30_000;
/// Default bound on one MySQL packet, binlog event or catalog result.
pub const DEFAULT_MAX_PROTOCOL_BYTES: usize = 16 * 1024 * 1024;
/// Default deadline for connecting and for each catalog or protocol exchange.
pub const DEFAULT_IO_TIMEOUT_MS: u64 = 30_000;
/// Default bound on the changes in one source transaction.
pub const DEFAULT_MAX_TRANSACTION_CHANGES: usize = 100_000;
/// Default bound on the encoded bytes of one source transaction.
pub const DEFAULT_MAX_TRANSACTION_BYTES: usize = 64 * 1024 * 1024;
/// Default deadline for assembling and publishing one source transaction.
pub const DEFAULT_MAX_TRANSACTION_DURATION_MS: u64 = 300_000;
/// Default total time allowed for reconnecting after a transport failure.
pub const DEFAULT_RECONNECT_BUDGET_MS: u64 = 300_000;
/// Upper bound on a configured reconnect budget (one day).
pub const MAX_RECONNECT_BUDGET_MS: u64 = 86_400_000;
/// Default `FLUSH TABLES WITH READ LOCK` wait of a coordinated snapshot.
pub const DEFAULT_SNAPSHOT_LOCK_TIMEOUT_MS: u64 = 10_000;
/// Largest accepted PEM bundle (CA, client certificate or client key).
pub const MAX_PEM_BYTES: usize = 64 * 1024;
/// Longest MySQL schema or table identifier, in characters.
pub const MAX_IDENTIFIER_CHARS: usize = 64;

/// Where streaming starts before the first committed checkpoint.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StartFrom {
    /// The current end of the binary log.
    #[default]
    End,
    /// The oldest retained binary log file.
    Beginning,
    /// After the GTID set in `startGtidSet`.
    Gtid,
    /// `startBinlogFile` at `startBinlogPosition`.
    Position,
    /// The boundary of a coordinated snapshot taken by the paired bootstrap
    /// provider (`FLUSH TABLES WITH READ LOCK` while the view is established).
    Snapshot,
}

/// An explicit key for a table without a usable primary key.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MySqlTableKey {
    /// Table name in the configured database.
    pub table: String,
    /// Key columns, in key order. Positional factories use `@N` offsets.
    pub key_columns: Vec<String>,
}

/// The shared, flat native MySQL configuration.
#[derive(Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MySqlSourceSettings {
    #[serde(default = "default_host")]
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    pub database: String,
    pub user: String,
    /// Optional when the host supplies activation credentials.
    #[serde(default)]
    pub password: String,
    #[serde(default)]
    pub ssl_mode: MySqlTlsMode,
    /// Additional trusted CA certificates (PEM).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_ca_pem: Option<String>,
    /// Client certificate chain (PEM) for mutual TLS.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_client_cert_pem: Option<String>,
    /// PKCS#8 private key (PEM) of the client certificate. Secret.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_client_key_pem: Option<String>,
    /// Replica server ID. Derived from the component ID when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_id: Option<NonZeroU32>,
    #[serde(default = "default_heartbeat_interval_ms")]
    pub heartbeat_interval_ms: NonZeroU64,
    /// Tables to capture; empty captures every supported base table.
    #[serde(default)]
    pub tables: Vec<String>,
    #[serde(default)]
    pub table_keys: Vec<MySqlTableKey>,
    #[serde(default)]
    pub start_from: StartFrom,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_gtid_set: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_binlog_file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_binlog_position: Option<u32>,
    #[serde(default = "default_snapshot_lock_timeout_ms")]
    pub snapshot_lock_timeout_ms: NonZeroU64,
    #[serde(default = "default_max_transaction_changes")]
    pub max_transaction_changes: NonZeroUsize,
    #[serde(default = "default_max_transaction_bytes")]
    pub max_transaction_bytes: NonZeroUsize,
    #[serde(default = "default_max_transaction_duration_ms")]
    pub max_transaction_duration_ms: NonZeroU64,
    #[serde(default = "default_max_protocol_bytes")]
    pub max_protocol_bytes: NonZeroUsize,
    #[serde(default = "default_io_timeout_ms")]
    pub io_timeout_ms: NonZeroU64,
    /// Total time to keep reconnecting after a transport failure; `0`
    /// disables reconnection.
    #[serde(default = "default_reconnect_budget_ms")]
    pub reconnect_budget_ms: u64,
}

impl fmt::Debug for MySqlSourceSettings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MySqlSourceSettings")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("database", &self.database)
            .field("user", &self.user)
            .field("password", &"[REDACTED]")
            .field("ssl_mode", &self.ssl_mode)
            .field(
                "tls_client_key_pem",
                &self.tls_client_key_pem.as_ref().map(|_| "[REDACTED]"),
            )
            .field("server_id", &self.server_id)
            .field("tables", &self.tables)
            .field("start_from", &self.start_from)
            .finish_non_exhaustive()
    }
}

fn default_host() -> String {
    "localhost".into()
}
fn default_port() -> u16 {
    DEFAULT_PORT
}
fn nonzero64(value: u64) -> NonZeroU64 {
    NonZeroU64::new(value).unwrap_or(NonZeroU64::MIN)
}
fn nonzero(value: usize) -> NonZeroUsize {
    NonZeroUsize::new(value).unwrap_or(NonZeroUsize::MIN)
}
fn default_heartbeat_interval_ms() -> NonZeroU64 {
    nonzero64(DEFAULT_HEARTBEAT_INTERVAL_MS)
}
fn default_snapshot_lock_timeout_ms() -> NonZeroU64 {
    nonzero64(DEFAULT_SNAPSHOT_LOCK_TIMEOUT_MS)
}
fn default_max_transaction_changes() -> NonZeroUsize {
    nonzero(DEFAULT_MAX_TRANSACTION_CHANGES)
}
fn default_max_transaction_bytes() -> NonZeroUsize {
    nonzero(DEFAULT_MAX_TRANSACTION_BYTES)
}
fn default_max_transaction_duration_ms() -> NonZeroU64 {
    nonzero64(DEFAULT_MAX_TRANSACTION_DURATION_MS)
}
fn default_max_protocol_bytes() -> NonZeroUsize {
    nonzero(DEFAULT_MAX_PROTOCOL_BYTES)
}
fn default_io_timeout_ms() -> NonZeroU64 {
    nonzero64(DEFAULT_IO_TIMEOUT_MS)
}
fn default_reconnect_budget_ms() -> u64 {
    DEFAULT_RECONNECT_BUDGET_MS
}

/// Derives a stable replica server ID in `1..2^31` from a component ID, so
/// two sources never share one by default and a restart keeps its own.
pub fn derived_server_id(component: &ComponentId) -> NonZeroU32 {
    let mut hash = Sha256::new();
    hash.update(b"drasi.mysql/server-id/v1\0");
    hash.update(component.as_str().as_bytes());
    let digest = hash.finalize();
    let value = u32::from_be_bytes([digest[0], digest[1], digest[2], digest[3]]) & 0x7fff_fffe;
    NonZeroU32::new(value.saturating_add(1)).unwrap_or(NonZeroU32::MIN)
}

/// Rejects a MySQL identifier that is empty, too long, contains NUL or ends
/// with a space (MySQL strips trailing spaces from names).
pub fn validate_identifier(kind: &str, value: &str) -> Result<()> {
    anyhow::ensure!(
        !value.is_empty()
            && value.chars().count() <= MAX_IDENTIFIER_CHARS
            && !value.contains('\0')
            && !value.ends_with(' '),
        "MySQL {kind} name must be 1 to 64 characters without NUL or a trailing space"
    );
    Ok(())
}

impl MySqlSourceSettings {
    /// A configuration with every operational default.
    pub fn new(
        host: impl Into<String>,
        port: u16,
        database: impl Into<String>,
        user: impl Into<String>,
    ) -> Self {
        Self {
            host: host.into(),
            port,
            database: database.into(),
            user: user.into(),
            password: String::new(),
            ssl_mode: MySqlTlsMode::default(),
            tls_ca_pem: None,
            tls_client_cert_pem: None,
            tls_client_key_pem: None,
            server_id: None,
            heartbeat_interval_ms: default_heartbeat_interval_ms(),
            tables: Vec::new(),
            table_keys: Vec::new(),
            start_from: StartFrom::End,
            start_gtid_set: None,
            start_binlog_file: None,
            start_binlog_position: None,
            snapshot_lock_timeout_ms: default_snapshot_lock_timeout_ms(),
            max_transaction_changes: default_max_transaction_changes(),
            max_transaction_bytes: default_max_transaction_bytes(),
            max_transaction_duration_ms: default_max_transaction_duration_ms(),
            max_protocol_bytes: default_max_protocol_bytes(),
            io_timeout_ms: default_io_timeout_ms(),
            reconnect_budget_ms: default_reconnect_budget_ms(),
        }
    }

    /// Source transaction limits built from the flat limit fields.
    pub fn transactions(&self) -> SourceTransactionLimits {
        SourceTransactionLimits {
            max_changes: self.max_transaction_changes,
            max_bytes: self.max_transaction_bytes,
            max_duration_ms: self.max_transaction_duration_ms,
        }
    }

    /// The engine start position described by the `start*` fields.
    pub fn start_position(&self) -> Result<MySqlStartPosition> {
        let extra = |name: &str, present: bool| -> Result<()> {
            anyhow::ensure!(
                !present,
                "{name} is only valid with the matching startFrom value"
            );
            Ok(())
        };
        let gtid = self.start_gtid_set.is_some();
        let file = self.start_binlog_file.is_some() || self.start_binlog_position.is_some();
        Ok(match self.start_from {
            StartFrom::End | StartFrom::Beginning | StartFrom::Snapshot => {
                extra("startGtidSet", gtid)?;
                extra("startBinlogFile/startBinlogPosition", file)?;
                match self.start_from {
                    StartFrom::End => MySqlStartPosition::End,
                    StartFrom::Beginning => MySqlStartPosition::Start,
                    _ => MySqlStartPosition::Snapshot {
                        lock_timeout_ms: self.snapshot_lock_timeout_ms,
                    },
                }
            }
            StartFrom::Gtid => {
                extra("startBinlogFile/startBinlogPosition", file)?;
                MySqlStartPosition::Gtid {
                    set: self
                        .start_gtid_set
                        .clone()
                        .context("startFrom gtid requires startGtidSet")?,
                }
            }
            StartFrom::Position => {
                extra("startGtidSet", gtid)?;
                MySqlStartPosition::Position {
                    file: self
                        .start_binlog_file
                        .clone()
                        .context("startFrom position requires startBinlogFile")?,
                    position: self
                        .start_binlog_position
                        .context("startFrom position requires startBinlogPosition")?,
                }
            }
        })
    }

    /// Validation that needs no owner identity and no I/O.
    pub fn validate_static(&self) -> Result<()> {
        for (name, value) in [("host", &self.host), ("user", &self.user)] {
            anyhow::ensure!(
                !value.trim().is_empty() && !value.contains('\0'),
                "MySQL {name} must be nonempty and contain no NUL"
            );
        }
        validate_identifier("database", &self.database)?;
        anyhow::ensure!(self.port != 0, "MySQL port must be 1..=65535");
        anyhow::ensure!(
            !self.password.contains('\0'),
            "MySQL password cannot contain NUL"
        );
        anyhow::ensure!(
            self.tls_client_cert_pem.is_some() == self.tls_client_key_pem.is_some(),
            "tlsClientCertPem and tlsClientKeyPem must be configured together"
        );
        anyhow::ensure!(
            self.tls_client_cert_pem.is_none() || self.ssl_mode != MySqlTlsMode::Disabled,
            "a TLS client certificate requires an sslMode other than disabled"
        );
        for (name, pem) in [
            ("tlsCaPem", &self.tls_ca_pem),
            ("tlsClientCertPem", &self.tls_client_cert_pem),
            ("tlsClientKeyPem", &self.tls_client_key_pem),
        ] {
            anyhow::ensure!(
                pem.as_ref().is_none_or(|pem| !pem.is_empty()
                    && pem.len() <= MAX_PEM_BYTES
                    && pem.len() <= self.max_protocol_bytes.get()),
                "MySQL {name} must be nonempty and fit 64 KiB and the protocol limit"
            );
        }
        for table in &self.tables {
            validate_identifier("table", table)?;
        }
        anyhow::ensure!(
            self.tables.iter().collect::<BTreeSet<_>>().len() == self.tables.len(),
            "MySQL tables must be distinct"
        );
        anyhow::ensure!(
            self.reconnect_budget_ms <= MAX_RECONNECT_BUDGET_MS,
            "MySQL reconnectBudgetMs is at most {MAX_RECONNECT_BUDGET_MS}"
        );
        self.start_position()?;
        Ok(())
    }

    /// The engine configuration for one factory. `server_id` is derived from
    /// `component` when not configured.
    pub fn engine_config(
        &self,
        component: &ComponentId,
        output: MySqlOutput,
        replay: Option<MySqlRetention>,
    ) -> Result<MySqlConfig> {
        self.validate_static()?;
        let config = MySqlConfig {
            connection: MySqlConnectionConfig {
                host: self.host.clone(),
                port: self.port,
                database: self.database.clone(),
                user: self.user.clone(),
                password: self.password.clone(),
                ssl_mode: self.ssl_mode,
                tls_ca_pem: self.tls_ca_pem.clone(),
                tls_client_cert_pem: self.tls_client_cert_pem.clone(),
                tls_client_key_pem: self.tls_client_key_pem.clone(),
                server_id: self
                    .server_id
                    .unwrap_or_else(|| derived_server_id(component)),
                heartbeat_interval_ms: self.heartbeat_interval_ms,
            },
            tables: self.tables.clone(),
            table_keys: self
                .table_keys
                .iter()
                .map(|key| crate::TableKeyConfig {
                    table: key.table.clone(),
                    key_columns: key.key_columns.clone(),
                })
                .collect(),
            start: self.start_position()?,
            output,
            replay,
            transactions: self.transactions(),
            max_protocol_bytes: self.max_protocol_bytes,
            io_timeout_ms: self.io_timeout_ms,
            reconnect_budget_ms: self.reconnect_budget_ms,
        };
        Ok(config)
    }

    /// The configuration with activation credentials applied.
    pub fn with_credentials(&self, user: String, password: String) -> Self {
        let mut settings = self.clone();
        settings.user = user;
        settings.password = password;
        settings
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn base() -> serde_json::Value {
        json!({"database": "app", "user": "drasi"})
    }

    #[test]
    fn defaults_fill_every_operational_field_and_round_trip() {
        let settings: MySqlSourceSettings = serde_json::from_value(base()).unwrap();
        assert_eq!(settings.host, "localhost");
        assert_eq!(settings.port, DEFAULT_PORT);
        assert_eq!(settings.start_from, StartFrom::End);
        assert_eq!(settings.reconnect_budget_ms, DEFAULT_RECONNECT_BUDGET_MS);
        assert_eq!(
            settings.heartbeat_interval_ms.get(),
            DEFAULT_HEARTBEAT_INTERVAL_MS
        );
        settings.validate_static().unwrap();
        let value = serde_json::to_value(&settings).unwrap();
        assert_eq!(
            serde_json::from_value::<MySqlSourceSettings>(value).unwrap(),
            settings
        );
        assert_eq!(
            settings,
            MySqlSourceSettings::new("localhost", 3306, "app", "drasi")
        );
    }

    #[test]
    fn unknown_and_legacy_fields_are_rejected() {
        for (field, value) in [
            ("settings", json!({})),
            ("ssl_mode", json!("disabled")),
            ("heartbeat_interval_seconds", json!(30)),
            ("start", json!({"mode": "end"})),
        ] {
            let mut config = base();
            config[field] = value;
            assert!(
                serde_json::from_value::<MySqlSourceSettings>(config).is_err(),
                "{field}"
            );
        }
    }

    #[test]
    fn start_fields_must_match_start_from() {
        let parse = |extra: serde_json::Value| {
            let mut config = base();
            for (key, value) in extra.as_object().unwrap() {
                config[key] = value.clone();
            }
            serde_json::from_value::<MySqlSourceSettings>(config)
                .unwrap()
                .start_position()
        };
        assert_eq!(
            parse(json!({"startFrom": "position", "startBinlogFile": "binlog.000001", "startBinlogPosition": 4}))
                .unwrap(),
            MySqlStartPosition::Position {
                file: "binlog.000001".into(),
                position: 4
            }
        );
        assert!(parse(json!({"startFrom": "position"})).is_err());
        assert!(parse(json!({"startFrom": "end", "startGtidSet": "x"})).is_err());
        assert!(parse(json!({"startFrom": "gtid"})).is_err());
        assert!(matches!(
            parse(json!({"startFrom": "snapshot", "snapshotLockTimeoutMs": 5})).unwrap(),
            MySqlStartPosition::Snapshot { lock_timeout_ms } if lock_timeout_ms.get() == 5
        ));
        assert_eq!(
            parse(json!({"startFrom": "beginning"})).unwrap(),
            MySqlStartPosition::Start
        );
    }

    #[test]
    fn identifiers_allow_any_mysql_name_but_reject_unsafe_values() {
        let mut settings = MySqlSourceSettings::new("h", 3306, "app-db", "u");
        settings.tables = vec!["order-items".into(), "Ünïcode tbl".into()];
        settings.validate_static().unwrap();
        settings.tables = vec!["a\0b".into()];
        assert!(settings.validate_static().is_err());
        settings.tables = vec!["x".repeat(65)];
        assert!(settings.validate_static().is_err());
        settings.tables = vec!["t".into(), "t".into()];
        assert!(settings.validate_static().is_err());
    }

    #[test]
    fn client_certificates_are_paired_and_secret_in_debug() {
        let mut settings = MySqlSourceSettings::new("h", 3306, "app", "u");
        settings.password = "hunter2".into();
        settings.tls_client_cert_pem = Some("cert".into());
        assert!(settings.validate_static().is_err());
        settings.tls_client_key_pem = Some("very-secret-key".into());
        settings.validate_static().unwrap();
        settings.ssl_mode = MySqlTlsMode::Disabled;
        assert!(settings.validate_static().is_err());
        let debug = format!("{settings:?}");
        assert!(!debug.contains("hunter2") && !debug.contains("very-secret-key"));
    }

    #[test]
    fn server_id_is_derived_stably_from_the_component() {
        let first = ComponentId::generate();
        let second = ComponentId::generate();
        assert_eq!(derived_server_id(&first), derived_server_id(&first));
        assert_ne!(derived_server_id(&first), derived_server_id(&second));
        assert!(derived_server_id(&first).get() < 1 << 31);
        let settings = MySqlSourceSettings::new("h", 3306, "app", "u");
        let config = settings
            .engine_config(&first, MySqlOutput::Changes, None)
            .unwrap();
        assert_eq!(config.connection.server_id, derived_server_id(&first));
        let mut explicit = settings.clone();
        explicit.server_id = NonZeroU32::new(7);
        assert_eq!(
            explicit
                .engine_config(&first, MySqlOutput::Changes, None)
                .unwrap()
                .connection
                .server_id
                .get(),
            7
        );
    }
}

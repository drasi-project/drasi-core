// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! The flat native SQL Server configuration shared by the native CDC source
//! and its bootstrap providers, so both select the same tables, keys and
//! value mapping and compute the same binding.
//!
//! One object of camelCase fields; unknown fields are rejected. `password`
//! is the only secret and is a top-level field so a host can resolve it from
//! a secret reference. `Debug` never prints it; serialization returns it
//! because `configuration()` is the persistence hook.

use super::identifiers::{column_name, TableName};
use crate::client::{ConnectionSettings, Encryption};
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, time::Duration};

/// Default SQL Server port.
pub const DEFAULT_PORT: u16 = 1433;
/// Default host.
pub const DEFAULT_HOST: &str = "localhost";
/// Default TCP, TLS and login deadline.
pub const DEFAULT_CONNECT_TIMEOUT_MS: u64 = 15_000;
/// Default per-round-trip deadline.
pub const DEFAULT_COMMAND_TIMEOUT_MS: u64 = 30_000;
/// Upper bound on both timeouts.
pub const MAX_TIMEOUT_MS: u64 = 3_600_000;
/// Default delay between polls that found no change.
pub const DEFAULT_POLL_INTERVAL_MS: u64 = 1_000;
/// Upper bound on `pollIntervalMs`.
pub const MAX_POLL_INTERVAL_MS: u64 = 3_600_000;
/// Default page size, in change groups and emitted changes.
pub const DEFAULT_MAX_ROWS: usize = 10_000;
/// Upper bound on `maxRows`.
pub const MAX_MAX_ROWS: usize = 1_000_000;
/// Default page byte budget.
pub const DEFAULT_MAX_BYTES: usize = 64 * 1024 * 1024;
/// Smallest and largest page byte budget.
pub const MIN_MAX_BYTES: usize = 1024;
pub const MAX_MAX_BYTES: usize = 256 * 1024 * 1024;
/// Default total time for reconnecting after a transport failure.
pub const DEFAULT_RECONNECT_BUDGET_MS: u64 = 300_000;
/// Upper bound on `reconnectBudgetMs`.
pub const MAX_RECONNECT_BUDGET_MS: u64 = 86_400_000;
/// Most selected tables.
pub const MAX_TABLES: usize = 256;

fn default_host() -> String {
    DEFAULT_HOST.into()
}
fn default_port() -> u16 {
    DEFAULT_PORT
}
fn default_connect_timeout() -> u64 {
    DEFAULT_CONNECT_TIMEOUT_MS
}
fn default_command_timeout() -> u64 {
    DEFAULT_COMMAND_TIMEOUT_MS
}
fn default_poll_interval() -> u64 {
    DEFAULT_POLL_INTERVAL_MS
}
fn default_max_rows() -> usize {
    DEFAULT_MAX_ROWS
}
fn default_max_bytes() -> usize {
    DEFAULT_MAX_BYTES
}
fn default_reconnect_budget() -> u64 {
    DEFAULT_RECONNECT_BUDGET_MS
}

/// Where a source without any committed cursor starts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StartPosition {
    /// The CDC maximum LSN when the source first polls.
    #[default]
    Current,
    /// The oldest retained change of each capture instance.
    Beginning,
}

/// An explicit key for a table without a primary key.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TableKey {
    /// `schema.table`, or `table` in `dbo`.
    pub table: String,
    /// Key columns, in key order.
    pub key_columns: Vec<String>,
}

/// Native SQL Server CDC settings.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NativeSettings {
    #[serde(default = "default_host")]
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    pub database: String,
    /// SQL login. Optional when the host binds an activation identity.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub user: String,
    /// The only secret field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    #[serde(default)]
    pub encryption: Encryption,
    #[serde(default)]
    pub trust_server_certificate: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trusted_ca_pem: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_name_in_certificate: Option<String>,
    #[serde(default = "default_connect_timeout")]
    pub connect_timeout_ms: u64,
    #[serde(default = "default_command_timeout")]
    pub command_timeout_ms: u64,
    /// The server's time zone (a `sys.time_zone_info` name) used to convert
    /// CDC commit times to UTC. Detected with `CURRENT_TIMEZONE_ID()` when
    /// absent (SQL Server 2022 and Azure SQL).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_time_zone: Option<String>,
    /// Selected tables; empty selects every CDC-enabled table except the
    /// barrier table.
    #[serde(default)]
    pub tables: Vec<String>,
    #[serde(default)]
    pub table_keys: Vec<TableKey>,
    #[serde(default)]
    pub start_position: StartPosition,
    /// Administrator-provisioned CDC table with a `uniqueidentifier token`
    /// column, used only by coordinated snapshots.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub barrier_table: Option<String>,
    #[serde(default = "default_poll_interval")]
    pub poll_interval_ms: u64,
    #[serde(default = "default_max_rows")]
    pub max_rows: usize,
    #[serde(default = "default_max_bytes")]
    pub max_bytes: usize,
    #[serde(default = "default_reconnect_budget")]
    pub reconnect_budget_ms: u64,
}

impl std::fmt::Debug for NativeSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeSettings")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("database", &self.database)
            .field("user", &self.user)
            .field("password", &self.password.as_ref().map(|_| "[REDACTED]"))
            .field("encryption", &self.encryption)
            .field("trust_server_certificate", &self.trust_server_certificate)
            .field("trusted_ca_pem", &self.trusted_ca_pem.is_some())
            .field("host_name_in_certificate", &self.host_name_in_certificate)
            .field("connect_timeout_ms", &self.connect_timeout_ms)
            .field("command_timeout_ms", &self.command_timeout_ms)
            .field("server_time_zone", &self.server_time_zone)
            .field("tables", &self.tables)
            .field("table_keys", &self.table_keys)
            .field("start_position", &self.start_position)
            .field("barrier_table", &self.barrier_table)
            .field("poll_interval_ms", &self.poll_interval_ms)
            .field("max_rows", &self.max_rows)
            .field("max_bytes", &self.max_bytes)
            .field("reconnect_budget_ms", &self.reconnect_budget_ms)
            .finish()
    }
}

/// Every top-level settings field, for providers that accept the settings
/// beside their own fields.
pub const SETTINGS_FIELDS: &[&str] = &[
    "host",
    "port",
    "database",
    "user",
    "password",
    "encryption",
    "trustServerCertificate",
    "trustedCaPem",
    "hostNameInCertificate",
    "connectTimeoutMs",
    "commandTimeoutMs",
    "serverTimeZone",
    "tables",
    "tableKeys",
    "startPosition",
    "barrierTable",
    "pollIntervalMs",
    "maxRows",
    "maxBytes",
    "reconnectBudgetMs",
];

impl NativeSettings {
    /// Settings with every default and the given database.
    pub fn new(database: impl Into<String>) -> Self {
        Self {
            host: default_host(),
            port: DEFAULT_PORT,
            database: database.into(),
            user: String::new(),
            password: None,
            encryption: Encryption::default(),
            trust_server_certificate: false,
            trusted_ca_pem: None,
            host_name_in_certificate: None,
            connect_timeout_ms: DEFAULT_CONNECT_TIMEOUT_MS,
            command_timeout_ms: DEFAULT_COMMAND_TIMEOUT_MS,
            server_time_zone: None,
            tables: Vec::new(),
            table_keys: Vec::new(),
            start_position: StartPosition::default(),
            barrier_table: None,
            poll_interval_ms: DEFAULT_POLL_INTERVAL_MS,
            max_rows: DEFAULT_MAX_ROWS,
            max_bytes: DEFAULT_MAX_BYTES,
            reconnect_budget_ms: DEFAULT_RECONNECT_BUDGET_MS,
        }
    }

    /// Parses `value` (an object of settings fields only) and validates it.
    pub fn from_value(value: serde_json::Value) -> Result<Self> {
        let settings: Self =
            serde_json::from_value(value).context("invalid SQL Server settings")?;
        settings.validate()?;
        Ok(settings)
    }

    /// Static validation, without the credential requirement (see
    /// [`Self::validate_credentials`]). No I/O.
    pub fn validate(&self) -> Result<()> {
        self.connection().validate()?;
        crate::client::validate_literal_credentials(&self.user, self.password.as_deref(), true)?;
        for (name, value) in [
            ("connectTimeoutMs", self.connect_timeout_ms),
            ("commandTimeoutMs", self.command_timeout_ms),
        ] {
            ensure!(
                (1..=MAX_TIMEOUT_MS).contains(&value),
                "{name} must be in 1..={MAX_TIMEOUT_MS}"
            );
        }
        ensure!(
            (1..=MAX_POLL_INTERVAL_MS).contains(&self.poll_interval_ms),
            "pollIntervalMs must be in 1..={MAX_POLL_INTERVAL_MS}"
        );
        ensure!(
            (1..=MAX_MAX_ROWS).contains(&self.max_rows),
            "maxRows must be in 1..={MAX_MAX_ROWS}"
        );
        ensure!(
            (MIN_MAX_BYTES..=MAX_MAX_BYTES).contains(&self.max_bytes),
            "maxBytes must be in {MIN_MAX_BYTES}..={MAX_MAX_BYTES}"
        );
        ensure!(
            self.reconnect_budget_ms <= MAX_RECONNECT_BUDGET_MS,
            "reconnectBudgetMs must be in 0..={MAX_RECONNECT_BUDGET_MS}"
        );
        if let Some(zone) = &self.server_time_zone {
            ensure!(
                !zone.is_empty() && zone.len() <= 128 && !zone.contains('\0'),
                "serverTimeZone must be a time zone name of at most 128 bytes"
            );
        }
        let tables = self.selected_tables()?;
        ensure!(
            tables.len() <= MAX_TABLES,
            "at most {MAX_TABLES} tables can be selected"
        );
        if let Some(barrier) = self.barrier()? {
            ensure!(
                !tables.iter().any(|table| table.key() == barrier.key()),
                "barrierTable cannot also be a selected table"
            );
        }
        let mut keyed = BTreeSet::new();
        for key in &self.table_keys {
            let table = TableName::parse(&key.table)?;
            ensure!(
                keyed.insert(table.key()),
                "tableKeys names {} twice",
                key.table
            );
            ensure!(
                !key.key_columns.is_empty(),
                "tableKeys entry for {} has no keyColumns",
                key.table
            );
            let mut unique = BTreeSet::new();
            for column in &key.key_columns {
                ensure!(
                    unique.insert(column_name(column)?.to_lowercase()),
                    "tableKeys entry for {} repeats column {column}",
                    key.table
                );
            }
        }
        Ok(())
    }

    /// Literal credentials are required unless an identity is bound.
    pub fn validate_credentials(&self, identity_bound: bool) -> Result<()> {
        crate::client::validate_literal_credentials(
            &self.user,
            self.password.as_deref(),
            identity_bound,
        )
    }

    /// The parsed, de-duplicated table selection.
    pub fn selected_tables(&self) -> Result<Vec<TableName>> {
        let mut seen = BTreeSet::new();
        let mut tables = Vec::new();
        for name in &self.tables {
            let table = TableName::parse(name)?;
            ensure!(seen.insert(table.key()), "table {name} is selected twice");
            tables.push(table);
        }
        Ok(tables)
    }

    /// The parsed barrier table.
    pub fn barrier(&self) -> Result<Option<TableName>> {
        self.barrier_table
            .as_deref()
            .map(TableName::parse)
            .transpose()
    }

    /// Explicit key columns for `table`, unquoted.
    pub fn explicit_key(&self, table: &TableName) -> Result<Option<Vec<String>>> {
        for key in &self.table_keys {
            if TableName::parse(&key.table)?.key() == table.key() {
                return key
                    .key_columns
                    .iter()
                    .map(|column| column_name(column))
                    .collect::<Result<_>>()
                    .map(Some);
            }
        }
        Ok(None)
    }

    pub fn connect_timeout(&self) -> Duration {
        Duration::from_millis(self.connect_timeout_ms)
    }
    pub fn command_timeout(&self) -> Duration {
        Duration::from_millis(self.command_timeout_ms)
    }
    pub fn poll_interval(&self) -> Duration {
        Duration::from_millis(self.poll_interval_ms)
    }
    pub fn reconnect_budget(&self) -> Duration {
        Duration::from_millis(self.reconnect_budget_ms)
    }

    /// The transport settings, in the shared client's terms.
    pub fn connection(&self) -> ConnectionSettings<'_> {
        ConnectionSettings {
            host: &self.host,
            port: self.port,
            database: &self.database,
            encryption: self.encryption,
            trust_server_certificate: self.trust_server_certificate,
            trusted_ca_pem: self.trusted_ca_pem.as_deref(),
            host_name_in_certificate: self.host_name_in_certificate.as_deref(),
            command_timeout: self.command_timeout(),
            connect_timeout: self.connect_timeout(),
            application_name: "drasi-native-mssql",
        }
    }

    /// The literal SQL credentials.
    pub fn literal_authentication(&self) -> crate::client::Authentication {
        crate::client::Authentication::Sql {
            user: self.user.clone(),
            password: self.password.clone().unwrap_or_default(),
        }
    }

    /// The selection and key facts a committed cursor depends on, for
    /// update planning: tables, keys, barrier and database.
    pub fn bound_facts_equal(&self, other: &Self) -> bool {
        self.database == other.database
            && self.tables == other.tables
            && self.table_keys == other.table_keys
            && self.barrier_table == other.barrier_table
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn defaults_round_trip_and_unknown_or_legacy_fields_are_rejected() {
        let settings = NativeSettings::from_value(json!({"database": "app"})).unwrap();
        assert_eq!(settings, NativeSettings::new("app"));
        assert_eq!(settings.encryption, Encryption::Required);
        let value = serde_json::to_value(&settings).unwrap();
        assert_eq!(NativeSettings::from_value(value.clone()).unwrap(), settings);
        let mut fields: Vec<_> = value.as_object().unwrap().keys().cloned().collect();
        fields.extend(
            [
                "user",
                "password",
                "trustedCaPem",
                "hostNameInCertificate",
                "serverTimeZone",
                "barrierTable",
            ]
            .map(String::from),
        );
        fields.sort();
        let mut expected: Vec<_> = SETTINGS_FIELDS.iter().map(|f| f.to_string()).collect();
        expected.sort();
        assert_eq!(fields, expected, "SETTINGS_FIELDS lists every field");
        for (field, value) in [
            ("pasword", json!("x")),
            ("connection", json!({})),
            ("poll_interval_ms", json!(5)),
            ("auth_mode", json!("sqlserver")),
            ("operationTimeoutMs", json!(5)),
        ] {
            let mut config = json!({"database": "app"});
            config[field] = value;
            assert!(NativeSettings::from_value(config).is_err(), "{field}");
        }
    }

    #[test]
    fn invalid_values_are_rejected() {
        for (field, value) in [
            ("database", json!("")),
            ("port", json!(0)),
            ("maxRows", json!(0)),
            ("maxBytes", json!(10)),
            ("pollIntervalMs", json!(0)),
            ("commandTimeoutMs", json!(0)),
            ("reconnectBudgetMs", json!(MAX_RECONNECT_BUDGET_MS + 1)),
            ("tables", json!(["Orders", "dbo.orders"])),
            ("tables", json!(["db.dbo.orders"])),
            ("tableKeys", json!([{"table": "t", "keyColumns": []}])),
            (
                "tableKeys",
                json!([{"table": "t", "keyColumns": ["a", "A"]}]),
            ),
            ("encryption", json!("on")),
            ("serverTimeZone", json!("")),
        ] {
            let mut config = json!({"database": "app"});
            config[field] = value;
            assert!(NativeSettings::from_value(config).is_err(), "{field}");
        }
        let barrier =
            json!({"database": "app", "tables": ["dbo.Barrier"], "barrierTable": "Barrier"});
        assert!(NativeSettings::from_value(barrier).is_err());
    }

    #[test]
    fn password_is_redacted_and_credentials_are_checked() {
        let settings = NativeSettings::from_value(
            json!({"database": "app", "user": "u", "password": "hunter2"}),
        )
        .unwrap();
        assert!(!format!("{settings:?}").contains("hunter2"));
        settings.validate_credentials(false).unwrap();
        let unbound = NativeSettings::new("app");
        assert!(unbound.validate_credentials(false).is_err());
        unbound.validate_credentials(true).unwrap();
    }

    #[test]
    fn quoted_tables_and_explicit_keys_resolve() {
        let settings = NativeSettings::from_value(json!({
            "database": "app",
            "tables": ["[Sales Ops].[Order Lines]"],
            "tableKeys": [{"table": "[sales ops].[order lines]", "keyColumns": ["[line id]"]}]
        }))
        .unwrap();
        let table = &settings.selected_tables().unwrap()[0];
        assert_eq!(table.table, "Order Lines");
        assert_eq!(
            settings.explicit_key(table).unwrap(),
            Some(vec!["line id".to_string()])
        );
    }
}

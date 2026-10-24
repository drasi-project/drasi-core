// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Native MySQL replication with directly owned network I/O.

use anyhow::{Context, Result};
use drasi_computation_contracts::SourceTransactionLimits;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    fmt,
    num::{NonZeroU32, NonZeroU64, NonZeroUsize},
    time::Duration,
};

pub mod binlog;
pub mod catalog;
pub mod connection;
pub mod progress;
pub mod rows;
pub mod schema;
pub mod session;
pub mod snapshot;
pub mod source;
pub mod standalone_snapshot;
#[cfg(test)]
mod tests;
pub use crate::dto::SslModeDto as MySqlTlsMode;
pub use connection::MySqlServerError;
pub use snapshot::MySqlSnapshot;
pub use source::MySqlSource;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MySqlConnectionConfig {
    pub host: String,
    pub port: u16,
    pub database: String,
    pub user: String,
    pub password: String,
    pub ssl_mode: MySqlTlsMode,
    pub tls_ca_pem: Option<String>,
    pub server_id: NonZeroU32,
    pub heartbeat_interval_ms: NonZeroU64,
}

impl MySqlConnectionConfig {
    pub fn validate(&self, max_bytes: usize) -> Result<()> {
        self.validate_with_activation_credentials(max_bytes, false)
    }

    pub fn validate_with_activation_credentials(
        &self,
        max_bytes: usize,
        credentials_bound: bool,
    ) -> Result<()> {
        anyhow::ensure!(
            !self.host.trim().is_empty()
                && self.port != 0
                && (credentials_bound || !self.user.is_empty())
                && crate::is_valid_identifier(&self.database)
                && self.database.len() <= 64
                && [&self.host, &self.database, &self.user, &self.password]
                    .iter()
                    .all(|value| !value.contains('\0')),
            "invalid native MySQL connection settings"
        );
        anyhow::ensure!(
            max_bytes >= 256 && max_bytes <= u32::MAX as usize,
            "MySQL protocol byte limit must be between 256 and u32::MAX"
        );
        anyhow::ensure!(
            [&self.host, &self.database, &self.user, &self.password]
                .iter()
                .map(|value| value.len())
                .sum::<usize>()
                <= max_bytes / 2,
            "MySQL connection settings exceed protocol byte limit"
        );
        anyhow::ensure!(
            self.tls_ca_pem.as_ref().is_none_or(|pem| !pem.is_empty()
                && pem.len() <= 64 * 1024
                && pem.len() <= max_bytes),
            "MySQL CA certificate must fit protocol limit and 64 KiB"
        );
        Ok(())
    }
}

impl fmt::Debug for MySqlConnectionConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MySqlConnectionConfig")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("database", &self.database)
            .field("user", &self.user)
            .field("password", &"[REDACTED]")
            .field("ssl_mode", &self.ssl_mode)
            .field("server_id", &self.server_id)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MySqlOutput {
    Changes,
    Transactions,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MySqlRetention {
    /// Resume only while the server retains the requested history; gaps fail.
    Server,
    /// Requires automatic expiry disabled; never changes settings or purges logs.
    UntilProcessed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum MySqlStartPosition {
    Start,
    End,
    Gtid {
        set: String,
    },
    Position {
        file: String,
        position: u32,
    },
    /// Opt-in server-wide read locking, only while establishing the initial view.
    Snapshot {
        lock_timeout_ms: NonZeroU64,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MySqlConfig {
    pub connection: MySqlConnectionConfig,
    pub tables: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub table_keys: Vec<crate::TableKeyConfig>,
    pub start: MySqlStartPosition,
    pub output: MySqlOutput,
    pub replay: Option<MySqlRetention>,
    pub transactions: SourceTransactionLimits,
    pub max_protocol_bytes: NonZeroUsize,
    pub io_timeout_ms: NonZeroU64,
}

impl MySqlConfig {
    pub fn validate(&self) -> Result<()> {
        self.validate_common()?;
        anyhow::ensure!(
            self.replay.is_none()
                || (self.output == MySqlOutput::Transactions
                    && matches!(
                        self.start,
                        MySqlStartPosition::Position { .. } | MySqlStartPosition::Snapshot { .. }
                    )),
            "MySQL replay requires transaction output and a fixed initial position"
        );
        if matches!(self.start, MySqlStartPosition::Snapshot { .. }) {
            anyhow::ensure!(
                self.replay.is_some(),
                "MySQL snapshot requires consumer-owned replay"
            );
        }
        Ok(())
    }

    pub fn validate_rows(&self) -> Result<()> {
        self.validate_common()?;
        anyhow::ensure!(
            self.output == MySqlOutput::Changes && self.replay.is_some(),
            "tracked MySQL rows require change output and explicit upstream replay"
        );
        Ok(())
    }

    fn validate_common(&self) -> Result<()> {
        let connection = &self.connection;
        connection.validate(self.max_protocol_bytes.get())?;
        anyhow::ensure!(
            connection.heartbeat_interval_ms.get() <= u64::MAX / 1_000_000,
            "MySQL heartbeat duration exceeds protocol range"
        );
        anyhow::ensure!(
            self.tables
                .iter()
                .all(|table| table.len() <= 64 && crate::is_valid_identifier(table))
                && self.tables.iter().collect::<BTreeSet<_>>().len() == self.tables.len(),
            "native MySQL requires distinct valid table names"
        );
        anyhow::ensure!(
            self.table_keys
                .iter()
                .map(|key| &key.table)
                .collect::<BTreeSet<_>>()
                .len()
                == self.table_keys.len(),
            "duplicate MySQL table key configuration"
        );
        for key in &self.table_keys {
            anyhow::ensure!(
                key.table.len() <= 64
                    && crate::is_valid_identifier(&key.table)
                    && (self.tables.is_empty() || self.tables.contains(&key.table))
                    && !key.key_columns.is_empty()
                    && key.key_columns.iter().collect::<BTreeSet<_>>().len()
                        == key.key_columns.len()
                    && key.key_columns.iter().all(|column| !column.is_empty()
                        && column.chars().count() <= 64
                        && !column.contains('\0')),
                "invalid MySQL table key configuration"
            );
        }
        if let MySqlStartPosition::Position { file, position } = &self.start {
            file_number(file)?;
            anyhow::ensure!(*position >= 4, "MySQL binlog positions start at four");
        }
        if let MySqlStartPosition::Gtid { set } = &self.start {
            parse_gtid_set(set)?;
        }
        if let MySqlStartPosition::Snapshot { lock_timeout_ms } = self.start {
            anyhow::ensure!(
                lock_timeout_ms.get() <= 31_536_000_000,
                "MySQL snapshot lock timeout exceeds one year"
            );
        }
        tokio::time::Instant::now()
            .checked_add(self.timeout())
            .context("invalid MySQL I/O deadline")?;
        tokio::time::Instant::now()
            .checked_add(self.transactions.duration())
            .context("invalid MySQL transaction deadline")?;
        Ok(())
    }

    pub fn timeout(&self) -> Duration {
        Duration::from_millis(self.io_timeout_ms.get())
    }
}

pub fn parse_gtid_set(value: &str) -> Result<Vec<mysql_common::packets::Sid<'static>>> {
    use std::str::FromStr;
    anyhow::ensure!(value.len() <= 60 * 1024, "MySQL GTID set exceeds 60 KiB");
    let values = value
        .split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(|entry| {
            mysql_common::packets::Sid::from_str(entry)
                .map_err(|error| anyhow::anyhow!("Invalid GTID set entry '{entry}': {error}"))
        })
        .collect::<Result<Vec<_>>>()?;
    anyhow::ensure!(
        !values.is_empty(),
        "Invalid GTID set: no GTID intervals were provided"
    );
    Ok(values)
}

pub fn file_number(file: &str) -> Result<u32> {
    let (prefix, number) = file
        .rsplit_once('.')
        .context("MySQL binlog file requires a numeric suffix")?;
    anyhow::ensure!(
        !prefix.is_empty()
            && prefix
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
            && number.len() >= 6
            && number.bytes().all(|byte| byte.is_ascii_digit()),
        "invalid MySQL binlog filename"
    );
    let number: u32 = number.parse()?;
    anyhow::ensure!(number != 0, "MySQL binlog file number must be positive");
    Ok(number)
}

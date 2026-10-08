// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Native MySQL replication with directly owned network I/O.

use anyhow::{Context, Result};
use drasi_lib::computation::v1::SourceTransactionLimits;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    fmt,
    num::{NonZeroU32, NonZeroU64, NonZeroUsize},
    time::Duration,
};

mod binlog;
mod catalog;
mod connection;
mod factory;
mod snapshot;
mod source;
#[cfg(test)]
mod tests;
pub use connection::MySqlServerError;
pub use drasi_mysql_common::dto::SslModeDto as MySqlTlsMode;
pub use factory::MySqlSourceFactory;
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
    End,
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
    pub start: MySqlStartPosition,
    pub output: MySqlOutput,
    pub replay: Option<MySqlRetention>,
    pub transactions: SourceTransactionLimits,
    pub max_protocol_bytes: NonZeroUsize,
    pub io_timeout_ms: NonZeroU64,
}

impl MySqlConfig {
    fn validate(&self) -> Result<()> {
        let connection = &self.connection;
        anyhow::ensure!(
            !connection.host.trim().is_empty()
                && connection.port != 0
                && !connection.user.is_empty()
                && drasi_mysql_common::is_valid_identifier(&connection.database)
                && connection.database.len() <= 64
                && [
                    &connection.host,
                    &connection.database,
                    &connection.user,
                    &connection.password
                ]
                .iter()
                .all(|value| !value.contains('\0')),
            "invalid native MySQL connection settings"
        );
        anyhow::ensure!(
            self.max_protocol_bytes.get() >= 256
                && self.max_protocol_bytes.get() <= u32::MAX as usize,
            "MySQL protocol byte limit must be between 256 and u32::MAX"
        );
        anyhow::ensure!(
            [
                &connection.host,
                &connection.database,
                &connection.user,
                &connection.password
            ]
            .iter()
            .map(|value| value.len())
            .sum::<usize>()
                <= self.max_protocol_bytes.get() / 2,
            "MySQL connection settings exceed protocol byte limit"
        );
        anyhow::ensure!(
            connection.heartbeat_interval_ms.get() <= u64::MAX / 1_000_000,
            "MySQL heartbeat duration exceeds protocol range"
        );
        anyhow::ensure!(
            connection
                .tls_ca_pem
                .as_ref()
                .is_none_or(|pem| !pem.is_empty()
                    && pem.len() <= 64 * 1024
                    && pem.len() <= self.max_protocol_bytes.get()),
            "MySQL CA certificate must fit the protocol limit and 64 KiB"
        );
        anyhow::ensure!(
            !self.tables.is_empty()
                && self.tables.iter().all(
                    |table| table.len() <= 64 && drasi_mysql_common::is_valid_identifier(table)
                )
                && self.tables.iter().collect::<BTreeSet<_>>().len() == self.tables.len(),
            "native MySQL requires distinct, explicitly selected table names"
        );
        if let MySqlStartPosition::Position { file, position } = &self.start {
            file_number(file)?;
            anyhow::ensure!(*position >= 4, "MySQL binlog positions start at four");
        }
        anyhow::ensure!(
            self.replay.is_none()
                || (self.output == MySqlOutput::Transactions
                    && !matches!(self.start, MySqlStartPosition::End)),
            "MySQL replay requires transaction output and a fixed initial position"
        );
        if let MySqlStartPosition::Snapshot { lock_timeout_ms } = self.start {
            anyhow::ensure!(
                self.replay.is_some(),
                "MySQL snapshot requires consumer-owned replay"
            );
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

    fn timeout(&self) -> Duration {
        Duration::from_millis(self.io_timeout_ms.get())
    }
}

fn file_number(file: &str) -> Result<u32> {
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

// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Data-only CDC, catalog, snapshot and retained SQL work shared by native roles.

mod catalog;
mod cursor;
mod engine;
mod paging;
mod snapshot;
mod work;

pub use catalog::{describe_schema, Catalog, Table};
pub use cursor::{Cursor, CursorComparator};
pub use engine::{element_size, poll, Change, Poll};
pub use snapshot::{start_snapshot, SnapshotRows, SnapshotRun};
pub use work::Work;

use crate::{validate_sql_identifier, AuthMode, MsSqlSourceConfig};
use anyhow::{ensure, Result};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, time::Duration};

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeConfig {
    pub connection: MsSqlSourceConfig,
    /// Pre-provisioned CDC table containing a uniqueidentifier `token` column.
    /// Only coordinated snapshots write to this explicitly selected table.
    #[serde(default)]
    pub barrier_table: Option<String>,
    #[serde(default = "default_limit")]
    /// Maximum queued rows / CDC output rows per page, not total dataset size.
    pub max_rows: usize,
    #[serde(default = "default_bytes")]
    /// Exact serialized graph-record payload bytes in a page/queue. Both CDC
    /// images count. One indivisible record/change must fit this budget.
    pub max_bytes: usize,
    #[serde(default = "default_timeout")]
    pub operation_timeout_ms: u64,
    #[serde(default = "default_timeout")]
    pub shutdown_timeout_ms: u64,
}

fn default_limit() -> usize {
    100_000
}
fn default_bytes() -> usize {
    64 * 1024 * 1024
}
fn default_timeout() -> u64 {
    30_000
}

pub fn table_name(name: &str) -> Result<(String, String)> {
    validate_sql_identifier(name)?;
    let parts: Vec<_> = name.split('.').collect();
    ensure!(
        parts.len() <= 2,
        "SQL Server tables must be schema-qualified or bare"
    );
    let (schema, table) = if parts.len() == 1 {
        ("dbo", parts[0])
    } else {
        (parts[0], parts[1])
    };
    validate_sql_identifier(schema)?;
    validate_sql_identifier(table)?;
    Ok((schema.to_owned(), table.to_owned()))
}

pub fn quoted(name: &str) -> String {
    format!("[{}]", name.replace(']', "]]"))
}

impl NativeConfig {
    pub fn validate(&self) -> Result<()> {
        self.validate_with_bound_credentials(false)
    }
    pub fn validate_with_bound_credentials(&self, bound: bool) -> Result<()> {
        let c = &self.connection;
        ensure!(
            !c.host.is_empty()
                && c.port > 0
                && !c.database.is_empty()
                && (bound || !c.user.is_empty()),
            "SQL Server host, port, database and user are required"
        );
        ensure!(
            c.auth_mode == AuthMode::SqlServer,
            "SQL Server Windows and Azure AD authentication are not implemented"
        );
        ensure!(
            [&c.host, &c.database, &c.user, &c.password]
                .iter()
                .all(|v| !v.contains('\0')),
            "SQL Server connection fields cannot contain NUL"
        );
        ensure!(
            c.poll_interval_ms > 0 && c.poll_interval_ms <= 3_600_000,
            "poll_interval_ms must be in 1..=3600000"
        );
        ensure!(
            (1..=1_000_000).contains(&self.max_rows)
                && (1024..=268_435_456).contains(&self.max_bytes),
            "SQL Server row/byte limits are out of range"
        );
        ensure!(
            [self.operation_timeout_ms, self.shutdown_timeout_ms]
                .iter()
                .all(|v| (1..=3_600_000).contains(v)),
            "SQL Server timeouts must be in 1..=3600000 milliseconds"
        );
        let mut tables = BTreeSet::new();
        for table in &c.tables {
            let (schema, name) = table_name(table)?;
            ensure!(
                tables.insert((schema.to_lowercase(), name.to_lowercase())),
                "duplicate SQL Server table"
            );
        }
        ensure!(tables.len() <= 256, "too many SQL Server tables");
        if let Some(barrier) = &self.barrier_table {
            let (schema, name) = table_name(barrier)?;
            ensure!(
                !tables.contains(&(schema.to_lowercase(), name.to_lowercase())),
                "CDC barrier table cannot also be a source table"
            );
        }
        for key in &c.table_keys {
            table_name(&key.table)?;
            ensure!(
                !key.key_columns.is_empty(),
                "custom SQL Server keys cannot be empty"
            );
            let mut unique = BTreeSet::new();
            for column in &key.key_columns {
                validate_sql_identifier(column)?;
                ensure!(
                    !column.contains('.') && unique.insert(column.to_lowercase()),
                    "invalid custom SQL Server key"
                );
            }
        }
        Ok(())
    }
    pub fn timeout(&self) -> Duration {
        Duration::from_millis(self.operation_timeout_ms)
    }
    pub fn shutdown_timeout(&self) -> Duration {
        Duration::from_millis(self.shutdown_timeout_ms)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn configuration_rejects_ambiguous_tables_auth_and_unbounded_work() {
        let mut config = NativeConfig {
            connection: MsSqlSourceConfig {
                database: "d".into(),
                user: "u".into(),
                ..Default::default()
            },
            barrier_table: None,
            max_rows: default_limit(),
            max_bytes: default_bytes(),
            operation_timeout_ms: default_timeout(),
            shutdown_timeout_ms: default_timeout(),
        };
        config.validate().unwrap();
        for name in ["db.dbo.orders", "dbo.1table", "dbo..orders", "orders;drop"] {
            assert!(table_name(name).is_err());
        }
        config.connection.tables = vec!["Orders".into(), "dbo.orders".into()];
        assert!(config.validate().is_err());
        config.connection.tables.clear();
        config.connection.auth_mode = AuthMode::AzureAd;
        assert!(config.validate().is_err());
        config.connection.auth_mode = AuthMode::SqlServer;
        config.max_rows = 0;
        assert!(config.validate().is_err());
    }
}

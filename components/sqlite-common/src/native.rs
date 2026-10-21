// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Shared SQLite execution without graph, query, storage owners or a plugin ABI.

use anyhow::{Context, Result};
use drasi_computation_contracts::SourceTransactionLimits;
use serde::{Deserialize, Serialize};
use std::{
    num::{NonZeroU64, NonZeroUsize},
    path::PathBuf,
    time::Duration,
};

pub mod consumer_snapshot;
pub mod handle;
pub mod journal;
pub mod progress;
pub mod runtime;
pub mod schema;
pub mod snapshot;
pub mod worker;
pub use crate::SqliteParam;
pub use handle::{SqliteHandle, SqliteTransactionHandle};
pub type SqliteRow = serde_json::Map<String, serde_json::Value>;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SqliteOutput {
    /// Ordinary graph changes. No durable source replay or atomic query visibility.
    Changes,
    /// Complete source transactions, for an explicitly transaction-enabled query.
    Transactions,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SqliteReplayConfig {
    pub max_transactions: NonZeroUsize,
    pub max_bytes: NonZeroUsize,
}

/// Explicit bounds apply to SQL admission, captured transactions and read results.
/// Neither output mode enables persistent delivery by itself.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SqliteConfig {
    pub path: Option<PathBuf>,
    /// Empty means all ordinary main-database tables.
    pub tables: Vec<String>,
    /// Explicit element-key columns; otherwise use the primary key or rowid.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub table_keys: Vec<crate::TableKeyConfig>,
    pub output: SqliteOutput,
    /// Opt-in source-owned transaction journal. Requires a file and actual
    /// persistent consumer progress, supplied to a replayable constructor.
    pub replay: Option<SqliteReplayConfig>,
    pub transactions: SourceTransactionLimits,
    pub max_sql_bytes: NonZeroUsize,
    pub command_capacity: NonZeroUsize,
    pub output_capacity: NonZeroUsize,
    pub shutdown_timeout_ms: NonZeroU64,
}

impl SqliteConfig {
    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            self.command_capacity.get() <= tokio::sync::Semaphore::MAX_PERMITS
                && self.output_capacity.get() <= tokio::sync::Semaphore::MAX_PERMITS,
            "SQLite channel capacity exceeds Tokio's supported bound"
        );
        anyhow::ensure!(
            self.transactions.max_duration_ms.get() <= i32::MAX as u64,
            "SQLite transaction duration exceeds its busy-timeout bound"
        );
        anyhow::ensure!(
            self.command_capacity.get() >= 2,
            "SQLite command capacity must be at least two"
        );
        anyhow::ensure!(
            self.max_sql_bytes.get() <= i32::MAX as usize
                && self.transactions.max_bytes.get() <= i32::MAX as usize,
            "SQLite byte limits must fit signed 32-bit lengths"
        );
        crate::validate_selection(&self.tables, &self.table_keys)?;
        tokio::time::Instant::now()
            .checked_add(self.transactions.duration())
            .context("SQLite transaction deadline is not representable")?;
        tokio::time::Instant::now()
            .checked_add(self.timeout())
            .context("SQLite cleanup deadline is not representable")?;
        Ok(())
    }

    pub fn timeout(&self) -> Duration {
        Duration::from_millis(self.shutdown_timeout_ms.get())
    }
}

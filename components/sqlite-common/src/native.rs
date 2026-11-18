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
pub mod error;
pub mod handle;
pub mod identity;
pub mod journal;
pub mod progress;
pub mod runtime;
pub mod schema;
pub mod snapshot;
pub mod worker;
pub use crate::SqliteParam;
pub use error::{SqlitePaused, SqliteStopping};
pub use handle::{SqliteHandle, SqliteTransactionHandle};
pub type SqliteRow = serde_json::Map<String, serde_json::Value>;

/// The coordination protocol of a single-owner replay source and its paired
/// `drasi.sqlite/coordinated-bootstrap` providers.
pub const COORDINATED_PROTOCOL: &str = "drasi.sqlite/coordinated-snapshot";

/// The coordination protocol of a shared (consumer group) source and its
/// paired `drasi.sqlite/shared-bootstrap` providers.
pub const SHARED_PROTOCOL: &str = "drasi.sqlite/shared-snapshot";

/// Version of both coordination protocols.
pub const PROTOCOL_VERSION: u32 = 1;

/// The coordination protocol of a journal source (`shared` selects the
/// consumer-group protocol).
pub fn coordination_protocol(shared: bool) -> drasi_computation_contracts::BootstrapProtocol {
    drasi_computation_contracts::BootstrapProtocol::new(
        if shared {
            SHARED_PROTOCOL
        } else {
            COORDINATED_PROTOCOL
        },
        PROTOCOL_VERSION,
    )
}

/// The element identities a source or file snapshot produces for `tables`
/// and `table_keys`, as the graph compares them.
pub fn element_identity(
    tables: &[String],
    table_keys: &[crate::TableKeyConfig],
) -> Result<drasi_computation_contracts::ElementIdentity> {
    Ok(drasi_computation_contracts::ElementIdentity {
        scheme: crate::ELEMENT_IDENTITY_SCHEME.into(),
        binding: crate::element_identity_binding(tables, table_keys)?,
    })
}

/// Whether a table is read for a query declaring `labels`: a row's label is
/// its table's name, and `None` reads every captured table.
pub fn table_selected(
    labels: Option<&drasi_computation_contracts::BootstrapLabels>,
    table: &str,
) -> bool {
    labels.is_none_or(|labels| labels.requests_node(table))
}

/// Where a source's replay progress is bound, which decides its declared
/// description.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SqliteProgress {
    /// No replay: the source keeps no progress.
    None,
    /// The journal replays from one query's cursor (`source_progress`).
    Single,
    /// The journal replays for a fixed consumer group (`consumer_progress`).
    Group,
}

/// The description a SQLite source declares, for the loaded plugin and the
/// in-process wrapper alike. Outputs carry only source replay progress. A
/// journal replays from cursors kept in the bound progress, so it requires
/// that progress to be durable and the graph refuses a volatile binding;
/// without a journal the source keeps no progress.
pub fn description(
    progress: SqliteProgress,
    descriptor: &drasi_computation_contracts::ComponentDescriptor,
) -> drasi_computation_contracts::ComponentDescription {
    use drasi_computation_contracts::{
        ComponentDescription, DependencyRequirement, DurabilityRequirement, OutputDescription,
        ProgressPersistence,
    };
    let dependency = match progress {
        SqliteProgress::None => {
            return ComponentDescription::new(ProgressPersistence::Volatile)
                .with_outputs(descriptor, OutputDescription::UNTRACKED)
        }
        SqliteProgress::Single => "source_progress",
        SqliteProgress::Group => "consumer_progress",
    };
    ComponentDescription::new(ProgressPersistence::Durable)
        .with_outputs(descriptor, OutputDescription::UNTRACKED)
        .with_dependency(
            dependency,
            DependencyRequirement {
                durability: DurabilityRequirement::Durable,
            },
        )
}

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

/// Per-connection behaviour that is not part of the legacy serialized
/// [`SqliteConfig`] shape. Every connection the source opens (the live worker
/// and coordinated snapshot workers) applies the same options.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SqliteConnectionOptions {
    /// SQL run once on each newly opened connection, before the authorizer and
    /// capture hooks are installed. It may create tables, indexes, views and
    /// triggers and set pragmas; it may not touch `__drasi_*` objects, attach
    /// databases or load extensions. Its changes are not captured.
    pub init_sql: Option<String>,
    /// Opt in to `journal_mode=WAL` for a file database without a source
    /// journal. Replay modes always use `journal_mode=DELETE` with an
    /// exclusive lock.
    pub wal: bool,
}

impl SqliteConnectionOptions {
    pub fn validate(&self, config: &SqliteConfig) -> Result<()> {
        if let Some(sql) = &self.init_sql {
            anyhow::ensure!(!sql.trim().is_empty(), "SQLite initSql must not be empty");
            anyhow::ensure!(!sql.contains('\0'), "SQLite initSql contains NUL");
            anyhow::ensure!(
                sql.len() <= config.max_sql_bytes.get(),
                "SQLite initSql exceeds maxSqlBytes"
            );
        }
        anyhow::ensure!(
            !self.wal || (config.path.is_some() && config.replay.is_none()),
            "SQLite WAL mode requires a file database without a source journal"
        );
        Ok(())
    }
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

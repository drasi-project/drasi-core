// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Native SQLite source.
//!
//! The source owns its database connection: writes arrive through its REST
//! API (or, for a Rust host that links this crate, through
//! [`SqliteSource::handle`]), run under an authorizer, and are captured by
//! SQLite's pre-update hook in the same transaction. Four factories select
//! the output and recovery model:
//!
//! - `drasi.sqlite/changes`: one graph-change envelope per commit; volatile.
//! - `drasi.sqlite/transactions`: complete source transactions; volatile.
//! - `drasi.sqlite/replay-transactions`: complete transactions in a
//!   source-owned journal in the database file, replayed from the query's
//!   persistent cursor; optionally initialized by a coordinated snapshot.
//! - `drasi.sqlite/shared-transactions`: the journal shared by a fixed set
//!   of persistent consumers.
//!
//! The source pauses at transaction boundaries, stops with Drain or Abort
//! within the host's deadline, supports managed updates, and drops its
//! journal tables on erasing removal. See `DESIGN.md` and `CONFIGURATION.md`
//! in this crate.

mod component;
pub mod config;
mod coordinator;
mod management;
mod rest;

use anyhow::{Context, Result};
use config::{Mode, SourceConfig};
use drasi_computation_contracts::{
    ComponentRole, ComponentUpdateMode, GraphChangeCodec, ImplementationIdentity,
};
use drasi_computation_plugin_sdk::{
    lifecycle::Assessment, Capabilities, Component, ConfigDetails, ControlSender, CreateRequest,
    CreatedComponent, Factory, FactoryMetadata, NativeConsumerProgress,
    NativeSourceBootstrapCoordinator, NativeSourceProgress, PluginDefinition,
    SourceBootstrapProgress, UpdateRequest,
};
use std::sync::Arc;

pub use component::SqliteSource;
pub use drasi_sqlite_common::{
    native::{
        SqliteConfig, SqliteHandle, SqliteOutput, SqlitePaused, SqliteReplayConfig, SqliteRow,
        SqliteStopping, SqliteTransactionHandle,
    },
    RestApiConfig, SqliteParam, TableKeyConfig,
};

pub const PLUGIN_ID: &str = env!("CARGO_PKG_NAME");
pub const SQLITE_CHANGES: &str = "drasi.sqlite/changes";
pub const SQLITE_TRANSACTIONS: &str = "drasi.sqlite/transactions";
pub const SQLITE_REPLAY: &str = "drasi.sqlite/replay-transactions";
pub const SQLITE_SHARED: &str = "drasi.sqlite/shared-transactions";

const MODES: [Mode; 4] = [
    Mode::Changes,
    Mode::Transactions,
    Mode::Replay,
    Mode::Shared,
];

struct SqliteFactory(Mode);

pub fn plugin() -> Result<PluginDefinition> {
    PluginDefinition::new(
        PLUGIN_ID,
        env!("CARGO_PKG_VERSION"),
        MODES
            .into_iter()
            .map(|mode| Arc::new(SqliteFactory(mode)) as Arc<dyn Factory>)
            .collect(),
        vec![GraphChangeCodec::schema()],
    )
}

fn implementation(mode: Mode) -> &'static str {
    match mode {
        Mode::Changes => SQLITE_CHANGES,
        Mode::Transactions => SQLITE_TRANSACTIONS,
        Mode::Replay => SQLITE_REPLAY,
        Mode::Shared => SQLITE_SHARED,
    }
}

/// Factory metadata. The constants it uses are proven valid by
/// `factory_metadata_constructs_without_database_io`.
fn metadata(mode: Mode) -> FactoryMetadata {
    FactoryMetadata {
        implementation: ImplementationIdentity::try_new(implementation(mode), "1")
            .expect("constant implementation"),
        role: ComponentRole::Source,
        completion: None,
        ports: SqliteSource::ports(mode.output()).expect("constant port descriptor"),
        configuration_version: 1,
        configuration: config::definition(mode).schema(),
        capabilities: Capabilities {
            management: true,
            update_modes: vec![ComponentUpdateMode::InPlace, ComponentUpdateMode::Restart],
            // Journal factories drop their journal tables; volatile factories
            // own nothing and implement erase as a documented no-op.
            erases_owned_state: true,
            ..Capabilities::default()
        },
    }
}

impl SqliteFactory {
    /// Parses and validates a request for this factory. No I/O.
    fn configuration(&self, request: &CreateRequest) -> Result<SourceConfig> {
        let mut configuration = request.configuration.clone();
        // `path: null` is the in-memory form; check it with its own rule
        // rather than as a type error.
        if let Some(object) = configuration.as_object_mut() {
            if object.get("path").is_some_and(serde_json::Value::is_null) {
                object.remove("path");
            }
        }
        metadata(self.0).configuration.validate(&configuration)?;
        config::definition(self.0)
            .details()
            .validate(&configuration)?;
        SourceConfig::from_value(self.0, &configuration)
    }

    fn graph<'a>(&self, request: &'a CreateRequest) -> Result<&'a str> {
        Ok(&request
            .scope
            .as_ref()
            .context("SQLite journal sources require graph scope")?
            .graph_id)
    }
}

impl Factory for SqliteFactory {
    fn metadata(&self) -> FactoryMetadata {
        metadata(self.0)
    }
    fn configuration_details(&self) -> Option<ConfigDetails> {
        Some(config::definition(self.0).details())
    }
    fn supports_update_planning(&self) -> bool {
        true
    }
    fn assess(&self, request: &UpdateRequest) -> Result<Assessment> {
        management::assess(self.0, request)
    }
    fn supports_source_progress(&self) -> bool {
        matches!(self.0, Mode::Replay)
    }
    fn supports_consumer_progress(&self) -> bool {
        matches!(self.0, Mode::Shared)
    }
    fn supports_bootstrap_coordination(&self) -> bool {
        self.0.journal()
    }
    fn create_bootstrap_coordinator(
        &self,
        request: &CreateRequest,
        progress: SourceBootstrapProgress,
    ) -> Result<Arc<dyn NativeSourceBootstrapCoordinator>> {
        let config = self.configuration(request)?;
        anyhow::ensure!(
            config.coordinated_snapshot,
            "SQLite coordinator requires coordinatedSnapshot: true"
        );
        anyhow::ensure!(
            matches!(
                (self.0, &progress),
                (Mode::Replay, SourceBootstrapProgress::Single(_))
                    | (Mode::Shared, SourceBootstrapProgress::Shared { .. })
            ),
            "SQLite coordinator progress mode does not match its source factory"
        );
        Ok(Arc::new(coordinator::Coordinator::new(
            request, config, progress,
        )?))
    }
    fn create(&self, request: &CreateRequest, control: ControlSender) -> Result<CreatedComponent> {
        anyhow::ensure!(
            !self.0.journal(),
            "SQLite journal factories require persistent source progress from the host"
        );
        let config = self.configuration(request)?;
        let source = SqliteSource::build(request.id.clone(), config, component::Progress::None)?
            .with_control(control);
        Ok(Component::Source(Box::new(source)).into())
    }
    fn create_with_progress(
        &self,
        request: &CreateRequest,
        control: ControlSender,
        progress: NativeSourceProgress,
    ) -> Result<CreatedComponent> {
        anyhow::ensure!(
            matches!(self.0, Mode::Replay),
            "SQLite factory does not accept single-owner progress"
        );
        let config = self.configuration(request)?;
        anyhow::ensure!(
            !config.coordinated_snapshot,
            "SQLite coordinated source requires its actual coordinator resource"
        );
        anyhow::ensure!(
            self.graph(request)? == progress.reader().graph_id(),
            "SQLite progress belongs to another graph"
        );
        let source = SqliteSource::build(
            request.id.clone(),
            config,
            component::Progress::Single(progress.reader()),
        )?
        .with_control(control);
        Ok(Component::Source(Box::new(source)).into())
    }
    fn create_with_consumer_progress(
        &self,
        request: &CreateRequest,
        control: ControlSender,
        progress: NativeConsumerProgress,
    ) -> Result<CreatedComponent> {
        anyhow::ensure!(
            matches!(self.0, Mode::Shared),
            "SQLite factory does not accept a consumer group"
        );
        let config = self.configuration(request)?;
        anyhow::ensure!(
            !config.coordinated_snapshot,
            "SQLite coordinated source requires its actual coordinator resource"
        );
        anyhow::ensure!(
            self.graph(request)? == progress.reader().graph_id(),
            "SQLite consumer group belongs to another graph"
        );
        let source = SqliteSource::with_consumers(request.id.clone(), config, progress)?
            .with_control(control);
        Ok(Component::Source(Box::new(source)).into())
    }
}

#[cfg(feature = "dynamic-plugin")]
drasi_computation_plugin_sdk::export_computation_plugin!(plugin());

#[cfg(test)]
mod tests;

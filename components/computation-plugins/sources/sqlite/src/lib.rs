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
    CreatedComponent, Factory, FactoryMetadata, HostServices, NativeSourceBootstrapCoordinator,
    PluginDefinition, ServiceBindings, SourceBootstrapCoordination, SourceBootstrapProgress,
    UpdatePlanner, UpdateRequest,
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
            // Outputs carry source replay progress, not a producer identity of their
            // own (Host admission, when bound, publishes under its durable identity).
            services: HostServices {
                source_progress: matches!(mode, Mode::Replay),
                consumer_progress: matches!(mode, Mode::Shared),
                bootstrap_coordination: mode.journal(),
                ..HostServices::default()
            },
            ..Capabilities::default()
        },
    }
}

/// The declared description (shared with the in-process wrapper): the
/// journal factories require durable progress; the others keep none.
pub fn describe(
    mode: Mode,
    descriptor: &drasi_computation_contracts::ComponentDescriptor,
) -> drasi_computation_plugin_sdk::ComponentDescription {
    use drasi_sqlite_common::native::SqliteProgress;
    drasi_sqlite_common::native::description(
        match mode {
            Mode::Replay => SqliteProgress::Single,
            Mode::Shared => SqliteProgress::Group,
            Mode::Changes | Mode::Transactions => SqliteProgress::None,
        },
        descriptor,
    )
}

/// The declared description for one configuration: the progress
/// requirements of [`describe`], the element identities every factory
/// produces (compared with a connected file snapshot), and, for a journal
/// factory with `coordinatedSnapshot: true`, the coordination protocol its
/// paired providers must use. Coordination is then required: the source
/// admits no live write before its coordinated snapshot.
pub fn describe_configuration(
    config: &SourceConfig,
    descriptor: &drasi_computation_contracts::ComponentDescriptor,
) -> Result<drasi_computation_plugin_sdk::ComponentDescription> {
    let keys: Vec<drasi_sqlite_common::TableKeyConfig> = config
        .settings
        .table_keys
        .iter()
        .map(|key| drasi_sqlite_common::TableKeyConfig {
            table: key.table.clone(),
            key_columns: key.key_columns.clone(),
        })
        .collect();
    let description = describe(config.mode, descriptor).with_element_identity(
        drasi_sqlite_common::native::element_identity(&config.settings.tables, &keys)?,
    );
    Ok(if config.mode.journal() && config.coordinated_snapshot {
        description.with_coordination(drasi_computation_plugin_sdk::BootstrapCoordination {
            protocol: drasi_sqlite_common::native::coordination_protocol(
                config.mode == Mode::Shared,
            ),
            required: true,
        })
    } else {
        description
    })
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

impl UpdatePlanner for SqliteFactory {
    fn assess(&self, request: &UpdateRequest) -> Result<Assessment> {
        management::assess(self.0, request)
    }
}

impl SourceBootstrapCoordination for SqliteFactory {
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
}

impl Factory for SqliteFactory {
    fn describe(
        &self,
        request: &drasi_computation_plugin_sdk::wire::DescribeRequest,
    ) -> anyhow::Result<drasi_computation_plugin_sdk::ComponentDescription> {
        describe_configuration(&self.configuration(&request.request)?, &request.descriptor)
    }
    fn metadata(&self) -> FactoryMetadata {
        metadata(self.0)
    }
    fn configuration_details(&self) -> ConfigDetails {
        config::definition(self.0).details()
    }
    fn update_planner(&self) -> Option<&dyn UpdatePlanner> {
        Some(self)
    }
    fn bootstrap_coordination(&self) -> Option<&dyn SourceBootstrapCoordination> {
        self.0
            .journal()
            .then_some(self as &dyn SourceBootstrapCoordination)
    }
    /// Each mode receives the progress its declaration binds: none for the
    /// volatile modes, one owner for replay and the consumer group for shared.
    fn create(
        &self,
        request: &CreateRequest,
        control: ControlSender,
        services: ServiceBindings,
    ) -> Result<CreatedComponent> {
        let config = self.configuration(request)?;
        let source = match self.0 {
            Mode::Changes | Mode::Transactions => {
                SqliteSource::build(request.id.clone(), config, component::Progress::None)?
            }
            Mode::Replay => {
                let progress = services.source_progress.context(
                    "SQLite journal factories require persistent source progress from the host",
                )?;
                anyhow::ensure!(
                    !config.coordinated_snapshot,
                    "SQLite coordinated source requires its actual coordinator resource"
                );
                anyhow::ensure!(
                    self.graph(request)? == progress.reader().graph_id(),
                    "SQLite progress belongs to another graph"
                );
                SqliteSource::build(
                    request.id.clone(),
                    config,
                    component::Progress::Single(progress.reader()),
                )?
            }
            Mode::Shared => {
                let progress = services.consumer_progress.context(
                    "SQLite journal factories require persistent source progress from the host",
                )?;
                anyhow::ensure!(
                    !config.coordinated_snapshot,
                    "SQLite coordinated source requires its actual coordinator resource"
                );
                anyhow::ensure!(
                    self.graph(request)? == progress.reader().graph_id(),
                    "SQLite consumer group belongs to another graph"
                );
                SqliteSource::with_consumers(request.id.clone(), config, progress)?
            }
        };
        Ok(Component::Source(Box::new(source.with_control(control))).into())
    }
}

#[cfg(feature = "dynamic-plugin")]
drasi_computation_plugin_sdk::export_computation_plugin!(plugin());

#[cfg(test)]
mod tests;

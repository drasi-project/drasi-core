// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Native PostgreSQL logical-replication source.
//!
//! Four factories stream a pgoutput publication: whole commits
//! (`drasi/postgres-transactions`), one envelope per row change
//! (`drasi.postgres/changes`), and shared variants of each for a fixed
//! consumer set. Each owns or borrows one replication slot, pauses at
//! transaction boundaries, reconnects after transport failures, supports
//! managed updates, and drops its managed slots on erasing removal.
//!
//! See `DESIGN.md` and `CONFIGURATION.md` in this crate.

pub mod bootstrap;
mod component;
pub mod config;
mod management;
mod rows;
mod shared;
mod whole;
mod worker;

use anyhow::{Context, Result};
use config::{Mode, SourceConfig};
use drasi_computation_contracts::{
    ComponentRole, ComponentUpdateMode, ImplementationIdentity, PortDescriptor, StreamId,
};
use drasi_computation_plugin_sdk::{
    lifecycle::Assessment, wire::DescribeRequest, ActivationCredentialContext, Capabilities,
    Component, ComponentDescription, ConfigDetails, ControlSender, CreateRequest, CreatedComponent,
    Factory, FactoryMetadata, NativeConsumerProgress, NativeSourceBootstrapCoordinator,
    NativeSourceProgress, PluginDefinition, SourceBootstrapProgress, UpdateRequest,
};
use std::sync::Arc;

pub use component::PostgresTransactionSource;
pub use drasi_postgres_common::{
    config::SslMode,
    replication::{PostgresTableKey, PostgresTransactionConfig, SlotIdentity, SlotOwnership},
};

pub const PLUGIN_ID: &str = env!("CARGO_PKG_NAME");
pub const POSTGRES_SOURCE: &str = "drasi/postgres-transactions";
pub const POSTGRES_ROWS: &str = "drasi.postgres/changes";
pub const POSTGRES_SHARED_ROWS: &str = "drasi.postgres/shared-changes";
pub const POSTGRES_SHARED_TRANSACTIONS: &str = "drasi.postgres/shared-transactions";

pub struct PostgresSourceFactory;
pub struct PostgresRowsFactory;
pub struct PostgresSharedRowsFactory;
pub struct PostgresSharedTransactionsFactory;

pub fn plugin() -> Result<PluginDefinition> {
    PluginDefinition::new(
        PLUGIN_ID,
        env!("CARGO_PKG_VERSION"),
        vec![
            Arc::new(PostgresSourceFactory),
            Arc::new(PostgresRowsFactory),
            Arc::new(PostgresSharedRowsFactory),
            Arc::new(PostgresSharedTransactionsFactory),
        ],
        vec![drasi_computation_contracts::GraphChangeCodec::schema()],
    )
}

#[cfg(feature = "dynamic-plugin")]
drasi_computation_plugin_sdk::export_computation_plugin!(plugin());

fn metadata(mode: Mode) -> FactoryMetadata {
    let (implementation, ports): (&str, Result<Vec<PortDescriptor>>) = match mode {
        Mode::Transactions => (
            POSTGRES_SOURCE,
            PostgresTransactionSource::transaction_ports(),
        ),
        Mode::Rows => (POSTGRES_ROWS, PostgresTransactionSource::row_ports()),
        Mode::SharedRows => (POSTGRES_SHARED_ROWS, PostgresTransactionSource::row_ports()),
        Mode::SharedTransactions => (
            POSTGRES_SHARED_TRANSACTIONS,
            PostgresTransactionSource::transaction_ports(),
        ),
    };
    FactoryMetadata {
        implementation: ImplementationIdentity::try_new(implementation, "1")
            .expect("constant implementation"),
        role: ComponentRole::Source,
        configuration_version: 1,
        configuration: config::definition(mode).schema(),
        ports: ports.expect("constant port descriptor"),
        completion: None,
        capabilities: Capabilities {
            management: true,
            update_modes: vec![ComponentUpdateMode::InPlace, ComponentUpdateMode::Restart],
            erases_owned_state: true,
            // Outputs carry source replay progress, not a producer identity of their
            // own (Host admission, when bound, publishes under its durable identity).
            ..Capabilities::default()
        },
    }
}

/// Parses and validates a factory request for `mode`.
fn parse(mode: Mode, request: &CreateRequest) -> Result<(SourceConfig, String)> {
    metadata(mode)
        .configuration
        .validate(&request.configuration)?;
    let config = SourceConfig::from_value(mode, &request.configuration)?;
    let scope = request
        .scope
        .as_ref()
        .context("PostgreSQL sources require graph scope")?;
    Ok((config, scope.graph_id.clone()))
}

fn credential_context(mode: Mode, request: &CreateRequest) -> Result<ActivationCredentialContext> {
    let config = SourceConfig::from_value(mode, &request.configuration)?;
    let context = ActivationCredentialContext {
        hostname: config.settings.host,
        port: config.settings.port,
    };
    context.validate()?;
    Ok(context)
}

/// The declared description: outputs carry only source replay progress,
/// kept in the progress bound to the source. A mode whose cursors must
/// survive a process restart requires that progress to be durable, so the
/// graph refuses wiring it to volatile progress before anything starts.
fn describe(mode: Mode, request: &DescribeRequest) -> Result<ComponentDescription> {
    let config = SourceConfig::from_value(mode, &request.request.configuration)?;
    let description = description(mode, config.recoverable(), &request.descriptor);
    // The source owns its paired providers' settings: it declares the
    // protocol they must use instead of their configuration repeating its own.
    Ok(match bootstrap::offer(mode, config.coordinated_snapshot) {
        Some(offer) => description.with_coordination(offer),
        None => description,
    })
}

/// The description of a `mode` source whose cursors are `recoverable` across
/// a process restart; in-process wrappers declare the same.
pub fn description(
    mode: Mode,
    recoverable: bool,
    descriptor: &drasi_computation_contracts::ComponentDescriptor,
) -> ComponentDescription {
    drasi_computation_plugin_sdk::wire::progress_source_description(
        descriptor,
        progress_dependency(mode),
        recoverable,
    )
}

/// The dependency the source's progress is bound to.
pub fn progress_dependency(mode: Mode) -> &'static str {
    match mode {
        Mode::Transactions | Mode::Rows => "source_progress",
        Mode::SharedRows | Mode::SharedTransactions => "consumer_progress",
    }
}

macro_rules! common_factory_methods {
    ($mode:expr) => {
        fn describe(&self, request: &DescribeRequest) -> Result<ComponentDescription> {
            describe($mode, request)
        }
        fn metadata(&self) -> FactoryMetadata {
            metadata($mode)
        }
        fn configuration_details(&self) -> Option<ConfigDetails> {
            Some(config::definition($mode).details())
        }
        fn supports_update_planning(&self) -> bool {
            true
        }
        fn assess(&self, request: &UpdateRequest) -> Result<Assessment> {
            management::assess($mode, request)
        }
        fn supports_activation_credentials(&self) -> bool {
            true
        }
        fn supports_bootstrap_coordination(&self) -> bool {
            true
        }
        fn create_bootstrap_coordinator(
            &self,
            request: &CreateRequest,
            progress: SourceBootstrapProgress,
        ) -> Result<Arc<dyn NativeSourceBootstrapCoordinator>> {
            let (config, _) = parse($mode, request)?;
            Ok(Arc::new(bootstrap::coordinator::Coordinator::new(
                request, $mode, config, progress,
            )?))
        }
        fn activation_credential_context(
            &self,
            request: &CreateRequest,
        ) -> Result<ActivationCredentialContext> {
            credential_context($mode, request)
        }
    };
}

impl Factory for PostgresSourceFactory {
    common_factory_methods!(Mode::Transactions);
    fn supports_source_progress(&self) -> bool {
        true
    }
    fn create(&self, _: &CreateRequest, _: ControlSender) -> Result<CreatedComponent> {
        anyhow::bail!("PostgreSQL source requires its actual persistent query progress owner")
    }
    fn create_with_progress(
        &self,
        request: &CreateRequest,
        _control: ControlSender,
        progress: NativeSourceProgress,
    ) -> Result<CreatedComponent> {
        let (config, graph) = parse(Mode::Transactions, request)?;
        let reader = progress.reader();
        anyhow::ensure!(
            reader.graph_id() == graph,
            "PostgreSQL progress belongs to another graph"
        );
        Ok(Component::Source(Box::new(PostgresTransactionSource::new(
            request.id.clone(),
            StreamId::try_new(config.stream)?,
            config.settings,
            reader,
            config.coordinated_snapshot,
        )?))
        .into())
    }
}

impl Factory for PostgresRowsFactory {
    common_factory_methods!(Mode::Rows);
    fn supports_source_progress(&self) -> bool {
        true
    }
    fn create(&self, _: &CreateRequest, _: ControlSender) -> Result<CreatedComponent> {
        anyhow::bail!("PostgreSQL rows require their actual query progress owner")
    }
    fn create_with_progress(
        &self,
        request: &CreateRequest,
        _control: ControlSender,
        progress: NativeSourceProgress,
    ) -> Result<CreatedComponent> {
        let (config, graph) = parse(Mode::Rows, request)?;
        let reader = progress.reader();
        anyhow::ensure!(
            reader.graph_id() == graph,
            "PostgreSQL row progress belongs to another graph"
        );
        Ok(Component::Source(Box::new(PostgresTransactionSource::rows(
            request.id.clone(),
            StreamId::try_new(config.stream)?,
            config.settings,
            reader,
            config.coordinated_snapshot,
            config.persistent,
        )?))
        .into())
    }
}

impl Factory for PostgresSharedRowsFactory {
    common_factory_methods!(Mode::SharedRows);
    fn supports_consumer_progress(&self) -> bool {
        true
    }
    fn create(&self, _: &CreateRequest, _: ControlSender) -> Result<CreatedComponent> {
        anyhow::bail!("shared PostgreSQL rows require their actual fixed consumer progress set")
    }
    fn create_with_consumer_progress(
        &self,
        request: &CreateRequest,
        _control: ControlSender,
        progress: NativeConsumerProgress,
    ) -> Result<CreatedComponent> {
        let (config, graph) = parse(Mode::SharedRows, request)?;
        let reader = progress.reader();
        anyhow::ensure!(
            reader.graph_id() == graph && reader.producer_id() == &request.id,
            "PostgreSQL consumer progress belongs to another graph or producer"
        );
        Ok(
            Component::Source(Box::new(PostgresTransactionSource::shared_rows(
                request.id.clone(),
                StreamId::try_new(config.stream)?,
                config.settings,
                reader,
                progress.consumer_ids(),
                config.persistent,
            )?))
            .into(),
        )
    }
}

impl Factory for PostgresSharedTransactionsFactory {
    common_factory_methods!(Mode::SharedTransactions);
    fn supports_consumer_progress(&self) -> bool {
        true
    }
    fn create(&self, _: &CreateRequest, _: ControlSender) -> Result<CreatedComponent> {
        anyhow::bail!("shared PostgreSQL transactions require their actual persistent consumer set")
    }
    fn create_with_consumer_progress(
        &self,
        request: &CreateRequest,
        _control: ControlSender,
        progress: NativeConsumerProgress,
    ) -> Result<CreatedComponent> {
        let (config, graph) = parse(Mode::SharedTransactions, request)?;
        anyhow::ensure!(
            progress.reader().graph_id() == graph && progress.reader().producer_id() == &request.id,
            "PostgreSQL transaction consumers belong to another graph or producer"
        );
        Ok(
            Component::Source(Box::new(PostgresTransactionSource::shared_transactions(
                request.id.clone(),
                StreamId::try_new(config.stream)?,
                config.settings,
                progress,
            )?))
            .into(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use drasi_computation_plugin_sdk::ExportedPlugin;

    #[test]
    fn factory_metadata_constructs_without_database_io() -> Result<()> {
        ExportedPlugin::new(plugin()?)?;
        #[cfg(feature = "dynamic-plugin")]
        assert!(!drasi_computation_plugin_metadata().is_null());
        for (name, metadata, ports) in [
            (
                POSTGRES_SOURCE,
                PostgresSourceFactory.metadata(),
                PostgresTransactionSource::transaction_ports()?,
            ),
            (
                POSTGRES_ROWS,
                PostgresRowsFactory.metadata(),
                PostgresTransactionSource::row_ports()?,
            ),
            (
                POSTGRES_SHARED_ROWS,
                PostgresSharedRowsFactory.metadata(),
                PostgresTransactionSource::row_ports()?,
            ),
            (
                POSTGRES_SHARED_TRANSACTIONS,
                PostgresSharedTransactionsFactory.metadata(),
                PostgresTransactionSource::transaction_ports()?,
            ),
        ] {
            assert_eq!(
                metadata.implementation,
                ImplementationIdentity::try_new(name, "1")?
            );
            assert_eq!(metadata.role, ComponentRole::Source);
            assert_eq!(metadata.completion, None);
            assert_eq!(metadata.ports, ports);
            assert_eq!(metadata.ports.len(), 1);
            assert_eq!(metadata.ports[0].id().as_str(), "out");
            assert_eq!(
                metadata.ports[0].direction(),
                drasi_computation_contracts::PortDirection::Output
            );
            assert_eq!(
                metadata.ports[0].schema(),
                drasi_computation_contracts::GraphChangeCodec::schema().descriptor()
            );
            assert!(metadata.configuration.fields["password"].secret);
            assert!(metadata.capabilities.management && metadata.capabilities.erases_owned_state);
        }
        let transaction_id = drasi_computation_contracts::ComponentId::generate();
        let transaction = PostgresTransactionSource::describe(transaction_id.clone())?;
        assert_eq!(transaction.id(), &transaction_id);
        assert_eq!(
            transaction.ports(),
            PostgresSourceFactory.metadata().ports.as_slice()
        );
        let row_id = drasi_computation_contracts::ComponentId::generate();
        let row = PostgresTransactionSource::describe_rows(row_id.clone())?;
        assert_eq!(row.id(), &row_id);
        assert_eq!(row.ports(), PostgresRowsFactory.metadata().ports.as_slice());
        assert!(PostgresSourceFactory.supports_source_progress());
        assert!(PostgresRowsFactory.supports_source_progress());
        assert!(PostgresSharedRowsFactory.supports_consumer_progress());
        assert!(PostgresSharedTransactionsFactory.supports_consumer_progress());
        Ok(())
    }
}

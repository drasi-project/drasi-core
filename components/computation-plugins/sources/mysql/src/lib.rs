// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Native MySQL binlog sources hosted in the caller's ComputationGraph.
//!
//! Seven factories stream MySQL row-based binary logs: untracked row changes
//! and transactions, whole transactions with query-owned or shared consumer
//! progress, shared row changes, and positional (`@N`) rows. Every factory
//! takes one flat camelCase configuration (shared with the native MySQL
//! bootstrap providers), pauses at transaction boundaries, reconnects after
//! transport failures, supports managed updates, and declares an idempotent
//! no-op erase because it owns no server-side state.
//!
//! See `DESIGN.md` and `CONFIGURATION.md` in this crate.

mod component;
pub mod config;
mod management;
#[cfg(test)]
mod tests;

use anyhow::{Context, Result};
use config::{Mode, SourceConfig};
use drasi_computation_contracts::{
    ComponentRole, ComponentUpdateMode, GraphChangeCodec, ImplementationIdentity, StreamId,
};
use drasi_computation_plugin_sdk::{
    lifecycle::Assessment, ActivationCredentialContext, Capabilities, Component, ConfigDetails,
    ControlSender, CreateRequest, CreatedComponent, Factory, FactoryMetadata,
    NativeConsumerProgress, NativeSourceProgress, PluginDefinition, UpdateRequest,
};
use drasi_mysql_common::native::{
    progress::ConsumerBinding,
    rows,
    source::{SourceStatus, StatusSink},
    MySqlSource,
};
use std::sync::Arc;

pub use component::MySqlComponent;
pub use drasi_mysql_common::native::{
    MySqlOutput, MySqlRetention, MySqlSourceSettings, MySqlTableKey, MySqlTlsMode, StartFrom,
};

pub const PLUGIN_ID: &str = env!("CARGO_PKG_NAME");
pub const MYSQL_CHANGES: &str = "drasi.mysql/changes";
pub const MYSQL_UNTRACKED_TRANSACTIONS: &str = "drasi.mysql/untracked-transactions";
pub const MYSQL_TRANSACTIONS: &str = "drasi.mysql/transactions";
pub const MYSQL_SHARED_TRANSACTIONS: &str = "drasi.mysql/shared-transactions";
pub const MYSQL_SHARED_CHANGES: &str = "drasi.mysql/shared-changes";
pub const MYSQL_POSITIONAL_CHANGES: &str = "drasi.mysql/positional-changes";
pub const MYSQL_SHARED_POSITIONAL_CHANGES: &str = "drasi.mysql/shared-positional-changes";

pub struct MySqlChangesFactory;
pub struct MySqlUntrackedTransactionsFactory;
pub struct MySqlTransactionsFactory;
pub struct MySqlSharedTransactionsFactory;
pub struct MySqlSharedChangesFactory;
pub struct MySqlPositionalChangesFactory;
pub struct MySqlSharedPositionalChangesFactory;

pub fn plugin() -> Result<PluginDefinition> {
    PluginDefinition::new(
        PLUGIN_ID,
        env!("CARGO_PKG_VERSION"),
        vec![
            Arc::new(MySqlChangesFactory),
            Arc::new(MySqlUntrackedTransactionsFactory),
            Arc::new(MySqlTransactionsFactory),
            Arc::new(MySqlSharedTransactionsFactory),
            Arc::new(MySqlSharedChangesFactory),
            Arc::new(MySqlPositionalChangesFactory),
            Arc::new(MySqlSharedPositionalChangesFactory),
        ],
        vec![GraphChangeCodec::schema()],
    )
}

#[cfg(feature = "dynamic-plugin")]
drasi_computation_plugin_sdk::export_computation_plugin!(plugin());

fn metadata(mode: Mode) -> FactoryMetadata {
    FactoryMetadata {
        implementation: ImplementationIdentity::try_new(mode.implementation(), "1")
            .expect("constant implementation"),
        role: ComponentRole::Source,
        configuration_version: 1,
        configuration: config::definition(mode).schema(),
        ports: MySqlSource::ports(mode.output()).expect("constant port descriptor"),
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
fn parse(mode: Mode, request: &CreateRequest) -> Result<SourceConfig> {
    metadata(mode)
        .configuration
        .validate(&request.configuration)?;
    SourceConfig::from_value(mode, &request.configuration)
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

/// Logs readiness changes with the component identity. The host's
/// `ControlSender` is bound only for components with a control interface,
/// which this source does not have.
fn status_sink(id: &drasi_computation_contracts::ComponentId) -> StatusSink {
    let id = id.clone();
    Arc::new(move |status| match status {
        SourceStatus::Ready => log::info!("Native MySQL source {id} is streaming"),
        SourceStatus::Reconnecting(error) => {
            log::warn!("Native MySQL source {id} is reconnecting: {error}")
        }
        SourceStatus::Failed(error) => log::error!("Native MySQL source {id} failed: {error}"),
    })
}

fn component(config: SourceConfig, source: MySqlSource) -> CreatedComponent {
    Component::Source(Box::new(MySqlComponent { config, source })).into()
}

/// An untracked source (`changes`, `untracked-transactions`,
/// `positional-changes`).
fn create_untracked(mode: Mode, request: &CreateRequest) -> Result<CreatedComponent> {
    let config = parse(mode, request)?;
    let mut source = MySqlSource::new(
        request.id.clone(),
        StreamId::try_new(config.stream.clone())?,
        config.engine_config(&request.id)?,
        None,
    )?;
    if mode.positional() {
        source = source.with_positional_metadata()?;
    }
    Ok(component(
        config,
        source.with_status(status_sink(&request.id)),
    ))
}

/// The consumer binding of a shared factory, checked against the graph.
fn consumers(
    request: &CreateRequest,
    progress: &NativeConsumerProgress,
) -> Result<ConsumerBinding> {
    let scope = request
        .scope
        .as_ref()
        .context("shared MySQL sources require graph scope")?;
    let view = progress.reader();
    anyhow::ensure!(
        view.graph_id() == scope.graph_id,
        "MySQL consumers belong to another graph"
    );
    ConsumerBinding::new(&request.id, view, progress.consumer_ids())
}

macro_rules! common_factory_methods {
    ($mode:expr) => {
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
        fn activation_credential_context(
            &self,
            request: &CreateRequest,
        ) -> Result<ActivationCredentialContext> {
            credential_context($mode, request)
        }
    };
}

impl Factory for MySqlChangesFactory {
    /// Outputs carry only source replay progress unless published through a Host admission channel.
    fn describe(
        &self,
        request: &drasi_computation_plugin_sdk::wire::DescribeRequest,
    ) -> anyhow::Result<drasi_computation_plugin_sdk::ComponentDescription> {
        Ok(request.source_description(
            drasi_computation_plugin_sdk::OutputDescription::UNTRACKED,
            drasi_computation_plugin_sdk::ProgressPersistence::Undeclared,
        ))
    }
    common_factory_methods!(Mode::Changes);
    fn create(&self, request: &CreateRequest, _: ControlSender) -> Result<CreatedComponent> {
        create_untracked(Mode::Changes, request)
    }
}

impl Factory for MySqlUntrackedTransactionsFactory {
    /// Outputs carry only source replay progress unless published through a Host admission channel.
    fn describe(
        &self,
        request: &drasi_computation_plugin_sdk::wire::DescribeRequest,
    ) -> anyhow::Result<drasi_computation_plugin_sdk::ComponentDescription> {
        Ok(request.source_description(
            drasi_computation_plugin_sdk::OutputDescription::UNTRACKED,
            drasi_computation_plugin_sdk::ProgressPersistence::Undeclared,
        ))
    }
    common_factory_methods!(Mode::UntrackedTransactions);
    fn create(&self, request: &CreateRequest, _: ControlSender) -> Result<CreatedComponent> {
        create_untracked(Mode::UntrackedTransactions, request)
    }
}

impl Factory for MySqlPositionalChangesFactory {
    /// Outputs carry only source replay progress unless published through a Host admission channel.
    fn describe(
        &self,
        request: &drasi_computation_plugin_sdk::wire::DescribeRequest,
    ) -> anyhow::Result<drasi_computation_plugin_sdk::ComponentDescription> {
        Ok(request.source_description(
            drasi_computation_plugin_sdk::OutputDescription::UNTRACKED,
            drasi_computation_plugin_sdk::ProgressPersistence::Undeclared,
        ))
    }
    common_factory_methods!(Mode::PositionalChanges);
    fn create(&self, request: &CreateRequest, _: ControlSender) -> Result<CreatedComponent> {
        create_untracked(Mode::PositionalChanges, request)
    }
}

impl Factory for MySqlTransactionsFactory {
    /// Outputs carry only source replay progress unless published through a Host admission channel.
    fn describe(
        &self,
        request: &drasi_computation_plugin_sdk::wire::DescribeRequest,
    ) -> anyhow::Result<drasi_computation_plugin_sdk::ComponentDescription> {
        Ok(request.source_description(
            drasi_computation_plugin_sdk::OutputDescription::UNTRACKED,
            drasi_computation_plugin_sdk::ProgressPersistence::Undeclared,
        ))
    }
    common_factory_methods!(Mode::Transactions);
    fn supports_source_progress(&self) -> bool {
        true
    }
    fn create(&self, _: &CreateRequest, _: ControlSender) -> Result<CreatedComponent> {
        anyhow::bail!("MySQL transactions require their actual query progress owner")
    }
    fn create_with_progress(
        &self,
        request: &CreateRequest,
        _: ControlSender,
        progress: NativeSourceProgress,
    ) -> Result<CreatedComponent> {
        let config = parse(Mode::Transactions, request)?;
        let scope = request
            .scope
            .as_ref()
            .context("MySQL transactions require graph scope")?;
        let reader = progress.reader();
        anyhow::ensure!(
            reader.graph_id() == scope.graph_id,
            "MySQL progress belongs to another graph"
        );
        let stream = StreamId::try_new(config.stream.clone())?;
        let engine = config.engine_config(&request.id)?;
        let source = if config.settings.start_from == StartFrom::Snapshot {
            MySqlSource::with_published_bootstrap(
                request.id.clone(),
                stream,
                engine,
                Some(reader),
                None,
            )?
        } else {
            MySqlSource::new(request.id.clone(), stream, engine, Some(reader))?
        };
        Ok(component(
            config,
            source.with_status(status_sink(&request.id)),
        ))
    }
}

impl Factory for MySqlSharedTransactionsFactory {
    /// Outputs carry only source replay progress unless published through a Host admission channel.
    fn describe(
        &self,
        request: &drasi_computation_plugin_sdk::wire::DescribeRequest,
    ) -> anyhow::Result<drasi_computation_plugin_sdk::ComponentDescription> {
        Ok(request.source_description(
            drasi_computation_plugin_sdk::OutputDescription::UNTRACKED,
            drasi_computation_plugin_sdk::ProgressPersistence::Undeclared,
        ))
    }
    common_factory_methods!(Mode::SharedTransactions);
    fn supports_consumer_progress(&self) -> bool {
        true
    }
    fn create(&self, _: &CreateRequest, _: ControlSender) -> Result<CreatedComponent> {
        anyhow::bail!("MySQL shared transactions require their actual fixed consumer set")
    }
    fn create_with_consumer_progress(
        &self,
        request: &CreateRequest,
        _: ControlSender,
        progress: NativeConsumerProgress,
    ) -> Result<CreatedComponent> {
        let config = parse(Mode::SharedTransactions, request)?;
        let consumers = consumers(request, &progress)?;
        let stream = StreamId::try_new(config.stream.clone())?;
        let engine = config.engine_config(&request.id)?;
        let source = if config.settings.start_from == StartFrom::Snapshot {
            MySqlSource::with_published_bootstrap(
                request.id.clone(),
                stream,
                engine,
                None,
                Some(consumers),
            )?
        } else {
            MySqlSource::new_with_consumer_progress(request.id.clone(), stream, engine, consumers)?
        };
        Ok(component(
            config,
            source.with_status(status_sink(&request.id)),
        ))
    }
}

/// A shared row source (`shared-changes`, `shared-positional-changes`).
fn create_shared_rows(
    mode: Mode,
    request: &CreateRequest,
    progress: &NativeConsumerProgress,
) -> Result<CreatedComponent> {
    let config = parse(mode, request)?;
    let consumers = consumers(request, progress)?;
    let mut source = MySqlSource::shared_rows(
        request.id.clone(),
        StreamId::try_new(config.stream.clone())?,
        config.engine_config(&request.id)?,
        consumers,
        rows::Options {
            persistent: config.persistent,
        },
    )?;
    if mode.positional() {
        source = source.with_positional_metadata()?;
    }
    Ok(component(
        config,
        source.with_status(status_sink(&request.id)),
    ))
}

impl Factory for MySqlSharedChangesFactory {
    /// Outputs carry only source replay progress unless published through a Host admission channel.
    fn describe(
        &self,
        request: &drasi_computation_plugin_sdk::wire::DescribeRequest,
    ) -> anyhow::Result<drasi_computation_plugin_sdk::ComponentDescription> {
        Ok(request.source_description(
            drasi_computation_plugin_sdk::OutputDescription::UNTRACKED,
            drasi_computation_plugin_sdk::ProgressPersistence::Undeclared,
        ))
    }
    common_factory_methods!(Mode::SharedChanges);
    fn supports_consumer_progress(&self) -> bool {
        true
    }
    fn create(&self, _: &CreateRequest, _: ControlSender) -> Result<CreatedComponent> {
        anyhow::bail!("MySQL shared rows require their actual fixed consumer set")
    }
    fn create_with_consumer_progress(
        &self,
        request: &CreateRequest,
        _: ControlSender,
        progress: NativeConsumerProgress,
    ) -> Result<CreatedComponent> {
        create_shared_rows(Mode::SharedChanges, request, &progress)
    }
}

impl Factory for MySqlSharedPositionalChangesFactory {
    /// Outputs carry only source replay progress unless published through a Host admission channel.
    fn describe(
        &self,
        request: &drasi_computation_plugin_sdk::wire::DescribeRequest,
    ) -> anyhow::Result<drasi_computation_plugin_sdk::ComponentDescription> {
        Ok(request.source_description(
            drasi_computation_plugin_sdk::OutputDescription::UNTRACKED,
            drasi_computation_plugin_sdk::ProgressPersistence::Undeclared,
        ))
    }
    common_factory_methods!(Mode::SharedPositionalChanges);
    fn supports_consumer_progress(&self) -> bool {
        true
    }
    fn create(&self, _: &CreateRequest, _: ControlSender) -> Result<CreatedComponent> {
        anyhow::bail!("positional MySQL replay requires its actual fixed consumer set")
    }
    fn create_with_consumer_progress(
        &self,
        request: &CreateRequest,
        _: ControlSender,
        progress: NativeConsumerProgress,
    ) -> Result<CreatedComponent> {
        create_shared_rows(Mode::SharedPositionalChanges, request, &progress)
    }
}

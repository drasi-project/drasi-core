// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Native MySQL sources hosted in the caller's ComputationGraph.

mod component;
#[cfg(test)]
mod tests;

use anyhow::{Context, Result};
use drasi_computation_contracts::{
    ComponentId, ComponentRole, GraphChangeCodec, ImplementationIdentity, StreamId,
};
use drasi_computation_plugin_sdk::{
    Capabilities, Component, ConfigSchema, ConfigType, ControlSender, CreateRequest,
    CreatedComponent, Factory, FactoryMetadata, NativeConsumerProgress, NativeSourceProgress,
    PluginDefinition,
};
use drasi_mysql_common::native::{progress::ConsumerBinding, MySqlSource};
use serde::Deserialize;
use std::sync::Arc;

pub use component::MySqlComponent;
pub use drasi_mysql_common::native::{
    MySqlConfig, MySqlConnectionConfig, MySqlOutput, MySqlRetention, MySqlStartPosition,
    MySqlTlsMode,
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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Configuration {
    stream: String,
    settings: MySqlConfig,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RowConfiguration {
    stream: String,
    settings: MySqlConfig,
    #[serde(default)]
    persistent: bool,
}

fn metadata(name: &str, output: MySqlOutput) -> FactoryMetadata {
    let mut configuration = ConfigSchema::from_fields([
        ("stream", ConfigType::String, true),
        ("settings", ConfigType::Object, true),
    ]);
    configuration
        .fields
        .get_mut("settings")
        .expect("settings")
        .secret = true;
    FactoryMetadata {
        implementation: ImplementationIdentity::try_new(name, "1").expect("implementation"),
        role: ComponentRole::Source,
        configuration_version: 1,
        configuration,
        ports: MySqlSource::describe(ComponentId::try_new("mysql").expect("id"), output)
            .expect("descriptor")
            .ports()
            .to_vec(),
        completion: None,
        capabilities: Capabilities::default(),
    }
}

fn configuration(
    request: &CreateRequest,
    output: MySqlOutput,
    replay: bool,
) -> Result<Configuration> {
    let value: Configuration = serde_json::from_value(request.configuration.clone())?;
    value.settings.validate()?;
    anyhow::ensure!(
        value.settings.output == output && value.settings.replay.is_some() == replay,
        "MySQL settings do not match this factory's output/recovery contract"
    );
    Ok(value)
}

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

impl Factory for MySqlChangesFactory {
    fn metadata(&self) -> FactoryMetadata {
        metadata(MYSQL_CHANGES, MySqlOutput::Changes)
    }

    fn create(&self, request: &CreateRequest, _: ControlSender) -> Result<CreatedComponent> {
        self.metadata()
            .configuration
            .validate(&request.configuration)?;
        let config = configuration(request, MySqlOutput::Changes, false)?;
        Ok(Component::Source(Box::new(MySqlComponent {
            source: MySqlSource::new(
                request.id.clone(),
                StreamId::try_new(config.stream)?,
                config.settings,
                None,
            )?,
        }))
        .into())
    }
}

impl Factory for MySqlPositionalChangesFactory {
    fn metadata(&self) -> FactoryMetadata {
        metadata(MYSQL_POSITIONAL_CHANGES, MySqlOutput::Changes)
    }
    fn create(&self, request: &CreateRequest, _: ControlSender) -> Result<CreatedComponent> {
        self.metadata()
            .configuration
            .validate(&request.configuration)?;
        let config = configuration(request, MySqlOutput::Changes, false)?;
        let source = MySqlSource::new(
            request.id.clone(),
            StreamId::try_new(config.stream)?,
            config.settings,
            None,
        )?
        .with_positional_metadata()?;
        Ok(Component::Source(Box::new(MySqlComponent { source })).into())
    }
}

impl Factory for MySqlSharedPositionalChangesFactory {
    fn metadata(&self) -> FactoryMetadata {
        let mut metadata = MySqlSharedChangesFactory.metadata();
        metadata.implementation =
            ImplementationIdentity::try_new(MYSQL_SHARED_POSITIONAL_CHANGES, "1")
                .expect("implementation");
        metadata
    }
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
        self.metadata()
            .configuration
            .validate(&request.configuration)?;
        let config: RowConfiguration = serde_json::from_value(request.configuration.clone())?;
        let scope = request
            .scope
            .as_ref()
            .context("positional MySQL rows require graph scope")?;
        let view = progress.reader();
        anyhow::ensure!(
            view.graph_id() == scope.graph_id,
            "MySQL consumers belong to another graph"
        );
        let consumers = ConsumerBinding::new(&request.id, view, progress.consumer_ids())?;
        let source = MySqlSource::shared_rows(
            request.id.clone(),
            StreamId::try_new(config.stream)?,
            config.settings,
            consumers,
            drasi_mysql_common::native::rows::Options {
                persistent: config.persistent,
            },
        )?
        .with_positional_metadata()?;
        Ok(Component::Source(Box::new(MySqlComponent { source })).into())
    }
}

impl Factory for MySqlUntrackedTransactionsFactory {
    fn metadata(&self) -> FactoryMetadata {
        metadata(MYSQL_UNTRACKED_TRANSACTIONS, MySqlOutput::Transactions)
    }
    fn create(&self, request: &CreateRequest, _: ControlSender) -> Result<CreatedComponent> {
        self.metadata()
            .configuration
            .validate(&request.configuration)?;
        let config = configuration(request, MySqlOutput::Transactions, false)?;
        Ok(Component::Source(Box::new(MySqlComponent {
            source: MySqlSource::new(
                request.id.clone(),
                StreamId::try_new(config.stream)?,
                config.settings,
                None,
            )?,
        }))
        .into())
    }
}

impl Factory for MySqlTransactionsFactory {
    fn metadata(&self) -> FactoryMetadata {
        metadata(MYSQL_TRANSACTIONS, MySqlOutput::Transactions)
    }
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
        self.metadata()
            .configuration
            .validate(&request.configuration)?;
        let config = configuration(request, MySqlOutput::Transactions, true)?;
        let scope = request
            .scope
            .as_ref()
            .context("MySQL transactions require graph scope")?;
        let reader = progress.reader();
        anyhow::ensure!(
            reader.graph_id() == scope.graph_id,
            "MySQL progress belongs to another graph"
        );
        let stream = StreamId::try_new(config.stream)?;
        let source = if matches!(config.settings.start, MySqlStartPosition::Snapshot { .. }) {
            MySqlSource::with_published_bootstrap(
                request.id.clone(),
                stream,
                config.settings,
                Some(reader),
                None,
            )?
        } else {
            MySqlSource::new(request.id.clone(), stream, config.settings, Some(reader))?
        };
        Ok(Component::Source(Box::new(MySqlComponent { source })).into())
    }
}

impl Factory for MySqlSharedTransactionsFactory {
    fn metadata(&self) -> FactoryMetadata {
        metadata(MYSQL_SHARED_TRANSACTIONS, MySqlOutput::Transactions)
    }
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
        self.metadata()
            .configuration
            .validate(&request.configuration)?;
        let config = configuration(request, MySqlOutput::Transactions, true)?;
        let scope = request
            .scope
            .as_ref()
            .context("MySQL shared transactions require graph scope")?;
        let view = progress.reader();
        anyhow::ensure!(
            view.graph_id() == scope.graph_id,
            "MySQL consumers belong to another graph"
        );
        let consumers = ConsumerBinding::new(&request.id, view, progress.consumer_ids())?;
        let stream = StreamId::try_new(config.stream)?;
        let source = if matches!(config.settings.start, MySqlStartPosition::Snapshot { .. }) {
            MySqlSource::with_published_bootstrap(
                request.id.clone(),
                stream,
                config.settings,
                None,
                Some(consumers),
            )?
        } else {
            MySqlSource::new_with_consumer_progress(
                request.id.clone(),
                stream,
                config.settings,
                consumers,
            )?
        };
        Ok(Component::Source(Box::new(MySqlComponent { source })).into())
    }
}

impl Factory for MySqlSharedChangesFactory {
    fn metadata(&self) -> FactoryMetadata {
        let mut metadata = metadata(MYSQL_SHARED_CHANGES, MySqlOutput::Changes);
        metadata.configuration.fields.insert(
            "persistent".into(),
            drasi_computation_plugin_sdk::ConfigField {
                value_type: ConfigType::Boolean,
                required: false,
                secret: false,
            },
        );
        metadata
    }
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
        self.metadata()
            .configuration
            .validate(&request.configuration)?;
        let config: RowConfiguration = serde_json::from_value(request.configuration.clone())?;
        let scope = request
            .scope
            .as_ref()
            .context("MySQL shared rows require graph scope")?;
        let view = progress.reader();
        anyhow::ensure!(
            view.graph_id() == scope.graph_id,
            "MySQL row consumers belong to another graph"
        );
        let consumers = ConsumerBinding::new(&request.id, view, progress.consumer_ids())?;
        let source = MySqlSource::shared_rows(
            request.id.clone(),
            StreamId::try_new(config.stream)?,
            config.settings,
            consumers,
            drasi_mysql_common::native::rows::Options {
                persistent: config.persistent,
            },
        )?;
        Ok(Component::Source(Box::new(MySqlComponent { source })).into())
    }
}

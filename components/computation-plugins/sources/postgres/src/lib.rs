// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

mod component;
mod rows;

use anyhow::{Context, Result};
use drasi_computation_contracts::{
    ComponentId, ComponentRole, ImplementationIdentity, SourceTransactionCodec, StreamId,
};
use drasi_computation_plugin_sdk::{
    Capabilities, Component, ConfigSchema, ConfigType, ControlSender, CreateRequest,
    CreatedComponent, Factory, FactoryMetadata, NativeSourceProgress, PluginDefinition,
};
use serde::Deserialize;
use std::sync::Arc;

pub use component::PostgresTransactionSource;
pub use drasi_postgres_common::{
    config::{PostgresSourceConfig, SslMode, TableKeyConfig},
    replication::PostgresTransactionConfig,
};
pub use rows::RowSourceOptions;

pub const PLUGIN_ID: &str = env!("CARGO_PKG_NAME");
pub const POSTGRES_SOURCE: &str = "drasi/postgres-transactions";
pub const POSTGRES_ROWS: &str = "drasi.postgres/changes";

pub struct PostgresSourceFactory;
pub struct PostgresRowsFactory;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Configuration {
    stream: String,
    settings: PostgresTransactionConfig,
    #[serde(default)]
    coordinated_snapshot: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RowConfiguration {
    stream: String,
    settings: PostgresTransactionConfig,
    #[serde(default)]
    coordinated_snapshot: bool,
    #[serde(default)]
    persistent: bool,
    auto_create_slot: Option<bool>,
    start_from_slot: Option<bool>,
    reconnect_attempts: Option<u32>,
    reconnect_delay_ms: Option<u64>,
}

pub fn plugin() -> Result<PluginDefinition> {
    PluginDefinition::new(
        PLUGIN_ID,
        env!("CARGO_PKG_VERSION"),
        vec![
            Arc::new(PostgresSourceFactory),
            Arc::new(PostgresRowsFactory),
        ],
        vec![
            SourceTransactionCodec::schema(),
            drasi_computation_contracts::GraphChangeCodec::schema(),
        ],
    )
}

impl Factory for PostgresRowsFactory {
    fn metadata(&self) -> FactoryMetadata {
        let mut metadata = PostgresSourceFactory.metadata();
        metadata.implementation =
            ImplementationIdentity::try_new(POSTGRES_ROWS, "1").expect("constant implementation");
        metadata.ports =
            PostgresTransactionSource::describe_rows(ComponentId::try_new("postgres").expect("id"))
                .expect("row descriptor")
                .ports()
                .to_vec();
        for name in ["persistent", "auto_create_slot", "start_from_slot"] {
            metadata.configuration.fields.insert(
                name.into(),
                drasi_computation_plugin_sdk::ConfigField {
                    value_type: ConfigType::Boolean,
                    required: false,
                    secret: false,
                },
            );
        }
        for name in ["reconnect_attempts", "reconnect_delay_ms"] {
            metadata.configuration.fields.insert(
                name.into(),
                drasi_computation_plugin_sdk::ConfigField {
                    value_type: ConfigType::Integer,
                    required: false,
                    secret: false,
                },
            );
        }
        metadata
    }
    fn supports_source_progress(&self) -> bool {
        true
    }
    fn create(
        &self,
        _request: &CreateRequest,
        _control: ControlSender,
    ) -> Result<CreatedComponent> {
        anyhow::bail!("PostgreSQL rows require their actual query progress owner")
    }
    fn create_with_progress(
        &self,
        request: &CreateRequest,
        _control: ControlSender,
        progress: NativeSourceProgress,
    ) -> Result<CreatedComponent> {
        self.metadata()
            .configuration
            .validate(&request.configuration)?;
        let config: RowConfiguration = serde_json::from_value(request.configuration.clone())?;
        let scope = request
            .scope
            .as_ref()
            .context("PostgreSQL rows require graph scope")?;
        let reader = progress.reader();
        anyhow::ensure!(
            reader.graph_id() == scope.graph_id,
            "PostgreSQL row progress belongs to another graph"
        );
        let recoverable = config.persistent || config.coordinated_snapshot;
        Ok(Component::Source(Box::new(PostgresTransactionSource::rows(
            request.id.clone(),
            StreamId::try_new(config.stream)?,
            config.settings,
            reader,
            config.coordinated_snapshot,
            RowSourceOptions {
                persistent: config.persistent,
                auto_create_slot: config.auto_create_slot.unwrap_or(!recoverable),
                start_from_slot: config.start_from_slot.unwrap_or(!recoverable),
                reconnect_attempts: config.reconnect_attempts.unwrap_or(1),
                reconnect_delay_ms: config.reconnect_delay_ms.unwrap_or(5000),
            },
        )?))
        .into())
    }
}

#[cfg(feature = "dynamic-plugin")]
drasi_computation_plugin_sdk::export_computation_plugin!(plugin());

impl Factory for PostgresSourceFactory {
    fn metadata(&self) -> FactoryMetadata {
        let mut configuration = ConfigSchema::from_fields([
            ("stream", ConfigType::String, true),
            ("settings", ConfigType::Object, true),
            ("coordinated_snapshot", ConfigType::Boolean, false),
        ]);
        configuration
            .fields
            .get_mut("settings")
            .expect("settings")
            .secret = true;
        FactoryMetadata {
            implementation: ImplementationIdentity::try_new(POSTGRES_SOURCE, "1")
                .expect("constant implementation"),
            role: ComponentRole::Source,
            configuration_version: 1,
            configuration,
            ports: PostgresTransactionSource::describe(
                ComponentId::try_new("postgres").expect("constant id"),
            )
            .expect("source descriptor")
            .ports()
            .to_vec(),
            completion: None,
            capabilities: Capabilities::default(),
        }
    }

    fn supports_source_progress(&self) -> bool {
        true
    }

    fn create(
        &self,
        _request: &CreateRequest,
        _control: ControlSender,
    ) -> Result<CreatedComponent> {
        anyhow::bail!("PostgreSQL source requires its actual persistent query progress owner")
    }

    fn create_with_progress(
        &self,
        request: &CreateRequest,
        _control: ControlSender,
        progress: NativeSourceProgress,
    ) -> Result<CreatedComponent> {
        self.metadata()
            .configuration
            .validate(&request.configuration)?;
        let config: Configuration = serde_json::from_value(request.configuration.clone())?;
        let scope = request
            .scope
            .as_ref()
            .context("PostgreSQL source requires graph scope")?;
        let reader = progress.reader();
        anyhow::ensure!(
            reader.graph_id() == scope.graph_id,
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

#[cfg(test)]
mod tests {
    use super::*;
    use drasi_computation_plugin_sdk::ExportedPlugin;

    #[test]
    fn source_factory_declares_only_transaction_schema_and_real_progress() -> Result<()> {
        ExportedPlugin::new(plugin()?)?;
        let metadata = PostgresSourceFactory.metadata();
        assert_eq!(metadata.ports.len(), 1);
        assert_eq!(
            metadata.ports[0].schema(),
            SourceTransactionCodec::schema().descriptor()
        );
        assert!(metadata.configuration.fields["settings"].secret);
        assert!(PostgresSourceFactory.supports_source_progress());
        Ok(())
    }
}

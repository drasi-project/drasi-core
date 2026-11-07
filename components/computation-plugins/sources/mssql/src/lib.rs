// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

mod source;
use anyhow::{Context, Result};
use drasi_computation_contracts::{
    ComponentRole, GraphChangeCodec, ImplementationIdentity, StreamId,
};
use drasi_computation_plugin_sdk::{
    Capabilities, Component, ConfigSchema, ConfigType, ControlSender, CreateRequest,
    CreatedComponent, Factory, FactoryMetadata, NativeConsumerProgress, PluginDefinition,
};
pub use drasi_mssql_common::native::NativeConfig as MsSqlConfig;
use serde::{Deserialize, Serialize};
pub use source::MsSqlSource;
use std::sync::Arc;

pub const PLUGIN_ID: &str = env!("CARGO_PKG_NAME");
pub const MSSQL_SOURCE: &str = "drasi.mssql/changes";
pub struct MsSqlSourceFactory;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Configuration {
    pub stream: String,
    pub settings: MsSqlConfig,
}

pub fn plugin() -> Result<PluginDefinition> {
    PluginDefinition::new(
        PLUGIN_ID,
        env!("CARGO_PKG_VERSION"),
        vec![Arc::new(MsSqlSourceFactory)],
        vec![GraphChangeCodec::schema()],
    )
}

impl Factory for MsSqlSourceFactory {
    fn metadata(&self) -> FactoryMetadata {
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
            implementation: ImplementationIdentity::try_new(MSSQL_SOURCE, "1")
                .expect("implementation"),
            role: ComponentRole::Source,
            configuration_version: 1,
            configuration,
            ports: MsSqlSource::ports().expect("constant port descriptor"),
            completion: None,
            capabilities: Capabilities::default(),
        }
    }
    fn supports_consumer_progress(&self) -> bool {
        true
    }
    fn create(&self, _: &CreateRequest, _: ControlSender) -> Result<CreatedComponent> {
        anyhow::bail!("SQL Server source requires its actual consumer-progress binding")
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
        let configuration: Configuration = serde_json::from_value(request.configuration.clone())?;
        let scope = request
            .scope
            .as_ref()
            .context("SQL Server source requires graph scope")?;
        let reader = progress.reader();
        anyhow::ensure!(
            reader.graph_id() == scope.graph_id && reader.producer_id() == &request.id,
            "SQL Server consumer scope mismatch"
        );
        Ok(Component::Source(Box::new(MsSqlSource::new(
            request.id.clone(),
            StreamId::try_new(configuration.stream)?,
            configuration.settings,
            reader,
            progress.consumer_ids(),
        )?))
        .into())
    }
}

#[cfg(feature = "dynamic-plugin")]
drasi_computation_plugin_sdk::export_computation_plugin!(plugin());

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn factory_metadata_constructs_without_database_io() -> Result<()> {
        drasi_computation_plugin_sdk::ExportedPlugin::new(plugin()?)?;
        #[cfg(feature = "dynamic-plugin")]
        assert!(!drasi_computation_plugin_metadata().is_null());
        let metadata = MsSqlSourceFactory.metadata();
        assert_eq!(metadata.role, ComponentRole::Source);
        assert_eq!(metadata.completion, None);
        assert_eq!(metadata.ports, MsSqlSource::ports()?);
        assert_eq!(metadata.ports.len(), 1);
        assert_eq!(metadata.ports[0].id().as_str(), "out");
        assert_eq!(
            metadata.ports[0].direction(),
            drasi_computation_contracts::PortDirection::Output
        );
        assert_eq!(
            metadata.ports[0].schema(),
            GraphChangeCodec::schema().descriptor()
        );
        let id = drasi_computation_contracts::ComponentId::generate();
        let descriptor = MsSqlSource::describe(id.clone())?;
        assert_eq!(descriptor.id(), &id);
        assert_eq!(descriptor.ports(), metadata.ports.as_slice());
        assert!(MsSqlSourceFactory.supports_consumer_progress());
        assert!(metadata.configuration.fields["settings"].secret);
        Ok(())
    }
}

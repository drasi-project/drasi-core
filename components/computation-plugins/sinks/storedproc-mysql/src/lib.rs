// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Native stored-procedure delivery with positional MySQL parameters.

mod component;
mod config;
#[cfg(test)]
mod tests;

use anyhow::Result;
use drasi_computation_contracts::{
    ComponentId, ComponentRole, ImplementationIdentity, QueryChangeCodec, SinkCompletion,
};
use drasi_computation_plugin_sdk::{
    ActivationCredentialContext, Capabilities, Component, ConfigSchema, ConfigType, ControlSender,
    CreateRequest, CreatedComponent, Factory, FactoryMetadata, PluginDefinition,
};
use serde::Deserialize;
use std::sync::Arc;

pub use component::MySqlStoredProcSink;
pub use config::MySqlStoredProcConfig;
pub use drasi_reaction_common::{QueryConfig, TemplateSpec};
pub const PLUGIN_ID: &str = env!("CARGO_PKG_NAME");
pub const MYSQL_STOREDPROC: &str = "drasi.mysql/storedproc";
pub struct MySqlStoredProcFactory;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Configuration {
    settings: MySqlStoredProcConfig,
}

pub fn plugin() -> Result<PluginDefinition> {
    PluginDefinition::new(
        PLUGIN_ID,
        env!("CARGO_PKG_VERSION"),
        vec![Arc::new(MySqlStoredProcFactory)],
        vec![QueryChangeCodec::schema()],
    )
}
#[cfg(feature = "dynamic-plugin")]
drasi_computation_plugin_sdk::export_computation_plugin!(plugin());

impl Factory for MySqlStoredProcFactory {
    fn supports_activation_credentials(&self) -> bool {
        true
    }
    fn activation_credential_context(
        &self,
        request: &CreateRequest,
    ) -> Result<ActivationCredentialContext> {
        let config: Configuration = serde_json::from_value(request.configuration.clone())?;
        let context = ActivationCredentialContext {
            hostname: config.settings.hostname,
            port: config.settings.port,
        };
        context.validate()?;
        Ok(context)
    }
    fn metadata(&self) -> FactoryMetadata {
        let mut configuration = ConfigSchema::from_fields([("settings", ConfigType::Object, true)]);
        configuration
            .fields
            .get_mut("settings")
            .expect("settings")
            .secret = true;
        FactoryMetadata {
            implementation: ImplementationIdentity::try_new(MYSQL_STOREDPROC, "1")
                .expect("implementation"),
            role: ComponentRole::Sink,
            configuration_version: 1,
            configuration,
            ports: MySqlStoredProcSink::describe(ComponentId::try_new("mysql").expect("id"))
                .expect("descriptor")
                .ports()
                .to_vec(),
            completion: Some(SinkCompletion::Handled),
            capabilities: Capabilities::default(),
        }
    }
    fn create(&self, request: &CreateRequest, _: ControlSender) -> Result<CreatedComponent> {
        self.metadata()
            .configuration
            .validate(&request.configuration)?;
        let config: Configuration = serde_json::from_value(request.configuration.clone())?;
        Ok(Component::Sink(Box::new(
            MySqlStoredProcSink::new_with_activation_credentials(
                request.id.clone(),
                config.settings,
                request.activation_credentials_bound,
            )?,
        ))
        .into())
    }
}

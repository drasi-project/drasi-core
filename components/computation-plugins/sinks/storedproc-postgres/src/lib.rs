// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

mod component;
mod config;

use anyhow::Result;
use drasi_computation_contracts::{
    ComponentId, ComponentRole, ImplementationIdentity, QueryChangeCodec, SinkCompletion,
};
use drasi_computation_plugin_sdk::{
    Capabilities, Component, ConfigSchema, ConfigType, ConsumerMode, ControlSender, CreateRequest,
    CreatedComponent, Factory, FactoryMetadata, PluginDefinition,
};
use serde::Deserialize;
use std::sync::Arc;

pub use component::PostgresStoredProcSink;
pub use config::PostgresStoredProcConfig;
pub use drasi_reaction_common::{QueryConfig, TemplateSpec};

pub const PLUGIN_ID: &str = env!("CARGO_PKG_NAME");
pub const POSTGRES_SINK: &str = "drasi.postgres/storedproc";

pub struct PostgresStoredProcFactory;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Configuration {
    settings: PostgresStoredProcConfig,
}

pub fn plugin() -> Result<PluginDefinition> {
    PluginDefinition::new(
        PLUGIN_ID,
        env!("CARGO_PKG_VERSION"),
        vec![Arc::new(PostgresStoredProcFactory)],
        vec![QueryChangeCodec::schema()],
    )
}

#[cfg(feature = "dynamic-plugin")]
drasi_computation_plugin_sdk::export_computation_plugin!(plugin());

impl Factory for PostgresStoredProcFactory {
    fn metadata(&self) -> FactoryMetadata {
        let mut configuration = ConfigSchema::from_fields([("settings", ConfigType::Object, true)]);
        configuration
            .fields
            .get_mut("settings")
            .expect("settings")
            .secret = true;
        FactoryMetadata {
            implementation: ImplementationIdentity::try_new(POSTGRES_SINK, "1")
                .expect("constant implementation"),
            role: ComponentRole::Sink,
            configuration_version: 1,
            configuration,
            ports: PostgresStoredProcSink::describe(
                ComponentId::try_new("postgres").expect("constant id"),
            )
            .expect("sink descriptor")
            .ports()
            .to_vec(),
            completion: Some(SinkCompletion::Handled),
            capabilities: Capabilities::default(),
        }
    }
    fn consumer_mode(&self) -> Option<ConsumerMode> {
        Some(ConsumerMode::External)
    }
    fn create(&self, request: &CreateRequest, _control: ControlSender) -> Result<CreatedComponent> {
        self.metadata()
            .configuration
            .validate(&request.configuration)?;
        let config: Configuration = serde_json::from_value(request.configuration.clone())?;
        Ok(Component::Consumer(Box::new(PostgresStoredProcSink::new(
            request.id.clone(),
            config.settings,
        )?))
        .into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use drasi_computation_plugin_sdk::{ExportedPlugin, NativeComponent};
    use serde_json::json;

    fn config() -> serde_json::Value {
        json!({
            "hostname": "no-database-is-opened-at-construction",
            "database": "test", "user": "user", "password": "secret",
            "queries": ["source.people"],
            "default_template": { "added": {"template": "CALL add_user({{param after.id}})"} }
        })
    }

    #[test]
    fn factory_declares_destination_completion_and_configuration_roundtrips() -> Result<()> {
        ExportedPlugin::new(plugin()?)?;
        assert_eq!(
            PostgresStoredProcFactory.consumer_mode(),
            Some(ConsumerMode::External)
        );
        assert!(PostgresStoredProcFactory.metadata().configuration.fields["settings"].secret);
        let sink = PostgresStoredProcSink::new(
            ComponentId::try_new("sink")?,
            serde_json::from_value(config())?,
        )?;
        let configuration = sink.configuration()?;
        assert_eq!(configuration["settings"]["password"], "secret");
        assert_eq!(configuration["settings"]["port"], 5432);
        assert_eq!(configuration["settings"]["command_timeout_ms"], 30_000);
        serde_json::from_value::<PostgresStoredProcConfig>(configuration["settings"].clone())?
            .validate()?;
        Ok(())
    }

    #[test]
    fn configuration_rejects_unsafe_sql_unknown_routes_and_invalid_bounds() {
        for (field, value) in [
            ("queries", json!([])),
            ("port", json!(0)),
            ("command_timeout_ms", json!(0)),
            ("shutdown_timeout_ms", json!(0)),
            ("connect_timeout_ms", json!(0)),
            (
                "default_template",
                json!({"added": {"template": "CALL x({{after.id}})"}}),
            ),
            (
                "routes",
                json!({"other": {"added": {"template": "CALL x()"}}}),
            ),
            ("tls_ca_pem", json!("certificate-with-ssl-disabled")),
            ("retry_attempts", json!(3)),
        ] {
            let mut value_config = config();
            value_config[field] = value;
            let result = serde_json::from_value::<PostgresStoredProcConfig>(value_config)
                .map_err(anyhow::Error::from)
                .and_then(|config| config.validate());
            assert!(result.is_err(), "{field}");
        }
    }
}

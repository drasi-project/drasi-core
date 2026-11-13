// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

mod component;
mod config;

use anyhow::Result;
use drasi_computation_contracts::{
    ComponentRole, GraphChangeCodec, ImplementationIdentity, SinkCompletion,
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
        vec![GraphChangeCodec::schema()],
    )
}

#[cfg(feature = "dynamic-plugin")]
drasi_computation_plugin_sdk::export_computation_plugin!(plugin());

impl Factory for PostgresStoredProcFactory {
    fn metadata(&self) -> FactoryMetadata {
        let mut configuration = ConfigSchema::from_fields([("settings", ConfigType::Object, true)]);
        configuration.constrain(
            "settings",
            drasi_computation_contracts::ConfigurationConstraint::try_new(serde_json::json!({
                "type": "object",
                "properties": {
                    "outputs": drasi_computation_contracts::ConfigurationConstraint::output_aliases().schema(),
                },
            })).expect("constant stored procedure identity schema"),
        ).expect("settings field");
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
            ports: PostgresStoredProcSink::ports().expect("constant port descriptor"),
            completion: Some(SinkCompletion::Handled),
            capabilities: Capabilities {
                binds_inputs: true,
                ..Capabilities::default()
            },
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
            &request.inputs,
        )?))
        .into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use drasi_computation_contracts::{ComponentId, PortDirection};
    use drasi_computation_plugin_sdk::{ExportedPlugin, NativeComponent};
    use serde_json::json;

    const OUTPUT_COMPONENT_ID: &str = "6439b52f-06c0-5e90-8905-d736f18e34ee";

    fn config() -> serde_json::Value {
        json!({
            "hostname": "no-database-is-opened-at-construction",
            "database": "test", "user": "user", "password": "secret",
            "outputs": {"people": {"component": OUTPUT_COMPONENT_ID, "port": "out"}},
            "default_template": { "added": {"template": "CALL add_user({{param after.id}})"} }
        })
    }

    fn inputs() -> Result<Vec<drasi_computation_contracts::InputBinding>> {
        use drasi_computation_contracts::{InputBinding, OutputEndpoint, PortId, StreamId};
        Ok(vec![InputBinding {
            input_port: PortId::try_new("in")?,
            output: OutputEndpoint::new(
                ComponentId::try_new(OUTPUT_COMPONENT_ID)?,
                PortId::try_new("out")?,
            ),
            stream: StreamId::try_new("people/out")?,
        }])
    }

    #[test]
    fn factory_metadata_constructs_without_database_io() -> Result<()> {
        ExportedPlugin::new(plugin()?)?;
        #[cfg(feature = "dynamic-plugin")]
        assert!(!drasi_computation_plugin_metadata().is_null());
        let metadata = PostgresStoredProcFactory.metadata();
        assert_eq!(metadata.role, ComponentRole::Sink);
        assert_eq!(metadata.completion, Some(SinkCompletion::Handled));
        assert_eq!(metadata.ports, PostgresStoredProcSink::ports()?);
        assert_eq!(metadata.ports.len(), 1);
        assert_eq!(metadata.ports[0].id().as_str(), "in");
        assert_eq!(metadata.ports[0].direction(), PortDirection::Input);
        assert_eq!(
            metadata.ports[0].schema(),
            GraphChangeCodec::schema().descriptor()
        );
        let id = ComponentId::generate();
        let descriptor = PostgresStoredProcSink::describe(id.clone())?;
        assert_eq!(descriptor.id(), &id);
        assert_eq!(descriptor.ports(), metadata.ports.as_slice());
        assert!(metadata.configuration.fields["settings"].secret);
        Ok(())
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
            ComponentId::try_new("a21a4fde-f642-54e1-93cf-bb9e177949d7")?,
            serde_json::from_value(config())?,
            &inputs()?,
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
            (
                "outputs",
                json!({"bad id": {"component": OUTPUT_COMPONENT_ID}}),
            ),
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
                .and_then(|config| {
                    config.validate()?;
                    let outputs = drasi_computation_contracts::SinkOutputs::from_inputs(
                        &inputs()?,
                        &config.outputs,
                    )?;
                    config.validate_outputs(&outputs)
                });
            assert!(result.is_err(), "{field}");
        }
    }

    #[test]
    fn a_sink_without_declared_inputs_fails_clearly() -> Result<()> {
        assert!(PostgresStoredProcSink::new(
            ComponentId::try_new("a21a4fde-f642-54e1-93cf-bb9e177949d7")?,
            serde_json::from_value(config())?,
            &[],
        )
        .is_err());
        Ok(())
    }
}

// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

mod component;

use drasi_computation_contracts::{
    ComponentRole, GraphChangeCodec, ImplementationIdentity, PipeRequirements, PortDescriptor,
    PortDirection, PortId, SinkCompletion, SinkOutputs,
};
use drasi_computation_plugin_sdk::{
    Capabilities, Component, ControlSender, CreateRequest, CreatedComponent, Factory,
    FactoryMetadata, PluginDefinition,
};
use std::sync::Arc;

pub use component::LogSinkConfig;

pub const PLUGIN_ID: &str = env!("CARGO_PKG_NAME");
pub const LOG_SINK: &str = "drasi.standard/log";

pub struct LogSinkFactory;

pub fn plugin() -> anyhow::Result<PluginDefinition> {
    PluginDefinition::new(
        PLUGIN_ID,
        env!("CARGO_PKG_VERSION"),
        vec![Arc::new(LogSinkFactory)],
        vec![GraphChangeCodec::schema()],
    )
}

#[cfg(feature = "dynamic-plugin")]
drasi_computation_plugin_sdk::export_computation_plugin!(plugin());

impl Factory for LogSinkFactory {
    fn configuration_details(&self) -> Option<drasi_computation_plugin_sdk::ConfigDetails> {
        Some(component::configuration_definition().details())
    }

    fn metadata(&self) -> FactoryMetadata {
        FactoryMetadata {
            implementation: ImplementationIdentity::try_new(LOG_SINK, "1").expect("constant"),
            role: ComponentRole::Sink,
            configuration_version: 1,
            configuration: component::configuration_definition().schema(),
            ports: vec![PortDescriptor::new(
                PortId::try_new("in").expect("constant"),
                PortDirection::Input,
                GraphChangeCodec::schema().descriptor().clone(),
                PipeRequirements::default(),
            )],
            completion: Some(SinkCompletion::Handled),
            capabilities: Capabilities {
                binds_inputs: true,
                ..Capabilities::default()
            },
        }
    }

    fn create(
        &self,
        request: &CreateRequest,
        _control: ControlSender,
    ) -> anyhow::Result<CreatedComponent> {
        let config = LogSinkConfig::parse(request.configuration.clone())?;
        let outputs = SinkOutputs::from_inputs(&request.inputs, &config.outputs)?;
        Ok(Component::Sink(Box::new(component::LogSink::new(
            self.metadata().descriptor(request.id.clone())?,
            config,
            outputs,
        )?))
        .into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use drasi_computation_plugin_sdk::ExportedPlugin;

    #[test]
    fn factory_exports_a_handled_native_sink() -> anyhow::Result<()> {
        ExportedPlugin::new(plugin()?)?;
        let metadata = LogSinkFactory.metadata();
        assert_eq!(metadata.role, ComponentRole::Sink);
        assert_eq!(metadata.completion, Some(SinkCompletion::Handled));
        assert!(metadata.capabilities.binds_inputs);
        Ok(())
    }
}

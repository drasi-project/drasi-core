// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

mod component;

use drasi_computation_contracts::{
    ComponentRole, GraphChangeCodec, ImplementationIdentity, PipeRequirements, PortDescriptor,
    PortDirection, PortId, SinkCompletion,
};
use drasi_computation_plugin_sdk::{
    Capabilities, Component, ControlSender, CreateRequest, CreatedComponent, Factory,
    FactoryMetadata, PluginDefinition,
};
use std::sync::Arc;

pub use component::{ProfilerSinkConfig, REPORT_TARGET};

pub const PLUGIN_ID: &str = env!("CARGO_PKG_NAME");
pub const PROFILER_SINK: &str = "drasi.standard/profiler";

pub struct ProfilerSinkFactory;

pub fn plugin() -> anyhow::Result<PluginDefinition> {
    PluginDefinition::new(
        PLUGIN_ID,
        env!("CARGO_PKG_VERSION"),
        vec![Arc::new(ProfilerSinkFactory)],
        vec![GraphChangeCodec::schema()],
    )
}

#[cfg(feature = "dynamic-plugin")]
drasi_computation_plugin_sdk::export_computation_plugin!(plugin());

impl Factory for ProfilerSinkFactory {
    /// Inputs do not key delivery on producer identity.
    fn describe(
        &self,
        request: &drasi_computation_plugin_sdk::wire::DescribeRequest,
    ) -> anyhow::Result<drasi_computation_plugin_sdk::ComponentDescription> {
        let config = ProfilerSinkConfig::parse(request.request.configuration.clone())?;
        Ok(request.unkeyed_sink(config.on_permanent_failure))
    }
    fn configuration_details(&self) -> Option<drasi_computation_plugin_sdk::ConfigDetails> {
        Some(component::configuration_definition().details())
    }

    fn metadata(&self) -> FactoryMetadata {
        FactoryMetadata {
            implementation: ImplementationIdentity::try_new(PROFILER_SINK, "1").expect("constant"),
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
            capabilities: Capabilities::default(),
        }
    }

    fn create(
        &self,
        request: &CreateRequest,
        _control: ControlSender,
    ) -> anyhow::Result<CreatedComponent> {
        Ok(Component::Sink(Box::new(component::ProfilerSink::new(
            self.metadata().descriptor(request.id.clone())?,
            ProfilerSinkConfig::parse(request.configuration.clone())?,
        )))
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
        let metadata = ProfilerSinkFactory.metadata();
        assert_eq!(metadata.role, ComponentRole::Sink);
        assert_eq!(metadata.completion, Some(SinkCompletion::Handled));
        Ok(())
    }
}

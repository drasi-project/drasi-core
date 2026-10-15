// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

mod component;

use drasi_computation_plugin_sdk::{
    Capabilities, Component, ConfigSchema, ConfigType, ControlSender, CreateRequest,
    CreatedComponent, Factory, FactoryMetadata, PluginDefinition,
};
use drasi_lib::computation::v1::{
    ComponentRole, GraphChangeCodec, ImplementationIdentity, PipeRequirements, PortDescriptor,
    PortDirection, PortId, SinkCompletion,
};
use std::sync::Arc;

pub const PLUGIN_ID: &str = env!("CARGO_PKG_NAME");
pub const CAPTURE: &str = "drasi.standard/capture";

pub struct CaptureFactory;

pub fn plugin() -> anyhow::Result<PluginDefinition> {
    PluginDefinition::new(
        PLUGIN_ID,
        env!("CARGO_PKG_VERSION"),
        vec![Arc::new(CaptureFactory)],
        vec![GraphChangeCodec::schema()],
    )
}

#[cfg(feature = "dynamic-plugin")]
drasi_computation_plugin_sdk::export_computation_plugin!(plugin());

impl Factory for CaptureFactory {
    fn metadata(&self) -> FactoryMetadata {
        FactoryMetadata {
            implementation: ImplementationIdentity::try_new(CAPTURE, "1").expect("constant"),
            role: ComponentRole::Sink,
            configuration_version: 1,
            configuration: ConfigSchema::from_fields([
                ("path", ConfigType::String, true),
                ("append", ConfigType::Boolean, false),
            ]),
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
        Ok(Component::Sink(Box::new(component::Capture::new(
            self.metadata().descriptor(request.id.clone())?,
            request.configuration.clone(),
        )?))
        .into())
    }
}

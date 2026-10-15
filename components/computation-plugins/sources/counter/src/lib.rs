// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

mod component;

use drasi_computation_plugin_sdk::{
    Capabilities, Component, ControlSender, CreateRequest, CreatedComponent, Factory,
    FactoryMetadata, PluginDefinition,
};
use drasi_lib::computation::v1::{
    ComponentRole, GraphChangeCodec, ImplementationIdentity, PipeRequirements, PortDescriptor,
    PortDirection, PortId,
};
use std::sync::Arc;

pub const PLUGIN_ID: &str = env!("CARGO_PKG_NAME");
pub const COUNTER: &str = "drasi.standard/volatile-counter";

pub struct CounterFactory;

pub fn plugin() -> anyhow::Result<PluginDefinition> {
    PluginDefinition::new(
        PLUGIN_ID,
        env!("CARGO_PKG_VERSION"),
        vec![Arc::new(CounterFactory)],
        vec![GraphChangeCodec::schema()],
    )
}

#[cfg(feature = "dynamic-plugin")]
drasi_computation_plugin_sdk::export_computation_plugin!(plugin());

impl Factory for CounterFactory {
    fn configuration_details(&self) -> Option<drasi_computation_plugin_sdk::ConfigDetails> {
        Some(component::configuration_definition().details())
    }

    fn metadata(&self) -> FactoryMetadata {
        FactoryMetadata {
            implementation: ImplementationIdentity::try_new(COUNTER, "1").expect("constant"),
            role: ComponentRole::Source,
            configuration_version: 1,
            configuration: component::configuration_definition().schema(),
            ports: vec![PortDescriptor::new(
                PortId::try_new("out").expect("constant"),
                PortDirection::Output,
                GraphChangeCodec::schema().descriptor().clone(),
                PipeRequirements::default(),
            )],
            completion: None,
            capabilities: Capabilities {
                control: true,
                ..Capabilities::default()
            },
        }
    }

    fn create(
        &self,
        request: &CreateRequest,
        _control: ControlSender,
    ) -> anyhow::Result<CreatedComponent> {
        let (counter, handler) = component::Counter::new(
            self.metadata().descriptor(request.id.clone())?,
            request.configuration.clone(),
        )?;
        Ok(CreatedComponent {
            component: Component::Source(Box::new(counter)),
            control_handler: Some(Arc::new(handler)),
        })
    }
}

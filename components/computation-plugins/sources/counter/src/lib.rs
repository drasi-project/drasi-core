// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

mod component;

use drasi_computation_contracts::{
    ComponentRole, ComponentUpdateMode, GraphChangeCodec, ImplementationIdentity, PipeRequirements,
    PortDescriptor, PortDirection, PortId,
};
use drasi_computation_plugin_sdk::{
    lifecycle::Assessment, Capabilities, Component, ControlSender, CreateRequest, CreatedComponent,
    Factory, FactoryMetadata, PluginDefinition, UpdateRequest,
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
    /// Outputs carry only source replay progress unless published through a Host admission channel.
    fn describe(
        &self,
        request: &drasi_computation_plugin_sdk::wire::DescribeRequest,
    ) -> anyhow::Result<drasi_computation_plugin_sdk::ComponentDescription> {
        Ok(request.source_description(
            drasi_computation_plugin_sdk::OutputDescription::UNTRACKED,
            drasi_computation_plugin_sdk::ProgressPersistence::Undeclared,
        ))
    }

    fn configuration_details(&self) -> drasi_computation_plugin_sdk::ConfigDetails {
        component::configuration_definition().details()
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
                management: true,
                update_modes: vec![ComponentUpdateMode::InPlace],
                // Outputs carry source replay progress, not a producer identity of their
                // own (Host admission, when bound, publishes under its durable identity).
                ..Capabilities::default()
            },
        }
    }

    fn create(
        &self,
        request: &CreateRequest,
        _control: ControlSender,
        _services: drasi_computation_plugin_sdk::ServiceBindings,
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
    fn update_planner(&self) -> Option<&dyn drasi_computation_plugin_sdk::UpdatePlanner> {
        Some(self)
    }
}

impl drasi_computation_plugin_sdk::UpdatePlanner for CounterFactory {
    fn assess(&self, request: &UpdateRequest) -> anyhow::Result<Assessment> {
        component::plan_update(request).map(Assessment::from)
    }
}

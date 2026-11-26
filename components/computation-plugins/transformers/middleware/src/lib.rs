// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

mod component;
mod stateful;

pub use stateful::{StatefulMiddlewareFactory, STATEFUL_MIDDLEWARE};

use drasi_computation_contracts::{
    ComponentRole, GraphChangeCodec, ImplementationIdentity, OutputProgress, PipeRequirements,
    PortDescriptor, PortDirection, PortId,
};
use drasi_computation_plugin_sdk::{
    Capabilities, Component, ControlSender, CreateRequest, CreatedComponent, Factory,
    FactoryMetadata, PluginDefinition,
};
use std::sync::Arc;

pub const PLUGIN_ID: &str = env!("CARGO_PKG_NAME");
pub const MIDDLEWARE: &str = "drasi.standard/middleware";

/// Stateless middleware. Its current state is its input's current state,
/// transformed on read; [`StatefulMiddlewareFactory`] maintains that state.
pub struct MiddlewareFactory;

fn configuration_definition() -> drasi_computation_plugin_sdk::ConfigDefinition {
    drasi_computation_plugin_sdk::ConfigDefinition::try_new(
        drasi_computation_middleware_common::Mode::Events.options(),
    )
    .expect("constant middleware configuration")
}

pub fn plugin() -> anyhow::Result<PluginDefinition> {
    PluginDefinition::new(
        PLUGIN_ID,
        env!("CARGO_PKG_VERSION"),
        vec![
            Arc::new(MiddlewareFactory),
            Arc::new(StatefulMiddlewareFactory),
        ],
        vec![GraphChangeCodec::schema()],
    )
}

#[cfg(feature = "dynamic-plugin")]
drasi_computation_plugin_sdk::export_computation_plugin!(plugin());

impl Factory for MiddlewareFactory {
    /// Outputs carry a new volatile producer identity on every construction.
    fn describe(
        &self,
        request: &drasi_computation_plugin_sdk::wire::DescribeRequest,
    ) -> anyhow::Result<drasi_computation_plugin_sdk::ComponentDescription> {
        serde_json::from_value::<component::Configuration>(request.request.configuration.clone())?;
        Ok(request.volatile_transformer())
    }
    fn configuration_details(&self) -> drasi_computation_plugin_sdk::ConfigDetails {
        configuration_definition().details()
    }

    fn metadata(&self) -> FactoryMetadata {
        FactoryMetadata {
            implementation: ImplementationIdentity::try_new(MIDDLEWARE, "1").expect("constant"),
            role: ComponentRole::Transformer,
            configuration_version: 1,
            configuration: configuration_definition().schema(),
            ports: [("in", PortDirection::Input), ("out", PortDirection::Output)]
                .into_iter()
                .map(|(id, direction)| {
                    PortDescriptor::new(
                        PortId::try_new(id).expect("constant"),
                        direction,
                        GraphChangeCodec::schema().descriptor().clone(),
                        PipeRequirements::default(),
                    )
                })
                .collect(),
            completion: None,
            capabilities: Capabilities {
                output_snapshots: vec![PortId::try_new("out").expect("constant port")],
                output_projections: [(
                    PortId::try_new("out").expect("constant port"),
                    PortId::try_new("in").expect("constant port"),
                )]
                .into(),
                input_binding_updates: true,
                output_progress: Some(OutputProgress::Origin),
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
        Ok(Component::Transformer(Box::new(component::Middleware::new(
            self.metadata().descriptor(request.id.clone())?,
            request.configuration.clone(),
            request.scope.as_ref(),
        )?))
        .into())
    }
}

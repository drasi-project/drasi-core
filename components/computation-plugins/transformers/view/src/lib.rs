// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! A memory-backed view of complete input elements, distinct from stateless
//! event middleware. See the crate README for mapping and recovery semantics.

#![deny(unused, missing_docs)]

mod component;
mod mapping;
mod output;
mod state;

use drasi_computation_contracts::{
    ComponentRole, GraphChangeCodec, ImplementationIdentity, OutputProgress, PipeRequirements,
    PortDescriptor, PortDirection, PortId,
};
use drasi_computation_plugin_sdk::{
    Capabilities, Component, ConfigDefinition, ControlSender, CreateRequest, CreatedComponent,
    Factory, FactoryMetadata, PluginDefinition,
};
use std::sync::Arc;

/// Native library identity.
pub const PLUGIN_ID: &str = env!("CARGO_PKG_NAME");
/// Configuration-only factory for the memory-backed view.
pub const VIEW: &str = "drasi.standard/view";

/// Constructs independent, initially empty view transformers.
pub struct ViewFactory;

fn configuration_definition() -> ConfigDefinition {
    ConfigDefinition::try_new(drasi_computation_middleware_common::Mode::Images.options())
        .expect("constant view configuration")
}

/// The plugin definition, suitable for static registration or native export.
pub fn plugin() -> anyhow::Result<PluginDefinition> {
    PluginDefinition::new(
        PLUGIN_ID,
        env!("CARGO_PKG_VERSION"),
        vec![Arc::new(ViewFactory)],
        vec![GraphChangeCodec::schema()],
    )
}

#[cfg(feature = "dynamic-plugin")]
#[allow(missing_docs, reason = "The shared SDK macro emits ABI entry points.")]
mod native_exports {
    drasi_computation_plugin_sdk::export_computation_plugin!(super::plugin());
}

impl Factory for ViewFactory {
    /// Outputs carry a new volatile producer identity on every construction.
    fn describe(
        &self,
        request: &drasi_computation_plugin_sdk::wire::DescribeRequest,
    ) -> anyhow::Result<drasi_computation_plugin_sdk::ComponentDescription> {
        Ok(request.volatile_transformer())
    }
    fn configuration_details(&self) -> drasi_computation_plugin_sdk::ConfigDetails {
        configuration_definition().details()
    }

    fn metadata(&self) -> FactoryMetadata {
        FactoryMetadata {
            implementation: ImplementationIdentity::try_new(VIEW, "1").expect("constant identity"),
            role: ComponentRole::Transformer,
            configuration_version: 1,
            configuration: configuration_definition().schema(),
            ports: [("in", PortDirection::Input), ("out", PortDirection::Output)]
                .into_iter()
                .map(|(id, direction)| {
                    PortDescriptor::new(
                        PortId::try_new(id).expect("constant port"),
                        direction,
                        GraphChangeCodec::schema().descriptor().clone(),
                        PipeRequirements::default(),
                    )
                })
                .collect(),
            completion: None,
            capabilities: Capabilities {
                output_snapshots: vec![PortId::try_new("out").expect("constant port")],
                output_progress: Some(OutputProgress::Origin),
                erases_owned_state: true,
                // Outputs carry a producer identity that is new on every construction.
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
        Ok(Component::Transformer(Box::new(component::View::new(
            self.metadata().descriptor(request.id.clone())?,
            request.configuration.clone(),
            request.scope.as_ref(),
        )?))
        .into())
    }
}

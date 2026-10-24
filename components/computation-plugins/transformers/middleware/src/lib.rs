// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

mod component;

use drasi_computation_contracts::{
    ComponentRole, GraphChangeCodec, ImplementationIdentity, PipeRequirements, PortDescriptor,
    PortDirection, PortId,
};
use drasi_computation_plugin_sdk::{
    Capabilities, Component, ControlSender, CreateRequest, CreatedComponent, Factory,
    FactoryMetadata, PluginDefinition,
};
use std::sync::Arc;

pub const PLUGIN_ID: &str = env!("CARGO_PKG_NAME");
pub const MIDDLEWARE: &str = "drasi.standard/middleware";

pub struct MiddlewareFactory;

fn configuration_definition() -> drasi_computation_plugin_sdk::ConfigDefinition {
    use drasi_computation_plugin_sdk::{ConfigDefinition, ConfigOption as Field, ConfigType::*};
    ConfigDefinition::try_new([
        Field::new("stream", String, true, "Nonempty output stream identifier."),
        Field::new(
            "middleware",
            Array,
            true,
            "Named middleware configurations; this component supplies map and relabel.",
        ),
        Field::new(
            "pipeline",
            Array,
            true,
            "Ordered middleware names to apply; every name must refer to a declared configuration.",
        ),
    ])
    .expect("constant middleware configuration")
}

pub fn plugin() -> anyhow::Result<PluginDefinition> {
    PluginDefinition::new(
        PLUGIN_ID,
        env!("CARGO_PKG_VERSION"),
        vec![Arc::new(MiddlewareFactory)],
        vec![GraphChangeCodec::schema()],
    )
}

#[cfg(feature = "dynamic-plugin")]
drasi_computation_plugin_sdk::export_computation_plugin!(plugin());

impl Factory for MiddlewareFactory {
    fn configuration_details(&self) -> Option<drasi_computation_plugin_sdk::ConfigDetails> {
        Some(configuration_definition().details())
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
            capabilities: Capabilities::default(),
        }
    }

    fn create(
        &self,
        request: &CreateRequest,
        _control: ControlSender,
    ) -> anyhow::Result<CreatedComponent> {
        Ok(Component::Transformer(Box::new(component::Middleware::new(
            self.metadata().descriptor(request.id.clone())?,
            request.configuration.clone(),
        )?))
        .into())
    }
}

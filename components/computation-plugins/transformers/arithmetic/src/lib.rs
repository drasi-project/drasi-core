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
pub const ARITHMETIC: &str = "drasi.standard/arithmetic";

pub struct ArithmeticFactory;

pub fn plugin() -> anyhow::Result<PluginDefinition> {
    PluginDefinition::new(
        PLUGIN_ID,
        env!("CARGO_PKG_VERSION"),
        vec![Arc::new(ArithmeticFactory)],
        vec![GraphChangeCodec::schema()],
    )
}

#[cfg(feature = "dynamic-plugin")]
drasi_computation_plugin_sdk::export_computation_plugin!(plugin());

impl Factory for ArithmeticFactory {
    fn configuration_details(&self) -> Option<drasi_computation_plugin_sdk::ConfigDetails> {
        Some(component::configuration_definition().details())
    }

    fn metadata(&self) -> FactoryMetadata {
        FactoryMetadata {
            implementation: ImplementationIdentity::try_new(ARITHMETIC, "1").expect("constant"),
            role: ComponentRole::Transformer,
            configuration_version: 1,
            configuration: component::configuration_definition().schema(),
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
                transactional: true,
                ..Capabilities::default()
            },
        }
    }

    fn create(
        &self,
        request: &CreateRequest,
        _control: ControlSender,
    ) -> anyhow::Result<CreatedComponent> {
        Ok(
            Component::Transactional(Box::new(component::Arithmetic::new(
                self.metadata().descriptor(request.id.clone())?,
                request.configuration.clone(),
            )?))
            .into(),
        )
    }
}

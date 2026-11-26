// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! A stateful view of complete input elements, distinct from stateless event
//! middleware. The view is one implementation of the shared stateful-transformer
//! runtime: its retained inputs, the outputs each input produced and its output
//! are durable, and it bootstraps, follows, replays and rebuilds like any other
//! stateful transformer. See the crate README for mapping semantics.

#![deny(unused, missing_docs)]

mod logic;
mod mapping;
mod state;
#[cfg(test)]
mod tests;

use anyhow::Context;
use drasi_computation_contracts::{
    ComponentRole, GraphChangeCodec, ImplementationIdentity, PipeRequirements, PortDescriptor,
    PortDirection, PortId,
};
use drasi_computation_middleware_common::Mode;
use drasi_computation_plugin_sdk::{
    transformer::{self, StatefulTransformer},
    Component, ConfigDefinition, ControlSender, CreateRequest, CreatedComponent, Factory,
    FactoryMetadata, PluginDefinition, ServiceBindings,
};
use std::sync::Arc;

/// Native library identity.
pub const PLUGIN_ID: &str = env!("CARGO_PKG_NAME");
/// The stateful view's implementation name.
pub const VIEW: &str = "drasi.standard/view";

/// Constructs stateful view transformers.
pub struct ViewFactory;

fn configuration_definition() -> ConfigDefinition {
    ConfigDefinition::try_new(
        Mode::Images
            .options()
            .into_iter()
            .chain(transformer::configuration_options()),
    )
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
    fn describe(
        &self,
        request: &drasi_computation_plugin_sdk::wire::DescribeRequest,
    ) -> anyhow::Result<drasi_computation_plugin_sdk::ComponentDescription> {
        transformer::describe(request)
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
            ports: [
                (transformer::INPUT_PORT, PortDirection::Input),
                (transformer::OUTPUT_PORT, PortDirection::Output),
            ]
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
            capabilities: transformer::capabilities(false),
        }
    }

    fn create(
        &self,
        request: &CreateRequest,
        _control: ControlSender,
        services: ServiceBindings,
    ) -> anyhow::Result<CreatedComponent> {
        let (logic, settings) = logic::ViewLogic::create(request)?;
        Ok(Component::Transformer(Box::new(StatefulTransformer::new(
            request,
            self.metadata().descriptor(request.id.clone())?,
            services
                .processing
                .context("the view requires host-owned processing")?,
            logic,
            settings,
        )?))
        .into())
    }
}

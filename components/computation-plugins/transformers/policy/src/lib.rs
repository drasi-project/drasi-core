// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! A native Rego policy transformer. Every input element is evaluated against
//! configured Rego modules and data, and its decision is maintained as an
//! output element. See the crate README for configuration and semantics.

#![deny(unused, missing_docs)]

mod component;
mod config;
mod decide;
mod digest;
mod output;
mod policy;
mod template;

use config::PolicyConfig;

use drasi_computation_contracts::{
    ComponentRole, ComponentUpdateMode, GraphChangeCodec, ImplementationIdentity, OutputProgress,
    PipeRequirements, PortDescriptor, PortDirection, PortId,
};
use drasi_computation_plugin_sdk::{
    lifecycle::Assessment, Capabilities, Component, ControlSender, CreateRequest, CreatedComponent,
    Factory, FactoryMetadata, PluginDefinition, ServiceBindings, UpdatePlanner, UpdateRequest,
};
use std::sync::Arc;

/// Native library identity.
pub const PLUGIN_ID: &str = env!("CARGO_PKG_NAME");
/// Configuration-only factory for the Rego policy transformer.
pub const POLICY: &str = "drasi.standard/policy";

/// Constructs independent, initially empty policy transformers.
pub struct PolicyFactory;

/// The plugin definition, suitable for static registration or native export.
pub fn plugin() -> anyhow::Result<PluginDefinition> {
    PluginDefinition::new(
        PLUGIN_ID,
        env!("CARGO_PKG_VERSION"),
        vec![Arc::new(PolicyFactory)],
        vec![GraphChangeCodec::schema()],
    )
}

#[cfg(feature = "dynamic-plugin")]
#[allow(missing_docs, reason = "The shared SDK macro emits ABI entry points.")]
mod native_exports {
    drasi_computation_plugin_sdk::export_computation_plugin!(super::plugin());
}

impl Factory for PolicyFactory {
    /// Outputs carry a new volatile producer identity on every construction.
    fn describe(
        &self,
        request: &drasi_computation_plugin_sdk::wire::DescribeRequest,
    ) -> anyhow::Result<drasi_computation_plugin_sdk::ComponentDescription> {
        Ok(request.volatile_transformer())
    }

    fn configuration_details(&self) -> drasi_computation_plugin_sdk::ConfigDetails {
        PolicyConfig::definition().details()
    }

    fn metadata(&self) -> FactoryMetadata {
        FactoryMetadata {
            implementation: ImplementationIdentity::try_new(POLICY, "1")
                .expect("constant identity"),
            role: ComponentRole::Transformer,
            configuration_version: 1,
            configuration: PolicyConfig::definition().schema(),
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
                continuations: true,
                output_snapshots: vec![PortId::try_new("out").expect("constant port")],
                output_progress: Some(OutputProgress::Origin),
                erases_owned_state: true,
                management: true,
                update_modes: vec![ComponentUpdateMode::InPlace, ComponentUpdateMode::Recreate],
                ..Capabilities::default()
            },
        }
    }

    fn update_planner(&self) -> Option<&dyn UpdatePlanner> {
        Some(self)
    }

    fn create(
        &self,
        request: &CreateRequest,
        _control: ControlSender,
        _services: ServiceBindings,
    ) -> anyhow::Result<CreatedComponent> {
        Ok(
            Component::Transformer(Box::new(component::PolicyTransformer::new(
                self.metadata().descriptor(request.id.clone())?,
                request.configuration.clone(),
                request.scope.as_ref(),
            )?))
            .into(),
        )
    }
}

impl UpdatePlanner for PolicyFactory {
    fn assess(&self, request: &UpdateRequest) -> anyhow::Result<Assessment> {
        component::assess(request)
    }
}

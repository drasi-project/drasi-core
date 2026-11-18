// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Native ComputationGraph gRPC sink (`drasi.network/grpc-sink`).
//!
//! Calls `drasi.v1.ChangeService/ProcessChanges` with ordered, typed graph
//! changes. A request completes (`Handled`) only when the receiver answers
//! `success=true` with an exact `items_processed` count. Every request carries
//! a stable `x-drasi-delivery-id` for receiver-side deduplication of retries
//! and replays. Plaintext, TLS and mutual TLS endpoints are supported; the
//! connection is lazy unless `requireConnectedAtStart` is set.
//!
//! See `DESIGN.md` and `CONFIGURATION.md` in this crate.

mod component;
pub mod config;
mod render;
mod transport;

use drasi_computation_contracts::{
    ComponentRole, ComponentUpdateMode, GraphChangeCodec, ImplementationIdentity, PipeRequirements,
    PortDescriptor, PortDirection, PortId, SinkCompletion, SinkOutputs,
};
use drasi_computation_plugin_sdk::{
    lifecycle::Assessment, Capabilities, Component, ControlSender, CreateRequest, CreatedComponent,
    Factory, FactoryMetadata, PluginDefinition, UpdateRequest,
};
use std::sync::Arc;

pub use config::{Compression, GrpcSinkConfig, OutputFormat, PermanentFailurePolicy};
pub use drasi_computation_network_common::proto;

pub const PLUGIN_ID: &str = env!("CARGO_PKG_NAME");
pub const GRPC_SINK: &str = "drasi.network/grpc-sink";

/// Factory for `drasi.network/grpc-sink`.
pub struct GrpcSinkFactory;

/// The plugin definition exported by the dynamic library.
pub fn plugin() -> anyhow::Result<PluginDefinition> {
    PluginDefinition::new(
        PLUGIN_ID,
        env!("CARGO_PKG_VERSION"),
        vec![Arc::new(GrpcSinkFactory)],
        vec![GraphChangeCodec::schema()],
    )
}

#[cfg(feature = "dynamic-plugin")]
drasi_computation_plugin_sdk::export_computation_plugin!(plugin());

impl Factory for GrpcSinkFactory {
    /// Inputs do not key delivery on producer identity.
    fn describe(
        &self,
        request: &drasi_computation_plugin_sdk::wire::DescribeRequest,
    ) -> anyhow::Result<drasi_computation_plugin_sdk::ComponentDescription> {
        let config = GrpcSinkConfig::parse(request.request.configuration.clone())?;
        Ok(request.unkeyed_sink(config.on_permanent_failure))
    }
    fn configuration_details(&self) -> drasi_computation_plugin_sdk::ConfigDetails {
        GrpcSinkConfig::definition().details()
    }

    fn metadata(&self) -> FactoryMetadata {
        FactoryMetadata {
            // Constant identifiers; validated by `metadata_is_valid`.
            implementation: ImplementationIdentity::try_new(GRPC_SINK, "1")
                .expect("constant implementation identity"),
            role: ComponentRole::Sink,
            configuration_version: 1,
            configuration: GrpcSinkConfig::definition().schema(),
            ports: vec![PortDescriptor::new(
                PortId::try_new("in").expect("constant port"),
                PortDirection::Input,
                GraphChangeCodec::schema().descriptor().clone(),
                PipeRequirements::default(),
            )],
            completion: Some(SinkCompletion::Handled),
            capabilities: Capabilities {
                binds_inputs: true,
                management: true,
                update_modes: vec![
                    ComponentUpdateMode::InPlace,
                    ComponentUpdateMode::Restart,
                    ComponentUpdateMode::Recreate,
                ],
                erases_owned_state: true,
                services: drasi_computation_plugin_sdk::HostServices {
                    sink_batching: true,
                    ..Default::default()
                },
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
        let config = GrpcSinkConfig::parse(request.configuration.clone())?;
        let outputs = SinkOutputs::from_inputs(&request.inputs, &config.outputs)?;
        Ok(Component::Sink(Box::new(component::GrpcSink::new(
            self.metadata().descriptor(request.id.clone())?,
            config,
            outputs,
        )?))
        .into())
    }
    fn update_planner(&self) -> Option<&dyn drasi_computation_plugin_sdk::UpdatePlanner> {
        Some(self)
    }
}

impl drasi_computation_plugin_sdk::UpdatePlanner for GrpcSinkFactory {
    fn assess(&self, request: &UpdateRequest) -> anyhow::Result<Assessment> {
        component::assess(request)
    }
}

#[cfg(test)]
mod tests;

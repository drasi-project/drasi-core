// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Native ComputationGraph gRPC source (`drasi.network/grpc-source`).
//!
//! Serves the published `drasi.v1.SourceService` (`SubmitEvent`, bidirectional
//! `StreamEvents`, `HealthCheck`) and the durable producer-session protocol
//! `drasi.admission.v1.AdmissionService`. Without an admission binding,
//! acceptance is volatile; with one, `SourceService` calls commit through the
//! source's own server-sequenced producer before acknowledging.
//!
//! The listener supports TLS (optionally mutual), bearer-token
//! authentication on every RPC, per-client rate limiting, connection and
//! stream limits, and binds loopback by default. Pause refuses new
//! admissions; Stop honours `Drain` and `Abort`.
//!
//! See `DESIGN.md` and `CONFIGURATION.md` in this crate.

mod component;
pub mod config;
mod durable_grpc;

use drasi_computation_contracts::{
    ComponentRole, ComponentUpdateMode, GraphChangeCodec, ImplementationIdentity, PipeRequirements,
    PortDescriptor, PortDirection, PortId,
};
use drasi_computation_network_common::{durable, ingress, wire};
use drasi_computation_plugin_sdk::{
    lifecycle::Assessment, Capabilities, Component, ControlSender, CreateRequest, CreatedComponent,
    Factory, FactoryMetadata, PluginDefinition, ServiceBindings, UpdateRequest,
};
use std::sync::Arc;

pub use config::GrpcSourceConfig;
pub mod proto {
    pub use drasi_grpc_common::source;
    pub mod admission {
        tonic::include_proto!("drasi.admission.v1");
    }
}

pub const PLUGIN_ID: &str = env!("CARGO_PKG_NAME");
pub const GRPC_SOURCE: &str = "drasi.network/grpc-source";

/// Factory for `drasi.network/grpc-source`.
pub struct GrpcSourceFactory;

/// The plugin definition exported by the dynamic library.
pub fn plugin() -> anyhow::Result<PluginDefinition> {
    PluginDefinition::new(
        PLUGIN_ID,
        env!("CARGO_PKG_VERSION"),
        vec![Arc::new(GrpcSourceFactory)],
        vec![GraphChangeCodec::schema()],
    )
}

#[cfg(feature = "dynamic-plugin")]
drasi_computation_plugin_sdk::export_computation_plugin!(plugin());

impl Factory for GrpcSourceFactory {
    /// Outputs carry a new volatile producer identity per construction unless published through a Host admission channel.
    fn describe(
        &self,
        request: &drasi_computation_plugin_sdk::wire::DescribeRequest,
    ) -> anyhow::Result<drasi_computation_plugin_sdk::ComponentDescription> {
        Ok(request.source_description(
            drasi_computation_plugin_sdk::OutputDescription::VOLATILE,
            drasi_computation_plugin_sdk::ProgressPersistence::Volatile,
        ))
    }
    fn configuration_details(&self) -> drasi_computation_plugin_sdk::ConfigDetails {
        GrpcSourceConfig::definition().details()
    }
    fn metadata(&self) -> FactoryMetadata {
        FactoryMetadata {
            // Constant identifiers; validated by `metadata_is_valid`.
            implementation: ImplementationIdentity::try_new(GRPC_SOURCE, "1")
                .expect("constant implementation identity"),
            role: ComponentRole::Source,
            configuration_version: 1,
            configuration: GrpcSourceConfig::definition().schema(),
            ports: vec![PortDescriptor::new(
                PortId::try_new("out").expect("constant port"),
                PortDirection::Output,
                GraphChangeCodec::schema().descriptor().clone(),
                PipeRequirements::default(),
            )],
            completion: None,
            capabilities: Capabilities {
                management: true,
                update_modes: vec![
                    ComponentUpdateMode::InPlace,
                    ComponentUpdateMode::Restart,
                    ComponentUpdateMode::Recreate,
                ],
                erases_owned_state: true,
                // Outputs carry a producer identity that is new on every construction
                // (Host admission, when bound, publishes under its durable identity instead).
                services: drasi_computation_plugin_sdk::HostServices {
                    source_admission: true,
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
        services: ServiceBindings,
    ) -> anyhow::Result<CreatedComponent> {
        let admission = services.admission;
        Ok(Component::Source(Box::new(component::GrpcSource::new(
            self.metadata().descriptor(request.id.clone())?,
            GrpcSourceConfig::from_value(request.configuration.clone())?,
            request.scope.as_ref(),
            admission,
        )?))
        .into())
    }
    fn update_planner(&self) -> Option<&dyn drasi_computation_plugin_sdk::UpdatePlanner> {
        Some(self)
    }
}

impl drasi_computation_plugin_sdk::UpdatePlanner for GrpcSourceFactory {
    fn assess(&self, request: &UpdateRequest) -> anyhow::Result<Assessment> {
        component::assess(request)
    }
}

#[cfg(test)]
mod tests;

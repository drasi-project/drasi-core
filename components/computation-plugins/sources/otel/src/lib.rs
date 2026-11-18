// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Native ComputationGraph OpenTelemetry source (`drasi.network/otel-source`).
//!
//! Receives OpenTelemetry Protocol (OTLP) exports over gRPC and/or HTTP
//! (binary protobuf) and projects an allowlisted subset into a live service
//! graph: `Service` nodes keyed by the resource's `service.name`, current
//! `Metric` values (`REPORTS`), service dependencies from spans
//! (`DEPENDS_ON`), recent error `LogEvent`s (`EMITS`) and `Heartbeat`s.
//! Dependencies and log events expire after a time-to-live measured from
//! receipt; cardinality caps bound every unbounded category.
//!
//! This is a correlation input, not a telemetry backend. Ingress is volatile:
//! an acknowledged export has been admitted to the source's bounded queue,
//! not persisted. The listeners support TLS (optionally mutual), bearer-token
//! authentication, per-client rate limiting and connection limits, and bind
//! loopback by default. Pause refuses exports and defers expiry; Stop honours
//! `Drain` and `Abort`.
//!
//! See `DESIGN.md` and `CONFIGURATION.md` in this crate.

mod component;
pub mod config;
mod engine;
mod grpc;
mod http;
pub mod otlp;
mod projection;
mod state;

use drasi_computation_contracts::{
    ComponentRole, ComponentUpdateMode, GraphChangeCodec, ImplementationIdentity, PipeRequirements,
    PortDescriptor, PortDirection, PortId,
};
use drasi_computation_plugin_sdk::{
    lifecycle::Assessment, Capabilities, Component, ControlSender, CreateRequest, CreatedComponent,
    Factory, FactoryMetadata, PluginDefinition, UpdateRequest,
};
use std::sync::Arc;

pub use config::OtelSourceConfig;
pub use engine::CounterSnapshot;
pub use otlp::proto;
pub use projection::{SOURCE_ORIGIN_ATTRIBUTE, SOURCE_ORIGIN_DERIVED};

pub const PLUGIN_ID: &str = env!("CARGO_PKG_NAME");
pub const OTEL_SOURCE: &str = "drasi.network/otel-source";

/// Factory for `drasi.network/otel-source`.
pub struct OtelSourceFactory;

/// The plugin definition exported by the dynamic library.
pub fn plugin() -> anyhow::Result<PluginDefinition> {
    PluginDefinition::new(
        PLUGIN_ID,
        env!("CARGO_PKG_VERSION"),
        vec![Arc::new(OtelSourceFactory)],
        vec![GraphChangeCodec::schema()],
    )
}

#[cfg(feature = "dynamic-plugin")]
drasi_computation_plugin_sdk::export_computation_plugin!(plugin());

impl Factory for OtelSourceFactory {
    /// Outputs carry a volatile producer identity that is new on every
    /// construction; there is no durable admission or replay.
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
        OtelSourceConfig::definition().details()
    }

    fn metadata(&self) -> FactoryMetadata {
        FactoryMetadata {
            // Constant identifiers; validated by `metadata_is_valid`.
            implementation: ImplementationIdentity::try_new(OTEL_SOURCE, "1")
                .expect("constant implementation identity"),
            role: ComponentRole::Source,
            configuration_version: 1,
            configuration: OtelSourceConfig::definition().schema(),
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
        Ok(Component::Source(Box::new(component::OtelSource::new(
            self.metadata().descriptor(request.id.clone())?,
            OtelSourceConfig::from_value(request.configuration.clone())?,
            request.scope.as_ref(),
        )?))
        .into())
    }
    fn update_planner(&self) -> Option<&dyn drasi_computation_plugin_sdk::UpdatePlanner> {
        Some(self)
    }
}

impl drasi_computation_plugin_sdk::UpdatePlanner for OtelSourceFactory {
    fn assess(&self, request: &UpdateRequest) -> anyhow::Result<Assessment> {
        component::assess(request)
    }
}

#[cfg(test)]
mod tests;

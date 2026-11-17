// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Native ComputationGraph HTTP source (`drasi.network/http-source`).
//!
//! Accepts graph changes over HTTP: standard single-event and batch routes,
//! or configured webhook routes that map arbitrary payloads. Without an
//! admission binding, acceptance is volatile. With one, standard and webhook
//! routes commit through the source's own server-sequenced producer before
//! acknowledging, and the producer-session protocol provides idempotent
//! client-sequenced admission.
//!
//! The listener supports TLS (optionally mutual), bearer-token
//! authentication, per-client rate limiting and connection limits, and binds
//! loopback by default. Pause refuses new admissions; Stop honours `Drain`
//! and `Abort`; most settings can be updated in place or at a restart.
//!
//! See `DESIGN.md` and `CONFIGURATION.md` in this crate.

mod component;
pub mod config;
mod durable_http;
mod schema;
mod webhook;

use drasi_computation_contracts::{
    ComponentRole, ComponentUpdateMode, GraphChangeCodec, ImplementationIdentity, PipeRequirements,
    PortDescriptor, PortDirection, PortId,
};
use drasi_computation_network_common::{durable, ingress, server, wire};
use drasi_computation_plugin_sdk::{
    lifecycle::Assessment, Capabilities, Component, ControlSender, CreateRequest, CreatedComponent,
    Factory, FactoryMetadata, NativeAdmission, PluginDefinition, UpdateRequest,
};
use std::sync::Arc;

pub use config::HttpSourceConfig;

pub const PLUGIN_ID: &str = env!("CARGO_PKG_NAME");
pub const HTTP_SOURCE: &str = "drasi.network/http-source";

/// Factory for `drasi.network/http-source`.
pub struct HttpSourceFactory;

/// The plugin definition exported by the dynamic library.
pub fn plugin() -> anyhow::Result<PluginDefinition> {
    PluginDefinition::new(
        PLUGIN_ID,
        env!("CARGO_PKG_VERSION"),
        vec![Arc::new(HttpSourceFactory)],
        vec![GraphChangeCodec::schema()],
    )
}

#[cfg(feature = "dynamic-plugin")]
drasi_computation_plugin_sdk::export_computation_plugin!(plugin());

impl Factory for HttpSourceFactory {
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
    fn configuration_details(&self) -> Option<drasi_computation_plugin_sdk::ConfigDetails> {
        Some(HttpSourceConfig::definition().details())
    }
    fn supports_source_admission(&self) -> bool {
        true
    }
    fn supports_update_planning(&self) -> bool {
        true
    }
    fn assess(&self, request: &UpdateRequest) -> anyhow::Result<Assessment> {
        component::assess(request)
    }

    fn metadata(&self) -> FactoryMetadata {
        FactoryMetadata {
            // Constant identifiers; validated by `metadata_is_valid`.
            implementation: ImplementationIdentity::try_new(HTTP_SOURCE, "1")
                .expect("constant implementation identity"),
            role: ComponentRole::Source,
            configuration_version: 1,
            configuration: HttpSourceConfig::definition().schema(),
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
                ..Capabilities::default()
            },
        }
    }

    fn create(
        &self,
        request: &CreateRequest,
        control: ControlSender,
    ) -> anyhow::Result<CreatedComponent> {
        self.create_with_admission(request, control, None)
    }

    fn create_with_admission(
        &self,
        request: &CreateRequest,
        _control: ControlSender,
        admission: Option<NativeAdmission>,
    ) -> anyhow::Result<CreatedComponent> {
        Ok(Component::Source(Box::new(component::HttpSource::new(
            self.metadata().descriptor(request.id.clone())?,
            HttpSourceConfig::from_value(request.configuration.clone())?,
            request.scope.as_ref(),
            admission,
        )?))
        .into())
    }
}

#[cfg(test)]
mod tests;

// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Native ComputationGraph log sink (`drasi.standard/log`).
//!
//! Renders each standard graph change with a per-output Handlebars template
//! (or as JSON / the legacy compact form) and emits it to the host's logger
//! (through the SDK's logging bridge when loaded as a library), stdout or
//! stderr. Each input completes (`Handled`) only after all of its lines were
//! emitted and, for stdout/stderr, flushed by a writer thread, so a blocked
//! stream never blocks the async runtime. Templates are compiled and validated
//! at construction; render failures follow an explicit `fallback` or `strict`
//! policy; most settings update in place.
//!
//! See `DESIGN.md` and `CONFIGURATION.md` in this crate.

mod component;
pub mod config;
mod output;
mod render;

use drasi_computation_contracts::{
    ComponentRole, ComponentUpdateMode, GraphChangeCodec, ImplementationIdentity, PipeRequirements,
    PortDescriptor, PortDirection, PortId, SinkCompletion, SinkOutputs,
};
use drasi_computation_plugin_sdk::{
    lifecycle::Assessment, Capabilities, Component, ControlSender, CreateRequest, CreatedComponent,
    Factory, FactoryMetadata, PluginDefinition, UpdateRequest,
};
use std::sync::Arc;

pub use config::LogSinkConfig;

pub const PLUGIN_ID: &str = env!("CARGO_PKG_NAME");
pub const LOG_SINK: &str = "drasi.standard/log";

/// Factory for `drasi.standard/log`.
pub struct LogSinkFactory;

/// The plugin definition exported by the dynamic library.
pub fn plugin() -> anyhow::Result<PluginDefinition> {
    PluginDefinition::new(
        PLUGIN_ID,
        env!("CARGO_PKG_VERSION"),
        vec![Arc::new(LogSinkFactory)],
        vec![GraphChangeCodec::schema()],
    )
}

#[cfg(feature = "dynamic-plugin")]
drasi_computation_plugin_sdk::export_computation_plugin!(plugin());

impl Factory for LogSinkFactory {
    /// Inputs do not key delivery on producer identity.
    fn describe(
        &self,
        request: &drasi_computation_plugin_sdk::wire::DescribeRequest,
    ) -> anyhow::Result<drasi_computation_plugin_sdk::ComponentDescription> {
        let config = LogSinkConfig::parse(request.request.configuration.clone())?;
        Ok(request.unkeyed_sink(config.on_permanent_failure))
    }
    fn configuration_details(&self) -> Option<drasi_computation_plugin_sdk::ConfigDetails> {
        Some(LogSinkConfig::definition().details())
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
            implementation: ImplementationIdentity::try_new(LOG_SINK, "1")
                .expect("constant implementation identity"),
            role: ComponentRole::Sink,
            configuration_version: 1,
            configuration: LogSinkConfig::definition().schema(),
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
                update_modes: vec![ComponentUpdateMode::InPlace, ComponentUpdateMode::Recreate],
                erases_owned_state: true,
                ..Capabilities::default()
            },
        }
    }

    fn create(
        &self,
        request: &CreateRequest,
        _control: ControlSender,
    ) -> anyhow::Result<CreatedComponent> {
        let config = LogSinkConfig::parse(request.configuration.clone())?;
        let outputs = SinkOutputs::from_inputs(&request.inputs, &config.outputs)?;
        Ok(Component::Sink(Box::new(component::LogSink::new(
            self.metadata().descriptor(request.id.clone())?,
            config,
            outputs,
        )?))
        .into())
    }
}

#[cfg(test)]
mod tests;

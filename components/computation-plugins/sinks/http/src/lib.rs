// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

mod component;
pub mod config;
pub mod delivery;

use drasi_computation_contracts::{
    ComponentRole, GraphChangeCodec, ImplementationIdentity, PipeRequirements, PortDescriptor,
    PortDirection, PortId, SinkCompletion, SinkOutputs,
};
use drasi_computation_network_common::wire;
use drasi_computation_plugin_sdk::{
    Capabilities, Component, ControlSender, CreateRequest, CreatedComponent, Factory,
    FactoryMetadata, PluginDefinition,
};
use std::sync::Arc;

pub use config::{FailurePolicy, HttpSinkConfig};

pub const PLUGIN_ID: &str = env!("CARGO_PKG_NAME");
pub const HTTP_SINK: &str = "drasi.network/http-sink";

pub struct HttpSinkFactory;

pub fn plugin() -> anyhow::Result<PluginDefinition> {
    PluginDefinition::new(
        PLUGIN_ID,
        env!("CARGO_PKG_VERSION"),
        vec![Arc::new(HttpSinkFactory)],
        vec![GraphChangeCodec::schema()],
    )
}

#[cfg(feature = "dynamic-plugin")]
drasi_computation_plugin_sdk::export_computation_plugin!(plugin());

impl Factory for HttpSinkFactory {
    fn supports_sink_batching(&self) -> bool {
        true
    }
    fn configuration_details(&self) -> Option<drasi_computation_plugin_sdk::ConfigDetails> {
        Some(HttpSinkConfig::definition().details())
    }

    fn metadata(&self) -> FactoryMetadata {
        FactoryMetadata {
            implementation: ImplementationIdentity::try_new(HTTP_SINK, "1").expect("constant"),
            role: ComponentRole::Sink,
            configuration_version: 1,
            configuration: HttpSinkConfig::definition().schema(),
            ports: vec![PortDescriptor::new(
                PortId::try_new("in").expect("constant"),
                PortDirection::Input,
                GraphChangeCodec::schema().descriptor().clone(),
                PipeRequirements::default(),
            )],
            completion: Some(SinkCompletion::Handled),
            capabilities: Capabilities {
                binds_inputs: true,
                ..Capabilities::default()
            },
        }
    }

    fn create(
        &self,
        request: &CreateRequest,
        _control: ControlSender,
    ) -> anyhow::Result<CreatedComponent> {
        let config = HttpSinkConfig::parse(request.configuration.clone())?;
        let outputs = SinkOutputs::from_inputs(&request.inputs, &config.outputs)?;
        config.validate_outputs(&outputs)?;
        Ok(Component::Sink(Box::new(component::HttpSink::new(
            self.metadata().descriptor(request.id.clone())?,
            config,
            outputs,
        )?))
        .into())
    }
}

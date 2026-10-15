// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

mod component;
pub mod config;
pub mod delivery;

use drasi_computation_network_common::wire;
use drasi_computation_plugin_sdk::{
    Capabilities, Component, ControlSender, CreateRequest, CreatedComponent, Factory,
    FactoryMetadata, PluginDefinition,
};
use drasi_lib::computation::v1::{
    ComponentRole, ImplementationIdentity, PipeRequirements, PortDescriptor, PortDirection, PortId,
    QueryChangeCodec, SinkCompletion,
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
        vec![QueryChangeCodec::schema()],
    )
}

#[cfg(feature = "dynamic-plugin")]
drasi_computation_plugin_sdk::export_computation_plugin!(plugin());

impl Factory for HttpSinkFactory {
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
                QueryChangeCodec::schema().descriptor().clone(),
                PipeRequirements::default(),
            )],
            completion: Some(SinkCompletion::Handled),
            capabilities: Capabilities::default(),
        }
    }

    fn create(
        &self,
        request: &CreateRequest,
        _control: ControlSender,
    ) -> anyhow::Result<CreatedComponent> {
        Ok(Component::Sink(Box::new(component::HttpSink::new(
            self.metadata().descriptor(request.id.clone())?,
            HttpSinkConfig::parse(request.configuration.clone())?,
        )?))
        .into())
    }
}

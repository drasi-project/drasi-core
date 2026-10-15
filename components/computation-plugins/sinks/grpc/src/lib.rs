// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

mod component;
pub mod config;
mod wire;

use drasi_computation_contracts::{
    ComponentRole, ImplementationIdentity, PipeRequirements, PortDescriptor, PortDirection, PortId,
    QueryChangeCodec, SinkCompletion,
};
use drasi_computation_plugin_sdk::{
    Capabilities, Component, ControlSender, CreateRequest, CreatedComponent, Factory,
    FactoryMetadata, PluginDefinition,
};
use std::sync::Arc;

pub use config::{FailurePolicy, GrpcSinkConfig, OutputFormat};
pub use drasi_computation_network_common::proto;
pub use wire::proto_item;

pub const PLUGIN_ID: &str = env!("CARGO_PKG_NAME");
pub const GRPC_SINK: &str = "drasi.network/grpc-sink";

pub struct GrpcSinkFactory;

pub fn plugin() -> anyhow::Result<PluginDefinition> {
    PluginDefinition::new(
        PLUGIN_ID,
        env!("CARGO_PKG_VERSION"),
        vec![Arc::new(GrpcSinkFactory)],
        vec![QueryChangeCodec::schema()],
    )
}

#[cfg(feature = "dynamic-plugin")]
drasi_computation_plugin_sdk::export_computation_plugin!(plugin());

impl Factory for GrpcSinkFactory {
    fn configuration_details(&self) -> Option<drasi_computation_plugin_sdk::ConfigDetails> {
        Some(GrpcSinkConfig::definition().details())
    }

    fn metadata(&self) -> FactoryMetadata {
        FactoryMetadata {
            implementation: ImplementationIdentity::try_new(GRPC_SINK, "1").expect("constant"),
            role: ComponentRole::Sink,
            configuration_version: 1,
            configuration: GrpcSinkConfig::definition().schema(),
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
        Ok(Component::Sink(Box::new(component::GrpcSink::new(
            self.metadata().descriptor(request.id.clone())?,
            GrpcSinkConfig::parse(request.configuration.clone())?,
        )?))
        .into())
    }
}

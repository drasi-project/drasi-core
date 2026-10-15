// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

mod component;
pub mod config;
mod durable_http;

use drasi_computation_contracts::{
    ComponentRole, GraphChangeCodec, ImplementationIdentity, PipeRequirements, PortDescriptor,
    PortDirection, PortId,
};
use drasi_computation_network_common::{durable, ingress, wire};
use drasi_computation_plugin_sdk::{
    Capabilities, Component, ControlSender, CreateRequest, CreatedComponent, Factory,
    FactoryMetadata, NativeAdmission, PluginDefinition,
};
use std::sync::Arc;

pub use config::HttpSourceConfig;
/// Compatibility name for this component's HTTP configuration.
pub type SourceConfig = HttpSourceConfig;

pub const PLUGIN_ID: &str = env!("CARGO_PKG_NAME");
pub const HTTP_SOURCE: &str = "drasi.network/http-source";

pub struct HttpSourceFactory;

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
    fn configuration_details(&self) -> Option<drasi_computation_plugin_sdk::ConfigDetails> {
        Some(HttpSourceConfig::definition().details())
    }
    fn supports_source_admission(&self) -> bool {
        true
    }

    fn metadata(&self) -> FactoryMetadata {
        FactoryMetadata {
            implementation: ImplementationIdentity::try_new(HTTP_SOURCE, "1").expect("constant"),
            role: ComponentRole::Source,
            configuration_version: 1,
            configuration: HttpSourceConfig::definition().schema(),
            ports: vec![PortDescriptor::new(
                PortId::try_new("out").expect("constant"),
                PortDirection::Output,
                GraphChangeCodec::schema().descriptor().clone(),
                PipeRequirements::default(),
            )],
            completion: None,
            capabilities: Capabilities::default(),
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

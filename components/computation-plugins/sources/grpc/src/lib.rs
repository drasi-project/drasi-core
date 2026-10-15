// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

mod component;
mod durable_grpc;

use drasi_computation_network_common::{config, durable, ingress, wire};
use drasi_computation_plugin_sdk::{
    Capabilities, Component, ConfigSchema, ConfigType, ControlSender, CreateRequest,
    CreatedComponent, Factory, FactoryMetadata, NativeAdmission, PluginDefinition,
};
use drasi_lib::computation::v1::{
    ComponentRole, GraphChangeCodec, ImplementationIdentity, PipeRequirements, PortDescriptor,
    PortDirection, PortId,
};
use std::sync::Arc;

pub use config::SourceConfig;
pub mod proto {
    pub use drasi_computation_network_common::proto::source;
    pub mod admission {
        tonic::include_proto!("drasi.admission.v1");
    }
}

pub const PLUGIN_ID: &str = env!("CARGO_PKG_NAME");
pub const GRPC_SOURCE: &str = "drasi.network/grpc-source";

pub struct GrpcSourceFactory;

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
    fn supports_source_admission(&self) -> bool {
        true
    }

    fn metadata(&self) -> FactoryMetadata {
        use ConfigType::*;
        FactoryMetadata {
            implementation: ImplementationIdentity::try_new(GRPC_SOURCE, "1").expect("constant"),
            role: ComponentRole::Source,
            configuration_version: 1,
            configuration: ConfigSchema::from_fields([
                ("stream", String, true),
                ("sourceId", String, false),
                ("host", String, false),
                ("port", Integer, false),
                ("timeoutMs", Integer, false),
                ("ingressCapacity", Integer, false),
                ("maxMessageBytes", Integer, false),
                ("adaptiveEnabled", Boolean, false),
            ]),
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
        Ok(Component::Source(Box::new(component::GrpcSource::new(
            self.metadata().descriptor(request.id.clone())?,
            SourceConfig::parse(request.configuration.clone(), true)?,
            request.scope.as_ref(),
            admission,
        )?))
        .into())
    }
}

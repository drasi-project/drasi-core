// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Native network components. Existing crates provide wire DTOs/generated service
//! definitions only: no Source/Reaction/Base/adapter object is constructed here.

mod config;
mod grpc;
mod http;
mod ingress;
pub mod wire;

pub use config::{FailurePolicy, GrpcSinkConfig, HttpSinkConfig, OutputFormat, SourceConfig};
pub mod proto {
    pub use drasi_reaction_grpc::proto::drasi_v1 as reaction;
    pub use drasi_source_grpc::proto as source;
}

use drasi_computation_plugin_sdk::{
    Capabilities, Component, ConfigField, ConfigSchema, ConfigType, ControlSender, CreateRequest,
    CreatedComponent, Factory, FactoryMetadata, PluginDefinition,
};
use drasi_lib::computation::v1::{
    ComponentRole, GraphChangeCodec, ImplementationIdentity, PipeRequirements, PortDescriptor,
    PortDirection, PortId, QueryChangeCodec, SinkCompletion,
};
use std::sync::Arc;

pub const PLUGIN_ID: &str = "drasi-computation-network";
pub const HTTP_SOURCE: &str = "drasi.network/http-source";
pub const GRPC_SOURCE: &str = "drasi.network/grpc-source";
pub const HTTP_SINK: &str = "drasi.network/http-sink";
pub const GRPC_SINK: &str = "drasi.network/grpc-sink";

pub fn plugin() -> anyhow::Result<PluginDefinition> {
    PluginDefinition::new(
        PLUGIN_ID,
        env!("CARGO_PKG_VERSION"),
        vec![
            Arc::new(NetworkFactory(Kind::HttpSource)),
            Arc::new(NetworkFactory(Kind::GrpcSource)),
            Arc::new(NetworkFactory(Kind::HttpSink)),
            Arc::new(NetworkFactory(Kind::GrpcSink)),
        ],
        vec![GraphChangeCodec::schema(), QueryChangeCodec::schema()],
    )
}
#[cfg(feature = "dynamic-plugin")]
drasi_computation_plugin_sdk::export_computation_plugin!(plugin());

#[derive(Clone, Copy)]
enum Kind {
    HttpSource,
    GrpcSource,
    HttpSink,
    GrpcSink,
}
struct NetworkFactory(Kind);
impl Factory for NetworkFactory {
    fn metadata(&self) -> FactoryMetadata {
        use ConfigType::*;
        let source = matches!(self.0, Kind::HttpSource | Kind::GrpcSource);
        let mut fields = if source {
            vec![
                ("stream", String, true),
                ("sourceId", String, false),
                ("host", String, false),
                ("port", Integer, false),
                ("timeoutMs", Integer, false),
                ("ingressCapacity", Integer, false),
                ("maxMessageBytes", Integer, false),
                ("adaptiveEnabled", Boolean, false),
            ]
        } else {
            vec![
                ("queryId", String, true),
                ("stream", String, false),
                ("timeoutMs", Integer, false),
                ("maxRetries", Integer, false),
                ("failurePolicy", String, false),
            ]
        };
        let name = match self.0 {
            Kind::HttpSource => {
                fields.push(("maxBatchEvents", Integer, false));
                HTTP_SOURCE
            }
            Kind::GrpcSource => GRPC_SOURCE,
            Kind::HttpSink => {
                fields.extend([("url", String, true), ("headers", Object, false)]);
                HTTP_SINK
            }
            Kind::GrpcSink => {
                fields.extend([
                    ("endpoint", String, false),
                    ("metadata", Object, false),
                    ("batchSize", Integer, false),
                    ("batchFlushTimeoutMs", Integer, false),
                    ("connectionRetryAttempts", Integer, false),
                    ("initialConnectionTimeoutMs", Integer, false),
                    ("outputFormat", String, false),
                ]);
                GRPC_SINK
            }
        };
        FactoryMetadata {
            implementation: ImplementationIdentity::try_new(name, "1")
                .expect("constant implementation"),
            role: if source {
                ComponentRole::Source
            } else {
                ComponentRole::Sink
            },
            configuration_version: 1,
            configuration: ConfigSchema {
                fields: fields
                    .into_iter()
                    .map(|(name, value_type, required)| {
                        (
                            name.into(),
                            ConfigField {
                                value_type,
                                required,
                                secret: false,
                            },
                        )
                    })
                    .collect(),
                allow_additional: false,
            },
            ports: vec![PortDescriptor::new(
                PortId::try_new(if source { "out" } else { "in" }).expect("constant port"),
                if source {
                    PortDirection::Output
                } else {
                    PortDirection::Input
                },
                if source {
                    GraphChangeCodec::schema()
                } else {
                    QueryChangeCodec::schema()
                }
                .descriptor()
                .clone(),
                PipeRequirements::default(),
            )],
            completion: (!source).then_some(SinkCompletion::Handled),
            capabilities: Capabilities::default(),
        }
    }
    fn create(
        &self,
        request: &CreateRequest,
        _: ControlSender,
    ) -> anyhow::Result<CreatedComponent> {
        let descriptor = self.metadata().descriptor(request.id.clone())?;
        Ok(match self.0 {
            Kind::HttpSource => Component::Source(Box::new(http::HttpSource::new(
                descriptor,
                SourceConfig::parse(request.configuration.clone(), false)?,
                request.scope.as_ref(),
            )?)),
            Kind::GrpcSource => Component::Source(Box::new(grpc::GrpcSource::new(
                descriptor,
                SourceConfig::parse(request.configuration.clone(), true)?,
                request.scope.as_ref(),
            )?)),
            Kind::HttpSink => Component::Sink(Box::new(http::HttpSink::new(
                descriptor,
                HttpSinkConfig::parse(request.configuration.clone())?,
            )?)),
            Kind::GrpcSink => Component::Sink(Box::new(grpc::GrpcSink::new(
                descriptor,
                GrpcSinkConfig::parse(request.configuration.clone())?,
            )?)),
        }
        .into())
    }
}

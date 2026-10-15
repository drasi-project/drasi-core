// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use drasi_computation_plugin_sdk::{
    Capabilities, Component, ConfigSchema, ConfigType, ControlSender, CreateRequest,
    CreatedComponent, Factory, FactoryMetadata, PluginDefinition,
};
use drasi_core::{middleware::MiddlewareTypeRegistry, models::SourceMiddlewareConfig};
use drasi_lib::computation::v1::{
    ComponentRole, GraphChangeCodec, ImplementationIdentity, MiddlewareTransformer,
    MiddlewareTransformerDefinition, PipeRequirements, PortDescriptor, PortDirection, PortId,
    StreamId,
};
use serde::Deserialize;
use std::sync::Arc;

pub const PLUGIN_ID: &str = env!("CARGO_PKG_NAME");
pub const MIDDLEWARE: &str = "drasi.standard/middleware";

pub struct MiddlewareFactory;

pub fn plugin() -> anyhow::Result<PluginDefinition> {
    PluginDefinition::new(
        PLUGIN_ID,
        env!("CARGO_PKG_VERSION"),
        vec![Arc::new(MiddlewareFactory)],
        vec![GraphChangeCodec::schema()],
    )
}

#[cfg(feature = "dynamic-plugin")]
drasi_computation_plugin_sdk::export_computation_plugin!(plugin());

impl Factory for MiddlewareFactory {
    fn metadata(&self) -> FactoryMetadata {
        FactoryMetadata {
            implementation: ImplementationIdentity::try_new(MIDDLEWARE, "1").expect("constant"),
            role: ComponentRole::Transformer,
            configuration_version: 1,
            configuration: ConfigSchema::from_fields([
                ("stream", ConfigType::String, true),
                ("middleware", ConfigType::Array, true),
                ("pipeline", ConfigType::Array, true),
            ]),
            ports: [("in", PortDirection::Input), ("out", PortDirection::Output)]
                .into_iter()
                .map(|(id, direction)| {
                    PortDescriptor::new(
                        PortId::try_new(id).expect("constant"),
                        direction,
                        GraphChangeCodec::schema().descriptor().clone(),
                        PipeRequirements::default(),
                    )
                })
                .collect(),
            completion: None,
            capabilities: Capabilities::default(),
        }
    }

    fn create(
        &self,
        request: &CreateRequest,
        _control: ControlSender,
    ) -> anyhow::Result<CreatedComponent> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Configuration {
            stream: StreamId,
            middleware: Vec<SourceMiddlewareConfig>,
            pipeline: Vec<String>,
        }
        let configuration: Configuration = serde_json::from_value(request.configuration.clone())?;
        let mut registry = MiddlewareTypeRegistry::new();
        registry.register(Arc::new(drasi_middleware::map::MapFactory::new()));
        registry.register(Arc::new(
            drasi_middleware::relabel::RelabelMiddlewareFactory::new(),
        ));
        Ok(Component::Transformer(Box::new(MiddlewareTransformer::new(
            MiddlewareTransformerDefinition {
                id: request.id.clone(),
                output_stream: configuration.stream,
                middleware: configuration.middleware,
                pipeline: configuration.pipeline,
            },
            Arc::new(registry),
        )?))
        .into())
    }
}

// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Native graph components, not Source/Reaction adapters. The counter and
//! standalone middleware/arithmetic paths are explicitly volatile examples.
//! Durable arithmetic uses the host's TransactionTransformer and borrowed state.

mod arithmetic;
mod capture;
mod counter;

use drasi_computation_plugin_sdk::{
    Capabilities, Component, ConfigField, ConfigSchema, ConfigType, ControlSender, CreateRequest,
    CreatedComponent, Factory, FactoryMetadata, PluginDefinition,
};
use drasi_core::{middleware::MiddlewareTypeRegistry, models::SourceMiddlewareConfig};
use drasi_lib::computation::v1::{
    ComponentRole, GraphChangeCodec, ImplementationIdentity, MiddlewareTransformer,
    MiddlewareTransformerDefinition, PipeRequirements, PortDescriptor, PortDirection, PortId,
    SinkCompletion, StreamId,
};
use serde::Deserialize;
use std::{collections::BTreeMap, sync::Arc};

pub const PLUGIN_ID: &str = "drasi-computation-standard";
pub const COUNTER: &str = "drasi.standard/volatile-counter";
pub const MIDDLEWARE: &str = "drasi.standard/middleware";
pub const CAPTURE: &str = "drasi.standard/capture";
pub const ARITHMETIC: &str = "drasi.standard/arithmetic";

/// Configuration-only registration. Each loaded library owns its own factories
/// and validators; the export macro sends only the independent native C ABI.
pub fn plugin() -> anyhow::Result<PluginDefinition> {
    PluginDefinition::new(
        PLUGIN_ID,
        env!("CARGO_PKG_VERSION"),
        vec![
            Arc::new(StandardFactory(Kind::Counter)),
            Arc::new(StandardFactory(Kind::Middleware)),
            Arc::new(StandardFactory(Kind::Capture)),
            Arc::new(StandardFactory(Kind::Arithmetic)),
        ],
        vec![GraphChangeCodec::schema()],
    )
}

#[cfg(feature = "dynamic-plugin")]
drasi_computation_plugin_sdk::export_computation_plugin!(plugin());

#[derive(Clone, Copy)]
enum Kind {
    Counter,
    Middleware,
    Capture,
    Arithmetic,
}
struct StandardFactory(Kind);

fn fields(items: &[(&str, ConfigType, bool)]) -> ConfigSchema {
    ConfigSchema {
        fields: items
            .iter()
            .map(|(name, value_type, required)| {
                (
                    name.to_string(),
                    ConfigField {
                        value_type: *value_type,
                        required: *required,
                        secret: false,
                    },
                )
            })
            .collect::<BTreeMap<_, _>>(),
        allow_additional: false,
    }
}
fn port(direction: PortDirection) -> PortDescriptor {
    PortDescriptor::new(
        PortId::try_new(if direction == PortDirection::Input {
            "in"
        } else {
            "out"
        })
        .expect("constant"),
        direction,
        GraphChangeCodec::schema().descriptor().clone(),
        PipeRequirements::default(),
    )
}
impl Factory for StandardFactory {
    fn metadata(&self) -> FactoryMetadata {
        use ConfigType::*;
        let (name, role, configuration, capabilities) = match self.0 {
            Kind::Counter => (
                COUNTER,
                ComponentRole::Source,
                fields(&[
                    ("stream", String, true),
                    ("count", Integer, false),
                    ("start", Integer, false),
                    ("step", Integer, false),
                    ("interval_ms", Integer, false),
                    ("paused", Boolean, false),
                ]),
                Capabilities {
                    control: true,
                    ..Capabilities::default()
                },
            ),
            Kind::Middleware => (
                MIDDLEWARE,
                ComponentRole::Transformer,
                fields(&[
                    ("stream", String, true),
                    ("middleware", Array, true),
                    ("pipeline", Array, true),
                ]),
                Capabilities::default(),
            ),
            Kind::Capture => (
                CAPTURE,
                ComponentRole::Sink,
                fields(&[("path", String, true), ("append", Boolean, false)]),
                Capabilities::default(),
            ),
            Kind::Arithmetic => (
                ARITHMETIC,
                ComponentRole::Transformer,
                fields(&[
                    ("stream", String, true),
                    ("field", String, false),
                    ("add", Integer, false),
                    ("multiply", Integer, false),
                    ("counter_property", String, false),
                ]),
                Capabilities {
                    transactional: true,
                    ..Capabilities::default()
                },
            ),
        };
        FactoryMetadata {
            implementation: ImplementationIdentity::try_new(name, "1").expect("constant"),
            role,
            configuration_version: 1,
            configuration,
            ports: match role {
                ComponentRole::Source => vec![port(PortDirection::Output)],
                ComponentRole::Sink => vec![port(PortDirection::Input)],
                _ => vec![port(PortDirection::Input), port(PortDirection::Output)],
            },
            completion: (role == ComponentRole::Sink).then_some(SinkCompletion::Handled),
            capabilities,
        }
    }
    fn create(
        &self,
        request: &CreateRequest,
        _control: ControlSender,
    ) -> anyhow::Result<CreatedComponent> {
        let descriptor = self.metadata().descriptor(request.id.clone())?;
        Ok(match self.0 {
            Kind::Counter => {
                let (counter, handler) =
                    counter::Counter::new(descriptor, request.configuration.clone())?;
                CreatedComponent {
                    component: Component::Source(Box::new(counter)),
                    control_handler: Some(Arc::new(handler)),
                }
            }
            Kind::Capture => Component::Sink(Box::new(capture::Capture::new(
                descriptor,
                request.configuration.clone(),
            )?))
            .into(),
            Kind::Arithmetic => Component::Transactional(Box::new(arithmetic::Arithmetic::new(
                descriptor,
                request.configuration.clone(),
            )?))
            .into(),
            Kind::Middleware => {
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct Configuration {
                    stream: StreamId,
                    middleware: Vec<SourceMiddlewareConfig>,
                    pipeline: Vec<String>,
                }
                let configuration: Configuration =
                    serde_json::from_value(request.configuration.clone())?;
                let mut registry = MiddlewareTypeRegistry::new();
                registry.register(Arc::new(drasi_middleware::map::MapFactory::new()));
                registry.register(Arc::new(
                    drasi_middleware::relabel::RelabelMiddlewareFactory::new(),
                ));
                Component::Transformer(Box::new(MiddlewareTransformer::new(
                    MiddlewareTransformerDefinition {
                        id: request.id.clone(),
                        output_stream: configuration.stream,
                        middleware: configuration.middleware,
                        pipeline: configuration.pipeline,
                    },
                    Arc::new(registry),
                )?))
                .into()
            }
        })
    }
}

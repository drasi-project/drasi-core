// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Native ComputationGraph HTTP sinks.
//!
//! * `drasi.network/http-sink` sends each graph change (or, with adaptive
//!   batching, each run of changes) as an HTTP request rendered from
//!   optional Handlebars templates. Completion is `Handled`: the destination
//!   answered a configured success status. Every request carries a stable
//!   `x-drasi-delivery-id` for deduplication.
//! * `drasi.network/http-delivery` is an external consumer for the host
//!   `DeliveryRunner`: it sends each input batch once to a confirming endpoint
//!   and completes only on an exact `200` echoing the batch identity.
//!
//! See `DESIGN.md` and `CONFIGURATION.md` in this crate.

mod component;
pub mod config;
pub mod consumer;
pub mod delivery;
mod render;
mod transport;

use drasi_computation_contracts::{
    ComponentRole, ComponentUpdateMode, GraphChangeCodec, ImplementationIdentity, PipeRequirements,
    PortDescriptor, PortDirection, PortId, SinkCompletion, SinkOutputs,
};
use drasi_computation_network_common::wire;
use drasi_computation_plugin_sdk::{
    lifecycle::Assessment, Capabilities, Component, ConsumerMode, ControlSender, CreateRequest,
    CreatedComponent, Factory, FactoryMetadata, PluginDefinition, UpdateRequest,
};
use std::sync::Arc;

pub use config::{HttpSinkConfig, PermanentFailurePolicy};
pub use consumer::{HttpDeliveryConfig, HttpDeliveryConsumer};
pub use transport::{StatusError, MAX_RESPONSE_BODY_BYTES};

pub const PLUGIN_ID: &str = env!("CARGO_PKG_NAME");
pub const HTTP_SINK: &str = "drasi.network/http-sink";
pub const HTTP_DELIVERY: &str = "drasi.network/http-delivery";

/// Factory for `drasi.network/http-sink`.
pub struct HttpSinkFactory;

/// Factory for `drasi.network/http-delivery`.
pub struct HttpDeliveryFactory;

/// The plugin definition exported by the dynamic library. The HTTP sink is
/// the first factory.
pub fn plugin() -> anyhow::Result<PluginDefinition> {
    PluginDefinition::new(
        PLUGIN_ID,
        env!("CARGO_PKG_VERSION"),
        vec![Arc::new(HttpSinkFactory), Arc::new(HttpDeliveryFactory)],
        vec![GraphChangeCodec::schema()],
    )
}

#[cfg(feature = "dynamic-plugin")]
drasi_computation_plugin_sdk::export_computation_plugin!(plugin());

fn input_port() -> PortDescriptor {
    PortDescriptor::new(
        PortId::try_new("in").expect("constant port"),
        PortDirection::Input,
        GraphChangeCodec::schema().descriptor().clone(),
        PipeRequirements::default(),
    )
}

fn managed() -> Capabilities {
    Capabilities {
        management: true,
        update_modes: vec![
            ComponentUpdateMode::InPlace,
            ComponentUpdateMode::Restart,
            ComponentUpdateMode::Recreate,
        ],
        erases_owned_state: true,
        ..Capabilities::default()
    }
}

impl Factory for HttpSinkFactory {
    /// Inputs do not key delivery on producer identity.
    fn describe(
        &self,
        request: &drasi_computation_plugin_sdk::wire::DescribeRequest,
    ) -> anyhow::Result<drasi_computation_plugin_sdk::ComponentDescription> {
        let config = HttpSinkConfig::parse(request.request.configuration.clone())?;
        Ok(request.unkeyed_sink(config.on_permanent_failure))
    }
    fn configuration_details(&self) -> drasi_computation_plugin_sdk::ConfigDetails {
        HttpSinkConfig::definition().details()
    }

    fn metadata(&self) -> FactoryMetadata {
        FactoryMetadata {
            // Constant identifiers; validated by `metadata_is_valid`.
            implementation: ImplementationIdentity::try_new(HTTP_SINK, "1")
                .expect("constant implementation identity"),
            role: ComponentRole::Sink,
            configuration_version: 1,
            configuration: HttpSinkConfig::definition().schema(),
            ports: vec![input_port()],
            completion: Some(SinkCompletion::Handled),
            capabilities: Capabilities {
                binds_inputs: true,
                services: drasi_computation_plugin_sdk::HostServices {
                    sink_batching: true,
                    ..Default::default()
                },
                ..managed()
            },
        }
    }

    fn create(
        &self,
        request: &CreateRequest,
        _control: ControlSender,
        _services: drasi_computation_plugin_sdk::ServiceBindings,
    ) -> anyhow::Result<CreatedComponent> {
        let config = HttpSinkConfig::parse(request.configuration.clone())?;
        let outputs = SinkOutputs::from_inputs(&request.inputs, &config.outputs)?;
        Ok(Component::Sink(Box::new(component::HttpSink::new(
            self.metadata().descriptor(request.id.clone())?,
            config,
            outputs,
        )?))
        .into())
    }
    fn update_planner(&self) -> Option<&dyn drasi_computation_plugin_sdk::UpdatePlanner> {
        Some(self)
    }
}

impl drasi_computation_plugin_sdk::UpdatePlanner for HttpSinkFactory {
    fn assess(&self, request: &UpdateRequest) -> anyhow::Result<Assessment> {
        component::assess(request)
    }
}

impl Factory for HttpDeliveryFactory {
    /// A consumer that delivers exactly once, keyed on durable producer identity.
    fn describe(
        &self,
        request: &drasi_computation_plugin_sdk::wire::DescribeRequest,
    ) -> anyhow::Result<drasi_computation_plugin_sdk::ComponentDescription> {
        let config = consumer::HttpDeliveryConfig::parse(request.request.configuration.clone())?;
        Ok(request.consumer(
            drasi_computation_plugin_sdk::DeliveryGuarantee::ExactlyOnce,
            config.on_permanent_failure,
        ))
    }
    fn configuration_details(&self) -> drasi_computation_plugin_sdk::ConfigDetails {
        HttpDeliveryConfig::definition().details()
    }

    fn metadata(&self) -> FactoryMetadata {
        FactoryMetadata {
            // Constant identifiers; validated by `metadata_is_valid`.
            implementation: ImplementationIdentity::try_new(HTTP_DELIVERY, "1")
                .expect("constant implementation identity"),
            role: ComponentRole::Sink,
            configuration_version: 1,
            configuration: HttpDeliveryConfig::definition().schema(),
            ports: vec![input_port()],
            completion: Some(SinkCompletion::Handled),
            capabilities: Capabilities {
                services: drasi_computation_plugin_sdk::HostServices {
                    consumer: Some(ConsumerMode::External),
                    ..Default::default()
                },
                ..managed()
            },
        }
    }

    fn create(
        &self,
        request: &CreateRequest,
        _control: ControlSender,
        _services: drasi_computation_plugin_sdk::ServiceBindings,
    ) -> anyhow::Result<CreatedComponent> {
        Ok(Component::Consumer(Box::new(HttpDeliveryConsumer::new(
            self.metadata().descriptor(request.id.clone())?,
            HttpDeliveryConfig::parse(request.configuration.clone())?,
        )?))
        .into())
    }
    fn update_planner(&self) -> Option<&dyn drasi_computation_plugin_sdk::UpdatePlanner> {
        Some(self)
    }
}

impl drasi_computation_plugin_sdk::UpdatePlanner for HttpDeliveryFactory {
    fn assess(&self, request: &UpdateRequest) -> anyhow::Result<Assessment> {
        consumer::assess(request)
    }
}

#[cfg(test)]
mod tests;

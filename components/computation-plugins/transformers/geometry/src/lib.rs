// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! A native planar geometry transformer: reads geometries from graph-change
//! records, groups them into configured sets and incrementally emits a record
//! for every pair of members satisfying a configured spatial relation. It is
//! one implementation of the shared stateful-transformer runtime: its inputs,
//! memberships and records are durable, and it bootstraps, follows, replays
//! and rebuilds like any other stateful transformer. See the crate README for
//! configuration and semantics.

#![deny(unused, missing_docs)]

mod config;
mod logic;
mod shape;
mod state;

#[cfg(test)]
mod tests;

use anyhow::Context;
use drasi_computation_contracts::{
    ComponentRole, ComponentUpdateMode, GraphChangeCodec, ImplementationIdentity, PipeRequirements,
    PortDescriptor, PortDirection, PortId,
};
use drasi_computation_plugin_sdk::{
    lifecycle::Assessment,
    transformer::{self, StatefulTransformer},
    Capabilities, Component, ControlSender, CreateRequest, CreatedComponent, Factory,
    FactoryMetadata, PluginDefinition, ServiceBindings, UpdatePlanner, UpdateRequest,
};
use std::sync::Arc;

/// Native library identity.
pub const PLUGIN_ID: &str = env!("CARGO_PKG_NAME");
/// Configuration-only factory for the planar geometry transformer.
pub const GEOMETRY: &str = "drasi.standard/geometry";

/// Constructs geometry transformers.
pub struct GeometryFactory;

/// The plugin definition, suitable for static registration or native export.
pub fn plugin() -> anyhow::Result<PluginDefinition> {
    PluginDefinition::new(
        PLUGIN_ID,
        env!("CARGO_PKG_VERSION"),
        vec![Arc::new(GeometryFactory)],
        vec![GraphChangeCodec::schema()],
    )
}

#[cfg(feature = "dynamic-plugin")]
#[allow(missing_docs, reason = "The shared SDK macro emits ABI entry points.")]
mod native_exports {
    drasi_computation_plugin_sdk::export_computation_plugin!(super::plugin());
}

impl Factory for GeometryFactory {
    fn describe(
        &self,
        request: &drasi_computation_plugin_sdk::wire::DescribeRequest,
    ) -> anyhow::Result<drasi_computation_plugin_sdk::ComponentDescription> {
        transformer::describe(request)
    }

    fn configuration_details(&self) -> drasi_computation_plugin_sdk::ConfigDetails {
        config::definition().details()
    }

    fn update_planner(&self) -> Option<&dyn UpdatePlanner> {
        Some(self)
    }

    fn metadata(&self) -> FactoryMetadata {
        FactoryMetadata {
            implementation: ImplementationIdentity::try_new(GEOMETRY, "1")
                .expect("constant identity"),
            role: ComponentRole::Transformer,
            configuration_version: 1,
            configuration: config::definition().schema(),
            ports: [
                (transformer::INPUT_PORT, PortDirection::Input),
                (transformer::OUTPUT_PORT, PortDirection::Output),
            ]
            .into_iter()
            .map(|(id, direction)| {
                PortDescriptor::new(
                    PortId::try_new(id).expect("constant port"),
                    direction,
                    GraphChangeCodec::schema().descriptor().clone(),
                    PipeRequirements::default(),
                )
            })
            .collect(),
            completion: None,
            capabilities: Capabilities {
                update_modes: vec![ComponentUpdateMode::InPlace, ComponentUpdateMode::Recreate],
                ..transformer::capabilities(true)
            },
        }
    }

    fn create(
        &self,
        request: &CreateRequest,
        _control: ControlSender,
        services: ServiceBindings,
    ) -> anyhow::Result<CreatedComponent> {
        transformer::validate_reconstruction(request)?;
        let (logic, settings) = logic::GeometryLogic::create(request)?;
        Ok(Component::Transformer(Box::new(StatefulTransformer::new(
            request,
            self.metadata().descriptor(request.id.clone())?,
            services
                .processing
                .context("the geometry transformer requires host-owned processing")?,
            logic,
            settings,
        )?))
        .into())
    }
}

/// Configuration-only planning; a running transformer also checks that an
/// in-place change fits what it retains.
impl UpdatePlanner for GeometryFactory {
    fn assess(&self, request: &UpdateRequest) -> anyhow::Result<Assessment> {
        Ok(logic::plan(None, request)?.into())
    }
}

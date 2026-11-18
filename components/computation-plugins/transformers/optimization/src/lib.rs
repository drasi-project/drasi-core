// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! A native mixed-integer/linear optimization transformer. A declarative
//! model over the current input dataset is re-solved in the background when
//! inputs change; solution, status and scenario rows are emitted
//! incrementally. See the crate README for the model and its semantics.

#![deny(unused, missing_docs)]

mod component;
mod config;
mod model;
mod output;
mod worker;

use drasi_computation_contracts::{
    ComponentRole, ComponentUpdateMode, GraphChangeCodec, ImplementationIdentity, OutputProgress,
    PipeRequirements, PortDescriptor, PortDirection, PortId,
};
use drasi_computation_plugin_sdk::{
    lifecycle::Assessment, Capabilities, Component, ControlSender, CreateRequest, CreatedComponent,
    Factory, FactoryMetadata, PluginDefinition, UpdateRequest,
};
use std::sync::Arc;

use crate::config::OptimizationConfig;

/// Native library identity.
pub const PLUGIN_ID: &str = env!("CARGO_PKG_NAME");
/// Configuration-only factory for the optimization transformer.
pub const OPTIMIZATION: &str = "drasi.standard/optimization";

/// Constructs independent, initially empty optimization transformers.
pub struct OptimizationFactory;

/// The plugin definition, suitable for static registration or native export.
pub fn plugin() -> anyhow::Result<PluginDefinition> {
    PluginDefinition::new(
        PLUGIN_ID,
        env!("CARGO_PKG_VERSION"),
        vec![Arc::new(OptimizationFactory)],
        vec![GraphChangeCodec::schema()],
    )
}

#[cfg(feature = "dynamic-plugin")]
#[allow(missing_docs, reason = "The shared SDK macro emits ABI entry points.")]
mod native_exports {
    drasi_computation_plugin_sdk::export_computation_plugin!(super::plugin());
}

impl Factory for OptimizationFactory {
    /// Outputs carry a new volatile producer identity on every construction.
    fn describe(
        &self,
        request: &drasi_computation_plugin_sdk::wire::DescribeRequest,
    ) -> anyhow::Result<drasi_computation_plugin_sdk::ComponentDescription> {
        Ok(request.volatile_transformer())
    }

    fn configuration_details(&self) -> Option<drasi_computation_plugin_sdk::ConfigDetails> {
        Some(OptimizationConfig::definition().details())
    }

    fn supports_update_planning(&self) -> bool {
        true
    }

    fn assess(&self, request: &UpdateRequest) -> anyhow::Result<Assessment> {
        component::assess(request)
    }

    fn metadata(&self) -> FactoryMetadata {
        FactoryMetadata {
            implementation: ImplementationIdentity::try_new(OPTIMIZATION, "1")
                .expect("constant identity"),
            role: ComponentRole::Transformer,
            configuration_version: 1,
            configuration: OptimizationConfig::definition().schema(),
            ports: [("in", PortDirection::Input), ("out", PortDirection::Output)]
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
                wakeups: true,
                output_snapshots: vec![PortId::try_new("out").expect("constant port")],
                output_progress: Some(OutputProgress::Origin),
                erases_owned_state: true,
                management: true,
                binds_inputs: true,
                update_modes: vec![ComponentUpdateMode::InPlace, ComponentUpdateMode::Recreate],
                ..Capabilities::default()
            },
        }
    }

    fn create(
        &self,
        request: &CreateRequest,
        control: ControlSender,
    ) -> anyhow::Result<CreatedComponent> {
        self.create_with_output_snapshots(request, control, None)
    }

    /// With an `output_snapshots` dependency, the transformer initializes
    /// from its upstream outputs' snapshots (see the README).
    fn supports_output_snapshots(&self) -> bool {
        true
    }

    fn create_with_output_snapshots(
        &self,
        request: &CreateRequest,
        _control: ControlSender,
        snapshots: Option<drasi_computation_plugin_sdk::NativeOutputSnapshots>,
    ) -> anyhow::Result<CreatedComponent> {
        let config = OptimizationConfig::parse(request.configuration.clone())?;
        Ok(
            Component::Transformer(Box::new(component::Optimization::new(
                self.metadata().descriptor(request.id.clone())?,
                config,
                request.scope.as_ref(),
                &request.inputs,
                snapshots.map(|snapshots| {
                    Arc::new(snapshots) as Arc<dyn drasi_computation_plugin_sdk::SnapshotSource>
                }),
            )?))
            .into(),
        )
    }
}

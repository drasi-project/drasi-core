// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Stateful middleware: the same pipeline as the stateless transformer, run
//! as a stateful transformer whose output is the transformed current state.
//! Storage, bootstrap from a provider or its input, recovery and the output
//! readers are the shared stateful-transformer runtime.

use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use drasi_computation_contracts::{
    ChangeEnvelope, ComponentId, ComponentRole, GraphChangeCodec, ImplementationIdentity,
    PipeRequirements, PortDescriptor, PortDirection, PortId, StreamId,
};
use drasi_computation_middleware_common::{Mode, Pipeline};
use drasi_computation_plugin_sdk::{
    transformer::{self, Observations, Phase, StatefulTransformer, TransformerLogic},
    Component, ConfigDefinition, ControlSender, CreateRequest, CreatedComponent, Factory,
    FactoryMetadata, NativeProcessingContext, ServiceBindings,
};
use drasi_core_types::models::{SourceChange, SourceMiddlewareConfig};
use serde::Deserialize;

pub const STATEFUL_MIDDLEWARE: &str = "drasi.standard/stateful-middleware";

/// The pipeline's own configuration; the runtime parses its shared options.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Configuration {
    stream: StreamId,
    middleware: Vec<SourceMiddlewareConfig>,
    pipeline: Vec<String>,
}

impl Configuration {
    fn parse(configuration: &serde_json::Value) -> Result<Self> {
        let mut fields = configuration
            .as_object()
            .context("middleware configuration must be an object")?
            .clone();
        for (name, _, _) in transformer::configuration_fields() {
            fields.remove(name);
        }
        Ok(serde_json::from_value(serde_json::Value::Object(fields))?)
    }
}

struct MiddlewareLogic {
    pipeline: Pipeline,
    owner: ComponentId,
    stream: StreamId,
}

#[async_trait]
impl TransformerLogic for MiddlewareLogic {
    /// The pipeline keeps no state of its own; the output is the state.
    type State = ();
    type Prepared = Vec<SourceChange>;
    type Evaluation = Vec<SourceChange>;

    async fn open(&self, _context: &Arc<NativeProcessingContext>) -> Result<Arc<()>> {
        Ok(Arc::new(()))
    }

    fn prepare(
        &self,
        input: &ChangeEnvelope,
        _scope: Option<ComponentId>,
        _source: Option<ComponentId>,
        _phase: Phase,
    ) -> Result<Vec<SourceChange>> {
        Ok(GraphChangeCodec::decode_changes(input)?)
    }

    async fn evaluate(
        &self,
        _state: &(),
        changes: &mut Vec<SourceChange>,
        _phase: Phase,
        _observations: &Observations,
    ) -> Result<Vec<SourceChange>> {
        let mut output = Vec::new();
        for change in changes.drain(..) {
            output.extend(
                self.pipeline
                    .apply(change)
                    .await
                    .with_context(|| format!("middleware pipeline failed in {}", self.owner))?,
            );
        }
        // A scheduled trigger is not part of the transformed state.
        output.retain(|change| !matches!(change, SourceChange::Future { .. }));
        Ok(output)
    }

    fn project(
        &self,
        input: &ChangeEnvelope,
        _changes: Vec<SourceChange>,
        output: Vec<SourceChange>,
        sequence: u64,
    ) -> Result<Option<ChangeEnvelope>> {
        if output.is_empty() {
            return Ok(None);
        }
        Ok(Some(GraphChangeCodec::derive_changes(
            input,
            &output,
            self.stream.clone(),
            sequence,
        )?))
    }
}

fn configuration_definition() -> ConfigDefinition {
    ConfigDefinition::try_new(
        Mode::Events
            .options()
            .into_iter()
            .chain(transformer::configuration_options()),
    )
    .expect("constant stateful middleware configuration")
}

/// Middleware that maintains the transformed current state of its input.
pub struct StatefulMiddlewareFactory;

impl Factory for StatefulMiddlewareFactory {
    fn describe(
        &self,
        request: &drasi_computation_plugin_sdk::wire::DescribeRequest,
    ) -> Result<drasi_computation_plugin_sdk::ComponentDescription> {
        transformer::describe(request)
    }

    fn configuration_details(&self) -> drasi_computation_plugin_sdk::ConfigDetails {
        configuration_definition().details()
    }

    fn metadata(&self) -> FactoryMetadata {
        FactoryMetadata {
            implementation: ImplementationIdentity::try_new(STATEFUL_MIDDLEWARE, "1")
                .expect("constant"),
            role: ComponentRole::Transformer,
            configuration_version: 1,
            configuration: configuration_definition().schema(),
            ports: [
                (transformer::INPUT_PORT, PortDirection::Input),
                (transformer::OUTPUT_PORT, PortDirection::Output),
            ]
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
            capabilities: transformer::capabilities(false),
        }
    }

    fn create(
        &self,
        request: &CreateRequest,
        _control: ControlSender,
        services: ServiceBindings,
    ) -> Result<CreatedComponent> {
        let configuration = Configuration::parse(&request.configuration)?;
        let pipeline = Pipeline::new(
            Mode::Events,
            &configuration.middleware,
            &configuration.pipeline,
        )?;
        let settings = transformer::Settings {
            stream: configuration.stream.clone(),
            capacity: transformer::output_capacity(request)?,
            dataset: request.id.to_string(),
            identity: serde_json::json!({
                "middleware": request.configuration["middleware"],
                "pipeline": configuration.pipeline,
            }),
            source_transactions: None,
        };
        let logic = MiddlewareLogic {
            pipeline,
            owner: request.id.clone(),
            stream: configuration.stream,
        };
        Ok(Component::Transformer(Box::new(StatefulTransformer::new(
            request,
            self.metadata().descriptor(request.id.clone())?,
            services
                .processing
                .context("stateful middleware requires host-owned processing")?,
            logic,
            settings,
        )?))
        .into())
    }
}

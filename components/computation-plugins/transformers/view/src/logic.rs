// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! The view as one implementation of the shared stateful-transformer runtime.

use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use drasi_computation_contracts::{ChangeEnvelope, ComponentId, GraphChangeCodec, StreamId};
use drasi_computation_middleware_common::{Mode, Pipeline};
use drasi_computation_plugin_sdk::{
    transformer::{self, HostState, Observations, Phase, TransformerLogic},
    CreateRequest, NativeProcessingContext,
};
use drasi_core_types::models::{SourceChange, SourceMiddlewareConfig};
use serde::Deserialize;

use crate::{mapping::Mapper, state};

/// The view's own configuration; the runtime parses its shared options.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Configuration {
    pub(super) stream: StreamId,
    pub(super) middleware: Vec<SourceMiddlewareConfig>,
    pub(super) pipeline: Vec<String>,
}

impl Configuration {
    pub(super) fn parse(configuration: &serde_json::Value) -> Result<Self> {
        let mut fields = configuration
            .as_object()
            .context("view configuration must be an object")?
            .clone();
        for (name, _, _) in transformer::configuration_fields() {
            fields.remove(name);
        }
        Ok(serde_json::from_value(serde_json::Value::Object(fields))?)
    }

    /// The complete-image mapping pipeline this configuration declares.
    pub(super) fn mapper(&self) -> Result<Pipeline> {
        Pipeline::new(Mode::Images, &self.middleware, &self.pipeline)
    }
}

pub(super) struct ViewLogic {
    mapper: Box<dyn Mapper>,
    stream: StreamId,
    state: HostState,
}

impl ViewLogic {
    pub(super) fn create(request: &CreateRequest) -> Result<(Self, transformer::Settings)> {
        let configuration = Configuration::parse(&request.configuration)?;
        let mapper = configuration.mapper()?;
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
        Ok((
            Self {
                mapper: Box::new(mapper),
                stream: configuration.stream,
                state: HostState::default(),
            },
            settings,
        ))
    }
}

/// The output envelope for one input envelope's net changes, or `None` when
/// the view did not change.
pub(super) fn project(
    input: &ChangeEnvelope,
    output: &[SourceChange],
    stream: &StreamId,
    sequence: u64,
) -> Result<Option<ChangeEnvelope>> {
    if output.is_empty() {
        return Ok(None);
    }
    let envelope = GraphChangeCodec::derive_changes(input, output, stream.clone(), sequence)?;
    envelope.validate_lineage()?;
    Ok(Some(envelope))
}

#[async_trait]
impl TransformerLogic for ViewLogic {
    /// The view's state lives in its host stores.
    type State = ();
    type Prepared = Vec<SourceChange>;
    type Evaluation = Vec<SourceChange>;

    fn bind(&self, context: &Arc<NativeProcessingContext>) -> Result<Box<dyn Send>> {
        self.state.bind(context)
    }

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
        state::transform(&self.state, changes, &*self.mapper).await
    }

    fn project(
        &self,
        input: &ChangeEnvelope,
        _changes: Vec<SourceChange>,
        output: Vec<SourceChange>,
        sequence: u64,
    ) -> Result<Option<ChangeEnvelope>> {
        project(input, &output, &self.stream, sequence)
    }
}

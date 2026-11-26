// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Geometry as one implementation of the shared stateful-transformer runtime.

use std::{
    num::NonZeroUsize,
    sync::{Arc, Mutex, RwLock},
};

use anyhow::{Context, Result};
use async_trait::async_trait;
use drasi_computation_contracts::{
    ChangeEnvelope, ComponentId, ComponentUpdatePlan, GraphChangeCodec, StreamId,
};
use drasi_computation_plugin_sdk::{
    transformer::{self, HostState, Observations, Phase, TransformerLogic},
    update_planning::{ensure_same_component, inputs_changed},
    ApplyUpdateRequest, CreateRequest, NativeProcessingContext, UpdateRequest,
};
use drasi_core_types::models::SourceChange;

use crate::{
    config::{self, GeometryConfig},
    state::{self, Engine, State},
};

pub(crate) struct GeometryLogic {
    /// Replaced by an in-place update.
    engine: RwLock<Arc<Engine>>,
    stream: StreamId,
    store: HostState,
}

impl GeometryLogic {
    pub(crate) fn create(request: &CreateRequest) -> Result<(Self, transformer::Settings)> {
        let config = GeometryConfig::parse(&request.configuration)?;
        let settings = transformer::Settings {
            stream: config.stream.clone(),
            capacity: transformer::output_capacity(request)?,
            dataset: request.id.to_string(),
            identity: config::identity(&config)?,
            source_transactions: None,
        };
        let stream = config.stream.clone();
        let engine = Engine::new(config, &request.id.to_string())?;
        Ok((
            Self {
                engine: RwLock::new(Arc::new(engine)),
                stream,
                store: HostState::default(),
            },
            settings,
        ))
    }

    pub(crate) fn engine(&self) -> Result<Arc<Engine>> {
        Ok(self
            .engine
            .read()
            .map_err(|_| anyhow::anyhow!("geometry configuration poisoned"))?
            .clone())
    }
}

/// Plan an update. While the transformer runs (`state`), an in-place change
/// must also fit what it retains.
pub(crate) fn plan(
    state: Option<&Mutex<State>>,
    request: &UpdateRequest,
) -> Result<ComponentUpdatePlan> {
    ensure_same_component(request)?;
    let current =
        GeometryConfig::parse(&request.current.configuration).context("current configuration")?;
    let proposed =
        GeometryConfig::parse(&request.proposed.configuration).context("proposed configuration")?;
    let mut in_place = config::in_place(&current, &proposed, inputs_changed(request))?;
    if let (true, Some(state)) = (in_place, state) {
        in_place = state
            .lock()
            .map_err(|_| anyhow::anyhow!("geometry state poisoned"))?
            .fits(&proposed.limits);
    }
    transformer::update_plan(request, in_place)
}

/// The output envelope for one input envelope's record changes, or `None`
/// when no record changed.
pub(crate) fn project(
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
impl TransformerLogic for GeometryLogic {
    type State = Mutex<State>;
    type Prepared = Vec<SourceChange>;
    type Evaluation = Vec<SourceChange>;

    fn bind(&self, context: &Arc<NativeProcessingContext>) -> Result<Box<dyn Send>> {
        self.store.bind(context)
    }

    async fn open(&self, _context: &Arc<NativeProcessingContext>) -> Result<Arc<Mutex<State>>> {
        let engine = self.engine()?;
        Ok(Arc::new(Mutex::new(
            State::open(&self.store, &engine).await?,
        )))
    }

    async fn reset(&self, state: &Mutex<State>) -> Result<()> {
        let sets = self.engine()?.config.sets.len();
        *state
            .lock()
            .map_err(|_| anyhow::anyhow!("geometry state poisoned"))? = State::new(sets);
        Ok(())
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
        state: &Mutex<State>,
        changes: &mut Vec<SourceChange>,
        _phase: Phase,
        _observations: &Observations,
    ) -> Result<Vec<SourceChange>> {
        let engine = self.engine()?;
        state::transform(&self.store, state, &engine, changes).await
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

    fn manages_updates(&self) -> bool {
        true
    }

    fn plan_update(
        &self,
        state: Option<&Mutex<State>>,
        request: &UpdateRequest,
    ) -> Option<Result<ComponentUpdatePlan>> {
        Some(plan(state, request))
    }

    fn apply_update(&self, request: &ApplyUpdateRequest) -> Result<NonZeroUsize> {
        let proposed = GeometryConfig::parse(&request.request.proposed.configuration)?;
        // Built completely before the swap, so a failure leaves the transformer unchanged.
        let engine = Engine::new(proposed, self.engine()?.namespace())?;
        let capacity = transformer::output_capacity(&request.request.proposed)?;
        *self
            .engine
            .write()
            .map_err(|_| anyhow::anyhow!("geometry configuration poisoned"))? = Arc::new(engine);
        Ok(capacity)
    }
}

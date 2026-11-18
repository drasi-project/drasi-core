// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! The geometry transformer component: lifecycle, management and transform.

use anyhow::{Context, Result};
use async_trait::async_trait;
use drasi_computation_contracts::{
    ComponentDescriptor, ComponentUpdateData, GraphChangeBatch, GraphChangeCodec,
    GraphProducerIdentity, GraphProducerProgress, InputEnvelope, OutputEnvelope, OutputReaders,
    PortId,
};
use drasi_computation_plugin_sdk::{
    lifecycle::{
        nothing_to_do, ApplyContext, ApplyError, Assessment, DisposeContext, InitializeContext,
        LifecycleError, LifecycleResult, PauseContext, ResumeContext, StartContext, StopContext,
        UpdateBoundary,
    },
    update_planning::{ensure_same_component, inputs_changed},
    ApplyUpdateRequest, NativeComponent, NativeManagement, NativeTransformer, Scope, UpdateRequest,
};

use crate::{
    config::{self, GeometryConfig},
    output::Output,
    state::{Engine, State},
};

pub(crate) struct GeometryTransformer {
    descriptor: ComponentDescriptor,
    engine: Engine,
    identity: GraphProducerIdentity,
    sequence: u64,
    running: bool,
    paused: bool,
    retired: bool,
    state: State,
    output: Output,
}

/// Pure planning used by the factory: the proposed configuration must be
/// constructible.
pub(crate) fn assess(request: &UpdateRequest) -> Result<Assessment> {
    ensure_same_component(request)?;
    let current = GeometryConfig::parse(request.current.configuration.clone())
        .context("current configuration")?;
    let proposed = GeometryConfig::parse(request.proposed.configuration.clone())
        .context("proposed configuration")?;
    Ok(config::plan(&current, &proposed, inputs_changed(request)))
}

fn refused(reason: impl std::fmt::Display) -> ApplyError {
    ApplyError::Refused {
        now_requires: None,
        reason: reason.to_string(),
    }
}

impl GeometryTransformer {
    pub(crate) fn new(
        descriptor: ComponentDescriptor,
        configuration: serde_json::Value,
        scope: Option<&Scope>,
    ) -> Result<Self> {
        let config = GeometryConfig::parse(configuration)?;
        let namespace = descriptor.id().to_string();
        let identity = GraphProducerIdentity::volatile(
            scope
                .map_or("volatile", |scope| scope.instance_id.as_str())
                .into(),
            scope
                .map_or("volatile", |scope| scope.graph_id.as_str())
                .into(),
            descriptor.id().clone(),
            config.stream.clone(),
        )?;
        let output = Output::new(identity.clone(), namespace.as_str().into())?;
        let state = State::new(config.sets.len());
        Ok(Self {
            descriptor,
            engine: Engine::new(config, &namespace)?,
            identity,
            sequence: 0,
            running: false,
            paused: false,
            retired: false,
            state,
            output,
        })
    }

    /// The factory plan, strengthened when the retained state exceeds new limits.
    fn instance_plan(
        &self,
        current: &GeometryConfig,
        proposed: &GeometryConfig,
        request: &UpdateRequest,
    ) -> Assessment {
        let assessment = config::plan(current, proposed, inputs_changed(request));
        match assessment {
            Assessment::Supported {
                boundary: UpdateBoundary::Pause,
                ..
            } if !self.state.fits(&proposed.limits) => {
                Assessment::supported(UpdateBoundary::Replace, ComponentUpdateData::Reset)
            }
            assessment => assessment,
        }
    }
}

#[async_trait]
impl NativeComponent for GeometryTransformer {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }

    fn configuration(&self) -> Result<serde_json::Value> {
        Ok(serde_json::to_value(&self.engine.config)?)
    }

    fn output_readers(&self) -> OutputReaders {
        OutputReaders::new().with_snapshot(
            PortId::try_new("out").expect("constant port"),
            self.output.reader(),
        )
    }

    fn management(&mut self) -> Option<&mut dyn NativeManagement> {
        Some(self)
    }

    async fn initialize(&mut self, _ctx: &InitializeContext) -> LifecycleResult {
        nothing_to_do()
    }

    /// Opens input handling. Retained state survives Stop/Start.
    async fn start(&mut self, ctx: &mut StartContext) -> LifecycleResult {
        if ctx.cancellation().is_cancelled() {
            return Err(LifecycleError::cancelled());
        }
        if self.retired {
            return Err(anyhow::anyhow!("geometry state has been deprovisioned").into());
        }
        if self.running {
            return Err(anyhow::anyhow!("geometry transformer already started").into());
        }
        self.output.set_running(true)?;
        self.running = true;
        self.paused = false;
        Ok(())
    }

    /// There is no worker: once the host settles the in-flight transform the
    /// pause is complete, and input offered while paused is refused.
    async fn pause(&mut self, _ctx: &PauseContext) -> LifecycleResult {
        if self.running {
            self.paused = true;
        }
        Ok(())
    }

    async fn resume(&mut self, _ctx: &ResumeContext) -> LifecycleResult {
        self.paused = false;
        Ok(())
    }

    async fn stop(&mut self, _ctx: &StopContext) -> LifecycleResult {
        if !self.retired {
            self.output.set_running(false)?;
        }
        self.running = false;
        self.paused = false;
        Ok(())
    }

    /// Erasing dispose drops the retained inputs and records; Preserve keeps them.
    async fn dispose(&mut self, ctx: &DisposeContext) -> LifecycleResult {
        if self.running {
            return Err(
                anyhow::anyhow!("stop the geometry transformer before disposing it").into(),
            );
        }
        if ctx.erases_owned_state() && !self.retired {
            self.output.retire()?;
            self.state = State::new(self.engine.config.sets.len());
            self.retired = true;
        }
        Ok(())
    }
}

#[async_trait]
impl NativeManagement for GeometryTransformer {
    fn assess(&self, request: &UpdateRequest) -> Result<Assessment> {
        ensure_same_component(request)?;
        let current = GeometryConfig::parse(request.current.configuration.clone())
            .context("current configuration")?;
        let proposed = GeometryConfig::parse(request.proposed.configuration.clone())
            .context("proposed configuration")?;
        Ok(self.instance_plan(&current, &proposed, request))
    }

    async fn apply(
        &mut self,
        ctx: &ApplyContext,
        update: &ApplyUpdateRequest,
    ) -> Result<(), ApplyError> {
        let current =
            GeometryConfig::parse(update.request.current.configuration.clone()).map_err(refused)?;
        let proposed = GeometryConfig::parse(update.request.proposed.configuration.clone())
            .map_err(refused)?;
        if current != self.engine.config {
            return Err(refused(
                "the geometry configuration changed after the update was assessed",
            ));
        }
        let Some(plan) = self
            .instance_plan(&current, &proposed, &update.request)
            .plan()
        else {
            return Err(refused("the update is not supported"));
        };
        if plan != update.plan {
            return Err(ApplyError::Refused {
                now_requires: Some(plan.mode.into()),
                reason: "the update now requires a different boundary".into(),
            });
        }
        match ctx.boundary() {
            UpdateBoundary::Pause => {
                if self.running && !self.paused {
                    return Err(refused("in-place updates require a completed pause"));
                }
            }
            UpdateBoundary::Stop => {
                return Err(refused("geometry updates do not use the Stop boundary"))
            }
            UpdateBoundary::Replace => return Err(refused("Replace is not an instance update")),
        }
        // Built completely before the swap, so a failure leaves the transformer unchanged.
        self.engine = Engine::new(proposed, self.engine.namespace()).map_err(refused)?;
        Ok(())
    }
}

#[async_trait]
impl NativeTransformer for GeometryTransformer {
    async fn transform(&mut self, input: InputEnvelope) -> Result<Vec<OutputEnvelope>> {
        anyhow::ensure!(
            self.running && !self.retired,
            "geometry transformer is not running"
        );
        anyhow::ensure!(!self.paused, "geometry transformer is paused");
        anyhow::ensure!(input.port.as_str() == "in", "unknown geometry input port");
        GraphChangeBatch::check_envelope(&input.envelope)
            .context("geometry transforms accept graph data, not scheduling or recovery frames")?;
        let sequence = self
            .sequence
            .checked_add(1)
            .context("geometry output sequence exhausted")?;
        let delta = self.state.prepare(
            GraphChangeCodec::decode_changes(&input.envelope)?,
            &self.engine,
        )?;
        let mut envelope = GraphChangeCodec::derive_changes(
            &input.envelope,
            &delta.changes,
            self.engine.config.stream.clone(),
            sequence,
        )?;
        envelope.validate_lineage()?;
        GraphProducerProgress::annotate(&mut envelope, &self.identity, sequence)?;
        let port = PortId::try_new("out")?;
        self.state.commit(delta);
        self.sequence = sequence;
        self.output.publish(self.state.outputs(), sequence)?;
        Ok(vec![OutputEnvelope {
            execution_context: input.execution_context.clone(),
            port,
            envelope,
        }])
    }
}

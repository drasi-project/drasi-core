// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use anyhow::{Context, Result};
use async_trait::async_trait;
use drasi_computation_contracts::{
    ChangeEnvelope, ComponentDescriptor, GraphChangeCodec, GraphProducerIdentity,
    GraphProducerProgress, InputBinding, InputEnvelope, OutputCursor, OutputEnvelope,
    OutputReadError, OutputReaders, PortId, StateProjector, StreamId,
};
use drasi_computation_middleware_common::{Mode, Pipeline};
use drasi_computation_plugin_sdk::lifecycle::{
    nothing_to_do, DisposeContext, InitializeContext, LifecycleResult, PauseContext, ResumeContext,
    StartContext, StopContext,
};
use drasi_computation_plugin_sdk::{NativeComponent, NativeInputBindings, NativeTransformer};
use drasi_core_types::models::{SourceChange, SourceMiddlewareConfig};
use serde::{Deserialize, Serialize};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

#[cfg(test)]
mod tests;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Configuration {
    stream: StreamId,
    middleware: Vec<SourceMiddlewareConfig>,
    pipeline: Vec<String>,
}

struct ProcessingGuard {
    failed: Arc<AtomicBool>,
    complete: bool,
}

impl Drop for ProcessingGuard {
    fn drop(&mut self) {
        if !self.complete {
            self.failed.store(true, Ordering::Release);
        }
    }
}

struct Mapping {
    pipeline: Pipeline,
    owner: GraphProducerIdentity,
    positional: bool,
}

impl Mapping {
    async fn apply(&self, input: &ChangeEnvelope) -> Result<Vec<SourceChange>> {
        let mut output = Vec::new();
        for change in GraphChangeCodec::decode_changes(input)? {
            output.extend(self.pipeline.apply(change).await.with_context(|| {
                format!(
                    "middleware pipeline failed in {}",
                    self.owner.component_id()
                )
            })?);
        }
        Ok(output)
    }
}

#[async_trait]
impl StateProjector for Mapping {
    fn cut_namespace(&self) -> Option<&GraphProducerIdentity> {
        self.positional.then_some(&self.owner)
    }

    async fn project(&self, batch: ChangeEnvelope) -> Result<ChangeEnvelope, OutputReadError> {
        drasi_computation_contracts::validate_state_batch(
            &batch,
            GraphChangeCodec::schema().descriptor(),
        )?;
        let rows = self.apply(&batch).await.map_err(OutputReadError::Failed)?;
        GraphChangeCodec::encode_changes(
            &rows,
            batch.system().stream().clone(),
            batch.system().sequence(),
            None,
        )
        .map_err(|error| OutputReadError::Failed(error.into()))
    }
}

pub(super) struct Middleware {
    descriptor: ComponentDescriptor,
    configuration: Configuration,
    mapping: Arc<Mapping>,
    projectable: bool,
    single_input: Option<InputBinding>,
    identity: GraphProducerIdentity,
    sequence: u64,
    running: bool,
    failed: Arc<AtomicBool>,
}

impl Middleware {
    pub fn new(
        descriptor: ComponentDescriptor,
        value: serde_json::Value,
        scope: Option<&drasi_computation_plugin_sdk::Scope>,
    ) -> Result<Self> {
        let configuration: Configuration = serde_json::from_value(value)?;
        let pipeline = Pipeline::new(
            Mode::Events,
            &configuration.middleware,
            &configuration.pipeline,
        )?;
        let identity = GraphProducerIdentity::volatile(
            scope
                .map_or("volatile", |scope| scope.instance_id.as_str())
                .into(),
            scope
                .map_or("volatile", |scope| scope.graph_id.as_str())
                .into(),
            descriptor.id().clone(),
            configuration.stream.clone(),
        )?;
        let failed = Arc::new(AtomicBool::new(false));
        let steps = configuration.middleware.iter().filter(|step| {
            configuration
                .pipeline
                .iter()
                .any(|name| name == step.name.as_ref())
        });
        // An event map may create or delete unrelated identities. Selecting
        // its insert rules alone cannot reconstruct the state of that history;
        // the stateful middleware maintains that state instead.
        let projectable = !steps.clone().any(|s| s.kind.as_ref() == "map");
        let positional = projectable && steps.clone().all(|s| s.kind.as_ref() == "relabel");
        let mapping = Arc::new(Mapping {
            pipeline,
            owner: identity.clone(),
            positional,
        });
        Ok(Self {
            descriptor,
            configuration,
            mapping,
            projectable,
            single_input: None,
            identity,
            sequence: 0,
            running: false,
            failed,
        })
    }

    fn check_running(&self) -> Result<()> {
        anyhow::ensure!(self.running, "middleware transformer is not running");
        anyhow::ensure!(
            !self.failed.load(Ordering::Acquire),
            "middleware transformer requires reconstruction before processing"
        );
        Ok(())
    }
}

#[async_trait]
impl NativeComponent for Middleware {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    fn configuration(&self) -> Result<serde_json::Value> {
        Ok(serde_json::to_value(&self.configuration)?)
    }
    fn output_readers(&self) -> OutputReaders {
        let readers = OutputReaders::new();
        let out = PortId::try_new("out").expect("constant port");
        if self.projectable {
            readers.with_projection(
                out,
                PortId::try_new("in").expect("constant port"),
                self.mapping.clone(),
            )
        } else {
            readers
        }
    }
    fn input_bindings(&mut self) -> Option<&mut dyn NativeInputBindings> {
        Some(self)
    }
    async fn initialize(&mut self, _ctx: &InitializeContext) -> LifecycleResult {
        nothing_to_do()
    }
    async fn start(&mut self, _ctx: &mut StartContext) -> LifecycleResult {
        if self.failed.load(Ordering::Acquire) {
            return Err(anyhow::anyhow!(
                "middleware transformer must be reconstructed after failed or cancelled processing"
            )
            .into());
        }
        self.running = true;
        Ok(())
    }
    // Transformation is driven by the host; there are no independent workers.
    async fn pause(&mut self, _ctx: &PauseContext) -> LifecycleResult {
        nothing_to_do()
    }
    async fn resume(&mut self, _ctx: &ResumeContext) -> LifecycleResult {
        nothing_to_do()
    }
    async fn stop(&mut self, _ctx: &StopContext) -> LifecycleResult {
        self.running = false;
        Ok(())
    }
    async fn dispose(&mut self, _ctx: &DisposeContext) -> LifecycleResult {
        nothing_to_do()
    }
}

#[async_trait]
impl NativeInputBindings for Middleware {
    async fn bind_inputs(&mut self, bindings: &[InputBinding]) -> Result<()> {
        self.single_input = match bindings {
            [binding] if binding.input_port.as_str() == "in" => Some(binding.clone()),
            _ => None,
        };
        Ok(())
    }
}

#[async_trait]
impl NativeTransformer for Middleware {
    async fn delivery_completed(&mut self, _outputs: &[OutputEnvelope]) -> Result<()> {
        self.check_running()
    }

    async fn transform(&mut self, input: InputEnvelope) -> Result<Vec<OutputEnvelope>> {
        self.check_running()?;
        let mut guard = ProcessingGuard {
            failed: self.failed.clone(),
            complete: false,
        };
        anyhow::ensure!(input.port.as_str() == "in", "unknown middleware input port");
        let sequence = self
            .sequence
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("middleware output sequence exhausted"))?;
        let output = self.mapping.apply(&input.envelope).await?;
        let mut envelope = GraphChangeCodec::derive_changes(
            &input.envelope,
            &output,
            self.configuration.stream.clone(),
            sequence,
        )?;
        let upstream = if let (Some(owner), Some(binding)) =
            (self.mapping.cut_namespace(), &self.single_input)
        {
            GraphProducerProgress::from_envelope(&input.envelope)?
                .filter(|p| {
                    p.identity().component_id() == &binding.output.component
                        && p.identity().stream() == &binding.stream
                })
                .map(|p| {
                    OutputCursor::try_new(p.identity().clone(), p.sequence())?.projected_by(owner)
                })
                .transpose()?
        } else {
            None
        };
        let (producer, logical) = upstream.as_ref().map_or((&self.identity, sequence), |cut| {
            (cut.producer(), cut.sequence())
        });
        GraphProducerProgress::annotate(&mut envelope, producer, logical)?;
        self.sequence = sequence;
        guard.complete = true;
        Ok(vec![OutputEnvelope {
            execution_context: input.execution_context,
            port: PortId::try_new("out")?,
            envelope,
        }])
    }
}

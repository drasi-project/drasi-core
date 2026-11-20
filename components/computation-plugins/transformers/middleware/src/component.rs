// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use anyhow::{Context, Result};
use async_trait::async_trait;
use drasi_computation_contracts::{
    BootstrapCompletion, BootstrapStatus, ChangeEnvelope, ComponentDescriptor, GraphChangeCodec,
    GraphProducerIdentity, GraphProducerProgress, InputEnvelope, OutputEnvelope, OutputReaders,
    PortId, StreamId,
};
use drasi_computation_middleware_common::{Mode, Pipeline};
use drasi_computation_plugin_sdk::lifecycle::{
    nothing_to_do, DisposeContext, InitializeContext, LifecycleResult, PauseContext, ResumeContext,
    StartContext, StopContext,
};
use drasi_computation_plugin_sdk::{
    BootstrapInput, NativeBootstrapInput, NativeComponent, NativeTransformer,
};
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
struct Configuration {
    stream: StreamId,
    middleware: Vec<SourceMiddlewareConfig>,
    pipeline: Vec<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    bootstrap: bool,
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

pub(super) struct Middleware {
    descriptor: ComponentDescriptor,
    configuration: Configuration,
    pipeline: Pipeline,
    identity: GraphProducerIdentity,
    sequence: u64,
    running: bool,
    failed: Arc<AtomicBool>,
    output: Arc<super::output::Output>,
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
                .filter(|_| configuration.bootstrap)
                .map_or("volatile", |scope| scope.instance_id.as_str())
                .into(),
            scope
                .filter(|_| configuration.bootstrap)
                .map_or("volatile", |scope| scope.graph_id.as_str())
                .into(),
            descriptor.id().clone(),
            configuration.stream.clone(),
        )?;
        let failed = Arc::new(AtomicBool::new(false));
        let output = Arc::new(super::output::Output::new(
            configuration.bootstrap,
            failed.clone(),
            identity.clone(),
        )?);
        Ok(Self {
            descriptor,
            configuration,
            pipeline,
            identity,
            sequence: 0,
            running: false,
            failed,
            output,
        })
    }

    fn check_running(&self) -> Result<()> {
        anyhow::ensure!(self.running, "middleware transformer is not running");
        anyhow::ensure!(
            !self.failed.load(Ordering::Acquire),
            "middleware transformer requires reconstruction before processing"
        );
        anyhow::ensure!(
            self.output.ready()?,
            "middleware initialization is not complete"
        );
        Ok(())
    }

    async fn process(&self, input: &ChangeEnvelope) -> Result<Vec<SourceChange>> {
        let mut output = Vec::new();
        for change in GraphChangeCodec::decode_changes(input)? {
            output.extend(self.pipeline.apply(change).await.with_context(|| {
                format!("middleware pipeline failed in {}", self.descriptor.id())
            })?);
        }
        Ok(output)
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
        OutputReaders::new().with_snapshot(
            PortId::try_new("out").expect("constant port"),
            self.output.clone(),
        )
    }
    async fn initialize(&mut self, _ctx: &InitializeContext) -> LifecycleResult {
        nothing_to_do()
    }
    async fn start(&mut self, _ctx: &mut StartContext) -> LifecycleResult {
        if self.configuration.bootstrap {
            self.identity = GraphProducerIdentity::volatile(
                self.identity.construction_scope().to_owned(),
                self.identity.graph_id().to_owned(),
                self.descriptor.id().clone(),
                self.configuration.stream.clone(),
            )?;
            self.sequence = 0;
            self.failed.store(false, Ordering::Release);
            self.output.reset(self.identity.clone())?;
        }
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
        self.output.stop()?;
        Ok(())
    }
    async fn dispose(&mut self, _ctx: &DisposeContext) -> LifecycleResult {
        nothing_to_do()
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
        let output = self.process(&input.envelope).await?;
        let mut envelope = GraphChangeCodec::derive_changes(
            &input.envelope,
            &output,
            self.configuration.stream.clone(),
            sequence,
        )?;
        GraphProducerProgress::annotate(&mut envelope, &self.identity, sequence)?;
        self.output.apply(&output, sequence)?;
        self.sequence = sequence;
        guard.complete = true;
        Ok(vec![OutputEnvelope {
            execution_context: input.execution_context,
            port: PortId::try_new("out")?,
            envelope,
        }])
    }
    fn bootstrap_input(&mut self) -> Option<BootstrapInput<'_>> {
        Some(BootstrapInput::Quiescent(self))
    }
}

#[async_trait]
impl NativeBootstrapInput for Middleware {
    fn bootstrap_status(&self) -> Result<BootstrapStatus> {
        Ok(BootstrapStatus {
            configured: self.configuration.bootstrap,
            complete: !self.configuration.bootstrap || self.output.ready()?,
            reset_allowed: false,
        })
    }

    async fn bootstrap(&mut self, input: InputEnvelope) -> Result<()> {
        anyhow::ensure!(
            self.configuration.bootstrap
                && self.running
                && !self.output.ready()?
                && !self.failed.load(Ordering::Acquire),
            "middleware is not accepting initialization"
        );
        anyhow::ensure!(
            input.port.as_str() == "in" && input.envelope.system().control().is_none(),
            "middleware bootstrap requires data on its initialization input"
        );
        let mut guard = ProcessingGuard {
            failed: self.failed.clone(),
            complete: false,
        };
        self.output
            .apply(&self.process(&input.envelope).await?, 0)?;
        guard.complete = true;
        Ok(())
    }

    /// The middleware declares a quiescent bootstrap input: the graph refuses a
    /// durable provider when the edge binds, and the SDK delivers a quiescent
    /// completion without watermarks or state.
    async fn bootstrap_completed(&mut self, _completion: BootstrapCompletion) -> Result<()> {
        anyhow::ensure!(
            self.running && !self.failed.load(Ordering::Acquire),
            "middleware initialization completed while not running"
        );
        self.output.activate()
    }
}

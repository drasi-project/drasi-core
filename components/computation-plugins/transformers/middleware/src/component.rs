// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use anyhow::{Context, Result};
use async_trait::async_trait;
use drasi_computation_contracts::{
    BootstrapCompletion, BootstrapStatus, ChangeEnvelope, ComponentDescriptor, GraphChangeCodec,
    GraphProducerIdentity, GraphProducerProgress, InputEnvelope, OutputEnvelope, OutputReaders,
    PortId, StreamId,
};
use drasi_computation_plugin_sdk::{NativeComponent, NativeTransformer};
use drasi_core_types::models::{SourceChange, SourceMiddlewareConfig};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Configuration {
    stream: StreamId,
    middleware: Vec<SourceMiddlewareConfig>,
    pipeline: Vec<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    bootstrap: bool,
}

enum Processor {
    Map(drasi_middleware::map::Map),
    Relabel(drasi_middleware::relabel::RelabelMiddleware),
}

impl Processor {
    async fn apply(&self, change: SourceChange) -> Result<Vec<SourceChange>> {
        Ok(match self {
            Self::Map(value) => value.apply(change).await?,
            Self::Relabel(value) => value.apply(change).await?,
        })
    }
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
    pipeline: Vec<Arc<Processor>>,
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
        let mut processors = BTreeMap::new();
        for config in &configuration.middleware {
            drasi_computation_contracts::data::validate_identifier("middleware", &config.name)?;
            drasi_computation_contracts::data::validate_identifier(
                "middleware kind",
                &config.kind,
            )?;
            anyhow::ensure!(
                !processors.contains_key(config.name.as_ref()),
                "duplicate middleware name"
            );
            let processor = match config.kind.as_ref() {
                "map" => Processor::Map(drasi_middleware::map::MapFactory::new().build(config)?),
                "relabel" => Processor::Relabel(
                    drasi_middleware::relabel::RelabelMiddlewareFactory::new().build(config)?,
                ),
                kind => anyhow::bail!("middleware kind {kind} is not registered"),
            };
            processors.insert(config.name.to_string(), Arc::new(processor));
        }
        let pipeline = configuration
            .pipeline
            .iter()
            .map(|name| {
                drasi_computation_contracts::data::validate_identifier(
                    "middleware pipeline step",
                    name,
                )?;
                processors.get(name).cloned().ok_or_else(|| {
                    anyhow::anyhow!("pipeline references undeclared middleware {name}")
                })
            })
            .collect::<Result<Vec<_>>>()?;
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
            let mut changes = vec![change];
            for processor in &self.pipeline {
                let mut next = Vec::new();
                for change in changes {
                    next.extend(processor.apply(change).await.with_context(|| {
                        format!("middleware pipeline failed in {}", self.descriptor.id())
                    })?);
                }
                changes = next;
            }
            output.extend(changes);
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
    async fn start(&mut self) -> Result<()> {
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
        anyhow::ensure!(
            !self.failed.load(Ordering::Acquire),
            "middleware transformer must be reconstructed after failed or cancelled processing"
        );
        self.running = true;
        Ok(())
    }
    async fn stop(&mut self) -> Result<()> {
        self.running = false;
        self.output.stop()?;
        Ok(())
    }
}

#[async_trait]
impl NativeTransformer for Middleware {
    fn quiescent_bootstrap_input(&self) -> Option<PortId> {
        Some(PortId::try_new("in").expect("constant port"))
    }

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

    async fn bootstrap_completed(&mut self, completion: BootstrapCompletion) -> Result<()> {
        anyhow::ensure!(
            self.running
                && !self.failed.load(Ordering::Acquire)
                && completion.watermarks.is_empty()
                && completion.state.is_none(),
            "middleware quiescent initialization cannot accept durable completion"
        );
        self.output.activate()
    }

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
}

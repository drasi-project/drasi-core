// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use anyhow::Result;
use async_trait::async_trait;
use drasi_computation_contracts::{
    ComponentDescriptor, GraphChangeCodec, GraphProducerIdentity, GraphProducerProgress,
    InputEnvelope, OutputEnvelope, OutputReaders, PortId, StreamId,
};
use drasi_computation_plugin_sdk::{NativeComponent, NativeTransformer, Scope};
use drasi_core_types::models::SourceMiddlewareConfig;
use serde::{Deserialize, Serialize};

use crate::{
    mapping::{Mapper, Pipeline},
    output::Output,
    state::State,
};

#[cfg(test)]
mod tests;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Configuration {
    stream: StreamId,
    middleware: Vec<SourceMiddlewareConfig>,
    pipeline: Vec<String>,
}

pub(super) struct View {
    descriptor: ComponentDescriptor,
    configuration: Configuration,
    mapper: Box<dyn Mapper>,
    identity: GraphProducerIdentity,
    sequence: u64,
    running: bool,
    retired: bool,
    state: State,
    output: Output,
}

impl View {
    pub(super) fn new(
        descriptor: ComponentDescriptor,
        configuration: serde_json::Value,
        scope: Option<&Scope>,
    ) -> Result<Self> {
        let configuration: Configuration = serde_json::from_value(configuration)?;
        let mapper = Box::new(Pipeline::new(
            &configuration.middleware,
            &configuration.pipeline,
        )?);
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
        let output = Output::new(identity.clone())?;
        Ok(Self {
            descriptor,
            configuration,
            mapper,
            identity,
            sequence: 0,
            running: false,
            retired: false,
            state: State::default(),
            output,
        })
    }
}

#[async_trait]
impl NativeComponent for View {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }

    fn configuration(&self) -> Result<serde_json::Value> {
        Ok(serde_json::to_value(&self.configuration)?)
    }

    fn supports_delete_owned_resources(&self) -> bool {
        true
    }

    fn output_readers(&self) -> OutputReaders {
        OutputReaders::new().with_snapshot(
            PortId::try_new("out").expect("constant port"),
            self.output.reader(),
        )
    }

    async fn start(&mut self) -> Result<()> {
        anyhow::ensure!(!self.retired, "view state has been deprovisioned");
        self.output.set_running(true)?;
        self.running = true;
        Ok(())
    }

    async fn stop(&mut self) -> Result<()> {
        if !self.retired {
            self.output.set_running(false)?;
        }
        self.running = false;
        Ok(())
    }

    async fn delete_owned_resources(&mut self) -> Result<()> {
        anyhow::ensure!(!self.running, "stop the view before removing its state");
        self.output.retire()?;
        self.state = State::default();
        self.retired = true;
        Ok(())
    }
}

#[async_trait]
impl NativeTransformer for View {
    async fn transform(&mut self, input: InputEnvelope) -> Result<Vec<OutputEnvelope>> {
        anyhow::ensure!(
            self.running && !self.retired,
            "view transformer is not running"
        );
        anyhow::ensure!(input.port.as_str() == "in", "unknown view input port");
        anyhow::ensure!(
            input.envelope.system().control().is_none(),
            "view transforms accept graph data, not scheduling or recovery controls"
        );
        let sequence = self
            .sequence
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("view output sequence exhausted"))?;
        let (next, changes) = self
            .state
            .prepare(
                GraphChangeCodec::decode_changes(&input.envelope)?,
                self.mapper.as_ref(),
            )
            .await?;
        let mut envelope = GraphChangeCodec::derive_changes(
            &input.envelope,
            &changes,
            self.configuration.stream.clone(),
            sequence,
        )?;
        envelope.validate_lineage()?;
        GraphProducerProgress::annotate(&mut envelope, &self.identity, sequence)?;
        let port = PortId::try_new("out")?;
        self.output.publish(next.outputs.clone(), sequence)?;
        self.state = next;
        self.sequence = sequence;
        Ok(vec![OutputEnvelope { port, envelope }])
    }
}

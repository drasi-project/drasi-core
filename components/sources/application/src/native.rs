// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Graph-polled, in-process application input without a SourceBase or dispatcher.

use std::{collections::HashMap, num::NonZeroUsize, sync::Arc};

use anyhow::{Context, Result};
use async_trait::async_trait;
use drasi_lib::computation::v1::{
    ComponentDescriptor, ComponentId, ComponentRecovery, ComponentSemanticKind,
    ComputationComponent, Endpoint, EnvelopeSource, GraphChangeCodec, GraphProducerIdentity,
    GraphProducerProgress, OutputEnvelope, PipeRequirements, PortDescriptor, PortDirection, PortId,
    SourceAdmission, StreamId,
};
use tokio::sync::{mpsc, Mutex, RwLock};

use crate::{ApplicationSourceHandle, InternalEvent};

/// Application input driven directly by the graph.
///
/// Handles retain queued input across stop/start and apply backpressure while
/// stopped. This channel constructor does not claim WAL or process-restart
/// durability. Connect its output to graph pipes; it owns no subscription,
/// forwarding task, source dispatcher or private graph.
pub struct ApplicationSource {
    descriptor: ComponentDescriptor,
    identity: GraphProducerIdentity,
    input: Input,
    running: bool,
    capacity: NonZeroUsize,
    properties: HashMap<String, serde_json::Value>,
}

enum Input {
    Channel(Box<ChannelInput>),
    Admission(Arc<SourceAdmission>),
}

struct ChannelInput {
    receiver: mpsc::Receiver<InternalEvent>,
    handle: ApplicationSourceHandle,
    pending: Option<InternalEvent>,
    sequence: u64,
}

fn descriptor(id: ComponentId) -> Result<ComponentDescriptor> {
    Ok(ComponentDescriptor::try_new(
        id,
        vec![PortDescriptor::new(
            PortId::try_new("out")?,
            PortDirection::Output,
            GraphChangeCodec::schema().descriptor().clone(),
            PipeRequirements::default(),
        )],
    )?
    .with_semantic_kind(ComponentSemanticKind::Source))
}

impl ApplicationSource {
    pub fn channel(
        construction_scope: impl Into<String>,
        graph_id: impl Into<String>,
        id: impl Into<String>,
        stream: StreamId,
        capacity: NonZeroUsize,
    ) -> Result<(Self, ApplicationSourceHandle)> {
        anyhow::ensure!(
            capacity.get() <= tokio::sync::Semaphore::MAX_PERMITS,
            "application channel capacity exceeds the runtime maximum"
        );
        let id = ComponentId::try_new(id.into())?;
        let identity = GraphProducerIdentity::volatile(
            construction_scope.into(),
            graph_id.into(),
            id.clone(),
            stream.clone(),
        )?;
        let descriptor = descriptor(id.clone())?;
        let (sender, receiver) = mpsc::channel(capacity.get());
        let handle = ApplicationSourceHandle {
            tx: sender,
            source_id: id.as_str().to_owned(),
            requires_wal: false,
            wal: Arc::new(RwLock::new(None)),
            enqueue_order: Arc::new(Mutex::new(())),
        };
        Ok((
            Self {
                descriptor,
                identity,
                input: Input::Channel(Box::new(ChannelInput {
                    receiver,
                    handle: handle.clone(),
                    pending: None,
                    sequence: 0,
                })),
                running: false,
                capacity,
                properties: HashMap::new(),
            },
            handle,
        ))
    }

    /// Bind an existing native persistent admission service, not another WAL.
    ///
    /// Register producer sessions and submit envelopes through `admission`.
    /// The graph owns admission validation, journal commits, replay and receipts.
    /// Calls reject before activation and while stopped; unknown acceptance must
    /// be resolved using the same session and sequence before advancing.
    pub fn durable(id: impl Into<String>, admission: Arc<SourceAdmission>) -> Result<Self> {
        let id = ComponentId::try_new(id.into())?;
        anyhow::ensure!(
            admission.component_id() == &id && admission.port().as_str() == "out",
            "application admission must own this source's out port"
        );
        anyhow::ensure!(
            admission.identity().persistent(),
            "durable application admission requires a persistent producer identity"
        );
        Ok(Self {
            descriptor: descriptor(id)?,
            identity: admission.identity().clone(),
            capacity: admission.channel().definition().capacity,
            input: Input::Admission(admission),
            running: false,
            properties: HashMap::new(),
        })
    }

    pub fn with_properties(mut self, properties: HashMap<String, serde_json::Value>) -> Self {
        self.properties = properties;
        self
    }

    /// Volatile channel handle. Durable callers use their native admission service.
    pub fn get_handle(&self) -> Option<ApplicationSourceHandle> {
        match &self.input {
            Input::Channel(input) => Some(input.handle.clone()),
            Input::Admission(_) => None,
        }
    }

    pub fn output(&self) -> Endpoint {
        Endpoint::new(
            self.descriptor.id().clone(),
            self.descriptor.ports()[0].id().clone(),
        )
    }
}

#[async_trait]
impl ComputationComponent for ApplicationSource {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }

    fn recovery_contract(&self) -> ComponentRecovery {
        match &self.input {
            Input::Channel(_) => ComponentRecovery::default(),
            Input::Admission(admission) => {
                ComponentRecovery::admitted(admission.channel().durability())
                    .replay_to_durable_delivery()
            }
        }
    }

    fn configuration(&self) -> Result<serde_json::Value> {
        Ok(serde_json::json!({
            "kind": "application",
            "constructionScope": self.identity.construction_scope(),
            "graphId": self.identity.graph_id(),
            "stream": self.identity.stream(),
            "durable": matches!(self.input, Input::Admission(_)),
            "capacity": self.capacity.get(),
            "properties": self.properties,
        }))
    }

    async fn start(&mut self) -> Result<()> {
        anyhow::ensure!(
            !self.running,
            "native application source is already running"
        );
        self.running = true;
        Ok(())
    }

    async fn stop(&mut self) -> Result<()> {
        self.running = false;
        Ok(())
    }
}

#[async_trait]
impl EnvelopeSource for ApplicationSource {
    fn admission(&self) -> Option<Arc<SourceAdmission>> {
        match &self.input {
            Input::Channel(_) => None,
            Input::Admission(admission) => Some(admission.clone()),
        }
    }

    async fn next(&mut self) -> Result<Option<OutputEnvelope>> {
        anyhow::ensure!(self.running, "native application source is not running");
        let Input::Channel(input) = &mut self.input else {
            anyhow::bail!("the graph must drive native durable admission instead of next")
        };
        if input.pending.is_none() {
            input.pending = input.receiver.recv().await;
        }
        let Some(event) = &input.pending else {
            return Ok(None);
        };
        let sequence = input
            .sequence
            .checked_add(1)
            .context("application sequence exhausted")?;
        let timestamp = chrono::DateTime::from_timestamp_millis(i64::try_from(
            event.change.get_transaction_time(),
        )?)
        .context("application event timestamp cannot be represented")?;
        let mut envelope = GraphChangeCodec::encode_changes(
            std::slice::from_ref(&event.change),
            self.identity.stream().clone(),
            sequence,
            Some(timestamp),
        )?;
        GraphProducerProgress::annotate(&mut envelope, &self.identity, sequence)?;
        input.sequence = sequence;
        input.pending = None;
        Ok(Some(OutputEnvelope {
            port: self.descriptor.ports()[0].id().clone(),
            envelope,
        }))
    }
}

#[cfg(test)]
mod tests;

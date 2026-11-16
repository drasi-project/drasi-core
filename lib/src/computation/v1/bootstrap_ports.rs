// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Addressed endpoints of a component's bootstrap relationships.
//!
//! Bootstrap edges are ordinary graph edges with their own pipes, but the graph
//! does not fan them out or feed them through the data runner. Instead it binds
//! each component's bootstrap pipe ends here: a consumer reads the reply pipe of
//! one producer, and a producer sends a session reply only on the pipe to the
//! consumer that requested it.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

use super::{
    ChangeEnvelope, ComponentId, EnvelopeReceiver, EnvelopeSender, OutputEndpoint, PortId,
};

/// The producer side of one bootstrap edge, as seen by its consumer.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct BootstrapInputEdge {
    /// The producer's bootstrap output.
    pub producer: OutputEndpoint,
    /// This component's bootstrap input port.
    pub input_port: PortId,
    /// The producer is a dedicated bootstrap provider rather than one of this
    /// component's data inputs. Its snapshot is not attributed to an input.
    pub local: bool,
}

/// The consumer side of one bootstrap edge, as seen by its producer.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct BootstrapOutputEdge {
    /// This component's bootstrap output port.
    pub output_port: PortId,
    pub consumer: ComponentId,
    pub input_port: PortId,
}

type SharedReceiver = Arc<tokio::sync::Mutex<Box<dyn EnvelopeReceiver>>>;

#[derive(Default)]
struct Table {
    inputs: BTreeMap<BootstrapInputEdge, (usize, SharedReceiver)>,
    outputs: BTreeMap<BootstrapOutputEdge, (usize, Arc<dyn EnvelopeSender>)>,
}

/// Cloneable view of one component's currently bound bootstrap edges.
#[derive(Clone, Default)]
pub struct BootstrapPorts {
    table: Arc<Mutex<Table>>,
}

impl std::fmt::Debug for BootstrapPorts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (inputs, outputs) = (self.inputs(), self.outputs());
        f.debug_struct("BootstrapPorts")
            .field("inputs", &inputs)
            .field("outputs", &outputs)
            .finish()
    }
}

impl BootstrapPorts {
    fn table(&self) -> std::sync::MutexGuard<'_, Table> {
        self.table
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Bound bootstrap inputs, ordered by producer endpoint.
    pub fn inputs(&self) -> Vec<BootstrapInputEdge> {
        self.table().inputs.keys().cloned().collect()
    }

    /// Bound bootstrap outputs, ordered by output port then consumer.
    pub fn outputs(&self) -> Vec<BootstrapOutputEdge> {
        self.table().outputs.keys().cloned().collect()
    }

    /// The reply sender for exactly one consumer's bootstrap edge.
    pub fn sender(&self, edge: &BootstrapOutputEdge) -> Option<Arc<dyn EnvelopeSender>> {
        self.table()
            .outputs
            .get(edge)
            .map(|(_, sender)| sender.clone())
    }

    /// Receive the next reply envelope from one producer. `None` means the
    /// edge is closed and drained, or no longer bound.
    pub async fn receive(
        &self,
        edge: &BootstrapInputEdge,
    ) -> anyhow::Result<Option<ChangeEnvelope>> {
        let Some(receiver) = self
            .table()
            .inputs
            .get(edge)
            .map(|(_, receiver)| receiver.clone())
        else {
            return Ok(None);
        };
        let mut receiver = receiver.lock().await;
        let Some(delivery) = receiver.receive().await? else {
            return Ok(None);
        };
        let (envelope, acknowledgement) = delivery.into_parts();
        if let Some(acknowledgement) = acknowledgement {
            acknowledgement
                .complete(super::HandlingOutcome::Handled)
                .await
                .map_err(|error| anyhow::anyhow!("acknowledge bootstrap reply: {error}"))?;
        }
        Ok(Some(envelope))
    }

    pub(crate) fn install_input(
        &self,
        index: usize,
        edge: BootstrapInputEdge,
        receiver: Box<dyn EnvelopeReceiver>,
    ) {
        self.table()
            .inputs
            .insert(edge, (index, Arc::new(tokio::sync::Mutex::new(receiver))));
    }

    pub(crate) fn install_output(
        &self,
        index: usize,
        edge: BootstrapOutputEdge,
        sender: Arc<dyn EnvelopeSender>,
    ) {
        self.table().outputs.insert(edge, (index, sender));
    }

    /// Release both ends of one graph edge, if bound here.
    pub(crate) fn remove_edge(&self, index: usize) {
        let mut table = self.table();
        table.inputs.retain(|_, (edge, _)| *edge != index);
        table.outputs.retain(|_, (edge, _)| *edge != index);
    }

    pub(crate) fn clear(&self) {
        let mut table = self.table();
        table.inputs.clear();
        table.outputs.clear();
    }
}

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
    bootstrap_window::Window, ChangeEnvelope, ComponentId, EnvelopeReceiver, EnvelopeSender,
    OutputEndpoint, PortId,
};
use tokio::sync::watch;

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

#[derive(Clone, Copy)]
struct ReadProgress {
    session: super::BootstrapSessionId,
    committed_intent: u64,
}

pub(super) struct BindingReceiver {
    receiver: tokio::sync::Mutex<Box<dyn EnvelopeReceiver>>,
    current: watch::Sender<Option<ReadProgress>>,
}
pub(super) type SharedReceiver = Arc<BindingReceiver>;

impl BindingReceiver {
    pub(super) fn release(&self, session: super::BootstrapSessionId) {
        self.current
            .send_if_modified(|current| current.take_if(|read| read.session == session).is_some());
    }

    pub(super) fn confirm_intent(
        &self,
        session: super::BootstrapSessionId,
        sequence: u64,
    ) -> anyhow::Result<()> {
        let confirmed = self.current.send_if_modified(|current| {
            if let Some(read) = current
                .as_mut()
                .filter(|read| read.session == session && read.committed_intent < sequence)
            {
                read.committed_intent = sequence;
                true
            } else {
                false
            }
        });
        anyhow::ensure!(
            confirmed,
            "bootstrap intent is stale or already acknowledged"
        );
        Ok(())
    }
}

/// Kept by both ends of the addressed read. A slow read must notice its
/// owner disappearing even if it never yields another row or pipe permit.
#[derive(Clone)]
pub(super) struct ReadLifetime {
    session: super::BootstrapSessionId,
    current: watch::Receiver<Option<ReadProgress>>,
}

impl ReadLifetime {
    pub(super) async fn ended(mut self) {
        let _ = self
            .current
            .wait_for(|read| read.is_none_or(|read| read.session != self.session))
            .await;
    }

    pub(super) async fn intent_committed(mut self, sequence: u64) -> anyhow::Result<()> {
        let read = *self
            .current
            .wait_for(|read| {
                read.is_none_or(|read| {
                    read.session != self.session || read.committed_intent >= sequence
                })
            })
            .await?;
        anyhow::ensure!(
            read.is_some_and(
                |read| read.session == self.session && read.committed_intent == sequence
            ),
            "bootstrap reader retired before acknowledging provider intent"
        );
        Ok(())
    }
}

struct BoundInput {
    index: usize,
    binding: SharedReceiver,
}

impl Drop for BoundInput {
    fn drop(&mut self) {
        self.binding.current.send_replace(None);
    }
}

struct BoundOutput {
    index: usize,
    sender: Arc<dyn EnvelopeSender>,
    reader: Option<super::CurrentStateOptions>,
    current: watch::Receiver<Option<ReadProgress>>,
    served: Option<super::BootstrapSessionId>,
}

pub(super) enum StateDelivery {
    Duplicate,
    Open {
        sender: Arc<dyn EnvelopeSender>,
        options: super::CurrentStateOptions,
        lifetime: ReadLifetime,
    },
}

#[derive(Default)]
struct Table {
    inputs: BTreeMap<BootstrapInputEdge, BoundInput>,
    outputs: BTreeMap<BootstrapOutputEdge, BoundOutput>,
    windows: BTreeMap<usize, std::sync::Weak<Window>>,
}

/// Cloneable view of one component's currently bound bootstrap edges.
#[derive(Clone)]
pub struct BootstrapPorts {
    table: Arc<Mutex<Table>>,
    window_changes: watch::Sender<()>,
}

impl Default for BootstrapPorts {
    fn default() -> Self {
        Self {
            table: Default::default(),
            window_changes: watch::channel(()).0,
        }
    }
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
    pub(super) fn window_for(
        &self,
        index: usize,
        limits: super::BootstrapWindowLimits,
        control: Arc<dyn super::PipeControl>,
    ) -> Arc<Window> {
        let window = Window::new(index, limits, self.window_changes.clone(), control);
        self.table().windows.insert(index, Arc::downgrade(&window));
        window
    }

    pub(super) fn arm_windows(&self) {
        for window in self
            .table()
            .windows
            .values()
            .filter_map(std::sync::Weak::upgrade)
        {
            window.arm();
        }
    }

    pub(super) fn check_windows(&self) -> anyhow::Result<()> {
        for window in self
            .table()
            .windows
            .values()
            .filter_map(std::sync::Weak::upgrade)
        {
            window.check()?;
        }
        Ok(())
    }

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

    /// Dropping an output generation without a correction would strand these
    /// subscribers. A stopped subscriber can still own a durable baseline.
    pub fn check_output_reset(&self) -> Result<(), super::OutputBindingError> {
        if let Some(edge) = self.table().outputs.keys().next() {
            return Err(super::OutputBindingError::ResetRequiresDownstreamRecovery {
                output: edge.output_port.clone(),
                consumer: edge.consumer.clone(),
                input: edge.input_port.clone(),
            });
        }
        Ok(())
    }

    pub(super) fn reply_to(
        &self,
        edge: &BootstrapOutputEdge,
        session: super::BootstrapSessionId,
    ) -> Option<(Arc<dyn EnvelopeSender>, ReadLifetime)> {
        let table = self.table();
        let bound = table.outputs.get(edge)?;
        if bound
            .current
            .borrow()
            .is_none_or(|read| read.session != session)
        {
            return None;
        }
        Some((
            bound.sender.clone(),
            ReadLifetime {
                session,
                current: bound.current.clone(),
            },
        ))
    }

    pub(super) fn materialized_output(
        &self,
        edge: &BootstrapOutputEdge,
        session: super::BootstrapSessionId,
    ) -> Option<StateDelivery> {
        let mut table = self.table();
        let bound = table.outputs.get_mut(edge)?;
        let options = bound.reader?;
        if bound
            .current
            .borrow()
            .is_none_or(|read| read.session != session)
            || bound.served == Some(session)
        {
            return Some(StateDelivery::Duplicate);
        }
        bound.served = Some(session);
        Some(StateDelivery::Open {
            sender: bound.sender.clone(),
            options,
            lifetime: ReadLifetime {
                session,
                current: bound.current.clone(),
            },
        })
    }
    /// Receive the next reply envelope from one producer. `None` means the
    /// edge is closed and drained, or no longer bound.
    pub async fn receive(
        &self,
        edge: &BootstrapInputEdge,
    ) -> anyhow::Result<Option<ChangeEnvelope>> {
        let Some(receiver) = self.input_receiver(edge) else {
            return Ok(None);
        };
        self.receive_on(edge, &receiver, None).await
    }

    pub(super) fn input_receiver(&self, edge: &BootstrapInputEdge) -> Option<SharedReceiver> {
        self.table()
            .inputs
            .get(edge)
            .map(|bound| bound.binding.clone())
    }

    pub(super) fn begin_read(
        &self,
        edge: &BootstrapInputEdge,
        session: super::BootstrapSessionId,
    ) -> Option<SharedReceiver> {
        let table = self.table();
        let binding = &table.inputs.get(edge)?.binding;
        binding.current.send_replace(Some(ReadProgress {
            session,
            committed_intent: 0,
        }));
        Some(binding.clone())
    }

    pub(super) fn is_current(&self, edge: &BootstrapInputEdge, receiver: &SharedReceiver) -> bool {
        self.table()
            .inputs
            .get(edge)
            .is_some_and(|bound| Arc::ptr_eq(&bound.binding, receiver))
    }

    pub(super) async fn receive_on(
        &self,
        edge: &BootstrapInputEdge,
        binding: &SharedReceiver,
        session: Option<super::BootstrapSessionId>,
    ) -> anyhow::Result<Option<ChangeEnvelope>> {
        let mut receiver = binding.receiver.lock().await;
        anyhow::ensure!(
            self.is_current(edge, binding),
            "bootstrap binding was replaced"
        );
        let mut changes = self.window_changes.subscribe();
        let mut sessions = binding.current.subscribe();
        let receive = receiver.receive();
        tokio::pin!(receive);
        let delivery = loop {
            self.check_windows()?;
            anyhow::ensure!(
                session.is_none()
                    || sessions.borrow_and_update().map(|read| read.session) == session,
                "bootstrap session was replaced"
            );
            tokio::select! {
                biased;
                changed = changes.changed() => { changed?; }
                changed = sessions.changed(), if session.is_some() => { changed?; }
                result = &mut receive => break result?,
            }
        };
        self.check_windows()?;
        let Some(delivery) = delivery else {
            return Ok(None);
        };
        anyhow::ensure!(
            self.is_current(edge, binding),
            "bootstrap binding was replaced"
        );
        let (envelope, _, acknowledgement) = delivery.into_parts();
        if let Some(acknowledgement) = acknowledgement {
            acknowledgement
                .complete(super::HandlingOutcome::Handled)
                .await
                .map_err(|error| anyhow::anyhow!("acknowledge bootstrap reply: {error}"))?;
        }
        Ok(Some(envelope))
    }

    pub(super) fn bind_session_pipe(
        &self,
        consumer: &Self,
        index: usize,
        output: BootstrapOutputEdge,
        input: BootstrapInputEdge,
        ends: (Arc<dyn EnvelopeSender>, Box<dyn EnvelopeReceiver>),
        reader: Option<super::CurrentStateOptions>,
    ) {
        let (sender, receiver) = ends;
        let (tx, current) = watch::channel(None);
        let binding = Arc::new(BindingReceiver {
            receiver: tokio::sync::Mutex::new(receiver),
            current: tx,
        });
        consumer
            .table()
            .inputs
            .insert(input, BoundInput { index, binding });
        self.table().outputs.insert(
            output,
            BoundOutput {
                index,
                sender,
                reader,
                current,
                served: None,
            },
        );
    }

    /// Release both ends of one graph edge, if bound here.
    pub(crate) fn remove_edge(&self, index: usize) {
        let mut table = self.table();
        table.inputs.retain(|_, bound| bound.index != index);
        table.outputs.retain(|_, bound| bound.index != index);
        table.windows.remove(&index);
    }

    pub(crate) fn clear(&self) {
        let mut table = self.table();
        table.inputs.clear();
        table.outputs.clear();
        table.windows.clear();
    }
}

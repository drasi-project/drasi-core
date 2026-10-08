// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Native, in-process ComputationGraph application delivery.
//!
//! The graph polls this sink directly: there is no legacy Reaction adapter,
//! extra processing queue, subscription forwarder or callback task. Channel
//! delivery is acceptance-only; callbacks complete at their returned future.
//! Neither boundary establishes durable or exactly-once external effects.

use std::{any::Any, future::Future, num::NonZeroUsize, panic::AssertUnwindSafe};

use anyhow::Result;
use async_trait::async_trait;
use drasi_lib::computation::v1::{
    ComponentDescriptor, ComponentId, ComponentSemanticKind, ComputationComponent, Endpoint,
    EnvelopeSink, InputEnvelope, PipeRequirements, PortDescriptor, PortDirection, PortId,
    SchemaDescriptor, SinkCompletion,
};
use futures::{future::BoxFuture, FutureExt};
use tokio::sync::{mpsc, Mutex};

pub const INPUT_PORT: &str = "in";

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ApplicationReactionError {
    #[error("application channel capacity {capacity} exceeds the runtime maximum {maximum}")]
    Capacity { capacity: usize, maximum: usize },
    #[error("native application reaction is not running")]
    NotRunning,
    #[error("native application reaction must finish stop before starting or handling more input")]
    CleanupRequired,
    #[error("native application reaction received an undeclared input port: {0}")]
    InputPort(PortId),
    #[error("input schema differs from the native application reaction's full schema descriptor")]
    SchemaMismatch,
    #[error("native application receiver is closed")]
    ReceiverClosed,
    #[error("application callback panicked: {0}")]
    CallbackPanicked(String),
    #[error("application callback was poisoned by a panic; replace the component")]
    CallbackPoisoned,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Stopped,
    Running,
    CleanupRequired,
}

struct Callback {
    handler: Box<dyn FnMut(InputEnvelope) -> BoxFuture<'static, Result<()>> + Send>,
    pending: Option<BoxFuture<'static, Result<()>>>,
    poisoned: bool,
}

enum Delivery {
    Channel(mpsc::Sender<InputEnvelope>),
    // Native component calls are exclusively borrowed. get_mut avoids a lock
    // on the data path while allowing Send-only futures in a Sync component.
    Callback(Mutex<Callback>),
}

/// An application-owned destination polled by ComputationGraph itself.
///
/// Connect a producer's declared output to [`Self::input`]. When building a
/// complete batch, use `ComponentBatch::builder().sink(Box::new(reaction))`;
/// when connecting to an existing node, use the node-first API below.
/// The schema is explicit; use `QueryChangeCodec::schema().descriptor().clone()`
/// for query results. Ordinary query wrappers have no data ports: build a
/// native query with `DrasiLib::computation_pipeline` or a native query factory.
/// The external receiver/callback binding is deliberately not reconstructible
/// from a configuration snapshot and never crosses a plugin ABI.
///
/// ```no_run
/// use std::num::NonZeroUsize;
/// use drasi_lib::{DrasiLib, computation::v1::*};
/// use drasi_reaction_application::NativeApplicationReaction;
///
/// async fn connect(
///     drasi: &DrasiLib,
///     query_output: Endpoint,
/// ) -> anyhow::Result<(ComponentHandle, tokio::sync::mpsc::Receiver<InputEnvelope>)> {
///     let (reaction, receiver) = NativeApplicationReaction::channel(
///         "application", QueryChangeCodec::schema().descriptor().clone(),
///         NonZeroUsize::new(64).unwrap(),
///     )?;
///     let input = reaction.input();
///     let component = drasi.add_computation_component(ComponentAddition::new(
///         ConstructedComponent::sink(Box::new(reaction)),
///     )).await?;
///     let report = drasi.computation_control()?.connect(
///         EdgeDefinition::new(query_output, input),
///         Box::new(BoundedPipeConfig { capacity: 64 }),
///         RelationshipPolicy::default(),
///     ).await?;
///     anyhow::ensure!(report.summary == OperationSummary::Completed, "{report:?}");
///     component.wait_created().await?;
///     // Start a stopped instance, or await this handle on a running instance,
///     // before publishing input. Apply a caller-chosen readiness deadline.
///     Ok((component, receiver))
/// }
/// ```
pub struct ApplicationReaction {
    descriptor: ComponentDescriptor,
    delivery: Delivery,
    state: State,
}

impl ApplicationReaction {
    /// Bounded, volatile handoff, with no worker or completion receipt.
    ///
    /// The receiver is single-consumer and remains usable across soft restarts.
    /// Dropping it closes admission. Stop does not drain this application-owned
    /// queue; close/drain it explicitly when the application no longer needs it.
    /// Cancelling a send before capacity is available publishes nothing.
    pub fn channel(
        id: impl Into<String>,
        schema: SchemaDescriptor,
        capacity: NonZeroUsize,
    ) -> Result<(Self, mpsc::Receiver<InputEnvelope>)> {
        let descriptor = Self::descriptor_for(id, schema)?;
        if capacity.get() > tokio::sync::Semaphore::MAX_PERMITS {
            return Err(ApplicationReactionError::Capacity {
                capacity: capacity.get(),
                maximum: tokio::sync::Semaphore::MAX_PERMITS,
            }
            .into());
        }
        let (sender, receiver) = mpsc::channel(capacity.get());
        Ok((
            Self {
                descriptor,
                delivery: Delivery::Channel(sender),
                state: State::Stopped,
            },
            receiver,
        ))
    }

    /// Invoke one asynchronous callback at a time, directly from the graph.
    ///
    /// Success means the callback future completed, not that arbitrary remote
    /// effects became durable. Errors retain their original cause. Cancellation
    /// retains the future for stop to finish; a cancelled/timed-out stop must be
    /// retried before restart. A panic permanently poisons this callback binding.
    ///
    /// Callbacks must cooperate with the async runtime and must not spawn
    /// unowned work. Returning after queuing work does not confirm that work.
    pub fn callback<F, Fut>(
        id: impl Into<String>,
        schema: SchemaDescriptor,
        mut handler: F,
    ) -> Result<Self>
    where
        F: FnMut(InputEnvelope) -> Fut + Send + 'static,
        Fut: Future<Output = Result<()>> + Send + 'static,
    {
        Ok(Self {
            descriptor: Self::descriptor_for(id, schema)?,
            delivery: Delivery::Callback(Mutex::new(Callback {
                handler: Box::new(move |input| Box::pin(handler(input))),
                pending: None,
                poisoned: false,
            })),
            state: State::Stopped,
        })
    }

    pub fn input(&self) -> Endpoint {
        Endpoint::new(
            self.descriptor.id().clone(),
            self.descriptor.ports()[0].id().clone(),
        )
    }

    fn descriptor_for(
        id: impl Into<String>,
        schema: SchemaDescriptor,
    ) -> Result<ComponentDescriptor> {
        Ok(ComponentDescriptor::try_new(
            ComponentId::try_new(id.into())?,
            vec![PortDescriptor::new(
                PortId::try_new(INPUT_PORT)?,
                PortDirection::Input,
                schema,
                PipeRequirements::default(),
            )],
        )?
        .with_semantic_kind(ComponentSemanticKind::Reaction))
    }
}

fn callback_panic(payload: Box<dyn Any + Send>) -> anyhow::Error {
    let message = payload
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| {
            payload
                .downcast_ref::<&str>()
                .map(|message| message.to_string())
        })
        .unwrap_or_else(|| "non-string panic payload".into());
    ApplicationReactionError::CallbackPanicked(message).into()
}

async fn finish_callback(callback: &mut Callback) -> Result<()> {
    let Some(pending) = callback.pending.as_mut() else {
        return Ok(());
    };
    let result = pending.await;
    callback.pending = None;
    if matches!(
        result.as_ref().err().and_then(|error| error.downcast_ref()),
        Some(ApplicationReactionError::CallbackPanicked(_))
    ) {
        callback.poisoned = true;
    }
    result
}

#[async_trait]
impl ComputationComponent for ApplicationReaction {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }

    async fn start(&mut self) -> Result<()> {
        if self.state != State::Stopped {
            return Err(ApplicationReactionError::CleanupRequired.into());
        }
        match &mut self.delivery {
            Delivery::Callback(callback) => {
                anyhow::ensure!(
                    !callback.get_mut().poisoned,
                    ApplicationReactionError::CallbackPoisoned
                );
            }
            Delivery::Channel(sender) => anyhow::ensure!(
                !sender.is_closed(),
                ApplicationReactionError::ReceiverClosed
            ),
        }
        self.state = State::Running;
        Ok(())
    }

    async fn stop(&mut self) -> Result<()> {
        self.state = State::CleanupRequired;
        if let Delivery::Callback(callback) = &mut self.delivery {
            finish_callback(callback.get_mut()).await?;
        }
        self.state = State::Stopped;
        Ok(())
    }
}

#[async_trait]
impl EnvelopeSink for ApplicationReaction {
    fn completion(&self) -> SinkCompletion {
        match &self.delivery {
            Delivery::Channel(_) => SinkCompletion::Accepted,
            Delivery::Callback(_) => SinkCompletion::Handled,
        }
    }

    async fn handle(&mut self, input: InputEnvelope) -> Result<()> {
        match self.state {
            State::Stopped => return Err(ApplicationReactionError::NotRunning.into()),
            State::CleanupRequired => {
                return Err(ApplicationReactionError::CleanupRequired.into());
            }
            State::Running => {}
        }
        let port = &self.descriptor.ports()[0];
        if &input.port != port.id() {
            return Err(ApplicationReactionError::InputPort(input.port).into());
        }
        if input.envelope.changes().schema() != port.schema() {
            return Err(ApplicationReactionError::SchemaMismatch.into());
        }
        match &mut self.delivery {
            Delivery::Channel(sender) => sender
                .send(input)
                .await
                .map_err(|_| ApplicationReactionError::ReceiverClosed.into()),
            Delivery::Callback(callback) => {
                self.state = State::CleanupRequired;
                let callback = callback.get_mut();
                let future = match std::panic::catch_unwind(AssertUnwindSafe(|| {
                    (callback.handler)(input)
                })) {
                    Ok(future) => future,
                    Err(payload) => {
                        callback.poisoned = true;
                        return Err(callback_panic(payload));
                    }
                };
                callback.pending = Some(Box::pin(async move {
                    AssertUnwindSafe(future)
                        .catch_unwind()
                        .await
                        .map_err(callback_panic)?
                }));
                finish_callback(callback).await?;
                self.state = State::Running;
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests;

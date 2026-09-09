// Copyright 2026 The Drasi Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::{mpsc, watch};

use super::{
    Delivery, EnqueueReceipt, Envelope, EnvelopeReceiver, EnvelopeSender, Pipe, PipeCapabilities,
    PipeError, SendFailure,
};

/// Out-of-band pipe lifecycle. Implementations must wake pending operations.
/// Close rejects new sends and drains accepted events; cancel may discard them.
/// This handle must not retain a sender or keep a drained receiver alive.
pub trait PipeControl: Send + Sync {
    fn close(&self);
    fn cancel(&self);
}

/// A fresh pipe and its sender-independent lifecycle handle.
pub struct ProvidedPipe {
    pub pipe: Box<dyn Pipe>,
    pub control: Arc<dyn PipeControl>,
}

/// In-process provider instance, separate from descriptive topology.
///
/// `capabilities` must be side-effect-free. Each `create` must return fresh,
/// exclusively graph-owned endpoints honoring that declaration. External sender
/// clones are not supported by the graph (they can prevent finite completion).
/// The graph requires cancellation-safe receive and synchronous control closure.
pub trait PipeProvider: Send + Sync {
    fn capabilities(&self) -> std::result::Result<PipeCapabilities, PipeError>;
    fn create(&self) -> std::result::Result<ProvidedPipe, PipeError>;
}

/// Configuration/provider for a volatile FIFO queue. Zero is rejected, not repaired.
#[derive(Debug, Clone, Copy)]
pub struct BoundedPipeConfig {
    pub capacity: usize,
}

impl PipeProvider for BoundedPipeConfig {
    fn capabilities(&self) -> std::result::Result<PipeCapabilities, PipeError> {
        if self.capacity > tokio::sync::Semaphore::MAX_PERMITS {
            return Err(PipeError::InvalidCapacity);
        }
        let capacity =
            std::num::NonZeroUsize::new(self.capacity).ok_or(PipeError::InvalidCapacity)?;
        Ok(PipeCapabilities::volatile_bounded(capacity))
    }

    fn create(&self) -> std::result::Result<ProvidedPipe, PipeError> {
        let pipe = BoundedPipe::new(self.capacity)?;
        let control = pipe.control();
        Ok(ProvidedPipe {
            pipe: Box::new(pipe),
            control,
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Closure {
    Open,
    Draining,
    Cancelled,
}

struct Control(watch::Sender<Closure>);

impl PipeControl for Control {
    fn close(&self) {
        self.0.send_if_modified(|state| {
            if *state == Closure::Open {
                *state = Closure::Draining;
                true
            } else {
                false
            }
        });
    }

    fn cancel(&self) {
        self.0.send_replace(Closure::Cancelled);
    }
}

struct Sender {
    sender: mpsc::Sender<Envelope>,
    control: Arc<Control>,
}

#[async_trait]
impl EnvelopeSender for Sender {
    async fn send(&self, envelope: Envelope) -> std::result::Result<EnqueueReceipt, SendFailure> {
        let mut closure = self.control.0.subscribe();
        let permit = tokio::select! {
            biased;
            _ = closure.wait_for(|state| *state != Closure::Open) => None,
            permit = self.sender.reserve() => permit.ok(),
        };
        // Holding the watch read guard makes enqueue and explicit close linearizable.
        // No await occurs after removing capacity or before returning acceptance.
        let state = self.control.0.borrow();
        match permit {
            Some(permit) if *state == Closure::Open => {
                let receipt = EnqueueReceipt::new(envelope.id().clone());
                permit.send(envelope);
                Ok(receipt)
            }
            _ => Err(SendFailure {
                envelope,
                error: PipeError::Closed,
            }),
        }
    }
}

struct Receiver {
    receiver: mpsc::Receiver<Envelope>,
    control: Arc<Control>,
}

#[async_trait]
impl EnvelopeReceiver for Receiver {
    async fn receive(&mut self) -> std::result::Result<Option<Delivery>, PipeError> {
        let mut closure = self.control.0.subscribe();
        loop {
            let state = *closure.borrow_and_update();
            match state {
                Closure::Open => {}
                Closure::Draining => self.receiver.close(),
                Closure::Cancelled => {
                    self.receiver.close();
                    // Cancellation is the documented immediate-loss boundary.
                    while self.receiver.try_recv().is_ok() {}
                    return Ok(None);
                }
            }
            tokio::select! {
                biased;
                _ = closure.changed() => continue,
                envelope = self.receiver.recv() => {
                    return Ok(envelope.map(|envelope| Delivery::new(envelope, None)));
                }
            }
        }
    }
}

impl Drop for Receiver {
    fn drop(&mut self) {
        self.control.cancel();
    }
}

/// Bounded in-process pipe, advertising only FIFO-per-stream and backpressure.
///
/// A producer must serialize sends in sequence order; timestamps are ignored.
/// Waiting for capacity does not reserve an event or enqueue a hidden copy.
/// Dropping a pending send before acceptance leaves the queue unchanged (the
/// dropped future owns its argument). Explicit close/receiver drop wakes pending
/// sends with `Closed` and the original unaccepted envelope. Receiving is
/// cancellation-safe: removal and returning a delivery occur in the same poll.
///
/// `control().close()` rejects new sends and drains buffered events; `cancel()`
/// discards buffered events when the receiver is next polled/dropped. Neither
/// reverses already delivered processing or external effects. The pipe itself
/// owns a sender: drop it after transferring endpoints for natural end-of-stream.
pub struct BoundedPipe {
    capabilities: PipeCapabilities,
    sender: Arc<Sender>,
    receiver: Option<Receiver>,
}

impl BoundedPipe {
    pub fn new(capacity: usize) -> std::result::Result<Self, PipeError> {
        let capabilities = BoundedPipeConfig { capacity }.capabilities()?;
        let (sender, receiver) = mpsc::channel(capacity);
        let (closure, _) = watch::channel(Closure::Open);
        let control = Arc::new(Control(closure));
        Ok(Self {
            capabilities,
            sender: Arc::new(Sender {
                sender,
                control: control.clone(),
            }),
            receiver: Some(Receiver { receiver, control }),
        })
    }

    pub fn control(&self) -> Arc<dyn PipeControl> {
        self.sender.control.clone()
    }
}

impl Pipe for BoundedPipe {
    fn capabilities(&self) -> &PipeCapabilities {
        &self.capabilities
    }

    fn sender(&self) -> Arc<dyn EnvelopeSender> {
        self.sender.clone()
    }

    fn take_receiver(&mut self) -> std::result::Result<Box<dyn EnvelopeReceiver>, PipeError> {
        self.receiver
            .take()
            .map(|receiver| Box::new(receiver) as Box<dyn EnvelopeReceiver>)
            .ok_or(PipeError::ReceiverTaken)
    }
}

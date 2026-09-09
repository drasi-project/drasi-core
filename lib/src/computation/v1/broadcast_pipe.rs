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

use std::{
    collections::VecDeque,
    num::NonZeroUsize,
    sync::{Arc, Mutex, MutexGuard},
};

use async_trait::async_trait;
use tokio::sync::Notify;

use super::{
    pipe_metrics::PipeMetrics, ChangeEnvelope, Delivery, EnqueueReceipt, EnvelopeReceiver,
    EnvelopeSender, Pipe, PipeCapabilities, PipeCapability, PipeControl, PipeError,
    PipeMetricsSnapshot, PipeProvider, ProvidedPipe, SendFailure,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BroadcastLagPolicy {
    Report,
    /// Explicitly lossy behavior: log/count skipped events and keep receiving.
    SkipWithNotification,
}

#[derive(Debug, Clone, Copy)]
pub struct BroadcastPipeConfig {
    pub capacity: usize,
    pub lag_policy: BroadcastLagPolicy,
}

impl PipeProvider for BroadcastPipeConfig {
    fn capabilities(&self) -> Result<PipeCapabilities, PipeError> {
        let capacity = NonZeroUsize::new(self.capacity).ok_or(PipeError::InvalidCapacity)?;
        Ok(PipeCapabilities::try_new(
            [PipeCapability::FifoPerStream],
            Some(capacity),
        )?)
    }
    fn create(&self) -> Result<ProvidedPipe, PipeError> {
        let pipe = BroadcastPipe::new(*self)?;
        let control = pipe.control();
        Ok(ProvidedPipe {
            pipe: Box::new(pipe),
            control,
        })
    }
}

struct Queue {
    values: VecDeque<ChangeEnvelope>,
    lagged: u64,
    closed: bool,
    cancelled: bool,
}

struct Shared {
    queue: Mutex<Queue>,
    notify: Notify,
    capacity: usize,
    metrics: PipeMetrics,
}

impl Shared {
    fn lock(&self) -> Result<MutexGuard<'_, Queue>, PipeError> {
        self.queue.lock().map_err(|error| {
            PipeError::Backend(anyhow::anyhow!("broadcast queue poisoned: {error}"))
        })
    }
    fn close(&self, cancel: bool) {
        let mut queue = match self.queue.lock() {
            Ok(queue) => queue,
            Err(error) => {
                log::error!("Closing a poisoned broadcast queue: {error}");
                error.into_inner()
            }
        };
        queue.closed = true;
        queue.cancelled |= cancel;
        if queue.cancelled {
            self.metrics.discarded(queue.values.len());
            queue.values.clear();
            queue.lagged = 0;
        }
        drop(queue);
        self.notify.notify_waiters();
    }
}

impl PipeControl for Shared {
    fn close(&self) {
        self.close(false);
    }
    fn cancel(&self) {
        self.close(true);
    }
    fn metrics(&self) -> Option<PipeMetricsSnapshot> {
        Some(self.metrics.snapshot())
    }
}

struct Sender(Arc<Shared>);

#[async_trait]
impl EnvelopeSender for Sender {
    async fn send(&self, envelope: ChangeEnvelope) -> Result<EnqueueReceipt, SendFailure> {
        let mut queue = match self.0.lock() {
            Ok(queue) => queue,
            Err(error) => return Err(SendFailure { envelope, error }),
        };
        if queue.closed {
            return Err(SendFailure {
                envelope,
                error: PipeError::Closed,
            });
        }
        if queue.values.len() == self.0.capacity {
            queue.values.pop_front();
            queue.lagged = queue.lagged.saturating_add(1);
            self.0.metrics.discarded(1);
        }
        let receipt = EnqueueReceipt::new(envelope.id().clone());
        queue.values.push_back(envelope);
        self.0.metrics.accepted();
        drop(queue);
        self.0.notify.notify_one();
        Ok(receipt)
    }
}

impl Drop for Sender {
    fn drop(&mut self) {
        self.0.close(false);
    }
}

struct Receiver {
    shared: Arc<Shared>,
    policy: BroadcastLagPolicy,
}

#[async_trait]
impl EnvelopeReceiver for Receiver {
    async fn receive(&mut self) -> Result<Option<Delivery>, PipeError> {
        loop {
            let notified = self.shared.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let mut queue = self.shared.lock()?;
                if queue.lagged > 0 {
                    let skipped = std::mem::take(&mut queue.lagged);
                    if self.policy == BroadcastLagPolicy::Report {
                        return Err(PipeError::Lagged { skipped });
                    }
                    log::warn!("Broadcast computation pipe skipped {skipped} envelopes under explicit lossy policy");
                }
                if let Some(envelope) = queue.values.pop_front() {
                    self.shared.metrics.delivered();
                    return Ok(Some(Delivery::new(envelope, None)));
                }
                if queue.closed {
                    return Ok(None);
                }
            }
            notified.await;
        }
    }
}

impl Drop for Receiver {
    fn drop(&mut self) {
        self.shared.close(true);
    }
}

/// Bounded, non-blocking delivery with explicit lag handling. Each graph edge has
/// one receiver; graph fanout clones branch lists while sharing immutable events.
/// No backpressure, durability, acknowledgement, or lossless claim is advertised.
pub struct BroadcastPipe {
    capabilities: PipeCapabilities,
    sender: Arc<Sender>,
    receiver: Option<Receiver>,
}

impl BroadcastPipe {
    pub fn new(config: BroadcastPipeConfig) -> Result<Self, PipeError> {
        let capabilities = config.capabilities()?;
        let shared = Arc::new(Shared {
            queue: Mutex::new(Queue {
                values: VecDeque::new(),
                lagged: 0,
                closed: false,
                cancelled: false,
            }),
            notify: Notify::new(),
            capacity: config.capacity,
            metrics: PipeMetrics::default(),
        });
        Ok(Self {
            capabilities,
            sender: Arc::new(Sender(shared.clone())),
            receiver: Some(Receiver {
                shared,
                policy: config.lag_policy,
            }),
        })
    }
    pub fn control(&self) -> Arc<dyn PipeControl> {
        self.sender.0.clone()
    }
}

impl Pipe for BroadcastPipe {
    fn metrics(&self) -> Option<PipeMetricsSnapshot> {
        self.control().metrics()
    }
    fn capabilities(&self) -> &PipeCapabilities {
        &self.capabilities
    }
    fn sender(&self) -> Arc<dyn EnvelopeSender> {
        self.sender.clone()
    }
    fn take_receiver(&mut self) -> Result<Box<dyn EnvelopeReceiver>, PipeError> {
        self.receiver
            .take()
            .map(|receiver| Box::new(receiver) as Box<dyn EnvelopeReceiver>)
            .ok_or(PipeError::ReceiverTaken)
    }
}

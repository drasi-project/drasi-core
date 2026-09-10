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
    collections::BTreeMap,
    num::NonZeroUsize,
    sync::{Arc, Mutex, MutexGuard},
};

use async_trait::async_trait;

use super::{
    Acknowledgement, ChangeEnvelope, Delivery, EnqueueReceipt, EnvelopeReceiver, EnvelopeSender,
    HandlingOutcome, Pipe, PipeCapabilities, PipeCapability, PipeControl, PipeError, PipeProvider,
    ProvidedPipe, ResourceCleanup, ResourceHandle, ResourceId, ResourceRole, RetainedEnvelopeStore,
    RetentionPolicy, SendFailure,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ReplayGapPolicy {
    Strict,
    /// Explicitly accept loss and advance to the oldest available entry.
    SkipWithNotification,
}

pub struct RetainedStoreResource(pub Arc<dyn RetainedEnvelopeStore>);

#[async_trait]
impl ResourceCleanup for RetainedStoreResource {
    async fn shutdown(&self) -> anyhow::Result<()> {
        self.0.shutdown().await
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RetainedPipeConfig {
    pub resource: ResourceId,
    pub capacity: NonZeroUsize,
    pub durable: bool,
    pub retention: RetentionPolicy,
    pub gap_policy: ReplayGapPolicy,
}

fn capabilities(
    capacity: NonZeroUsize,
    durable: bool,
    retention: RetentionPolicy,
) -> Result<PipeCapabilities, PipeError> {
    let mut capabilities = vec![
        PipeCapability::FifoPerStream,
        PipeCapability::RetainedHistory,
        PipeCapability::ExplicitAcknowledgement,
    ];
    if durable {
        capabilities.extend([PipeCapability::DurableAcceptance, PipeCapability::Replay]);
    }
    if retention == RetentionPolicy::Backpressure {
        capabilities.push(PipeCapability::Backpressure);
    }
    Ok(PipeCapabilities::try_new(capabilities, Some(capacity))?)
}

impl PipeProvider for RetainedPipeConfig {
    fn specification(&self) -> Option<super::DesiredPipe> {
        Some(super::DesiredPipe::Retained(self.clone()))
    }
    fn capabilities(&self) -> Result<PipeCapabilities, PipeError> {
        capabilities(self.capacity, self.durable, self.retention)
    }
    fn resource_dependencies(&self) -> BTreeMap<ResourceId, ResourceRole> {
        BTreeMap::from([(self.resource.clone(), ResourceRole::StateStore)])
    }
    fn exclusive_resources(&self) -> Vec<ResourceId> {
        vec![self.resource.clone()]
    }
    fn validate_resources(
        &self,
        resources: &BTreeMap<ResourceId, ResourceHandle>,
    ) -> Result<(), PipeError> {
        if let Some(resource) = resources.get(&self.resource) {
            let resource = resource
                .get::<RetainedStoreResource>()
                .map_err(|error| PipeError::Backend(error.into()))?;
            if resource.0.capacity() != self.capacity
                || resource.0.durable() != self.durable
                || resource.0.retention_policy() != self.retention
            {
                return Err(PipeError::Backend(anyhow::anyhow!(
                    "retained store capabilities differ from desired pipe"
                )));
            }
        }
        Ok(())
    }
    fn create(&self) -> Result<ProvidedPipe, PipeError> {
        Err(PipeError::Backend(anyhow::anyhow!(
            "retained pipe requires its declared store resource"
        )))
    }
    fn create_with_resources(
        &self,
        resources: &BTreeMap<ResourceId, ResourceHandle>,
    ) -> Result<ProvidedPipe, PipeError> {
        let resource = resources
            .get(&self.resource)
            .ok_or_else(|| {
                PipeError::Backend(anyhow::anyhow!(
                    "retained store resource {} is not realized",
                    self.resource
                ))
            })?
            .get::<RetainedStoreResource>()
            .map_err(|error| PipeError::Backend(error.into()))?;
        if resource.0.capacity() != self.capacity
            || resource.0.durable() != self.durable
            || resource.0.retention_policy() != self.retention
        {
            return Err(PipeError::Backend(anyhow::anyhow!(
                "retained store capabilities differ from desired pipe"
            )));
        }
        let pipe = RetainedPipe::new(resource.0.clone(), self.gap_policy)?;
        let control = pipe.control();
        Ok(ProvidedPipe {
            pipe: Box::new(pipe),
            control,
        })
    }
}

#[derive(Default)]
struct State {
    closed: bool,
    cancelled: bool,
    sends: usize,
    delivery_pending: bool,
}

struct Shared {
    store: Arc<dyn RetainedEnvelopeStore>,
    generation: u64,
    state: Mutex<State>,
}

impl Shared {
    fn lock(&self) -> Result<MutexGuard<'_, State>, PipeError> {
        self.state.lock().map_err(|error| {
            PipeError::Backend(anyhow::anyhow!("retained pipe state poisoned: {error}"))
        })
    }
    fn current(&self) -> bool {
        self.store.generation() == self.generation
    }
    fn close(&self, cancel: bool) {
        let mut state = match self.state.lock() {
            Ok(state) => state,
            Err(error) => {
                log::error!("Closing a poisoned retained pipe: {error}");
                error.into_inner()
            }
        };
        state.closed = true;
        state.cancelled |= cancel;
        drop(state);
        if cancel {
            self.store.revoke_generation(self.generation);
        }
        self.store.notify().notify_waiters();
    }
}

#[async_trait]
impl PipeControl for Shared {
    async fn is_idle(&self) -> Result<bool, PipeError> {
        {
            let state = self.lock()?;
            if state.cancelled || !self.current() {
                return Err(PipeError::Closed);
            }
            if state.sends > 0 || state.delivery_pending {
                return Ok(false);
            }
        }
        let progress = self.store.progress(self.generation).await?;
        Ok(self.store.next(self.generation, progress).await?.is_none())
    }
    fn close(&self) {
        self.close(false);
    }
    fn cancel(&self) {
        self.close(true);
    }
}

struct SendAdmission(Arc<Shared>);

impl Drop for SendAdmission {
    fn drop(&mut self) {
        let mut state = match self.0.state.lock() {
            Ok(state) => state,
            Err(error) => {
                log::error!("Retaining interrupted send state: {error}");
                error.into_inner()
            }
        };
        state.sends -= 1;
        drop(state);
        self.0.store.notify().notify_waiters();
    }
}

struct Sender(Arc<Shared>);

#[async_trait]
impl EnvelopeSender for Sender {
    async fn send(&self, envelope: ChangeEnvelope) -> Result<EnqueueReceipt, SendFailure> {
        {
            let mut state = match self.0.lock() {
                Ok(state) => state,
                Err(error) => return Err(SendFailure { envelope, error }),
            };
            if state.closed || !self.0.current() {
                return Err(SendFailure {
                    envelope,
                    error: PipeError::Closed,
                });
            }
            state.sends += 1;
        }
        let _admission = SendAdmission(self.0.clone());
        loop {
            let notified = self.0.store.notify().notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let state = match self.0.lock() {
                    Ok(state) => state,
                    Err(error) => return Err(SendFailure { envelope, error }),
                };
                if state.closed || !self.0.current() {
                    return Err(SendFailure {
                        envelope,
                        error: PipeError::Closed,
                    });
                }
            }
            match self.0.store.append(self.0.generation, &envelope).await {
                Ok(_) => {
                    if !self.0.current() {
                        return Err(SendFailure {
                            envelope,
                            error: PipeError::AcceptanceUnknown {
                                source: anyhow::anyhow!(
                                    "send completed across a revoked pipe generation"
                                ),
                            },
                        });
                    }
                    return Ok(EnqueueReceipt::new(envelope.id().clone()));
                }
                Err(PipeError::CapacityExhausted) => notified.await,
                Err(error) => return Err(SendFailure { envelope, error }),
            }
        }
    }
}

impl Drop for Sender {
    fn drop(&mut self) {
        self.0.close(false);
    }
}

struct Ack {
    shared: Arc<Shared>,
    position: u64,
}

#[async_trait]
impl Acknowledgement for Ack {
    async fn complete(self: Box<Self>, outcome: HandlingOutcome) -> Result<(), PipeError> {
        if self.shared.lock()?.cancelled || !self.shared.current() {
            return Err(PipeError::Closed);
        }
        match outcome {
            HandlingOutcome::Handled => {
                self.shared
                    .store
                    .acknowledge(self.shared.generation, self.position)
                    .await
            }
            HandlingOutcome::Failed { reason } => {
                log::warn!(
                    "Retained delivery {} remains unacknowledged: {reason}",
                    self.position
                );
                Ok(())
            }
        }
    }
}

impl Drop for Ack {
    fn drop(&mut self) {
        let mut state = match self.shared.state.lock() {
            Ok(state) => state,
            Err(error) => {
                log::error!("Releasing retained delivery in poisoned state: {error}");
                error.into_inner()
            }
        };
        state.delivery_pending = false;
        drop(state);
        self.shared.store.notify().notify_waiters();
    }
}

struct Receiver {
    shared: Arc<Shared>,
    gap_policy: ReplayGapPolicy,
}

#[async_trait]
impl EnvelopeReceiver for Receiver {
    async fn receive(&mut self) -> Result<Option<Delivery>, PipeError> {
        loop {
            let notified = self.shared.store.notify().notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let pending = {
                let state = self.shared.lock()?;
                if state.cancelled || !self.shared.current() {
                    return Ok(None);
                }
                state.delivery_pending
            };
            if pending {
                notified.await;
                continue;
            }
            let progress = self.shared.store.progress(self.shared.generation).await?;
            let next = self
                .shared
                .store
                .next(self.shared.generation, progress)
                .await;
            match next {
                Ok(Some(stored)) => {
                    let mut state = self.shared.lock()?;
                    if state.cancelled || !self.shared.current() {
                        return Ok(None);
                    }
                    state.delivery_pending = true;
                    return Ok(Some(Delivery::new(
                        stored.envelope,
                        Some(Box::new(Ack {
                            shared: self.shared.clone(),
                            position: stored.position,
                        })),
                    )));
                }
                Err(PipeError::PositionUnavailable { requested, oldest })
                    if self.gap_policy == ReplayGapPolicy::SkipWithNotification =>
                {
                    log::warn!("Retained pipe explicitly skips gap after {requested} to {oldest}");
                    self.shared
                        .store
                        .acknowledge(self.shared.generation, oldest.saturating_sub(1))
                        .await?;
                    continue;
                }
                Err(error) => return Err(error),
                Ok(None) => {
                    let state = self.shared.lock()?;
                    if state.closed && state.sends == 0 {
                        return Ok(None);
                    }
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

/// Retained, one-consumer delivery. Dropped/failed acknowledgements remain
/// replayable. Close drains through the declared local handling boundary;
/// cancellation retains stored work and revokes this generation's handles.
/// Accepted durable sends survive the configured storage's documented boundary.
pub struct RetainedPipe {
    capabilities: PipeCapabilities,
    sender: Arc<Sender>,
    receiver: Option<Receiver>,
}

impl RetainedPipe {
    pub fn new(
        store: Arc<dyn RetainedEnvelopeStore>,
        gap_policy: ReplayGapPolicy,
    ) -> Result<Self, PipeError> {
        let capabilities =
            capabilities(store.capacity(), store.durable(), store.retention_policy())?;
        let generation = store.acquire_generation()?;
        let shared = Arc::new(Shared {
            store,
            generation,
            state: Mutex::new(State::default()),
        });
        Ok(Self {
            capabilities,
            sender: Arc::new(Sender(shared.clone())),
            receiver: Some(Receiver { shared, gap_policy }),
        })
    }
    pub fn control(&self) -> Arc<dyn PipeControl> {
        self.sender.0.clone()
    }
}

impl Pipe for RetainedPipe {
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

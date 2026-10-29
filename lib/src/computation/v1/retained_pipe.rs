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
    batch: Option<BatchCollection>,
}

struct BatchCollection {
    buffer: super::pipe::batching::BatchBuffer,
    lease: Arc<BatchLease>,
}

struct BatchLease {
    shared: Arc<Shared>,
    cursor: tokio::sync::Mutex<BatchCursor>,
}
struct BatchCursor {
    next: Option<u64>,
    interrupted: bool,
}
impl Drop for BatchLease {
    fn drop(&mut self) {
        let mut state = self.shared.state.lock().unwrap_or_else(|error| {
            log::error!("Releasing retained batch in poisoned state: {error}");
            error.into_inner()
        });
        state.delivery_pending = false;
        drop(state);
        self.shared.store.notify().notify_waiters();
    }
}
struct BatchAck {
    lease: Arc<BatchLease>,
    position: u64,
}
#[async_trait]
impl Acknowledgement for BatchAck {
    async fn complete(self: Box<Self>, outcome: HandlingOutcome) -> Result<(), PipeError> {
        if self.lease.shared.lock()?.cancelled || !self.lease.shared.current() {
            return Err(PipeError::Closed);
        }
        let mut cursor = self.lease.cursor.lock().await;
        if cursor.interrupted || Some(self.position) != cursor.next {
            return Err(PipeError::Backend(anyhow::anyhow!(
                "retained batch acknowledgement would skip uncompleted work"
            )));
        }
        match outcome {
            HandlingOutcome::Handled => {
                // Cancellation during persistence must fence the remaining lease.
                cursor.interrupted = true;
                self.lease
                    .shared
                    .store
                    .acknowledge(self.lease.shared.generation, self.position)
                    .await?;
                cursor.next = self.position.checked_add(1);
                cursor.interrupted = false;
                Ok(())
            }
            HandlingOutcome::Failed { reason } => {
                log::warn!(
                    "Retained batch delivery {} remains unacknowledged: {reason}",
                    self.position
                );
                Ok(())
            }
        }
    }
}

#[async_trait]
impl EnvelopeReceiver for Receiver {
    fn supports_batch_receive(&self) -> bool {
        true
    }
    fn take_pending_batch(&mut self) -> Result<Vec<Delivery>, PipeError> {
        if self.shared.lock()?.cancelled || !self.shared.current() {
            return Err(PipeError::Closed);
        }
        Ok(self
            .batch
            .take()
            .map(|batch| batch.buffer.deliveries.into_iter().collect())
            .unwrap_or_default())
    }

    async fn receive_batch(
        &mut self,
        limits: super::SinkBatching,
        port: &super::PortId,
    ) -> Result<Vec<Delivery>, PipeError> {
        limits.validate().map_err(PipeError::Backend)?;
        let shared = self.shared.clone();
        loop {
            if self.shared.lock()?.cancelled || !self.shared.current() {
                return Err(PipeError::Closed);
            }
            if self.batch.as_ref().is_some_and(|batch| batch.buffer.full()) {
                return self.take_pending_batch();
            }
            let notified = shared.store.notify().notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let pending = {
                let state = self.shared.lock()?;
                if state.cancelled || !self.shared.current() {
                    return Err(PipeError::Closed);
                }
                state.delivery_pending && self.batch.is_none()
            };
            if pending {
                notified.await;
                continue;
            }
            let after = match &self.batch {
                Some(batch) => batch.buffer.after,
                None => self.shared.store.progress(self.shared.generation).await?,
            };
            match self.shared.store.next(self.shared.generation, after).await {
                Ok(Some(stored)) => {
                    let bytes = match &self.batch {
                        Some(batch) => batch.buffer.size(&stored.envelope)?,
                        None => super::SinkBatching::input_size(&super::InputEnvelope {
                            port: port.clone(),
                            envelope: stored.envelope.clone(),
                        })
                        .map_err(PipeError::Backend)?,
                    };
                    if self.batch.is_none() {
                        let buffer = super::pipe::batching::BatchBuffer::new(limits, port.clone())?;
                        let mut state = self.shared.lock()?;
                        if state.cancelled || !self.shared.current() {
                            return Err(PipeError::Closed);
                        }
                        state.delivery_pending = true;
                        self.batch = Some(BatchCollection {
                            buffer,
                            lease: Arc::new(BatchLease {
                                shared: self.shared.clone(),
                                cursor: tokio::sync::Mutex::new(BatchCursor {
                                    next: Some(stored.position),
                                    interrupted: false,
                                }),
                            }),
                        });
                    }
                    let batch = self.batch.as_mut().expect("batch lease");
                    if !batch.buffer.can_push(&stored.envelope, bytes) {
                        return self.take_pending_batch();
                    }
                    let delivery = Delivery::new(
                        stored.envelope,
                        Some(Box::new(BatchAck {
                            lease: batch.lease.clone(),
                            position: stored.position,
                        })),
                    );
                    batch.buffer.push(stored.position, delivery, bytes);
                }
                Err(PipeError::PositionUnavailable { .. }) if self.batch.is_some() => {
                    return self.take_pending_batch()
                }
                Err(PipeError::PositionUnavailable { requested, oldest })
                    if self.gap_policy == ReplayGapPolicy::SkipWithNotification =>
                {
                    log::warn!("Retained pipe explicitly skips gap after {requested} to {oldest}");
                    self.shared
                        .store
                        .acknowledge(self.shared.generation, oldest.saturating_sub(1))
                        .await?;
                }
                Err(error) => return Err(error),
                Ok(None) => {
                    let closed = {
                        let state = self.shared.lock()?;
                        state.closed && state.sends == 0
                    };
                    if closed {
                        return self.take_pending_batch();
                    }
                    if let Some(batch) = &self.batch {
                        tokio::select! {
                            _ = notified => {},
                            _ = tokio::time::sleep_until(batch.buffer.deadline) => return self.take_pending_batch(),
                        }
                    } else {
                        notified.await;
                    }
                }
            }
        }
    }

    async fn receive(&mut self) -> Result<Option<Delivery>, PipeError> {
        if let Some(batch) = &mut self.batch {
            if self.shared.lock()?.cancelled || !self.shared.current() {
                return Ok(None);
            }
            batch.buffer.finished = true;
            let delivery = batch.buffer.deliveries.pop_front();
            if batch.buffer.deliveries.is_empty() {
                self.batch = None;
            }
            if delivery.is_some() {
                return Ok(delivery);
            }
        }
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
            receiver: Some(Receiver {
                shared,
                gap_policy,
                batch: None,
            }),
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

#[cfg(test)]
mod foundation_tests {
    use super::*;

    #[tokio::test]
    async fn poisoned_binding_preserves_errors_and_releases_inflight_bookkeeping() {
        let store = Arc::new(super::super::MemoryEnvelopeStore::new(
            NonZeroUsize::new(1).expect("capacity"),
            RetentionPolicy::Backpressure,
        ));
        let mut pipe = RetainedPipe::new(store, ReplayGapPolicy::Strict).expect("pipe");
        let mut receiver = pipe.take_receiver().expect("receiver");
        let shared = pipe.sender.0.clone();
        shared.lock().expect("lock").sends = 1;
        let admission = SendAdmission(shared.clone());
        let ack = Box::new(Ack {
            shared: shared.clone(),
            position: 1,
        });
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = shared.state.lock().expect("lock");
            panic!("injected owner failure");
        }))
        .is_err());
        let failure = pipe
            .sender()
            .send(super::super::pipe::test_envelope(1))
            .await
            .expect_err("poisoned send");
        assert!(matches!(failure.error, PipeError::Backend(_)));
        assert!(matches!(
            receiver.receive().await,
            Err(PipeError::Backend(_))
        ));
        assert!(matches!(shared.is_idle().await, Err(PipeError::Backend(_))));
        assert!(matches!(
            ack.complete(HandlingOutcome::Handled).await,
            Err(PipeError::Backend(_))
        ));
        drop(admission);
        shared.close(false);
        shared.close(true);
        let state = shared
            .state
            .lock()
            .err()
            .expect("poison retained")
            .into_inner();
        assert_eq!(state.sends, 0);
        assert!(!state.delivery_pending);
        assert!(state.cancelled);
    }
}

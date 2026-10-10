// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::sync::Weak;

use drasi_core::{
    computation::{TransactionGroupContext, TransactionGroupMutation},
    interface::{IndexError, SourceCheckpoint},
};
use tokio::sync::OwnedMutexGuard;

use super::*;

const METADATA_KEY: &str = "metadata";
const HEADER: &[u8; 4] = b"DQG1";

pub(super) struct SharedChannel {
    owner: Weak<QosChannel>,
    publisher: Arc<Mutex<()>>,
    subscriber_writes: StdMutex<std::collections::BTreeSet<String>>,
}

impl SharedChannel {
    pub(super) fn new(owner: Weak<QosChannel>) -> Self {
        Self {
            owner,
            publisher: Arc::new(Mutex::new(())),
            subscriber_writes: StdMutex::new(std::collections::BTreeSet::new()),
        }
    }

    pub(super) fn subscriber_write_pending(&self, subscriber: &str) -> Result<bool, PipeError> {
        Ok(self
            .subscriber_writes
            .lock()
            .map_err(|_| backend("shared subscriber ownership poisoned"))?
            .contains(subscriber))
    }
}

impl Persistent {
    pub(super) async fn read_metadata(&self) -> Result<Option<SourceCheckpoint>, IndexError> {
        if !self.shared {
            return self
                .transaction
                .resources()
                .checkpoint_store()
                .expect("validated transaction")
                .read_checkpoint(&self.key)
                .await;
        }
        let rows = self
            .transaction
            .resources()
            .outbox_writer()
            .expect("validated transaction")
            .read_from(METADATA_KEY, 0)
            .await?;
        let [(1, bytes)] = rows.as_slice() else {
            return if rows.is_empty() {
                Ok(None)
            } else {
                Err(IndexError::CorruptedData)
            };
        };
        if bytes.len() < 12
            || bytes.len() > admission::MAX_METADATA_BYTES + 12
            || bytes.get(..4) != Some(HEADER.as_slice())
        {
            return Err(IndexError::CorruptedData);
        }
        let sequence = u64::from_be_bytes(
            bytes[4..12]
                .try_into()
                .map_err(|_| IndexError::CorruptedData)?,
        );
        Ok(Some(SourceCheckpoint::new(
            sequence,
            Some(Bytes::copy_from_slice(&bytes[12..])),
        )))
    }

    pub(super) async fn stage_metadata(&self, head: u64, bytes: &Bytes) -> Result<(), IndexError> {
        if !self.shared {
            return self
                .transaction
                .resources()
                .checkpoint_store()
                .expect("validated transaction")
                .stage_checkpoint(&self.key, head, Some(bytes))
                .await;
        }
        if bytes.len() > admission::MAX_METADATA_BYTES {
            return Err(IndexError::other(backend(
                "receipt metadata exceeds its 4 MiB bound",
            )));
        }
        let mut record = Vec::with_capacity(bytes.len() + 12);
        record.extend_from_slice(HEADER);
        record.extend_from_slice(&head.to_be_bytes());
        record.extend_from_slice(bytes);
        self.transaction
            .resources()
            .outbox_writer()
            .expect("validated transaction")
            .append(METADATA_KEY, 1, &record)
            .await
    }
}

/// Reserves one future append without holding the storage transaction.
/// Acknowledgements can continue while the producer waits for capacity.
pub(crate) struct QosReservation {
    channel: Weak<QosChannel>,
    _publisher: OwnedMutexGuard<()>,
}

impl QosReservation {
    pub(crate) fn mutation(
        self,
        envelope: ChangeEnvelope,
    ) -> Result<Box<dyn TransactionGroupMutation>, PipeError> {
        let channel = self.channel.upgrade().ok_or(PipeError::Closed)?;
        let store = channel.storage()?.ok_or(PipeError::Closed)?;
        let candidate = replay::Candidate::new(&envelope, &store.codec)?;
        Ok(Box::new(SharedMutation {
            channel: self.channel.clone(),
            action: Action::Append {
                envelope,
                candidate,
                endpoint: None,
                _reservation: self,
            },
            update: None,
            receipt: None,
            reply: None,
        }))
    }
}

impl Drop for QosReservation {
    fn drop(&mut self) {
        if let Some(channel) = self.channel.upgrade() {
            channel.changed.notify_waiters();
        }
    }
}

enum Action {
    Append {
        envelope: ChangeEnvelope,
        candidate: replay::Candidate,
        endpoint: Option<(String, Arc<Binding>)>,
        _reservation: QosReservation,
    },
    Complete {
        subscriber: String,
        binding: Arc<Binding>,
        position: u64,
        skip: bool,
        _ownership: SubscriberWrite,
    },
    Retire {
        subscriber: String,
        _ownership: SubscriberWrite,
    },
}

struct SubscriberWrite {
    channel: Weak<QosChannel>,
    subscriber: String,
}

impl Drop for SubscriberWrite {
    fn drop(&mut self) {
        if let Some(channel) = self.channel.upgrade() {
            if let Some(shared) = &channel.shared {
                match shared.subscriber_writes.lock() {
                    Ok(mut pending) => {
                        pending.remove(&self.subscriber);
                    }
                    Err(error) => {
                        channel.closed.store(true, Ordering::Release);
                        log::error!("Shared subscriber ownership cleanup failed: {error}");
                    }
                }
                channel.changed.notify_waiters();
            }
        }
    }
}

struct Update {
    metadata: Metadata,
    append: Option<(u64, ChangeEnvelope, u64)>,
}

type Reply = Arc<StdMutex<Option<EnqueueReceipt>>>;

struct SharedMutation {
    channel: Weak<QosChannel>,
    action: Action,
    update: Option<Update>,
    receipt: Option<EnqueueReceipt>,
    reply: Option<Reply>,
}

#[async_trait]
impl TransactionGroupMutation for SharedMutation {
    async fn stage(&mut self, context: &TransactionGroupContext<'_>) -> Result<(), IndexError> {
        self.prepare(context).await.map_err(IndexError::other)
    }

    async fn committed(&mut self) -> Result<(), IndexError> {
        let channel = self
            .channel
            .upgrade()
            .ok_or_else(|| IndexError::other(PipeError::Closed))?;
        let mut state = channel.state.lock().await;
        channel.check().map_err(IndexError::other)?;
        if let Some(update) = self.update.take() {
            if let Some((position, envelope, floor)) = update.append {
                state.append_committed(position, envelope, floor, None);
            }
            state.metadata = update.metadata;
        }
        if let Some(reply) = &self.reply {
            *reply
                .lock()
                .map_err(|_| IndexError::other(backend("shared receipt publication poisoned")))? =
                self.receipt.take();
        }
        channel.changed.notify_waiters();
        Ok(())
    }
}

impl SharedMutation {
    async fn prepare(&mut self, context: &TransactionGroupContext<'_>) -> Result<(), PipeError> {
        let channel = self.channel.upgrade().ok_or(PipeError::Closed)?;
        let store = channel.storage()?.ok_or(PipeError::Closed)?;
        context
            .require_member(&store.transaction)
            .map_err(|error| PipeError::Backend(error.into()))?;
        let state = channel.state.lock().await;
        channel.check()?;
        let mut metadata = state.metadata.clone();
        let append = match &self.action {
            Action::Append {
                envelope,
                candidate,
                endpoint,
                ..
            } => {
                if let Some((subscriber, binding)) = endpoint {
                    Endpoint {
                        channel: channel.clone(),
                        subscriber: subscriber.clone(),
                        binding: binding.clone(),
                    }
                    .check_writable()?;
                }
                if envelope.system().stream() != &channel.definition.stream {
                    return Err(backend("shared QoS received another producer stream"));
                }
                let replay = metadata
                    .replay
                    .as_mut()
                    .ok_or_else(|| backend("shared replay state is missing"))?;
                if let Some(position) = replay.check(candidate)? {
                    self.receipt =
                        Some(EnqueueReceipt::new(envelope.id().clone()).with_position(position));
                    return Ok(());
                }
                if let Some(previous) = metadata.producer_sequence {
                    if envelope.system().sequence() <= previous {
                        return Err(ReplayRejection::TransportSequence {
                            previous,
                            received: envelope.system().sequence(),
                        }
                        .into());
                    }
                }
                if !has_capacity(&state, channel.definition.capacity.get()) {
                    return Err(backend("reserved shared QoS capacity is unavailable"));
                }
                let position = metadata
                    .head
                    .checked_add(1)
                    .ok_or_else(|| backend("QoS journal sequence exhausted"))?;
                let floor = state.entries.keys().next().copied().unwrap_or(1);
                let retain_from = if state.entries.len() == channel.definition.capacity.get() {
                    floor + 1
                } else {
                    floor
                };
                replay.record(candidate, position);
                metadata.head = position;
                metadata.producer_sequence = Some(envelope.system().sequence());
                self.receipt =
                    Some(EnqueueReceipt::new(envelope.id().clone()).with_position(position));
                Some((position, envelope.clone(), retain_from))
            }
            Action::Complete {
                subscriber,
                binding,
                position,
                skip,
                ..
            } => {
                Endpoint {
                    channel: channel.clone(),
                    subscriber: subscriber.clone(),
                    binding: binding.clone(),
                }
                .check()?;
                let cursor = metadata
                    .cursors
                    .get_mut(subscriber)
                    .ok_or_else(|| backend("unknown QoS subscriber"))?;
                if cursor.retired
                    || *position > metadata.head
                    || *position < cursor.position
                    || !skip && cursor.position.checked_add(1) != Some(*position)
                {
                    return Err(backend("QoS acknowledgement would skip unprocessed work"));
                }
                cursor.position = *position;
                None
            }
            Action::Retire { subscriber, .. } => {
                let bindings = channel
                    .bindings
                    .lock()
                    .map_err(|_| backend("QoS bindings poisoned"))?;
                if bindings
                    .consumers
                    .get(subscriber)
                    .is_some_and(|binding| !binding.cancelled.load(Ordering::Acquire))
                {
                    return Err(backend("disconnect a QoS subscriber before retiring it"));
                }
                metadata
                    .cursors
                    .get_mut(subscriber)
                    .ok_or_else(|| backend("unknown QoS subscriber"))?
                    .retired = true;
                None
            }
        };
        let bytes = Bytes::from(
            serde_json::to_vec(&metadata).map_err(|error| PipeError::Backend(error.into()))?,
        );
        if bytes.len() > admission::MAX_METADATA_BYTES {
            return Err(backend("receipt metadata exceeds its 4 MiB bound"));
        }
        if let Some((position, envelope, floor)) = &append {
            let record = store
                .codec
                .encode(envelope)
                .map_err(|error| PipeError::Backend(error.into()))?;
            store
                .transaction
                .resources()
                .outbox_writer()
                .expect("validated transaction")
                .append_and_trim(&store.key, *position, &record, *floor)
                .await
                .map_err(|error| PipeError::Backend(error.into()))?;
        }
        store
            .stage_metadata(metadata.head, &bytes)
            .await
            .map_err(|error| PipeError::Backend(error.into()))?;
        self.update = Some(Update { metadata, append });
        Ok(())
    }
}

fn has_capacity(state: &State, capacity: usize) -> bool {
    let completed = state
        .metadata
        .cursors
        .values()
        .filter(|cursor| !cursor.retired)
        .map(|cursor| cursor.position)
        .min()
        .unwrap_or(state.metadata.head);
    state.metadata.head - completed < capacity as u64
}

impl QosChannel {
    fn reserve_subscriber_write(
        &self,
        subscriber: &str,
        owner: Option<&Binding>,
    ) -> Result<SubscriberWrite, PipeError> {
        self.check()?;
        if !self.definition.subscribers.contains_key(subscriber) {
            return Err(backend("unknown QoS subscriber"));
        }
        let shared = self.shared.as_ref().ok_or(PipeError::Closed)?;
        let bindings = self
            .bindings
            .lock()
            .map_err(|_| backend("QoS bindings poisoned"))?;
        let current = bindings.consumers.get(subscriber);
        match owner {
            Some(owner)
                if owner.cancelled.load(Ordering::Acquire)
                    || !current.is_some_and(|current| current.generation == owner.generation) =>
            {
                return Err(PipeError::Closed);
            }
            None if current.is_some_and(|current| !current.cancelled.load(Ordering::Acquire)) => {
                return Err(backend("disconnect a QoS subscriber before retiring it"));
            }
            _ => {}
        }
        if !shared
            .subscriber_writes
            .lock()
            .map_err(|_| backend("shared subscriber ownership poisoned"))?
            .insert(subscriber.into())
        {
            return Err(backend(
                "QoS subscriber has an unfinished progress operation",
            ));
        }
        Ok(SubscriberWrite {
            channel: shared.owner.clone(),
            subscriber: subscriber.into(),
        })
    }

    pub(crate) fn is_shared(&self) -> bool {
        self.shared.is_some()
    }

    pub(crate) async fn validate_retirement(
        &self,
        groups: &[drasi_core::computation::TransactionGroupRetirement],
        allow_pending: bool,
    ) -> Result<(), PipeError> {
        self.check()?;
        if !self.is_shared()
            || !self.storage()?.is_some_and(|storage| {
                groups
                    .iter()
                    .any(|group| group.contains(&storage.transaction))
            })
        {
            return Err(backend(
                "retirement requires the journal's actual frozen storage group",
            ));
        }
        let progress = self.progress().await?;
        if !allow_pending
            && progress.processed.iter().any(|(id, position)| {
                *position != progress.accepted && !progress.retired.contains(id)
            })
        {
            return Err(backend(
                "journal still has unhandled subscriber obligations",
            ));
        }
        Ok(())
    }

    pub(crate) fn shares_transaction(
        &self,
        transaction: &ComputationTransaction,
    ) -> Result<bool, PipeError> {
        self.check()?;
        Ok(self.shared.is_some()
            && self
                .storage()?
                .is_some_and(|store| store.transaction.shares_transaction_group(transaction)))
    }

    pub(crate) fn shares_query(
        &self,
        query: &drasi_core::computation::ComputationQuery,
    ) -> Result<bool, PipeError> {
        self.check()?;
        Ok(self.shared.is_some()
            && self
                .storage()?
                .is_some_and(|store| query.shares_transaction_group(&store.transaction)))
    }

    pub(crate) async fn reserve(&self) -> Result<QosReservation, PipeError> {
        self.reserve_for(None).await
    }

    async fn reserve_for(&self, endpoint: Option<&Endpoint>) -> Result<QosReservation, PipeError> {
        let shared = self
            .shared
            .as_ref()
            .ok_or_else(|| backend("QoS channel is not shared"))?;
        let publisher = shared.publisher.clone().lock_owned();
        tokio::pin!(publisher);
        let publisher = loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            self.check()?;
            if let Some(endpoint) = endpoint {
                endpoint.check_writable()?;
            }
            tokio::select! {
                publisher = &mut publisher => break publisher,
                changed = self.wait_changed(notified) => changed?,
            }
        };
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let state = self.state.lock().await;
            self.check()?;
            if let Some(endpoint) = endpoint {
                endpoint.check_writable()?;
            }
            if has_capacity(&state, self.definition.capacity.get()) {
                return Ok(QosReservation {
                    channel: shared.owner.clone(),
                    _publisher: publisher,
                });
            }
            drop(state);
            self.wait_changed(notified).await?;
        }
    }

    pub(super) async fn wait_changed(
        &self,
        changed: impl std::future::Future<Output = ()>,
    ) -> Result<(), PipeError> {
        if self.shared.is_none() {
            changed.await;
            return Ok(());
        }
        let store = self.storage()?.ok_or(PipeError::Closed)?;
        tokio::select! {
            _ = changed => Ok(()),
            result = store.transaction.wait_for_transaction_group_failure() => {
                result.map_err(|error| PipeError::Backend(error.into()))?;
                self.check()
            }
        }
    }

    async fn run_shared(&self, mutation: SharedMutation, append: bool) -> Result<(), PipeError> {
        let store = self.storage()?.ok_or(PipeError::Closed)?;
        let result = store
            .transaction
            .run_with_mutations(async {
                Ok((
                    (),
                    vec![Box::new(mutation) as Box<dyn TransactionGroupMutation>],
                ))
            })
            .await;
        self.changed.notify_waiters();
        result.map_err(|error| {
            if !store.transaction.recovery_required() {
                PipeError::Backend(error.into())
            } else if append {
                PipeError::AcceptanceUnknown {
                    source: error.into(),
                }
            } else {
                PipeError::AcknowledgementUnknown {
                    source: error.into(),
                }
            }
        })
    }

    pub(super) async fn publish_shared(
        &self,
        envelope: &ChangeEnvelope,
        endpoint: Option<&Endpoint>,
    ) -> Result<EnqueueReceipt, PipeError> {
        let store = self.storage()?.ok_or(PipeError::Closed)?;
        let candidate = replay::Candidate::new(envelope, &store.codec)?;
        {
            let state = self.state.lock().await;
            self.check()?;
            if let Some(endpoint) = endpoint {
                endpoint.check_writable()?;
            }
            if let Some(position) = state
                .metadata
                .replay
                .as_ref()
                .ok_or_else(|| backend("shared replay state is missing"))?
                .check(&candidate)?
            {
                return Ok(EnqueueReceipt::new(envelope.id().clone()).with_position(position));
            }
        }
        let reservation = self.reserve_for(endpoint).await?;
        if let Some(endpoint) = endpoint {
            endpoint.check_writable()?;
        }
        let reply = Arc::new(StdMutex::new(None));
        self.run_shared(
            SharedMutation {
                channel: reservation.channel.clone(),
                action: Action::Append {
                    envelope: envelope.clone(),
                    candidate,
                    endpoint: endpoint
                        .map(|endpoint| (endpoint.subscriber.clone(), endpoint.binding.clone())),
                    _reservation: reservation,
                },
                update: None,
                receipt: None,
                reply: Some(reply.clone()),
            },
            true,
        )
        .await?;
        if let Some(endpoint) = endpoint {
            endpoint
                .check_writable()
                .map_err(|error| PipeError::AcceptanceUnknown {
                    source: error.into(),
                })?;
        }
        let receipt = reply
            .lock()
            .map_err(|_| backend("shared receipt publication poisoned"))?
            .take()
            .ok_or_else(|| backend("shared commit did not publish its receipt"))?;
        Ok(receipt)
    }

    pub(super) async fn complete_shared(
        &self,
        endpoint: &Endpoint,
        position: u64,
        skip: bool,
    ) -> Result<(), PipeError> {
        let shared = self.shared.as_ref().ok_or(PipeError::Closed)?;
        let ownership =
            self.reserve_subscriber_write(&endpoint.subscriber, Some(&endpoint.binding))?;
        self.run_shared(
            SharedMutation {
                channel: shared.owner.clone(),
                action: Action::Complete {
                    subscriber: endpoint.subscriber.clone(),
                    binding: endpoint.binding.clone(),
                    position,
                    skip,
                    _ownership: ownership,
                },
                update: None,
                receipt: None,
                reply: None,
            },
            false,
        )
        .await?;
        endpoint
            .check()
            .map_err(|error| PipeError::AcknowledgementUnknown {
                source: error.into(),
            })
    }

    pub(super) async fn retire_shared(&self, subscriber: &str) -> Result<(), PipeError> {
        let shared = self.shared.as_ref().ok_or(PipeError::Closed)?;
        let ownership = self.reserve_subscriber_write(subscriber, None)?;
        self.run_shared(
            SharedMutation {
                channel: shared.owner.clone(),
                action: Action::Retire {
                    subscriber: subscriber.into(),
                    _ownership: ownership,
                },
                update: None,
                receipt: None,
                reply: None,
            },
            false,
        )
        .await
    }
}

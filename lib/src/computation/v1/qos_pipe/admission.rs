// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Opt-in client admission into the same journal and subscriber obligations as
//! the outgoing QoS pipes. No second WAL, queue or pruning worker is created.

use super::*;
use drasi_core::interface::FailureMode;
use sha2::{Digest, Sha256};

pub(super) const MAX_METADATA_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AdmissionRejection {
    #[error("client admission is not enabled")]
    NotEnabled,
    #[error("admission is busy")]
    Busy,
    #[error("unknown, retired or foreign producer session")]
    SessionExpired,
    #[error("producer sequence {received} is not the expected next number {expected:?}")]
    Sequence {
        received: u64,
        expected: Option<u64>,
    },
    #[error("producer sequence {0} is outside the retained receipt window")]
    ReceiptExpired(u64),
    #[error("producer sequence {0} was already accepted with a different payload")]
    PayloadConflict(u64),
    #[error("producer still has unhandled subscriber obligations")]
    PendingObligations,
    #[error("admission identifiers must not exceed 256 bytes")]
    IdentifierTooLong,
}

impl From<AdmissionRejection> for PipeError {
    fn from(error: AdmissionRejection) -> Self {
        PipeError::Backend(error.into())
    }
}

/// Bounds apply to one channel, not to an unbounded set of client names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdmissionOptions {
    pub construction_scope: String,
    pub graph_id: String,
    pub component_id: ComponentId,
    pub failure_scope: FailureMode,
    pub max_producers: NonZeroUsize,
    pub receipts_per_producer: NonZeroUsize,
}

impl AdmissionOptions {
    pub(super) fn validate_channel(
        &self,
        definition: &QosChannelDefinition,
    ) -> Result<(), PipeError> {
        self.validate()?;
        bounded_identifier(definition.stream.as_str())?;
        if !definition.durable || definition.retention != RetentionPolicy::Backpressure {
            return Err(backend(
                "client admission requires a lossless persistent QoS channel",
            ));
        }
        Ok(())
    }

    fn validate(&self) -> Result<(), PipeError> {
        data::validate_identifier("admission scope", &self.construction_scope)?;
        data::validate_identifier("admission graph", &self.graph_id)?;
        for value in [&self.construction_scope, &self.graph_id, self.component_id.as_str()] {
            bounded_identifier(value)?;
        }
        if self.max_producers.get() > 1024
            || self.receipts_per_producer.get() > 1024
            || self
                .max_producers
                .get()
                .saturating_mul(self.receipts_per_producer.get())
                > 16_384
        {
            return Err(backend("admission supports at most 1024 producers, 1024 receipts per producer and 16384 total receipts"));
        }
        Ok(())
    }
}

pub(super) fn bounded_identifier(value: &str) -> Result<(), PipeError> {
    if value.len() > 256 {
        Err(AdmissionRejection::IdentifierTooLong.into())
    } else {
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProducerSession {
    pub incarnation: uuid::Uuid,
    pub producer: ComponentId,
    pub epoch: u64,
}

/// This receipt proves acceptance at the configured storage boundary, not that
/// a query or external destination has handled the event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdmissionReceipt {
    pub session: ProducerSession,
    pub sequence: u64,
    pub position: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProducerStatus {
    pub session: ProducerSession,
    pub next_sequence: Option<u64>,
    pub earliest_receipt: Option<u64>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Receipt {
    pub(super) digest: [u8; 32],
    pub(super) position: u64,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Producer {
    epoch: u64,
    head: u64,
    last_position: u64,
    receipts: BTreeMap<u64, Receipt>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct AdmissionState {
    version: u32,
    options: AdmissionOptions,
    identity: GraphProducerIdentity,
    epoch: u64,
    producers: BTreeMap<ComponentId, Producer>,
}

impl AdmissionState {
    pub(super) fn new(
        options: AdmissionOptions,
        definition: &QosChannelDefinition,
    ) -> Result<Self, PipeError> {
        options.validate_channel(definition)?;
        let identity = GraphProducerIdentity::new(
            options.construction_scope.clone(),
            options.graph_id.clone(),
            options.component_id.clone(),
            definition.stream.clone(),
            true,
        )
        .map_err(PipeError::Backend)?;
        Ok(Self {
            version: 1,
            options,
            identity,
            epoch: 0,
            producers: BTreeMap::new(),
        })
    }

    pub(super) fn options(&self) -> &AdmissionOptions {
        &self.options
    }

    pub(super) fn identity(&self) -> &GraphProducerIdentity {
        &self.identity
    }

    pub(super) fn failure_scope(&self) -> FailureMode {
        self.options.failure_scope
    }

    pub(super) fn validate_entry(
        &self,
        position: u64,
        envelope: &ChangeEnvelope,
    ) -> Result<(), PipeError> {
        let progress =
            GraphProducerProgress::from_envelope(envelope).map_err(PipeError::Backend)?;
        let id = EnvelopeId::try_new(
            format!("admission:{}", self.identity.incarnation()),
            Bytes::copy_from_slice(&position.to_be_bytes()),
        )?;
        if !progress.is_some_and(|progress| {
            progress.identity() == &self.identity && progress.sequence() == position
        }) || envelope.system().sequence() != position
            || envelope.id() != &id
        {
            return Err(backend(
                "admission journal contains inconsistent producer progress",
            ));
        }
        Ok(())
    }

    pub(super) fn validate(
        &self,
        definition: &QosChannelDefinition,
        head: u64,
    ) -> Result<(), PipeError> {
        self.options.validate()?;
        bounded_identifier(definition.stream.as_str())?;
        self.identity.validate().map_err(PipeError::Backend)?;
        if self.version != 1
            || !definition.durable
            || definition.retention != RetentionPolicy::Backpressure
            || self.identity.stream() != &definition.stream
            || !self.identity.persistent()
            || self.identity.construction_scope() != self.options.construction_scope
            || self.identity.graph_id() != self.options.graph_id
            || self.identity.component_id() != &self.options.component_id
            || self.producers.len() > self.options.max_producers.get()
        {
            return Err(backend(
                "admission identity, bounds or journal configuration changed",
            ));
        }
        let mut epochs = std::collections::BTreeSet::new();
        let mut positions = std::collections::BTreeSet::new();
        let mut accepted = 0u64;
        for (name, producer) in &self.producers {
            bounded_identifier(name.as_str())?;
            accepted = accepted
                .checked_add(producer.head)
                .filter(|accepted| *accepted <= head)
                .ok_or_else(|| backend("admission producer totals exceed the journal head"))?;
            let count = producer
                .head
                .min(self.options.receipts_per_producer.get() as u64);
            if producer.epoch == 0
                || producer.epoch > self.epoch
                || !epochs.insert(producer.epoch)
                || producer.last_position > head
                || producer.head > producer.last_position
                || producer.receipts.len() as u64 != count
                || (producer.head == 0) != (producer.last_position == 0)
            {
                return Err(backend("admission producer progress is inconsistent"));
            }
            let mut previous = 0;
            for (offset, (sequence, receipt)) in producer.receipts.iter().enumerate() {
                if *sequence != producer.head - count + 1 + offset as u64
                    || receipt.position == 0
                    || receipt.position > producer.last_position
                    || receipt.position <= previous
                    || *sequence > receipt.position
                    || !positions.insert(receipt.position)
                {
                    return Err(backend("admission receipt history is inconsistent"));
                }
                previous = receipt.position;
            }
            if previous != producer.last_position {
                return Err(backend("admission receipt head is inconsistent"));
            }
        }
        Ok(())
    }

    fn session(&self, producer: &ComponentId) -> Result<ProducerSession, PipeError> {
        let state = self
            .producers
            .get(producer)
            .ok_or(AdmissionRejection::SessionExpired)?;
        Ok(ProducerSession {
            incarnation: self.identity.incarnation(),
            producer: producer.clone(),
            epoch: state.epoch,
        })
    }

    fn producer(&self, session: &ProducerSession) -> Result<&Producer, PipeError> {
        if &self.session(&session.producer)? != session {
            return Err(AdmissionRejection::SessionExpired.into());
        }
        Ok(&self.producers[&session.producer])
    }
}

impl QosChannel {
    /// Configure durable client acceptance before accepting the first event.
    /// Reopen must supply exactly the persisted options; no silent downgrade.
    pub async fn enable_admission(&self, options: AdmissionOptions) -> Result<(), PipeError> {
        options.validate_channel(&self.definition)?;

        self.durability
            .require(options.failure_scope)
            .map_err(|error| PipeError::Backend(error.into()))?;
        let mut state = self.state.lock().await;
        self.check()?;
        if state.metadata.replay.is_some() {
            return Err(backend(
                "client admission and output replay tracking are mutually exclusive",
            ));
        }
        if let Some(admission) = &state.metadata.admission {
            return if admission.options == options {
                Ok(())
            } else {
                Err(backend("admission options differ from persisted settings"))
            };
        }
        if state.metadata.head != 0 {
            return Err(backend(
                "admission must be configured before the first channel event",
            ));
        }
        let mut metadata = state.metadata.clone();
        metadata.admission = Some(AdmissionState::new(options, &self.definition)?);
        self.persist(&metadata, None).await?;
        state.metadata = metadata;
        Ok(())
    }

    /// Registration is idempotent by producer name while that session is active.
    /// Retired names can be registered again, but the persisted epoch never repeats.
    pub async fn register_producer(
        &self,
        producer: ComponentId,
    ) -> Result<ProducerSession, PipeError> {
        bounded_identifier(producer.as_str())?;
        let mut state = self.state.lock().await;
        self.check()?;
        let mut metadata = state.metadata.clone();
        let admission = metadata
            .admission
            .as_mut()
            .ok_or(AdmissionRejection::NotEnabled)?;
        if admission.producers.contains_key(&producer) {
            return admission.session(&producer);
        }
        if admission.producers.len() >= admission.options.max_producers.get() {
            return Err(PipeError::CapacityExhausted);
        }
        admission.epoch = admission
            .epoch
            .checked_add(1)
            .ok_or_else(|| backend("producer epochs exhausted"))?;
        admission.producers.insert(
            producer.clone(),
            Producer {
                epoch: admission.epoch,
                head: 0,
                last_position: 0,
                receipts: BTreeMap::new(),
            },
        );
        let session = admission.session(&producer)?;
        self.persist(&metadata, None).await?;
        state.metadata = metadata;
        Ok(session)
    }

    pub async fn producer_status(
        &self,
        session: &ProducerSession,
    ) -> Result<ProducerStatus, PipeError> {
        let state = self.state.lock().await;
        self.check()?;
        let admission = state
            .metadata
            .admission
            .as_ref()
            .ok_or(AdmissionRejection::NotEnabled)?;
        let producer = admission.producer(session)?;
        Ok(ProducerStatus {
            session: session.clone(),
            next_sequence: producer.head.checked_add(1),
            earliest_receipt: producer.receipts.keys().next().copied(),
        })
    }

    /// Retiring never drops pending subscriber obligations. Previously returned
    /// tokens stay invalid even if the producer name is subsequently reused.
    pub async fn retire_producer(&self, session: &ProducerSession) -> Result<(), PipeError> {
        let mut state = self.state.lock().await;
        self.check()?;
        let mut metadata = state.metadata.clone();
        let admission = metadata
            .admission
            .as_mut()
            .ok_or(AdmissionRejection::NotEnabled)?;
        let producer = admission.producer(session)?;
        if metadata
            .cursors
            .values()
            .any(|cursor| !cursor.retired && cursor.position < producer.last_position)
        {
            return Err(AdmissionRejection::PendingObligations.into());
        }
        admission.producers.remove(&session.producer);
        self.persist(&metadata, None).await?;
        state.metadata = metadata;
        Ok(())
    }

    /// Look up a potentially lost response. None means this sequence is exactly
    /// the next unaccepted input. Older expired requests and gaps are errors.
    pub async fn admission_receipt(
        &self,
        session: &ProducerSession,
        sequence: u64,
    ) -> Result<Option<AdmissionReceipt>, PipeError> {
        let state = self.state.lock().await;
        self.check()?;
        let admission = state
            .metadata
            .admission
            .as_ref()
            .ok_or(AdmissionRejection::NotEnabled)?;
        let producer = admission.producer(session)?;
        if let Some(receipt) = producer.receipts.get(&sequence) {
            return Ok(Some(AdmissionReceipt {
                session: session.clone(),
                sequence,
                position: receipt.position,
            }));
        }
        if producer.head.checked_add(1) == Some(sequence) {
            return Ok(None);
        }
        Err(if sequence > 0 && sequence <= producer.head {
            AdmissionRejection::ReceiptExpired(sequence)
        } else {
            AdmissionRejection::Sequence {
                received: sequence,
                expected: producer.head.checked_add(1),
            }
        }
        .into())
    }

    /// Atomically commit input, its retry receipt and every subscriber obligation.
    /// Full capacity or another admission in progress returns an explicit failure,
    /// not an additional hidden request queue. After an uncertain commit the
    /// channel is fenced until shutdown/reconstruction and receipt lookup.
    pub async fn admit(
        &self,
        session: &ProducerSession,
        sequence: u64,
        input: &ChangeEnvelope,
    ) -> Result<AdmissionReceipt, PipeError> {
        self.admit_validated(session, sequence, input, |_| Ok(()))
            .await
    }

    pub(super) async fn admit_validated(
        &self,
        session: &ProducerSession,
        sequence: u64,
        input: &ChangeEnvelope,
        validate: impl FnOnce(&ChangeEnvelope) -> anyhow::Result<()> + Send,
    ) -> Result<AdmissionReceipt, PipeError> {
        let mut state = self
            .state
            .try_lock()
            .map_err(|_| AdmissionRejection::Busy)?;
        self.check()?;
        let storage = self
            .storage()?
            .ok_or_else(|| backend("client admission requires persistent storage"))?;
        let admission = state
            .metadata
            .admission
            .as_ref()
            .ok_or(AdmissionRejection::NotEnabled)?;
        self.durability
            .require(admission.options.failure_scope)
            .map_err(|error| PipeError::Backend(error.into()))?;
        let producer = admission.producer(session)?;
        let digest: [u8; 32] = Sha256::digest(
            storage
                .codec
                .encode(input)
                .map_err(|error| PipeError::Backend(error.into()))?,
        )
        .into();
        if let Some(saved) = producer.receipts.get(&sequence) {
            if saved.digest != digest {
                return Err(AdmissionRejection::PayloadConflict(sequence).into());
            }
            return Ok(AdmissionReceipt {
                session: session.clone(),
                sequence,
                position: saved.position,
            });
        }
        if producer.head.checked_add(1) != Some(sequence) {
            return Err(if sequence > 0 && sequence <= producer.head {
                AdmissionRejection::ReceiptExpired(sequence)
            } else {
                AdmissionRejection::Sequence {
                    received: sequence,
                    expected: producer.head.checked_add(1),
                }
            }
            .into());
        }
        if !state
            .metadata
            .cursors
            .values()
            .any(|cursor| !cursor.retired)
        {
            return Err(backend(
                "admission requires at least one active subscriber obligation",
            ));
        }
        let position = state
            .metadata
            .head
            .checked_add(1)
            .ok_or_else(|| backend("admission journal sequence exhausted"))?;
        let mut system = SystemMetadata::new(self.definition.stream.clone(), position);
        if let Some(timestamp) = input.system().timestamp() {
            system = system.with_timestamp(timestamp);
        }
        let mut envelope = input.derive(
            EnvelopeId::try_new(
                format!("admission:{}", admission.identity.incarnation()),
                Bytes::copy_from_slice(&position.to_be_bytes()),
            )?,
            input.changes().clone(),
            system,
        );
        GraphProducerProgress::annotate(&mut envelope, &admission.identity, position)
            .map_err(PipeError::Backend)?;
        validate(&envelope).map_err(PipeError::Backend)?;
        let (retain_from, budget) = state.prepare_append(&self.definition, &envelope)?;
        let mut metadata = state.metadata.clone();
        metadata.head = position;
        metadata.producer_sequence = Some(position);
        let admission = metadata.admission.as_mut().expect("validated admission");
        let producer = admission
            .producers
            .get_mut(&session.producer)
            .expect("validated producer");
        producer.head = sequence;
        producer.last_position = position;
        producer
            .receipts
            .insert(sequence, Receipt { digest, position });
        while producer.receipts.len() > admission.options.receipts_per_producer.get() {
            producer.receipts.pop_first();
        }
        let _wake = WakeOnDrop(&self.changed);
        self.persist(&metadata, Some((position, &envelope, retain_from)))
            .await?;
        state.append_committed(position, envelope, retain_from, budget);
        state.metadata = metadata;
        Ok(AdmissionReceipt {
            session: session.clone(),
            sequence,
            position,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(producers: usize, receipts: usize) -> AdmissionOptions {
        AdmissionOptions {
            construction_scope: "scope".into(),
            graph_id: "graph".into(),
            component_id: ComponentId::try_new("source").unwrap(),
            failure_scope: FailureMode::ProcessRestart,
            max_producers: NonZeroUsize::new(producers).unwrap(),
            receipts_per_producer: NonZeroUsize::new(receipts).unwrap(),
        }
    }

    #[test]
    fn admission_limits_accept_exact_boundaries_and_reject_the_next_value() {
        for (producers, receipts) in [(1024, 16), (16, 1024), (128, 128)] {
            options(producers, receipts).validate().unwrap();
            assert!(options(producers + 1, receipts).validate().is_err());
            assert!(options(producers, receipts + 1).validate().is_err());
        }
        let mut definition = options(1, 1);
        definition.graph_id = "g".repeat(256);
        definition.validate().unwrap();
        definition.graph_id.push('g');
        assert!(definition.validate().is_err());
    }

    #[test]
    fn maximum_receipt_ledger_fits_the_serialized_metadata_budget() {
        let definition = QosChannelDefinition {
            stream: StreamId::try_new("source/out").unwrap(),
            capacity: NonZeroUsize::new(1).unwrap(),
            durable: true,
            retention: RetentionPolicy::Backpressure,
            subscribers: BTreeMap::from([("consumer".into(), SubscriptionStart::Earliest)]),
        };
        let identity = GraphProducerIdentity::new(
            "scope".into(),
            "graph".into(),
            ComponentId::try_new("source").unwrap(),
            definition.stream.clone(),
            true,
        )
        .unwrap();
        let producers = (0..1024)
            .map(|index| {
                let head = u64::MAX / 1024;
                let last_position = u64::MAX - (1023 - index) * 16;
                (
                    ComponentId::try_new(format!("{index:0256}")).unwrap(),
                    Producer {
                        epoch: index + 1,
                        head,
                        last_position,
                        receipts: (0..16)
                            .map(|offset| {
                                (
                                    head - offset,
                                    Receipt {
                                        digest: [255; 32],
                                        position: last_position - offset,
                                    },
                                )
                            })
                            .collect(),
                    },
                )
            })
            .collect();
        let admission = AdmissionState {
            version: 1,
            options: options(1024, 16),
            identity,
            epoch: 1024,
            producers,
        };
        admission.validate(&definition, u64::MAX).unwrap();
        let mut metadata = initial(&definition).unwrap();
        assert!(!serde_json::to_value(&metadata)
            .unwrap()
            .as_object()
            .unwrap()
            .contains_key("admission"));
        metadata.admission = Some(admission);
        metadata.head = u64::MAX;
        metadata.producer_sequence = Some(u64::MAX);
        let size = serde_json::to_vec(&metadata).unwrap().len();
        assert!(
            size <= MAX_METADATA_BYTES,
            "maximum receipt ledger used {size} bytes"
        );
    }
}

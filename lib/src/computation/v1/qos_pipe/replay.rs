// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Optional receipts for retained producer outputs replayed with new transport IDs.

pub(super) use super::super::output_identity::Candidate;
use super::super::output_identity::Producer;
pub use super::super::output_identity::ReplayRejection;
use super::*;
use drasi_core::interface::FailureMode;

/// One persistent producer and a bounded retry window per outgoing channel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplayOptions {
    pub failure_scope: FailureMode,
    pub receipt_capacity: NonZeroUsize,
}

impl ReplayOptions {
    pub(super) fn validate(&self, definition: &QosChannelDefinition) -> Result<(), PipeError> {
        if !definition.durable || definition.retention != RetentionPolicy::Backpressure {
            return Err(backend(
                "output replay tracking requires a lossless persistent QoS channel",
            ));
        }
        if self.receipt_capacity.get() > 1024 {
            return Err(backend(
                "output replay tracking supports at most 1024 receipts",
            ));
        }
        admission::bounded_identifier(definition.stream.as_str())
    }
}

impl From<ReplayRejection> for PipeError {
    fn from(error: ReplayRejection) -> Self {
        PipeError::Backend(error.into())
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ReplayState {
    version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    identity: Option<uuid::Uuid>,
    options: ReplayOptions,
    producer: Option<Producer>,
    sequence: u64,
    receipts: BTreeMap<u64, admission::Receipt>,
}

impl ReplayState {
    pub(super) fn new(options: ReplayOptions) -> Self {
        Self {
            version: 2,
            identity: Some(uuid::Uuid::new_v4()),
            options,
            producer: None,
            sequence: 0,
            receipts: BTreeMap::new(),
        }
    }
    pub(super) fn identity(&self) -> Option<uuid::Uuid> {
        self.identity
    }

    pub(super) fn options(&self) -> ReplayOptions {
        self.options.clone()
    }

    pub(super) fn failure_scope(&self) -> FailureMode {
        self.options.failure_scope
    }

    pub(super) fn check(&self, candidate: &Candidate) -> Result<Option<u64>, PipeError> {
        if self
            .producer
            .as_ref()
            .is_some_and(|producer| producer != &candidate.producer)
        {
            return Err(ReplayRejection::ProducerChanged.into());
        }
        if candidate.sequence > self.sequence {
            return Ok(None);
        }
        let receipt = self
            .receipts
            .get(&candidate.sequence)
            .ok_or(ReplayRejection::ReceiptExpired(candidate.sequence))?;
        if receipt.digest != candidate.digest {
            return Err(ReplayRejection::PayloadConflict(candidate.sequence).into());
        }
        Ok(Some(receipt.position))
    }

    pub(super) fn record(&mut self, candidate: &Candidate, position: u64) {
        self.producer = Some(candidate.producer.clone());
        self.sequence = candidate.sequence;
        self.receipts.insert(
            candidate.sequence,
            admission::Receipt {
                digest: candidate.digest,
                position,
            },
        );
        while self.receipts.len() > self.options.receipt_capacity.get() {
            self.receipts.pop_first();
        }
    }

    pub(super) fn validate(
        &self,
        definition: &QosChannelDefinition,
        head: u64,
        entries: &BTreeMap<u64, ChangeEnvelope>,
        codec: &EnvelopeCodec,
    ) -> Result<(), PipeError> {
        self.options.validate(definition)?;
        let count = head.min(self.options.receipt_capacity.get() as u64);
        let valid_version = match (self.version, self.identity) {
            (1, None) => true,
            (2, Some(identity)) => !identity.is_nil(),
            _ => false,
        };
        if !valid_version
            || (head == 0) != self.producer.is_none()
            || self.receipts.len() as u64 != count
            || self
                .receipts
                .last_key_value()
                .map_or(0, |(sequence, _)| *sequence)
                != self.sequence
            || self
                .receipts
                .iter()
                .enumerate()
                .any(|(offset, (sequence, receipt))| {
                    *sequence == 0 || receipt.position != head - count + 1 + offset as u64
                })
        {
            return Err(backend("output replay receipt history is inconsistent"));
        }
        if let Some(producer) = &self.producer {
            producer.validate(&definition.stream)?;
        }
        let mut previous = 0;
        let mut previous_transport = None;
        for (position, envelope) in entries {
            let candidate = Candidate::new(envelope, codec)?;
            if self.producer.as_ref() != Some(&candidate.producer)
                || candidate.sequence <= previous
                || previous_transport
                    .is_some_and(|sequence| envelope.system().sequence() <= sequence)
                || candidate.sequence > self.sequence
                || (*position == head && candidate.sequence != self.sequence)
            {
                return Err(backend(
                    "output replay journal has inconsistent producer progress",
                ));
            }
            if *position > head - count && self.check(&candidate)? != Some(*position) {
                return Err(backend(
                    "output replay receipt disagrees with its journal entry",
                ));
            }
            previous = candidate.sequence;
            previous_transport = Some(envelope.system().sequence());
        }
        Ok(())
    }
}

impl QosChannel {
    /// Enable restart-stable output receipts before the first publication or binding.
    ///
    /// Publish new outputs in increasing logical and transport order. A retry may have a
    /// different transport ID/sequence and immediate-query completion timings,
    /// but must preserve all other content.
    /// Changed/expired retries and producer replacement reject, never append.
    /// Query snapshots, recovery skips and legacy transport-only identities are
    /// not live output identities. This does not confirm all graph branches or
    /// atomically commit the producer's separate state.
    pub async fn enable_replay(&self, options: ReplayOptions) -> Result<(), PipeError> {
        options.validate(&self.definition)?;
        self.durability
            .require(options.failure_scope)
            .map_err(|error| PipeError::Backend(error.into()))?;
        let mut state = self.state.lock().await;
        self.check()?;
        if state.metadata.admission.is_some() {
            return Err(backend(
                "client admission and output replay tracking are mutually exclusive",
            ));
        }

        if let Some(replay) = &state.metadata.replay {
            if replay.options != options {
                return Err(backend(
                    "output replay options differ from persisted settings",
                ));
            }
            if replay.identity.is_some() {
                return Ok(());
            }
        } else if state.metadata.head != 0 {
            return Err(backend(
                "output replay tracking must be configured before the first event",
            ));
        }
        let _setup = self.begin_replay_setup()?;
        let mut metadata = state.metadata.clone();
        let identity = uuid::Uuid::new_v4();
        let replay = metadata.replay.get_or_insert_with(|| ReplayState {
            version: 2,
            identity: Some(identity),
            options,
            producer: None,
            sequence: 0,
            receipts: BTreeMap::new(),
        });
        replay.version = 2;
        replay.identity = Some(identity);
        self.persist(&metadata, None).await?;
        state.metadata = metadata;
        self.replay_identity
            .set(identity)
            .map_err(|_| backend("output journal identity was already installed"))?;
        Ok(())
    }

    /// Identity of this persisted replay journal, independent of resource names.
    /// None means output replay tracking is disabled. This is not proof that
    /// independent stores share a transaction or that storage rollback is safe.
    /// It remains readable during writes and after shutdown; it does not
    /// establish that the channel is currently available for publication.
    pub fn output_journal_identity(&self) -> Option<uuid::Uuid> {
        self.replay_identity.get().copied()
    }

    fn begin_replay_setup(&self) -> Result<ReplaySetup<'_>, PipeError> {
        let bindings = self
            .bindings
            .lock()
            .map_err(|_| backend("QoS bindings poisoned"))?;
        if bindings.generation != 0 {
            return Err(backend(
                "output replay tracking must be configured before binding pipes",
            ));
        }
        self.configuring_replay.store(true, Ordering::Release);
        Ok(ReplaySetup(self))
    }
}

struct ReplaySetup<'a>(&'a QosChannel);

impl Drop for ReplaySetup<'_> {
    fn drop(&mut self) {
        self.0.configuring_replay.store(false, Ordering::Release);
    }
}

#[cfg(test)]
mod tests;

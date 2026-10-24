// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::collections::BTreeSet;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex, Weak,
};

use bytes::Bytes;
use drasi_core::interface::SourceCheckpoint;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{ComponentId, PortId};

pub(super) const CHECKPOINT: &str = "\0computation:output-bindings:v1";
pub(super) const MAX_DESTINATIONS: usize = 256;
const MAX_BYTES: usize = 1024 * 1024;

/// A logical destination and the durable journal retaining its obligation.
/// Consumer configuration/generation is deliberately not part of this identity:
/// pending output follows a replacement with the same component and input port.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputDestination {
    pub output: PortId,
    pub consumer: ComponentId,
    pub input: PortId,
    pub journal: uuid::Uuid,
    pub subscriber: String,
}

/// Bounded, canonical membership for unconfirmed producer output.
/// Fast/untracked branches are absent; this is not an external-effect receipt.
#[derive(Debug, Clone, Default)]
pub struct OutputBindings {
    destinations: Vec<OutputDestination>,
    fingerprint: Option<Bytes>,
    shared: Vec<SharedDestination>,
}

#[derive(Debug, Clone)]
struct SharedDestination {
    output: PortId,
    journal: uuid::Uuid,
    channel: Weak<super::QosChannel>,
}

impl PartialEq for OutputBindings {
    fn eq(&self, other: &Self) -> bool {
        self.destinations == other.destinations && self.fingerprint == other.fingerprint
    }
}

impl Eq for OutputBindings {}

pub(crate) struct OutputReservations(Vec<super::qos_pipe::QosReservation>);

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
struct SharedHandoffError(#[source] anyhow::Error);

pub(super) fn index_error(error: anyhow::Error) -> drasi_core::interface::IndexError {
    drasi_core::interface::IndexError::other(SharedHandoffError(error))
}

impl OutputReservations {
    pub(crate) fn mutations(
        self,
        envelope: Option<&super::ChangeEnvelope>,
    ) -> anyhow::Result<Vec<Box<dyn drasi_core::computation::TransactionGroupMutation>>> {
        let Some(envelope) = envelope else {
            return Ok(Vec::new());
        };
        self.0
            .into_iter()
            .map(|reservation| reservation.mutation(envelope.clone()).map_err(Into::into))
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OutputBindingError {
    #[error("pending output must reach its original destinations before their bindings change")]
    PendingDestinationsChanged,
    #[error("recover the interrupted producer before changing its output bindings")]
    RecoveryRequired,
    #[error(
        "complete pending output and unbind its durable destinations before resetting query state"
    )]
    ResetRequiresUnboundDestinations,
    #[error("invalid output bindings: {0}")]
    Invalid(String),
    #[error("output binding publication is poisoned")]
    Poisoned,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    version: u32,
    destinations: Vec<OutputDestination>,
}

impl OutputBindings {
    pub fn try_new(
        destinations: impl IntoIterator<Item = OutputDestination>,
    ) -> Result<Self, OutputBindingError> {
        let mut destinations: Vec<_> = destinations
            .into_iter()
            .take(MAX_DESTINATIONS + 1)
            .collect();
        destinations.sort();
        let result = Self {
            destinations,
            fingerprint: None,
            shared: Vec::new(),
        };
        result.validate()?;
        result.with_fingerprint()
    }

    pub fn destinations(&self) -> &[OutputDestination] {
        &self.destinations
    }

    pub(crate) fn attach_shared(
        &mut self,
        output: &PortId,
        channel: &Arc<super::QosChannel>,
    ) -> Result<(), OutputBindingError> {
        if !channel.is_shared() {
            return Ok(());
        }
        let journal = channel.output_journal_identity().ok_or_else(|| {
            OutputBindingError::Invalid("shared journal identity is missing".into())
        })?;
        if !self
            .destinations
            .iter()
            .any(|destination| destination.journal == journal && &destination.output == output)
        {
            return Err(OutputBindingError::Invalid(
                "shared output has no destination obligation".into(),
            ));
        }
        if let Some(previous) = self.shared.iter().find(|entry| entry.journal == journal) {
            if &previous.output != output || !previous.channel.ptr_eq(&Arc::downgrade(channel)) {
                return Err(OutputBindingError::Invalid(
                    "different outputs claim the same shared journal".into(),
                ));
            }
            return Ok(());
        }
        self.shared.push(SharedDestination {
            output: output.clone(),
            journal,
            channel: Arc::downgrade(channel),
        });
        self.shared.sort_by_key(|entry| entry.journal);
        Ok(())
    }

    pub(crate) fn has_shared(&self) -> bool {
        !self.shared.is_empty()
    }

    pub(crate) fn validate_transaction(
        &self,
        transaction: &drasi_core::computation::ComputationTransaction,
    ) -> anyhow::Result<()> {
        for entry in &self.shared {
            let channel = entry.channel.upgrade().ok_or(super::PipeError::Closed)?;
            anyhow::ensure!(
                entry.output.as_str() == "out" && channel.shares_transaction(transaction)?,
                "shared output must belong to this producer's actual storage group and output port"
            );
        }
        Ok(())
    }

    pub(crate) fn validate_query(
        &self,
        query: &drasi_core::computation::ComputationQuery,
    ) -> anyhow::Result<()> {
        for entry in &self.shared {
            let channel = entry.channel.upgrade().ok_or(super::PipeError::Closed)?;
            anyhow::ensure!(
                entry.output.as_str() == "out" && channel.shares_query(query)?,
                "shared output must belong to this query's actual storage group and output port"
            );
        }
        Ok(())
    }

    pub(crate) async fn reserve(&self) -> anyhow::Result<OutputReservations> {
        let mut reservations = Vec::with_capacity(self.shared.len());
        for entry in &self.shared {
            let channel = entry.channel.upgrade().ok_or(super::PipeError::Closed)?;
            reservations.push(channel.reserve().await?);
        }
        Ok(OutputReservations(reservations))
    }

    fn validate(&self) -> Result<(), OutputBindingError> {
        let invalid = |message: &str| OutputBindingError::Invalid(message.into());
        if self.destinations.len() > MAX_DESTINATIONS {
            return Err(invalid("at most 256 destinations are supported"));
        }
        if self.destinations.windows(2).any(|pair| pair[0] >= pair[1]) {
            return Err(invalid("destinations must be unique and ordered"));
        }
        let mut subscribers = BTreeSet::new();
        let mut endpoints = BTreeSet::new();
        for destination in &self.destinations {
            if destination.journal.is_nil() {
                return Err(invalid("journal identity is missing"));
            }
            if !subscribers.insert((destination.journal, &destination.subscriber))
                || !endpoints.insert((
                    &destination.output,
                    &destination.consumer,
                    &destination.input,
                ))
            {
                return Err(invalid(
                    "a subscriber or logical endpoint is bound more than once",
                ));
            }
            for id in [
                destination.output.as_str(),
                destination.consumer.as_str(),
                destination.input.as_str(),
                destination.subscriber.as_str(),
            ] {
                super::data::validate_identifier("output destination", id)
                    .map_err(|error| OutputBindingError::Invalid(error.to_string()))?;
                if id.len() > 256 {
                    return Err(invalid("destination identifiers must fit in 256 bytes"));
                }
            }
        }
        Ok(())
    }

    pub(super) fn load(checkpoint: Option<&SourceCheckpoint>) -> Result<Self, OutputBindingError> {
        let Some(checkpoint) = checkpoint else {
            return Ok(Self::default());
        };
        let bytes = checkpoint
            .source_position
            .as_deref()
            .filter(|bytes| checkpoint.sequence == 1 && bytes.len() <= MAX_BYTES)
            .ok_or_else(|| OutputBindingError::Invalid("invalid checkpoint".into()))?;
        let record: Record = serde_json::from_slice(bytes)
            .map_err(|error| OutputBindingError::Invalid(error.to_string()))?;
        if record.version != 1 {
            return Err(OutputBindingError::Invalid("unsupported version".into()));
        }
        let result = Self {
            destinations: record.destinations,
            fingerprint: None,
            shared: Vec::new(),
        };
        result.validate()?;
        result.with_fingerprint()
    }

    fn with_fingerprint(mut self) -> Result<Self, OutputBindingError> {
        if !self.destinations.is_empty() {
            self.fingerprint = Some(Bytes::copy_from_slice(&Sha256::digest(self.encode()?)));
        }
        Ok(self)
    }

    pub(super) fn fingerprint(&self) -> Option<&Bytes> {
        self.fingerprint.as_ref()
    }

    pub(super) fn validate_progress(
        &self,
        position: Option<&Bytes>,
    ) -> Result<(), OutputBindingError> {
        if self.fingerprint() != position {
            return Err(OutputBindingError::Invalid(
                "destination membership disagrees with durable output progress".into(),
            ));
        }
        Ok(())
    }

    pub(super) fn encode(&self) -> Result<Bytes, OutputBindingError> {
        self.validate()?;
        let bytes = serde_json::to_vec(&Record {
            version: 1,
            destinations: self.destinations.clone(),
        })
        .map_err(|error| OutputBindingError::Invalid(error.to_string()))?;
        if bytes.len() > MAX_BYTES {
            return Err(OutputBindingError::Invalid(
                "checkpoint exceeds 1 MiB".into(),
            ));
        }
        Ok(bytes.into())
    }

    pub(super) fn validate_change(
        &self,
        proposed: &Self,
        pending: bool,
    ) -> Result<(), OutputBindingError> {
        proposed.validate()?;
        if pending && self != proposed {
            return Err(OutputBindingError::PendingDestinationsChanged);
        }
        Ok(())
    }
}

/// A live preflight view, not another transaction owner. Only the producer
/// publishes confirmed state here, after using its existing transaction.
#[derive(Debug)]
pub(crate) struct OutputBindingState {
    bindings: Mutex<Option<OutputBindings>>,
    standalone: Mutex<Option<StandaloneOwner>>,
    pending: AtomicBool,
    pub(super) failure: Arc<AtomicBool>,
}

struct StandaloneOwner {
    provider: Weak<dyn drasi_core::computation::ComputationIndexProvider>,
    transaction: Weak<drasi_core::computation::ComputationTransaction>,
}

impl std::fmt::Debug for StandaloneOwner {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("StandaloneOwner")
    }
}

impl OutputBindingState {
    pub(crate) fn has_standalone_owner(&self) -> Result<bool, OutputBindingError> {
        Ok(self
            .standalone
            .lock()
            .map_err(|_| OutputBindingError::Poisoned)?
            .is_some())
    }

    pub(super) fn bind_standalone_owner(
        &self,
        provider: &Arc<dyn drasi_core::computation::ComputationIndexProvider>,
        transaction: Weak<drasi_core::computation::ComputationTransaction>,
    ) -> Result<(), OutputBindingError> {
        if provider.transaction_group().is_none() {
            *self
                .standalone
                .lock()
                .map_err(|_| OutputBindingError::Poisoned)? = Some(StandaloneOwner {
                provider: Arc::downgrade(provider),
                transaction,
            });
        }
        Ok(())
    }

    pub(crate) fn freeze_standalone(
        &self,
        provider: &Arc<dyn drasi_core::computation::ComputationIndexProvider>,
    ) -> anyhow::Result<Option<drasi_core::computation::ComputationTransactionRetirement>> {
        let owner = self
            .standalone
            .lock()
            .map_err(|_| OutputBindingError::Poisoned)?;
        let Some(owner) = owner
            .as_ref()
            .filter(|owner| owner.provider.ptr_eq(&Arc::downgrade(provider)))
        else {
            return Ok(None);
        };
        let transaction = owner
            .transaction
            .upgrade()
            .ok_or(OutputBindingError::RecoveryRequired)?;
        Ok(Some(transaction.freeze_for_retirement()?))
    }

    #[cfg(test)]
    fn validate_drained(&self) -> Result<(), OutputBindingError> {
        self.validate_retirement(false)
    }

    pub(crate) fn validate_retirement(
        &self,
        allow_pending: bool,
    ) -> Result<(), OutputBindingError> {
        if self
            .bindings
            .lock()
            .map_err(|_| OutputBindingError::Poisoned)?
            .is_none()
        {
            return Err(OutputBindingError::RecoveryRequired);
        }
        if allow_pending {
            Ok(())
        } else {
            self.validate_unresolved()
        }
    }

    pub(super) fn new(failure: Arc<AtomicBool>) -> Arc<Self> {
        Arc::new(Self {
            bindings: Mutex::new(None),
            standalone: Mutex::new(None),
            pending: AtomicBool::new(false),
            failure,
        })
    }

    pub(super) fn validate(&self, proposed: &OutputBindings) -> Result<(), OutputBindingError> {
        let bindings = self
            .bindings
            .lock()
            .map_err(|_| OutputBindingError::Poisoned)?;
        if let Some(current) = bindings.as_ref() {
            if current != proposed && self.failure.load(Ordering::Acquire) {
                return Err(OutputBindingError::RecoveryRequired);
            }
            current.validate_change(proposed, self.pending.load(Ordering::Acquire))?;
        }
        Ok(())
    }

    pub(super) fn validate_unresolved(&self) -> Result<(), OutputBindingError> {
        if self.failure.load(Ordering::Acquire) {
            return Err(OutputBindingError::RecoveryRequired);
        }
        if self.pending.load(Ordering::Acquire) {
            return Err(OutputBindingError::PendingDestinationsChanged);
        }
        Ok(())
    }

    pub(super) fn publish(
        &self,
        bindings: OutputBindings,
        pending: bool,
    ) -> Result<(), OutputBindingError> {
        let mut saved = self
            .bindings
            .lock()
            .map_err(|_| OutputBindingError::Poisoned)?;
        self.pending.store(pending, Ordering::Release);
        *saved = Some(bindings);
        Ok(())
    }

    pub(super) fn pending(&self, pending: bool) {
        self.pending.store(pending, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "current_thread")]
    async fn standalone_retirement_matches_actual_provider_not_its_path() {
        use crate::computation::v1::LegacyIndexProviderAdapter;
        use drasi_core::computation::{ComputationIndexProvider, ComputationTransaction};
        use drasi_index_rocksdb::RocksDbIndexProvider;

        let directory = tempfile::tempdir().unwrap();
        let provider: Arc<dyn ComputationIndexProvider> = LegacyIndexProviderAdapter::new(
            Arc::new(RocksDbIndexProvider::new(directory.path(), false, false)),
        );
        let foreign: Arc<dyn ComputationIndexProvider> = LegacyIndexProviderAdapter::new(Arc::new(
            RocksDbIndexProvider::new(directory.path(), false, false),
        ));
        let transaction = Arc::new(
            ComputationTransaction::try_new(
                provider.create_indexes("graph", "query").await.unwrap(),
            )
            .unwrap(),
        );
        let state = OutputBindingState::new(Arc::new(AtomicBool::new(false)));
        assert!(!state.has_standalone_owner().unwrap());
        state
            .bind_standalone_owner(&provider, Arc::downgrade(&transaction))
            .unwrap();
        assert!(state.freeze_standalone(&foreign).unwrap().is_none());
        assert!(state.validate_drained().is_err());
        state.publish(OutputBindings::default(), false).unwrap();
        let frozen = state.freeze_standalone(&provider).unwrap().unwrap();
        state.validate_drained().unwrap();
        assert!(state.freeze_standalone(&provider).is_err());
        frozen.resume();
        transaction.shutdown().await.unwrap();
        assert!(state.freeze_standalone(&provider).is_err());
        drop(transaction);
        assert!(state.freeze_standalone(&provider).is_err());
    }

    #[test]
    fn retirement_requires_initialized_nonpending_nonfailed_output() {
        let failure = Arc::new(AtomicBool::new(false));
        let state = OutputBindingState::new(failure.clone());
        assert!(state.validate_drained().is_err());
        state.publish(OutputBindings::default(), true).unwrap();
        assert!(state.validate_drained().is_err());
        state.pending(false);
        state.validate_drained().unwrap();
        failure.store(true, Ordering::Release);
        assert!(state.validate_drained().is_err());
    }

    fn destination(index: usize) -> OutputDestination {
        OutputDestination {
            output: PortId::try_new("out").unwrap(),
            consumer: ComponentId::try_new(format!("consumer-{index:03}")).unwrap(),
            input: PortId::try_new("in").unwrap(),
            journal: uuid::Uuid::from_u128(1),
            subscriber: format!("subscriber-{index:03}"),
        }
    }

    #[test]
    fn output_bindings_reject_changed_pending_destinations_and_uncertain_owners() {
        let original = OutputBindings::try_new([destination(0)]).unwrap();
        original.validate_change(&original, true).unwrap();
        let mut candidates = vec![OutputBindings::default()];
        for field in 0..5 {
            let mut changed = destination(0);
            match field {
                0 => changed.output = PortId::try_new("different").unwrap(),
                1 => changed.consumer = ComponentId::try_new("different").unwrap(),
                2 => changed.input = PortId::try_new("different").unwrap(),
                3 => changed.journal = uuid::Uuid::from_u128(2),
                4 => changed.subscriber = "different".into(),
                _ => unreachable!(),
            }
            candidates.push(OutputBindings::try_new([changed]).unwrap());
        }
        candidates.push(OutputBindings::try_new([destination(0), destination(1)]).unwrap());
        for candidate in candidates {
            assert_eq!(
                original.validate_change(&candidate, true),
                Err(OutputBindingError::PendingDestinationsChanged)
            );
            original.validate_change(&candidate, false).unwrap();
        }
        assert_eq!(
            OutputBindings::default().validate_change(&original, true),
            Err(OutputBindingError::PendingDestinationsChanged)
        );
        let failure = Arc::new(AtomicBool::new(false));
        let state = OutputBindingState::new(failure.clone());
        state.publish(original.clone(), true).unwrap();
        state.validate(&original).unwrap();
        assert!(state.validate_unresolved().is_err());
        state.pending(false);
        state.validate(&OutputBindings::default()).unwrap();
        failure.store(true, Ordering::Release);
        assert_eq!(
            state.validate(&OutputBindings::default()),
            Err(OutputBindingError::RecoveryRequired)
        );
        assert_eq!(
            state.validate_unresolved(),
            Err(OutputBindingError::RecoveryRequired)
        );
        state.validate(&original).unwrap();
    }

    #[test]
    fn output_bindings_validate_limits_and_persisted_membership() {
        let bindings = OutputBindings::try_new((0..MAX_DESTINATIONS).map(destination)).unwrap();
        bindings.validate_progress(bindings.fingerprint()).unwrap();
        assert!(bindings.validate_progress(None).is_err());
        assert!(OutputBindings::default()
            .validate_progress(bindings.fingerprint())
            .is_err());
        assert_eq!(bindings.fingerprint().unwrap().len(), 32);
        assert_eq!(OutputBindings::default().fingerprint(), None);
        let bytes = bindings.encode().unwrap();
        assert!(bytes.len() <= MAX_BYTES);
        assert_eq!(
            OutputBindings::load(Some(&SourceCheckpoint::new(1, Some(bytes.clone())))).unwrap(),
            bindings
        );
        assert!(OutputBindings::try_new((0..=MAX_DESTINATIONS).map(destination)).is_err());
        let mut longest = destination(0);
        longest.output = PortId::try_new("x".repeat(256)).unwrap();
        longest.consumer = ComponentId::try_new("x".repeat(256)).unwrap();
        longest.input = PortId::try_new("x".repeat(256)).unwrap();
        longest.subscriber = "x".repeat(256);
        OutputBindings::try_new([longest.clone()])
            .unwrap()
            .encode()
            .unwrap();
        longest.subscriber.push('x');
        assert!(OutputBindings::try_new([longest]).is_err());
        for variant in 0..7 {
            let mut record: Record = serde_json::from_slice(&bytes).unwrap();
            match variant {
                0 => record.version = 2,
                1 => record.destinations.reverse(),
                2 => record.destinations[0].journal = uuid::Uuid::nil(),
                3 => record.destinations[0].subscriber = String::new(),
                4 => record.destinations[1] = record.destinations[0].clone(),
                5 => record.destinations[1].subscriber = record.destinations[0].subscriber.clone(),
                6 => record.destinations[1].consumer = record.destinations[0].consumer.clone(),
                _ => unreachable!(),
            }
            let saved = SourceCheckpoint::new(1, Some(serde_json::to_vec(&record).unwrap().into()));
            assert!(OutputBindings::load(Some(&saved)).is_err(), "{variant}");
        }
        assert!(OutputBindings::load(Some(&SourceCheckpoint::new(2, Some(bytes)))).is_err());
        assert!(OutputBindings::load(Some(&SourceCheckpoint::new(1, None))).is_err());
        assert!(OutputBindings::load(Some(&SourceCheckpoint::new(
            1,
            Some(vec![0; MAX_BYTES + 1].into())
        )))
        .is_err());
        assert!(OutputBindings::load(None)
            .unwrap()
            .destinations()
            .is_empty());
    }
}

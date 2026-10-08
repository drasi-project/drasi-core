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
    num::{NonZeroU64, NonZeroUsize},
    sync::{Arc, OnceLock},
    time::Duration,
};

use bincode::Options;
use bytes::Bytes;
use chrono::{DateTime, Utc};
use drasi_core::models::SourceChange;
use serde::{Deserialize, Serialize};

use super::{
    ChangeEnvelope, ChangeOperation, ChangeSet, ChangeSetId, ComponentId, ContextEntry,
    ContextValue, ContractError, Record, RecordId, RecordImage, RecordValidationError,
    RecordValidator, Schema, SchemaDescriptor, SchemaId, SchemaVersion, StreamId, SystemMetadata,
};

const TRANSACTION_CONTEXT: &str = "drasi.source-transaction.v1";
const REPLAY_CONTEXT: &str = "drasi.source-transaction-replay.v1";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplayProgress {
    version: u32,
    source_id: String,
    transaction_id: Vec<u8>,
    sequence: u64,
}

/// Explicit admission bounds for opt-in complete-source-transaction processing.
/// Bytes count the complete binary transaction payload, not transport framing.
/// The pipe's envelope limit is an additional bound, never a fragmentation hint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceTransactionLimits {
    pub max_changes: NonZeroUsize,
    pub max_bytes: NonZeroUsize,
    pub max_duration_ms: NonZeroU64,
}

impl SourceTransactionLimits {
    pub fn duration(self) -> Duration {
        Duration::from_millis(self.max_duration_ms.get())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceTransactionLimit {
    Changes,
    Bytes,
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SourceTransactionError {
    #[error("source transaction exceeds its {limit}-unit {kind:?} limit")]
    Limit {
        kind: SourceTransactionLimit,
        limit: usize,
    },
    #[error("source transaction exceeded its assembly/processing deadline")]
    Deadline,
    #[error("source transaction deadline is not representable")]
    InvalidDeadline,
    #[error("failed transaction assembly cannot publish its partial changes")]
    Interrupted,
    #[error("source transaction identity is empty, oversized or inconsistent")]
    Identity,
    #[error("source transaction requires a nonempty, supported commit position")]
    Position,
    #[error("source transaction contains a change from another source")]
    MixedSources,
    #[error("source transaction cannot contain a scheduled query notification")]
    ScheduledChange,
    #[error("expected one complete source transaction, not a fragment or ordinary change")]
    Envelope,
    #[error(transparent)]
    Contract(#[from] ContractError),
    #[error("invalid source transaction frame: {0}")]
    Encoding(#[from] Box<bincode::ErrorKind>),
    #[error("invalid source transaction context: {0}")]
    Context(#[from] serde_json::Error),
}

#[derive(Serialize, Deserialize)]
struct Frame {
    version: u32,
    source_id: String,
    transaction_id: Vec<u8>,
    position: Vec<u8>,
    changes: Vec<SourceChange>,
}

impl Frame {
    fn validate(&self) -> Result<(), SourceTransactionError> {
        ComponentId::try_new(self.source_id.as_str())?;
        if self.version != 1 || self.transaction_id.is_empty() || self.transaction_id.len() > 256 {
            return Err(SourceTransactionError::Identity);
        }
        if self.position.is_empty()
            || self.position.len() > crate::sources::SourceBase::MAX_SOURCE_POSITION_BYTES
        {
            return Err(SourceTransactionError::Position);
        }
        for change in &self.changes {
            validate_change(&self.source_id, change)?;
        }
        Ok(())
    }
}

fn validate_change(source: &str, change: &SourceChange) -> Result<(), SourceTransactionError> {
    if matches!(change, SourceChange::Future { .. }) {
        return Err(SourceTransactionError::ScheduledChange);
    }
    if change.get_reference().source_id.as_ref() != source {
        return Err(SourceTransactionError::MixedSources);
    }
    Ok(())
}

fn decode_frame(payload: &[u8]) -> Result<Frame, SourceTransactionError> {
    let frame: Frame = bincode::options()
        .with_fixint_encoding()
        .with_limit(payload.len() as u64)
        .reject_trailing_bytes()
        .deserialize(payload)?;
    frame.validate()?;
    Ok(frame)
}

/// An incomplete assembly has no envelope/serialization API. After any rejected
/// change it remains poisoned; restarting from the last committed cursor is
/// required instead of publishing a prefix with a higher limit.
pub struct SourceTransactionBuilder {
    source_id: String,
    changes: Vec<SourceChange>,
    change_bytes: u64,
    limits: SourceTransactionLimits,
    deadline: tokio::time::Instant,
    failed: bool,
}

impl SourceTransactionBuilder {
    pub fn new(
        source_id: &str,
        limits: SourceTransactionLimits,
    ) -> Result<Self, SourceTransactionError> {
        ComponentId::try_new(source_id)?;
        let deadline = tokio::time::Instant::now()
            .checked_add(limits.duration())
            .ok_or(SourceTransactionError::InvalidDeadline)?;
        Ok(Self {
            source_id: source_id.into(),
            changes: Vec::new(),
            change_bytes: 0,
            limits,
            deadline,
            failed: false,
        })
    }

    /// The source must also select on this deadline while waiting for input.
    /// Checking only when another row arrives would leave idle assembly unbounded.
    pub fn deadline(&self) -> tokio::time::Instant {
        self.deadline
    }

    pub fn check_deadline(&mut self) -> Result<(), SourceTransactionError> {
        if self.failed {
            return Err(SourceTransactionError::Interrupted);
        }
        if tokio::time::Instant::now() >= self.deadline {
            self.failed = true;
            return Err(SourceTransactionError::Deadline);
        }
        Ok(())
    }

    pub fn push(&mut self, change: SourceChange) -> Result<(), SourceTransactionError> {
        self.check_deadline()?;
        let result = self.admit(&change);
        if result.is_err() {
            self.failed = true;
        }
        let bytes = result?;
        self.check_deadline()?;
        self.change_bytes = bytes;
        self.changes.push(change);
        Ok(())
    }

    fn admit(&self, change: &SourceChange) -> Result<u64, SourceTransactionError> {
        if self.changes.len() == self.limits.max_changes.get() {
            return Err(SourceTransactionError::Limit {
                kind: SourceTransactionLimit::Changes,
                limit: self.limits.max_changes.get(),
            });
        }
        validate_change(&self.source_id, change)?;
        let bytes = self
            .change_bytes
            .checked_add(bincode::serialized_size(change)?);
        match bytes {
            Some(bytes) if bytes <= self.limits.max_bytes.get() as u64 => Ok(bytes),
            _ => Err(SourceTransactionError::Limit {
                kind: SourceTransactionLimit::Bytes,
                limit: self.limits.max_bytes.get(),
            }),
        }
    }

    /// Call only after the upstream commit is known. Returning a value does not
    /// acknowledge the upstream transaction; the configured completion boundary
    /// owns that decision. Failure consumes the assembly without publishing it.
    pub fn commit(
        self,
        transaction_id: Bytes,
        position: Bytes,
    ) -> Result<SourceTransaction, SourceTransactionError> {
        Ok(self.prepare(transaction_id, position)?.confirm_committed())
    }

    /// Validate and stage a complete group before a source-owned database commit.
    /// The prepared value has no envelope API. Store its bytes in the same
    /// database transaction as the source changes, then confirm only after commit.
    pub fn prepare(
        mut self,
        transaction_id: Bytes,
        position: Bytes,
    ) -> Result<PreparedSourceTransaction, SourceTransactionError> {
        self.check_deadline()?;
        if transaction_id.is_empty() || transaction_id.len() > 256 {
            return Err(SourceTransactionError::Identity);
        }
        if position.is_empty()
            || position.len() > crate::sources::SourceBase::MAX_SOURCE_POSITION_BYTES
        {
            return Err(SourceTransactionError::Position);
        }
        let frame = Frame {
            version: 1,
            source_id: self.source_id,
            transaction_id: transaction_id.to_vec(),
            position: position.to_vec(),
            changes: self.changes,
        };
        frame.validate()?;
        if bincode::serialized_size(&frame)? > self.limits.max_bytes.get() as u64 {
            return Err(SourceTransactionError::Limit {
                kind: SourceTransactionLimit::Bytes,
                limit: self.limits.max_bytes.get(),
            });
        }
        if tokio::time::Instant::now() >= self.deadline {
            return Err(SourceTransactionError::Deadline);
        }
        Ok(PreparedSourceTransaction { frame })
    }
}

/// Inert source-transaction storage, not evidence that the source has committed.
pub struct PreparedSourceTransaction {
    frame: Frame,
}

impl PreparedSourceTransaction {
    pub fn encode(&self) -> Result<Bytes, SourceTransactionError> {
        Ok(Bytes::from(bincode::serialize(&self.frame)?))
    }

    /// Call only after the source transaction containing these changes commits.
    /// The pre-commit admission deadline no longer applies to committed data.
    pub fn confirm_committed(self) -> SourceTransaction {
        SourceTransaction { frame: self.frame }
    }
}

/// One committed, bounded group. The complete group occupies one typed record,
/// so a pipe cannot accidentally deliver its rows as separate source changes.
pub struct SourceTransaction {
    frame: Frame,
}

impl SourceTransaction {
    pub fn source_id(&self) -> &str {
        &self.frame.source_id
    }

    pub fn transaction_id(&self) -> &[u8] {
        &self.frame.transaction_id
    }

    pub fn position(&self) -> &[u8] {
        &self.frame.position
    }

    pub fn into_changes(self) -> Vec<SourceChange> {
        self.frame.changes
    }

    /// Retain the upstream logical position while assigning a fresh transport
    /// sequence on every emission, including replays within the same graph.
    pub fn into_replay_envelope(
        self,
        stream: StreamId,
        source_sequence: u64,
        transport_sequence: u64,
        timestamp: DateTime<Utc>,
    ) -> Result<ChangeEnvelope, SourceTransactionError> {
        if source_sequence == 0 {
            return Err(SourceTransactionError::Identity);
        }
        let progress = ReplayProgress {
            version: 1,
            source_id: self.frame.source_id.clone(),
            transaction_id: self.frame.transaction_id.clone(),
            sequence: source_sequence,
        };
        let mut envelope = self.into_envelope(stream, transport_sequence, timestamp)?;
        envelope.append_annotation(ContextEntry::try_new(
            ComponentId::try_new(progress.source_id.as_str())?,
            REPLAY_CONTEXT,
            ContextValue::Bytes(Arc::from(serde_json::to_vec(&progress)?)),
        )?)?;
        Ok(envelope)
    }

    pub fn into_envelope(
        self,
        stream: StreamId,
        sequence: u64,
        timestamp: DateTime<Utc>,
    ) -> Result<ChangeEnvelope, SourceTransactionError> {
        let schema = SourceTransactionCodec::schema();
        let record = Record::try_new(
            &schema,
            RecordId::try_new(
                self.frame.source_id.as_str(),
                Bytes::copy_from_slice(&self.frame.transaction_id),
            )?,
            RecordImage::Full,
            Bytes::from(bincode::serialize(&self.frame)?),
        )?;
        let changes = ChangeSet::try_new(
            ChangeSetId::try_new(
                stream.as_str(),
                Bytes::copy_from_slice(&sequence.to_be_bytes()),
            )?,
            schema.descriptor().clone(),
            vec![ChangeOperation::Added {
                ordinal: 0,
                after: record,
            }],
        )?;
        let mut envelope = ChangeEnvelope::new(
            super::emission_id(&stream, sequence)?,
            changes,
            SystemMetadata::new(stream, sequence)
                .with_timestamp(timestamp)
                .with_source_position(Bytes::from(self.frame.position)),
        );
        envelope.append_annotation(ContextEntry::try_new(
            ComponentId::try_new(self.frame.source_id.as_str())?,
            TRANSACTION_CONTEXT,
            ContextValue::Bytes(Arc::from(serde_json::to_vec(&(
                1u32,
                &self.frame.source_id,
                &self.frame.transaction_id,
            ))?)),
        )?)?;
        Ok(envelope)
    }
}

pub struct SourceTransactionCodec;

impl SourceTransactionCodec {
    /// Restore bytes read from a committed source-owned transaction journal.
    /// The caller, not the codec, must establish the journal's commit boundary.
    pub fn decode_committed(
        payload: &[u8],
        limits: SourceTransactionLimits,
    ) -> Result<SourceTransaction, SourceTransactionError> {
        if payload.len() > limits.max_bytes.get() {
            return Err(SourceTransactionError::Limit {
                kind: SourceTransactionLimit::Bytes,
                limit: limits.max_bytes.get(),
            });
        }
        let frame = decode_frame(payload)?;
        if frame.changes.len() > limits.max_changes.get() {
            return Err(SourceTransactionError::Limit {
                kind: SourceTransactionLimit::Changes,
                limit: limits.max_changes.get(),
            });
        }
        Ok(SourceTransaction { frame })
    }

    pub fn schema() -> Arc<Schema> {
        static SCHEMA: OnceLock<Arc<Schema>> = OnceLock::new();
        SCHEMA.get_or_init(|| Arc::new(Schema::new(
            SchemaDescriptor::try_new(
                SchemaId::try_new("drasi.source-transaction").expect("schema id"),
                SchemaVersion::try_new(1).expect("schema version"),
                "drasi-source-transaction-bincode1",
                Bytes::from_static(b"CommittedSourceTransaction-v1;source-id;transaction-id;commit-position;ordered-SourceChange-v1;one-full-record"),
            ).expect("transaction schema"),
            Arc::new(TransactionValidator),
        ))).clone()
    }

    pub fn decode(
        envelope: &ChangeEnvelope,
        limits: SourceTransactionLimits,
    ) -> Result<SourceTransaction, SourceTransactionError> {
        super::data::validate_schema(Self::schema().descriptor(), envelope.changes().schema())?;
        let [ChangeOperation::Added { ordinal: 0, after }] = envelope.changes().operations() else {
            return Err(SourceTransactionError::Envelope);
        };
        let SourceTransaction { frame } = Self::decode_committed(after.payload(), limits)?;
        if after.identity().namespace() != frame.source_id
            || after.identity().value().as_ref() != frame.transaction_id
        {
            return Err(SourceTransactionError::Identity);
        }
        if envelope.system().source_position().map(Bytes::as_ref) != Some(frame.position.as_slice())
        {
            return Err(SourceTransactionError::Position);
        }
        let expected = serde_json::to_vec(&(1u32, &frame.source_id, &frame.transaction_id))?;
        let mut entries = envelope
            .annotations()
            .entries()
            .filter(|entry| entry.key() == TRANSACTION_CONTEXT);
        let valid = entries.next().is_some_and(|entry| {
            entry.contributor() == frame.source_id
                && matches!(entry.value(), ContextValue::Bytes(bytes) if bytes.as_ref() == expected)
        });
        if !valid || entries.next().is_some() {
            return Err(SourceTransactionError::Identity);
        }
        Self::replay_progress(envelope)?;
        Ok(SourceTransaction { frame })
    }

    pub(super) fn replay_progress(
        envelope: &ChangeEnvelope,
    ) -> Result<Option<(String, u64)>, SourceTransactionError> {
        if envelope.changes().schema().id().as_str() != "drasi.source-transaction" {
            return Ok(None);
        }
        let mut entries = envelope
            .annotations()
            .entries()
            .filter(|entry| entry.key() == REPLAY_CONTEXT);
        let Some(entry) = entries.next() else {
            return Ok(None);
        };
        if entries.next().is_some() {
            return Err(SourceTransactionError::Identity);
        }
        super::data::validate_schema(Self::schema().descriptor(), envelope.changes().schema())?;
        let ContextValue::Bytes(bytes) = entry.value() else {
            return Err(SourceTransactionError::Identity);
        };
        let progress: ReplayProgress = serde_json::from_slice(&bytes)?;
        let [ChangeOperation::Added { ordinal: 0, after }] = envelope.changes().operations() else {
            return Err(SourceTransactionError::Envelope);
        };
        if progress.version != 1
            || progress.sequence == 0
            || entry.contributor() != progress.source_id
            || after.identity().namespace() != progress.source_id
            || after.identity().value().as_ref() != progress.transaction_id
        {
            return Err(SourceTransactionError::Identity);
        }
        Ok(Some((progress.source_id, progress.sequence)))
    }
}

struct TransactionValidator;

impl RecordValidator for TransactionValidator {
    fn validate_identity(
        &self,
        schema: &SchemaDescriptor,
        identity: &RecordId,
    ) -> Result<(), RecordValidationError> {
        if schema != SourceTransactionCodec::schema().descriptor()
            || ComponentId::try_new(identity.namespace()).is_err()
            || identity.value().is_empty()
            || identity.value().len() > 256
        {
            return Err(RecordValidationError::new(
                "identity",
                "invalid source transaction identity",
            ));
        }
        Ok(())
    }

    fn validate(
        &self,
        schema: &SchemaDescriptor,
        identity: &RecordId,
        image: RecordImage,
        payload: &[u8],
    ) -> Result<(), RecordValidationError> {
        self.validate_identity(schema, identity)?;
        let frame = decode_frame(payload)
            .map_err(|error| RecordValidationError::new("transaction", error.to_string()))?;
        if image != RecordImage::Full
            || identity.namespace() != frame.source_id
            || identity.value().as_ref() != frame.transaction_id
        {
            return Err(RecordValidationError::new(
                "transaction",
                "transaction identity or image mismatch",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use drasi_core::models::{Element, ElementMetadata, ElementPropertyMap, ElementReference};

    fn limits(changes: usize, bytes: usize) -> SourceTransactionLimits {
        SourceTransactionLimits {
            max_changes: NonZeroUsize::new(changes).unwrap(),
            max_bytes: NonZeroUsize::new(bytes).unwrap(),
            max_duration_ms: NonZeroU64::new(10_000).unwrap(),
        }
    }

    fn change(source: &str, id: &str) -> SourceChange {
        SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new(source, id),
                    labels: Arc::from([Arc::from("Person")]),
                    effective_from: 1000,
                },
                properties: ElementPropertyMap::new(),
            },
        }
    }

    fn builder(limits: SourceTransactionLimits, count: usize) -> SourceTransactionBuilder {
        let mut builder = SourceTransactionBuilder::new("source", limits).unwrap();
        for index in 0..count {
            builder.push(change("source", &index.to_string())).unwrap();
        }
        builder
    }

    fn commit(
        builder: SourceTransactionBuilder,
    ) -> Result<SourceTransaction, SourceTransactionError> {
        builder.commit(
            Bytes::from_static(b"transaction"),
            Bytes::from_static(b"commit-position"),
        )
    }

    fn envelope(count: usize) -> ChangeEnvelope {
        commit(builder(limits(10, 4096), count))
            .unwrap()
            .into_envelope(
                StreamId::try_new("source-output").unwrap(),
                7,
                DateTime::from_timestamp_millis(1000).unwrap(),
            )
            .unwrap()
    }

    #[tokio::test]
    async fn prepared_storage_is_bounded_and_restores_only_as_a_complete_committed_group() {
        let prepared = builder(limits(2, 4096), 2)
            .prepare(
                Bytes::from_static(b"transaction"),
                Bytes::from_static(b"position"),
            )
            .unwrap();
        let bytes = prepared.encode().unwrap();
        assert!(SourceTransactionCodec::decode_committed(&bytes, limits(1, 4096)).is_err());
        assert!(
            SourceTransactionCodec::decode_committed(&bytes, limits(2, bytes.len() - 1)).is_err()
        );
        let restored =
            SourceTransactionCodec::decode_committed(&bytes, limits(2, bytes.len())).unwrap();
        assert_eq!(restored.transaction_id(), b"transaction");
        assert_eq!(restored.position(), b"position");
        assert_eq!(restored.into_changes().len(), 2);
        assert_eq!(prepared.confirm_committed().into_changes().len(), 2);
        let mut malformed = bytes.to_vec();
        malformed.push(0);
        assert!(SourceTransactionCodec::decode_committed(&malformed, limits(2, 4096)).is_err());
    }

    #[tokio::test]
    async fn transaction_replay_context_binds_logical_identity_without_reusing_transport_sequence()
    {
        let replay = |sequence| {
            commit(builder(limits(2, 4096), 1))
                .unwrap()
                .into_replay_envelope(
                    StreamId::try_new("stream").unwrap(),
                    42,
                    sequence,
                    DateTime::from_timestamp_millis(1000).unwrap(),
                )
                .unwrap()
        };
        for sequence in [1, 2] {
            let envelope = replay(sequence);
            SourceTransactionCodec::decode(&envelope, limits(2, 4096)).unwrap();
            let progress =
                super::super::producer_progress::GraphInputProgress::from_envelope(&envelope)
                    .unwrap();
            assert_eq!(progress.sequence, 42);
            assert_eq!(
                progress.identity,
                super::super::SourceProgressKey::Source("source".into())
            );
            assert_eq!(
                progress.position.as_deref(),
                Some(b"commit-position".as_slice())
            );
            assert_eq!(envelope.system().sequence(), sequence);
        }
        let mut duplicate = replay(3);
        let entry = duplicate
            .annotations()
            .entries()
            .find(|entry| entry.key() == REPLAY_CONTEXT)
            .unwrap();
        let extra = ContextEntry::try_new(
            ComponentId::try_new("source").unwrap(),
            REPLAY_CONTEXT,
            entry.value(),
        )
        .unwrap();
        duplicate.append_annotation(extra).unwrap();
        assert!(SourceTransactionCodec::decode(&duplicate, limits(2, 4096)).is_err());
        for (source, transaction, sequence) in [
            ("wrong", b"transaction".as_slice(), 42),
            ("source", b"wrong".as_slice(), 42),
            ("source", b"transaction".as_slice(), 0),
        ] {
            let mut envelope = envelope(1);
            envelope
                .append_annotation(
                    ContextEntry::try_new(
                        ComponentId::try_new("source").unwrap(),
                        REPLAY_CONTEXT,
                        ContextValue::Bytes(Arc::from(
                            serde_json::to_vec(&ReplayProgress {
                                version: 1,
                                source_id: source.into(),
                                transaction_id: transaction.to_vec(),
                                sequence,
                            })
                            .unwrap(),
                        )),
                    )
                    .unwrap(),
                )
                .unwrap();
            assert!(SourceTransactionCodec::decode(&envelope, limits(2, 4096)).is_err());
        }
    }

    #[tokio::test]
    async fn complete_frame_preserves_identity_position_context_and_order_through_storage() {
        let input = envelope(3);
        let mut codec = super::super::EnvelopeCodec::new(NonZeroUsize::new(16384).unwrap());
        codec
            .register_schema(SourceTransactionCodec::schema())
            .unwrap();
        let bytes = codec.encode(&input).unwrap();
        let restored = codec.decode(&bytes).unwrap();
        assert_eq!(codec.encode(&restored).unwrap(), bytes);
        let transaction = SourceTransactionCodec::decode(&restored, limits(3, 4096)).unwrap();
        assert_eq!(transaction.source_id(), "source");
        assert_eq!(transaction.transaction_id(), b"transaction");
        assert_eq!(transaction.position(), b"commit-position");
        assert_eq!(
            bincode::serialize(&transaction.into_changes()).unwrap(),
            bincode::serialize(
                &(0..3)
                    .map(|n| change("source", &n.to_string()))
                    .collect::<Vec<_>>()
            )
            .unwrap()
        );
        assert!(super::super::GraphChangeCodec::decode_changes(&restored).is_err());
        assert_eq!(restored.changes().operations().len(), 1);
        assert_eq!(restored.annotations().entries().count(), 1);
        let empty = SourceTransactionCodec::decode(&envelope(0), limits(1, 4096)).unwrap();
        assert!(
            empty.into_changes().is_empty(),
            "a committed empty group still advances its position"
        );
        let mut binary = super::super::BinaryEnvelopeCodec::new(NonZeroUsize::new(16384).unwrap());
        binary
            .register_schema(SourceTransactionCodec::schema())
            .unwrap();
        let wire = binary.encode(&input).unwrap();
        let transferred = binary.decode(&wire).unwrap();
        assert_eq!(binary.encode(&transferred).unwrap(), wire);
        assert_eq!(
            SourceTransactionCodec::decode(&transferred, limits(3, 4096))
                .unwrap()
                .into_changes()
                .len(),
            3
        );
        assert!(binary.decode(&wire[..wire.len() - 1]).is_err());
    }

    #[tokio::test]
    async fn exact_change_limit_rejects_and_poisoned_assembly_cannot_publish_a_prefix() {
        let mut assembly = builder(limits(2, 4096), 2);
        assert!(matches!(
            assembly.push(change("source", "third")),
            Err(SourceTransactionError::Limit {
                kind: SourceTransactionLimit::Changes,
                limit: 2,
            })
        ));
        assert!(matches!(
            assembly.push(change("source", "later")),
            Err(SourceTransactionError::Interrupted)
        ));
        assert!(matches!(
            commit(assembly),
            Err(SourceTransactionError::Interrupted)
        ));
        let retry = commit(builder(limits(3, 4096), 3)).unwrap();
        assert_eq!(retry.into_changes().len(), 3);
        assert!(matches!(
            SourceTransactionCodec::decode(&envelope(3), limits(2, 4096)),
            Err(SourceTransactionError::Limit {
                kind: SourceTransactionLimit::Changes,
                limit: 2
            })
        ));
    }

    #[tokio::test]
    async fn byte_limit_includes_metadata_and_accepts_the_exact_boundary() {
        let input = envelope(1);
        let [ChangeOperation::Added { after, .. }] = input.changes().operations() else {
            panic!("one transaction record");
        };
        let size = after.payload().len();
        assert!(commit(builder(limits(1, size), 1)).is_ok());
        assert!(SourceTransactionCodec::decode(&input, limits(1, size)).is_ok());
        assert!(matches!(
            commit(builder(limits(1, size - 1), 1)),
            Err(SourceTransactionError::Limit { kind: SourceTransactionLimit::Bytes, limit }) if limit == size - 1
        ));
        assert!(matches!(
            SourceTransactionCodec::decode(&input, limits(1, size - 1)),
            Err(SourceTransactionError::Limit { kind: SourceTransactionLimit::Bytes, limit }) if limit == size - 1
        ));
        let mut tiny = builder(limits(1, 1), 0);
        assert!(matches!(
            tiny.push(change("source", "large")),
            Err(SourceTransactionError::Limit {
                kind: SourceTransactionLimit::Bytes,
                limit: 1,
            })
        ));
        assert!(matches!(
            commit(tiny),
            Err(SourceTransactionError::Interrupted)
        ));
    }

    #[tokio::test]
    async fn assembly_deadline_rejects_idle_and_nonempty_partial_groups() {
        for count in [0, 1] {
            let mut bounded = limits(2, 4096);
            bounded.max_duration_ms = NonZeroU64::new(5).unwrap();
            let mut assembly = builder(bounded, count);
            tokio::time::sleep_until(assembly.deadline()).await;
            assert!(matches!(
                assembly.check_deadline(),
                Err(SourceTransactionError::Deadline)
            ));
            assert!(matches!(
                commit(assembly),
                Err(SourceTransactionError::Interrupted)
            ));
        }
        let mut bounded = limits(1, 4096);
        bounded.max_duration_ms = NonZeroU64::new(5).unwrap();
        let assembly = builder(bounded, 1);
        tokio::time::sleep_until(assembly.deadline()).await;
        assert!(matches!(
            commit(assembly),
            Err(SourceTransactionError::Deadline)
        ));
    }

    #[tokio::test]
    async fn mixed_sources_and_invalid_commit_metadata_never_create_a_complete_group() {
        let mut assembly = builder(limits(2, 4096), 1);
        assert!(matches!(
            assembly.push(change("another", "1")),
            Err(SourceTransactionError::MixedSources)
        ));
        assert!(matches!(
            commit(assembly),
            Err(SourceTransactionError::Interrupted)
        ));
        let mut scheduled = builder(limits(1, 4096), 0);
        assert!(matches!(
            scheduled.push(SourceChange::Future {
                future_ref: drasi_core::interface::FutureElementRef {
                    element_ref: ElementReference::new("source", "one"),
                    original_time: 1000,
                    due_time: 2000,
                    group_signature: 1,
                },
            }),
            Err(SourceTransactionError::ScheduledChange)
        ));
        assert!(matches!(
            commit(scheduled),
            Err(SourceTransactionError::Interrupted)
        ));
        for identity in [Bytes::new(), Bytes::from(vec![1; 257])] {
            assert!(matches!(
                builder(limits(1, 4096), 1).commit(identity, Bytes::from_static(b"position")),
                Err(SourceTransactionError::Identity)
            ));
        }
        for position in [
            Bytes::new(),
            Bytes::from(vec![
                1;
                crate::sources::SourceBase::MAX_SOURCE_POSITION_BYTES
                    + 1
            ]),
        ] {
            assert!(matches!(
                builder(limits(1, 16384), 1).commit(Bytes::from_static(b"tx"), position),
                Err(SourceTransactionError::Position)
            ));
        }
    }

    #[tokio::test]
    async fn truncated_changed_or_fragmented_records_are_not_committed_groups() {
        let input = envelope(1);
        let [ChangeOperation::Added { after, .. }] = input.changes().operations() else {
            panic!("one transaction record");
        };
        let schema = SourceTransactionCodec::schema();
        let payload = after.payload();
        for length in [0, 1, payload.len() - 1] {
            assert!(Record::try_new(
                &schema,
                after.identity().clone(),
                RecordImage::Full,
                payload.slice(..length),
            )
            .is_err());
        }
        let mut wrong = decode_frame(payload).unwrap();
        wrong.version = 2;
        assert!(Record::try_new(
            &schema,
            after.identity().clone(),
            RecordImage::Full,
            Bytes::from(bincode::serialize(&wrong).unwrap()),
        )
        .is_err());
        assert!(Record::try_new(
            &schema,
            RecordId::try_new("source", Bytes::from_static(b"wrong-id")).unwrap(),
            RecordImage::Full,
            payload.clone(),
        )
        .is_err());
        let fragmented = ChangeSet::try_new(
            input.changes().id().clone(),
            schema.descriptor().clone(),
            vec![
                ChangeOperation::Added {
                    ordinal: 0,
                    after: after.clone(),
                },
                ChangeOperation::Added {
                    ordinal: 1,
                    after: after.clone(),
                },
            ],
        )
        .unwrap();
        let fragmented = input.derive(
            input.id().clone(),
            fragmented,
            input.system().as_ref().clone(),
        );
        assert!(matches!(
            SourceTransactionCodec::decode(&fragmented, limits(2, 4096)),
            Err(SourceTransactionError::Envelope)
        ));
    }

    #[tokio::test]
    async fn commit_position_and_context_cannot_be_rebound_or_duplicated() {
        let input = envelope(1);
        let changed = input.derive(
            input.id().clone(),
            input.changes().clone(),
            input
                .system()
                .as_ref()
                .clone()
                .with_source_position(Bytes::from_static(b"wrong-position")),
        );
        assert!(matches!(
            SourceTransactionCodec::decode(&changed, limits(1, 4096)),
            Err(SourceTransactionError::Position)
        ));
        let missing = ChangeEnvelope::new(
            input.id().clone(),
            input.changes().clone(),
            input.system().as_ref().clone(),
        );
        assert!(matches!(
            SourceTransactionCodec::decode(&missing, limits(1, 4096)),
            Err(SourceTransactionError::Identity)
        ));
        let mut duplicate = input.clone();
        duplicate
            .append_annotation(input.annotations().entries().next().unwrap())
            .unwrap();
        assert!(matches!(
            SourceTransactionCodec::decode(&duplicate, limits(1, 4096)),
            Err(SourceTransactionError::Identity)
        ));
    }

    #[test]
    fn limits_require_positive_explicit_values_and_round_trip_configuration() {
        let settings = limits(2, 4096);
        let valid = serde_json::to_value(settings).unwrap();
        assert_eq!(
            serde_json::from_value::<SourceTransactionLimits>(valid.clone()).unwrap(),
            settings
        );
        for key in ["max_changes", "max_bytes", "max_duration_ms"] {
            let mut invalid = valid.clone();
            invalid[key] = serde_json::json!(0);
            assert!(serde_json::from_value::<SourceTransactionLimits>(invalid).is_err());
        }
        assert!(serde_json::from_value::<SourceTransactionLimits>(serde_json::json!({})).is_err());
    }
}

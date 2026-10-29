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

//! A complete source transaction is ordinary graph data: its operations are the
//! standard `GraphChangeCodec` records. Completeness is a separate, scoped
//! annotation bound to the envelope's own logical `ChangeSetId`, exactly like a
//! query projection. The graph schema alone never means "complete transaction".

use std::{
    num::{NonZeroU64, NonZeroUsize},
    sync::Arc,
    time::Duration,
};

use bincode::Options;
use bytes::Bytes;
use chrono::{DateTime, Utc};
use drasi_core_types::models::SourceChange;
use serde::{Deserialize, Serialize};

use super::{
    ChangeEnvelope, ChangeOperation, ChangeSet, ChangeSetId, ComponentId, ContextEntry,
    ContextValue, ContractError, GraphChangeCodec, GraphCodecError, Record, RecordId, RecordImage,
    Schema, StreamId, SystemMetadata, UpdateSemantics,
};

const TRANSACTION_CONTEXT: &str = "drasi.source-transaction.v1";
const REPLAY_CONTEXT: &str = "drasi.source-transaction-replay.v1";
const CONTEXT_VERSION: u32 = 1;

/// Fixed bincode framing of the private prepared-storage encoding: the version,
/// three length prefixes and the operation count, then a tag and two length
/// prefixes per operation. Record and metadata bytes are counted separately.
const STORED_FRAME_OVERHEAD: usize = 36;
const STORED_OPERATION_OVERHEAD: usize = 20;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplayProgress {
    version: u32,
    source_id: String,
    transaction_id: Vec<u8>,
    sequence: u64,
}

/// The explicit completion marker. It is valid only for the logical change set
/// named here; an annotation inherited by any other change set is history.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TransactionContext {
    version: u32,
    change_set_namespace: String,
    #[serde(with = "hex")]
    change_set_value: Vec<u8>,
    source_id: String,
    #[serde(with = "hex")]
    transaction_id: Vec<u8>,
    #[serde(with = "hex")]
    position: Vec<u8>,
    operations: u64,
    complete: bool,
}

/// Just enough of a marker to decide whether it belongs to the current change set.
#[derive(Deserialize)]
struct ContextScope {
    change_set_namespace: String,
    #[serde(with = "hex")]
    change_set_value: Vec<u8>,
}

impl ContextScope {
    fn is_current(&self, id: &ChangeSetId) -> bool {
        self.change_set_namespace == id.namespace() && self.change_set_value == id.value()[..]
    }
}

mod hex {
    use serde::{de::Error, Deserialize, Deserializer, Serializer};

    #[allow(clippy::ptr_arg)]
    pub fn serialize<S: Serializer>(bytes: &Vec<u8>, serializer: S) -> Result<S::Ok, S::Error> {
        const DIGITS: &[u8; 16] = b"0123456789abcdef";
        let mut text = String::with_capacity(bytes.len() * 2);
        for byte in bytes {
            text.push(DIGITS[usize::from(byte >> 4)] as char);
            text.push(DIGITS[usize::from(byte & 15)] as char);
        }
        serializer.serialize_str(&text)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
        fn nibble<E: Error>(digit: u8) -> Result<u8, E> {
            match digit {
                b'0'..=b'9' => Ok(digit - b'0'),
                b'a'..=b'f' => Ok(digit - b'a' + 10),
                _ => Err(E::custom("expected lowercase hexadecimal")),
            }
        }
        let text = String::deserialize(deserializer)?;
        if text.len() % 2 != 0 {
            return Err(D::Error::custom("odd hexadecimal length"));
        }
        text.as_bytes()
            .chunks(2)
            .map(|pair| Ok(nibble::<D::Error>(pair[0])? << 4 | nibble::<D::Error>(pair[1])?))
            .collect()
    }
}

/// Explicit admission bounds for opt-in complete-source-transaction processing.
/// Bytes count the complete graph wire data: the identity and payload of every
/// operation record plus the source id, transaction id and commit position.
/// Neither transport framing nor the private prepared-storage framing counts.
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
    #[error("source transaction contains a malformed change: {0}")]
    InvalidChange(#[from] drasi_core_types::models::SourceChangeError),
    #[error("expected one complete source transaction, not a fragment or control event")]
    Envelope,
    #[error("the envelope has no current complete source transaction marker")]
    Unmarked,
    #[error(transparent)]
    Contract(#[from] ContractError),
    #[error("invalid source transaction graph data: {0}")]
    Graph(Box<GraphCodecError>),
    #[error("invalid source transaction frame: {0}")]
    Encoding(#[from] Box<bincode::ErrorKind>),
    #[error("invalid source transaction context: {0}")]
    Context(#[from] serde_json::Error),
}

impl From<GraphCodecError> for SourceTransactionError {
    fn from(error: GraphCodecError) -> Self {
        Self::Graph(Box::new(error))
    }
}

fn validate_change(source: &str, change: &SourceChange) -> Result<(), SourceTransactionError> {
    if matches!(change, SourceChange::Future { .. }) {
        return Err(SourceTransactionError::ScheduledChange);
    }
    if change.get_reference().source_id.as_ref() != source {
        return Err(SourceTransactionError::MixedSources);
    }
    change.validate()?;
    Ok(())
}

fn validate_identity(
    source_id: &str,
    transaction_id: &[u8],
    position: &[u8],
) -> Result<(), SourceTransactionError> {
    ComponentId::try_new(source_id)?;
    if transaction_id.is_empty() || transaction_id.len() > 256 {
        return Err(SourceTransactionError::Identity);
    }
    if position.is_empty() || position.len() > crate::MAX_SOURCE_POSITION_BYTES {
        return Err(SourceTransactionError::Position);
    }
    Ok(())
}

fn record_bytes(record: &Record) -> u64 {
    (record.identity().value().len() + record.payload().len()) as u64
}

fn operation_bytes(operation: &ChangeOperation) -> u64 {
    match operation {
        ChangeOperation::Added { after, .. } => record_bytes(after),
        ChangeOperation::Updated { before, after, .. } => {
            before.as_ref().map_or(0, record_bytes) + record_bytes(after)
        }
        ChangeOperation::Deleted {
            identity, before, ..
        } => identity.identity().value().len() as u64 + before.as_ref().map_or(0, record_bytes),
    }
}

fn metadata_bytes(source_id: &str, transaction_id: &[u8], position: &[u8]) -> u64 {
    (source_id.len() + transaction_id.len() + position.len()) as u64
}

/// The complete group as validated graph operations plus its decoded changes.
struct Group {
    source_id: String,
    transaction_id: Bytes,
    position: Bytes,
    changes: Vec<SourceChange>,
    operations: Vec<ChangeOperation>,
}

impl Group {
    fn wire_bytes(&self) -> u64 {
        self.operations.iter().map(operation_bytes).fold(
            metadata_bytes(&self.source_id, &self.transaction_id, &self.position),
            u64::saturating_add,
        )
    }

    fn check_limits(&self, limits: SourceTransactionLimits) -> Result<(), SourceTransactionError> {
        if self.operations.len() > limits.max_changes.get() {
            return Err(SourceTransactionError::Limit {
                kind: SourceTransactionLimit::Changes,
                limit: limits.max_changes.get(),
            });
        }
        if self.wire_bytes() > limits.max_bytes.get() as u64 {
            return Err(SourceTransactionError::Limit {
                kind: SourceTransactionLimit::Bytes,
                limit: limits.max_bytes.get(),
            });
        }
        Ok(())
    }
}

/// Private prepared-storage encoding. It carries the same graph records the wire
/// carries, with the image implied by the operation kind, so restoring a group
/// passes through the standard graph record validator rather than a second schema.
#[derive(Serialize, Deserialize)]
struct StoredFrame {
    version: u32,
    source_id: String,
    transaction_id: Vec<u8>,
    position: Vec<u8>,
    operations: Vec<StoredOperation>,
}

#[derive(Serialize, Deserialize)]
enum StoredOperation {
    Added(StoredRecord),
    Replaced(StoredRecord),
    Patched(StoredRecord),
    Deleted(StoredRecord),
}

#[derive(Serialize, Deserialize)]
struct StoredRecord {
    identity: Vec<u8>,
    payload: Vec<u8>,
}

impl StoredOperation {
    fn from_operation(operation: &ChangeOperation) -> Result<Self, SourceTransactionError> {
        let stored = |record: &Record| StoredRecord {
            identity: record.identity().value().to_vec(),
            payload: record.payload().to_vec(),
        };
        Ok(match operation {
            ChangeOperation::Added { after, .. } => Self::Added(stored(after)),
            ChangeOperation::Updated {
                before: None,
                after,
                semantics: UpdateSemantics::Replace,
                ..
            } => Self::Replaced(stored(after)),
            ChangeOperation::Updated {
                before: None,
                after,
                semantics: UpdateSemantics::Patch,
                ..
            } => Self::Patched(stored(after)),
            ChangeOperation::Deleted {
                before: Some(before),
                ..
            } => Self::Deleted(stored(before)),
            _ => return Err(SourceTransactionError::Envelope),
        })
    }

    fn into_operation(self, ordinal: u64) -> Result<ChangeOperation, SourceTransactionError> {
        let schema = GraphChangeCodec::schema();
        let record = |stored: StoredRecord, image| {
            Record::try_new(
                &schema,
                RecordId::try_new(
                    crate::graph_codec::GRAPH_IDENTITY,
                    Bytes::from(stored.identity),
                )?,
                image,
                Bytes::from(stored.payload),
            )
        };
        Ok(match self {
            Self::Added(stored) => ChangeOperation::Added {
                ordinal,
                after: record(stored, RecordImage::Full)?,
            },
            Self::Replaced(stored) => ChangeOperation::Updated {
                ordinal,
                before: None,
                after: record(stored, RecordImage::Full)?,
                semantics: UpdateSemantics::Replace,
            },
            Self::Patched(stored) => ChangeOperation::Updated {
                ordinal,
                before: None,
                after: record(stored, RecordImage::Patch)?,
                semantics: UpdateSemantics::Patch,
            },
            Self::Deleted(stored) => {
                let before = record(stored, RecordImage::Partial)?;
                ChangeOperation::Deleted {
                    ordinal,
                    identity: before.reference().clone(),
                    before: Some(before),
                }
            }
        })
    }
}

/// Decode ordered operations into changes and validate each one for `source`.
/// `event_time` supplies the time identity-only deletions need, if any.
fn changes_from_operations(
    source: &str,
    operations: &[ChangeOperation],
    event_time: &dyn Fn() -> Result<u64, GraphCodecError>,
) -> Result<Vec<SourceChange>, SourceTransactionError> {
    let changes: Vec<SourceChange> = GraphChangeCodec::decode_operations(operations, event_time)?
        .into_iter()
        .map(|(_, change)| change)
        .collect();
    for change in &changes {
        validate_change(source, change)?;
    }
    Ok(changes)
}

/// An incomplete assembly has no envelope/serialization API. After any rejected
/// change it remains poisoned; restarting from the last committed cursor is
/// required instead of publishing a prefix with a higher limit.
pub struct SourceTransactionBuilder {
    source_id: String,
    changes: Vec<SourceChange>,
    operations: Vec<ChangeOperation>,
    operation_bytes: u64,
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
            operations: Vec::new(),
            operation_bytes: 0,
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
        let (operation, bytes) = result?;
        self.check_deadline()?;
        self.operation_bytes = bytes;
        self.operations.push(operation);
        self.changes.push(change);
        Ok(())
    }

    fn admit(
        &self,
        change: &SourceChange,
    ) -> Result<(ChangeOperation, u64), SourceTransactionError> {
        if self.changes.len() == self.limits.max_changes.get() {
            return Err(SourceTransactionError::Limit {
                kind: SourceTransactionLimit::Changes,
                limit: self.limits.max_changes.get(),
            });
        }
        validate_change(&self.source_id, change)?;
        let operation = GraphChangeCodec::encode_operation(self.changes.len() as u64, change)?;
        let total = self
            .operation_bytes
            .saturating_add(operation_bytes(&operation));
        if total.saturating_add(self.source_id.len() as u64) > self.limits.max_bytes.get() as u64 {
            return Err(SourceTransactionError::Limit {
                kind: SourceTransactionLimit::Bytes,
                limit: self.limits.max_bytes.get(),
            });
        }
        Ok((operation, total))
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
        validate_identity(&self.source_id, &transaction_id, &position)?;
        let group = Group {
            source_id: self.source_id,
            transaction_id,
            position,
            changes: self.changes,
            operations: self.operations,
        };
        group.check_limits(self.limits)?;
        if tokio::time::Instant::now() >= self.deadline {
            return Err(SourceTransactionError::Deadline);
        }
        Ok(PreparedSourceTransaction { group })
    }
}

/// Inert source-transaction storage, not evidence that the source has committed.
pub struct PreparedSourceTransaction {
    group: Group,
}

impl PreparedSourceTransaction {
    pub fn encode(&self) -> Result<Bytes, SourceTransactionError> {
        let frame = StoredFrame {
            version: CONTEXT_VERSION,
            source_id: self.group.source_id.clone(),
            transaction_id: self.group.transaction_id.to_vec(),
            position: self.group.position.to_vec(),
            operations: self
                .group
                .operations
                .iter()
                .map(StoredOperation::from_operation)
                .collect::<Result<_, _>>()?,
        };
        Ok(Bytes::from(bincode::serialize(&frame)?))
    }

    /// Call only after the source transaction containing these changes commits.
    /// The pre-commit admission deadline no longer applies to committed data.
    pub fn confirm_committed(self) -> SourceTransaction {
        SourceTransaction { group: self.group }
    }
}

/// One committed, bounded group. Its operations are ordinary graph records in
/// one change set, and only an envelope carrying the scoped completion marker
/// is a complete transaction.
pub struct SourceTransaction {
    group: Group,
}

impl SourceTransaction {
    pub fn source_id(&self) -> &str {
        &self.group.source_id
    }

    pub fn transaction_id(&self) -> &[u8] {
        &self.group.transaction_id
    }

    pub fn position(&self) -> &[u8] {
        &self.group.position
    }

    pub fn into_changes(self) -> Vec<SourceChange> {
        self.group.changes
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
            version: CONTEXT_VERSION,
            source_id: self.group.source_id.clone(),
            transaction_id: self.group.transaction_id.to_vec(),
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

    /// The only constructor of a completion marker: the whole validated group
    /// becomes one standard graph change set, marked for that change set alone.
    pub fn into_envelope(
        self,
        stream: StreamId,
        sequence: u64,
        timestamp: DateTime<Utc>,
    ) -> Result<ChangeEnvelope, SourceTransactionError> {
        let Group {
            source_id,
            transaction_id,
            position,
            operations,
            ..
        } = self.group;
        let id = ChangeSetId::try_new(
            stream.as_str(),
            Bytes::copy_from_slice(&sequence.to_be_bytes()),
        )?;
        let context = TransactionContext {
            version: CONTEXT_VERSION,
            change_set_namespace: id.namespace().to_owned(),
            change_set_value: id.value().to_vec(),
            source_id,
            transaction_id: transaction_id.to_vec(),
            position: position.to_vec(),
            operations: operations.len() as u64,
            complete: true,
        };
        let changes = ChangeSet::try_new(
            id,
            GraphChangeCodec::schema().descriptor().clone(),
            operations,
        )?;
        let mut envelope = ChangeEnvelope::new(
            super::emission_id(&stream, sequence)?,
            changes,
            SystemMetadata::new(stream, sequence)
                .with_timestamp(timestamp)
                .with_source_position(position),
        );
        envelope.append_annotation(ContextEntry::try_new(
            ComponentId::try_new(context.source_id.as_str())?,
            TRANSACTION_CONTEXT,
            ContextValue::Bytes(Arc::from(serde_json::to_vec(&context)?)),
        )?)?;
        Ok(envelope)
    }
}

/// Identity of a current, valid complete-transaction marker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceTransactionMetadata {
    source_id: String,
    transaction_id: Bytes,
    position: Bytes,
    operations: usize,
}

impl SourceTransactionMetadata {
    pub fn source_id(&self) -> &str {
        &self.source_id
    }

    pub fn transaction_id(&self) -> &[u8] {
        &self.transaction_id
    }

    pub fn position(&self) -> &[u8] {
        &self.position
    }

    pub fn operations(&self) -> usize {
        self.operations
    }
}

struct Current {
    metadata: SourceTransactionMetadata,
    replay: Option<u64>,
}

pub struct SourceTransactionCodec;

impl SourceTransactionCodec {
    /// Restore bytes read from a committed source-owned transaction journal.
    /// The caller, not the codec, must establish the journal's commit boundary.
    pub fn decode_committed(
        payload: &[u8],
        limits: SourceTransactionLimits,
    ) -> Result<SourceTransaction, SourceTransactionError> {
        let ceiling = limits
            .max_bytes
            .get()
            .saturating_add(
                limits
                    .max_changes
                    .get()
                    .saturating_mul(STORED_OPERATION_OVERHEAD),
            )
            .saturating_add(STORED_FRAME_OVERHEAD);
        if payload.len() > ceiling {
            return Err(SourceTransactionError::Limit {
                kind: SourceTransactionLimit::Bytes,
                limit: limits.max_bytes.get(),
            });
        }
        let frame: StoredFrame = bincode::options()
            .with_fixint_encoding()
            .with_limit(payload.len() as u64)
            .reject_trailing_bytes()
            .deserialize(payload)?;
        if frame.version != CONTEXT_VERSION {
            return Err(SourceTransactionError::Identity);
        }
        validate_identity(&frame.source_id, &frame.transaction_id, &frame.position)?;
        if frame.operations.len() > limits.max_changes.get() {
            return Err(SourceTransactionError::Limit {
                kind: SourceTransactionLimit::Changes,
                limit: limits.max_changes.get(),
            });
        }
        let operations = frame
            .operations
            .into_iter()
            .enumerate()
            .map(|(ordinal, operation)| operation.into_operation(ordinal as u64))
            .collect::<Result<Vec<_>, _>>()?;
        // Stored groups carry only metadata deletions, so no event time exists.
        let changes = changes_from_operations(&frame.source_id, &operations, &|| {
            Err(GraphCodecError::MissingEventTime)
        })?;
        let group = Group {
            source_id: frame.source_id,
            transaction_id: Bytes::from(frame.transaction_id),
            position: Bytes::from(frame.position),
            changes,
            operations,
        };
        group.check_limits(limits)?;
        Ok(SourceTransaction { group })
    }

    /// Complete transactions use the standard graph schema. The marker, not the
    /// schema, distinguishes one; see [`Self::metadata`].
    pub fn schema() -> Arc<Schema> {
        GraphChangeCodec::schema()
    }

    /// Whether the envelope carries a current, valid completion marker for its
    /// own change set. A matching schema, an inherited marker or a malformed one
    /// is not a complete transaction.
    pub fn is_complete_transaction(envelope: &ChangeEnvelope) -> bool {
        matches!(Self::metadata(envelope), Ok(Some(_)))
    }

    /// The current marker, `Ok(None)` when absent or inherited from another
    /// logical change set, or an error when the current marker is malformed,
    /// duplicated or contradicts the envelope.
    pub fn metadata(
        envelope: &ChangeEnvelope,
    ) -> Result<Option<SourceTransactionMetadata>, SourceTransactionError> {
        Ok(Self::current(envelope)?.map(|current| current.metadata))
    }

    /// Decode exactly one complete transaction. An envelope without its own
    /// current marker is rejected, however well formed its graph records are.
    pub fn decode(
        envelope: &ChangeEnvelope,
        limits: SourceTransactionLimits,
    ) -> Result<SourceTransaction, SourceTransactionError> {
        let metadata = Self::metadata(envelope)?.ok_or(SourceTransactionError::Unmarked)?;
        let operations = envelope.changes().operations();
        if operations.len() > limits.max_changes.get() {
            return Err(SourceTransactionError::Limit {
                kind: SourceTransactionLimit::Changes,
                limit: limits.max_changes.get(),
            });
        }
        let mut group = Group {
            source_id: metadata.source_id,
            transaction_id: metadata.transaction_id,
            position: metadata.position,
            changes: Vec::new(),
            operations: operations.to_vec(),
        };
        group.check_limits(limits)?;
        let time = envelope
            .system()
            .timestamp()
            .and_then(|timestamp| u64::try_from(timestamp.timestamp_millis()).ok());
        group.changes = changes_from_operations(&group.source_id, operations, &|| {
            time.ok_or(GraphCodecError::MissingEventTime)
        })?;
        Ok(SourceTransaction { group })
    }

    pub fn replay_progress(
        envelope: &ChangeEnvelope,
    ) -> Result<Option<(String, u64)>, SourceTransactionError> {
        Ok(Self::current(envelope)?.and_then(|current| {
            current
                .replay
                .map(|sequence| (current.metadata.source_id, sequence))
        }))
    }

    fn current(envelope: &ChangeEnvelope) -> Result<Option<Current>, SourceTransactionError> {
        let mut replay = Vec::new();
        let mut entries = envelope.annotations().entries();
        let entry = loop {
            match entries.next() {
                None => return Ok(None),
                Some(entry) if entry.key() == TRANSACTION_CONTEXT => break entry,
                Some(entry) if entry.key() == REPLAY_CONTEXT => replay.push(entry),
                Some(_) => {}
            }
        };
        let id = envelope.changes().id();
        let bytes = Self::bytes(&entry)?;
        let scope: ContextScope = serde_json::from_slice(&bytes)?;
        if !scope.is_current(id) {
            return Ok(None);
        }
        let older_is_current = entries
            .filter(|older| older.key() == TRANSACTION_CONTEXT)
            .any(|older| {
                Self::bytes(&older)
                    .ok()
                    .and_then(|bytes| serde_json::from_slice::<ContextScope>(&bytes).ok())
                    .is_some_and(|scope| scope.is_current(id))
            });
        if older_is_current || replay.len() > 1 {
            return Err(SourceTransactionError::Identity);
        }
        let context: TransactionContext = serde_json::from_slice(&bytes)?;
        if context.version != CONTEXT_VERSION
            || !context.complete
            || entry.contributor() != context.source_id
        {
            return Err(SourceTransactionError::Identity);
        }
        validate_identity(
            &context.source_id,
            &context.transaction_id,
            &context.position,
        )?;
        super::data::validate_schema(
            GraphChangeCodec::schema().descriptor(),
            envelope.changes().schema(),
        )?;
        if envelope.system().control().is_some()
            || context.operations != envelope.changes().operations().len() as u64
        {
            return Err(SourceTransactionError::Envelope);
        }
        if envelope.system().source_position().map(Bytes::as_ref)
            != Some(context.position.as_slice())
        {
            return Err(SourceTransactionError::Position);
        }
        let replay = replay
            .pop()
            .map(|entry| Self::replay_sequence(&entry, &context))
            .transpose()?;
        Ok(Some(Current {
            metadata: SourceTransactionMetadata {
                operations: envelope.changes().operations().len(),
                source_id: context.source_id,
                transaction_id: Bytes::from(context.transaction_id),
                position: Bytes::from(context.position),
            },
            replay,
        }))
    }

    fn replay_sequence(
        entry: &ContextEntry,
        context: &TransactionContext,
    ) -> Result<u64, SourceTransactionError> {
        let progress: ReplayProgress = serde_json::from_slice(&Self::bytes(entry)?)?;
        if progress.version != CONTEXT_VERSION
            || progress.sequence == 0
            || entry.contributor() != progress.source_id
            || progress.source_id != context.source_id
            || progress.transaction_id != context.transaction_id
        {
            return Err(SourceTransactionError::Identity);
        }
        Ok(progress.sequence)
    }

    fn bytes(entry: &ContextEntry) -> Result<Arc<[u8]>, SourceTransactionError> {
        match entry.value() {
            ContextValue::Bytes(bytes) => Ok(bytes),
            _ => Err(SourceTransactionError::Identity),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use drasi_core_types::models::{
        Element, ElementMetadata, ElementPropertyMap, ElementReference, ElementValue,
    };

    fn limits(changes: usize, bytes: usize) -> SourceTransactionLimits {
        SourceTransactionLimits {
            max_changes: NonZeroUsize::new(changes).unwrap(),
            max_bytes: NonZeroUsize::new(bytes).unwrap(),
            max_duration_ms: NonZeroU64::new(10_000).unwrap(),
        }
    }

    fn element(source: &str, id: &str, time: u64) -> Element {
        Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new(source, id),
                labels: Arc::from([Arc::from("Person")]),
                effective_from: time,
            },
            properties: ElementPropertyMap::new(),
        }
    }

    fn change(source: &str, id: &str) -> SourceChange {
        SourceChange::Insert {
            element: element(source, id, 1000),
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

    fn timestamp() -> DateTime<Utc> {
        DateTime::from_timestamp_millis(1000).unwrap()
    }

    fn stream() -> StreamId {
        StreamId::try_new("source-output").unwrap()
    }

    fn envelope(count: usize) -> ChangeEnvelope {
        commit(builder(limits(10, 4096), count))
            .unwrap()
            .into_envelope(stream(), 7, timestamp())
            .unwrap()
    }

    fn wire_size(envelope: &ChangeEnvelope) -> usize {
        let metadata = SourceTransactionCodec::metadata(envelope).unwrap().unwrap();
        (envelope
            .changes()
            .operations()
            .iter()
            .map(operation_bytes)
            .sum::<u64>()
            + metadata_bytes(
                metadata.source_id(),
                metadata.transaction_id(),
                metadata.position(),
            )) as usize
    }

    fn marker(envelope: &ChangeEnvelope) -> ContextEntry {
        envelope
            .annotations()
            .entries()
            .find(|entry| entry.key() == TRANSACTION_CONTEXT)
            .unwrap()
    }

    fn marker_with(
        envelope: &ChangeEnvelope,
        edit: impl FnOnce(&mut serde_json::Value),
    ) -> ContextEntry {
        let ContextValue::Bytes(bytes) = marker(envelope).value() else {
            panic!("marker bytes");
        };
        let mut value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        edit(&mut value);
        ContextEntry::try_new(
            ComponentId::try_new("source").unwrap(),
            TRANSACTION_CONTEXT,
            ContextValue::Bytes(Arc::from(serde_json::to_vec(&value).unwrap())),
        )
        .unwrap()
    }

    /// An ordinary graph batch given a well-formed marker for its own change set,
    /// standing in for a producer that never validated a source transaction.
    fn forged(changes: &[SourceChange], source: &str) -> ChangeEnvelope {
        let batch =
            GraphChangeCodec::encode_changes(changes, stream(), 7, Some(timestamp())).unwrap();
        let positioned = batch.derive(
            batch.id().clone(),
            batch.changes().clone(),
            batch
                .system()
                .as_ref()
                .clone()
                .with_source_position(Bytes::from_static(b"commit-position")),
        );
        envelope_with_marker(&positioned, source)
    }

    fn envelope_with_marker(input: &ChangeEnvelope, source: &str) -> ChangeEnvelope {
        let id = input.changes().id();
        let context = TransactionContext {
            version: 1,
            change_set_namespace: id.namespace().to_owned(),
            change_set_value: id.value().to_vec(),
            source_id: source.into(),
            transaction_id: b"transaction".to_vec(),
            position: b"commit-position".to_vec(),
            operations: input.changes().operations().len() as u64,
            complete: true,
        };
        input
            .append_context(
                ContextEntry::try_new(
                    ComponentId::try_new(source).unwrap(),
                    TRANSACTION_CONTEXT,
                    ContextValue::Bytes(Arc::from(serde_json::to_vec(&context).unwrap())),
                )
                .unwrap(),
            )
            .unwrap()
    }

    #[tokio::test]
    async fn one_ordered_mixed_transaction_is_ordinary_standard_graph_records() {
        let changes = vec![
            change("source", "0"),
            SourceChange::update_with_removals(
                element("source", "0", 2000),
                ["gone".to_string()].into(),
            )
            .unwrap(),
            SourceChange::replace(element("source", "0", 3000)),
            SourceChange::update(element("source", "0", 4000)),
            SourceChange::Delete {
                metadata: ElementMetadata {
                    reference: ElementReference::new("source", "0"),
                    labels: Arc::from([Arc::from("Person")]),
                    effective_from: 5000,
                },
            },
        ];
        let mut assembly = SourceTransactionBuilder::new("source", limits(8, 4096)).unwrap();
        for change in &changes {
            assembly.push(change.clone()).unwrap();
        }
        let envelope = commit(assembly)
            .unwrap()
            .into_envelope(stream(), 1, timestamp())
            .unwrap();

        // The data is the standard graph schema and operation shapes, in order.
        assert_eq!(
            envelope.changes().schema(),
            GraphChangeCodec::schema().descriptor()
        );
        assert_eq!(
            SourceTransactionCodec::schema().descriptor(),
            GraphChangeCodec::schema().descriptor()
        );
        let shapes: Vec<_> = envelope
            .changes()
            .operations()
            .iter()
            .map(|operation| match operation {
                ChangeOperation::Added { ordinal, after } => (*ordinal, "add", after.image()),
                ChangeOperation::Updated {
                    ordinal,
                    after,
                    semantics: UpdateSemantics::Patch,
                    ..
                } => (*ordinal, "patch", after.image()),
                ChangeOperation::Updated {
                    ordinal,
                    after,
                    semantics: UpdateSemantics::Replace,
                    ..
                } => (*ordinal, "replace", after.image()),
                ChangeOperation::Deleted {
                    ordinal, before, ..
                } => (*ordinal, "delete", before.as_ref().unwrap().image()),
            })
            .collect();
        assert_eq!(
            shapes,
            vec![
                (0, "add", RecordImage::Full),
                (1, "patch", RecordImage::Patch),
                (2, "replace", RecordImage::Full),
                (3, "patch", RecordImage::Patch),
                (4, "delete", RecordImage::Partial),
            ]
        );
        assert_eq!(
            GraphChangeCodec::decode_changes(&envelope).unwrap(),
            changes
        );
        assert!(SourceTransactionCodec::is_complete_transaction(&envelope));
        let restored = SourceTransactionCodec::decode(&envelope, limits(8, 4096)).unwrap();
        assert_eq!(restored.into_changes(), changes);

        // An update that sets and removes one property is malformed and poisons assembly.
        let mut assembly = SourceTransactionBuilder::new("source", limits(8, 4096)).unwrap();
        let mut conflicting = element("source", "0", 2000);
        if let Element::Node { properties, .. } = &mut conflicting {
            properties.insert("both", ElementValue::Null);
        }
        assert!(matches!(
            assembly.push(SourceChange::Update {
                element: conflicting,
                removed_properties: ["both".to_string()].into(),
            }),
            Err(SourceTransactionError::InvalidChange(_))
        ));
        assert!(matches!(
            assembly.push(change("source", "1")),
            Err(SourceTransactionError::Interrupted)
        ));
    }

    #[tokio::test]
    async fn prepared_storage_is_bounded_and_restores_only_as_a_complete_committed_group() {
        let changes = vec![
            change("source", "0"),
            SourceChange::update_with_removals(
                element("source", "0", 2000),
                ["gone".to_string()].into(),
            )
            .unwrap(),
            SourceChange::replace(element("source", "0", 3000)),
            SourceChange::Delete {
                metadata: ElementMetadata {
                    reference: ElementReference::new("source", "0"),
                    labels: Arc::from([Arc::from("Person")]),
                    effective_from: 5000,
                },
            },
        ];
        let mut assembly = SourceTransactionBuilder::new("source", limits(4, 4096)).unwrap();
        for change in &changes {
            assembly.push(change.clone()).unwrap();
        }
        let prepared = assembly
            .prepare(
                Bytes::from_static(b"transaction"),
                Bytes::from_static(b"position"),
            )
            .unwrap();
        let bytes = prepared.encode().unwrap();
        let wire = prepared.group.wire_bytes() as usize;
        assert!(
            bytes.len() <= wire + STORED_FRAME_OVERHEAD + 4 * STORED_OPERATION_OVERHEAD,
            "the private storage framing is bounded by the wire size and fixed overheads"
        );
        assert!(SourceTransactionCodec::decode_committed(&bytes, limits(3, 4096)).is_err());
        assert!(matches!(
            SourceTransactionCodec::decode_committed(&bytes, limits(4, wire - 1)),
            Err(SourceTransactionError::Limit {
                kind: SourceTransactionLimit::Bytes,
                ..
            })
        ));
        let restored = SourceTransactionCodec::decode_committed(&bytes, limits(4, wire)).unwrap();
        assert_eq!(restored.transaction_id(), b"transaction");
        assert_eq!(restored.position(), b"position");
        let envelope = restored.into_envelope(stream(), 3, timestamp()).unwrap();
        assert_eq!(
            SourceTransactionCodec::decode(&envelope, limits(4, wire))
                .unwrap()
                .into_changes(),
            changes
        );
        assert_eq!(prepared.confirm_committed().into_changes(), changes);

        let mut malformed = bytes.to_vec();
        malformed.push(0);
        assert!(SourceTransactionCodec::decode_committed(&malformed, limits(4, 4096)).is_err());
        for length in [0, 1, bytes.len() - 1] {
            assert!(
                SourceTransactionCodec::decode_committed(&bytes[..length], limits(4, 4096))
                    .is_err()
            );
        }
        let mut stale: StoredFrame = bincode::deserialize(&bytes).unwrap();
        stale.version = 2;
        assert!(SourceTransactionCodec::decode_committed(
            &bincode::serialize(&stale).unwrap(),
            limits(4, 4096)
        )
        .is_err());
        let mut mismatched: StoredFrame = bincode::deserialize(&bytes).unwrap();
        let Some(StoredOperation::Added(first)) = mismatched.operations.first_mut() else {
            panic!("stored operations");
        };
        first.identity =
            GraphChangeCodec::element_reference(&ElementReference::new("source", "other"))
                .unwrap()
                .identity()
                .value()
                .to_vec();
        assert!(SourceTransactionCodec::decode_committed(
            &bincode::serialize(&mismatched).unwrap(),
            limits(4, 4096)
        )
        .is_err());
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
                    timestamp(),
                )
                .unwrap()
        };
        for sequence in [1, 2] {
            let envelope = replay(sequence);
            SourceTransactionCodec::decode(&envelope, limits(2, 4096)).unwrap();
            let progress = SourceTransactionCodec::replay_progress(&envelope)
                .unwrap()
                .expect("explicit replay progress");
            assert_eq!(progress, ("source".into(), 42));
            assert_eq!(
                envelope
                    .system()
                    .source_position()
                    .map(|position| position.as_ref()),
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
        duplicate.append_annotation(entry).unwrap();
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

        // Replay progress is scoped to the marker's change set: a fresh change set
        // does not inherit it, and an ordinary envelope never has any.
        let derived = GraphChangeCodec::derive_changes(
            &replay(4),
            &[change("source", "0")],
            StreamId::try_new("derived").unwrap(),
            1,
        )
        .unwrap();
        assert_eq!(
            SourceTransactionCodec::replay_progress(&derived).unwrap(),
            None
        );
        let ordinary =
            GraphChangeCodec::encode_changes(&[change("source", "0")], stream(), 1, None).unwrap();
        assert_eq!(
            SourceTransactionCodec::replay_progress(&ordinary).unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn complete_group_preserves_identity_position_context_and_order_through_storage() {
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
            transaction.into_changes(),
            (0..3)
                .map(|n| change("source", &n.to_string()))
                .collect::<Vec<_>>()
        );
        assert_eq!(restored.changes().operations().len(), 3);
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
        assert!(SourceTransactionCodec::decode(&envelope(3), limits(3, 4096)).is_ok());
    }

    #[tokio::test]
    async fn byte_limit_includes_metadata_and_accepts_the_exact_boundary() {
        let input = envelope(1);
        let size = wire_size(&input);
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
        // One byte of commit metadata alone is enough to cross the boundary.
        assert!(matches!(
            builder(limits(1, size), 1).commit(
                Bytes::from_static(b"transaction!"),
                Bytes::from_static(b"commit-position")
            ),
            Err(SourceTransactionError::Limit {
                kind: SourceTransactionLimit::Bytes,
                ..
            })
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
                future_ref: drasi_core_types::interface::FutureElementRef {
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
            Bytes::from(vec![1; crate::MAX_SOURCE_POSITION_BYTES + 1]),
        ] {
            assert!(matches!(
                builder(limits(1, 1 << 20), 1).commit(Bytes::from_static(b"tx"), position),
                Err(SourceTransactionError::Position)
            ));
        }
    }

    #[tokio::test]
    async fn ordinary_batches_and_forged_or_inconsistent_markers_are_not_complete_groups() {
        let input = envelope(2);
        let changes = GraphChangeCodec::decode_changes(&input).unwrap();

        // The same standard records without a marker are an ordinary batch.
        let ordinary =
            GraphChangeCodec::encode_changes(&changes, stream(), 7, Some(timestamp())).unwrap();
        assert!(!SourceTransactionCodec::is_complete_transaction(&ordinary));
        assert_eq!(SourceTransactionCodec::metadata(&ordinary).unwrap(), None);
        assert!(matches!(
            SourceTransactionCodec::decode(&ordinary, limits(8, 4096)),
            Err(SourceTransactionError::Unmarked)
        ));

        // A marker for another logical change set is inherited history.
        let derived = GraphChangeCodec::derive_changes(
            &input,
            &changes,
            StreamId::try_new("derived").unwrap(),
            1,
        )
        .unwrap();
        assert!(!SourceTransactionCodec::is_complete_transaction(&derived));
        assert!(matches!(
            SourceTransactionCodec::decode(&derived, limits(8, 4096)),
            Err(SourceTransactionError::Unmarked)
        ));
        let regrouped = input.derive(
            input.id().clone(),
            GraphChangeCodec::encode_changes(&changes, stream(), 8, None)
                .unwrap()
                .changes()
                .clone(),
            input.system().as_ref().clone(),
        );
        assert!(matches!(
            SourceTransactionCodec::decode(&regrouped, limits(8, 4096)),
            Err(SourceTransactionError::Unmarked)
        ));

        // The marker states the operation count: a fragment of the group is refused.
        let regenerated = forged(&changes, "source");
        assert!(SourceTransactionCodec::decode(&regenerated, limits(8, 4096)).is_ok());
        let fragment = input.derive(
            input.id().clone(),
            GraphChangeCodec::encode_changes(&changes[..1], stream(), 7, None)
                .unwrap()
                .changes()
                .clone(),
            input.system().as_ref().clone(),
        );
        assert!(matches!(
            SourceTransactionCodec::decode(&fragment, limits(8, 4096)),
            Err(SourceTransactionError::Envelope)
        ));

        // Malformed, unknown-version, incomplete, wrong-contributor markers fail.
        for edit in [
            Box::new(|value: &mut serde_json::Value| value["version"] = 2.into())
                as Box<dyn Fn(&mut serde_json::Value)>,
            Box::new(|value| value["complete"] = false.into()),
            Box::new(|value| value["source_id"] = "other".into()),
            Box::new(|value| value["transaction_id"] = "".into()),
            Box::new(|value| value["position"] = "zz".into()),
            Box::new(|value| value["operations"] = 3.into()),
            Box::new(|value| value["extra"] = true.into()),
        ] {
            let bare = ChangeEnvelope::new(
                input.id().clone(),
                input.changes().clone(),
                input.system().as_ref().clone(),
            );
            let broken = bare.append_context(marker_with(&input, edit)).unwrap();
            assert!(SourceTransactionCodec::decode(&broken, limits(8, 4096)).is_err());
            assert!(!SourceTransactionCodec::is_complete_transaction(&broken));
        }
    }

    #[tokio::test]
    async fn marker_survives_reemit_and_unchanged_relay_but_not_fresh_change_sets() {
        let input = envelope(2);
        let again = input.reemit(9).unwrap();
        assert_eq!(again.system().sequence(), 9);
        assert_eq!(
            SourceTransactionCodec::decode(&again, limits(8, 4096))
                .unwrap()
                .into_changes(),
            SourceTransactionCodec::decode(&input, limits(8, 4096))
                .unwrap()
                .into_changes()
        );

        // A relay that keeps the same logical change set keeps its completeness.
        let relay = input.derive(
            super::super::emission_id(&StreamId::try_new("relay").unwrap(), 1).unwrap(),
            input.changes().clone(),
            SystemMetadata::new(StreamId::try_new("relay").unwrap(), 1)
                .with_timestamp(timestamp())
                .with_source_position(input.system().source_position().unwrap().clone()),
        );
        assert!(SourceTransactionCodec::is_complete_transaction(&relay));

        // Transformed output has its own change set, however it keeps annotations.
        let transformed = GraphChangeCodec::derive_changes(
            &input,
            &GraphChangeCodec::decode_changes(&input).unwrap(),
            StreamId::try_new("transformed").unwrap(),
            1,
        )
        .unwrap();
        assert!(!SourceTransactionCodec::is_complete_transaction(
            &transformed
        ));
        assert!(SourceTransactionCodec::replay_progress(&transformed)
            .unwrap()
            .is_none());
        assert_eq!(
            transformed.annotations().entries().count(),
            input.annotations().entries().count(),
            "annotations were inherited yet completeness was not"
        );
    }

    #[tokio::test]
    async fn invalid_sources_controls_and_positions_are_rejected_explicitly() {
        // A well-formed marker over another source's records.
        let mixed = forged(&[change("another", "1")], "source");
        assert!(matches!(
            SourceTransactionCodec::decode(&mixed, limits(8, 4096)),
            Err(SourceTransactionError::MixedSources)
        ));

        // A marked envelope cannot carry an event-local control.
        let input = envelope(1);
        let controlled = input.derive(
            input.id().clone(),
            input.changes().clone(),
            input.system().as_ref().clone().with_control(
                super::super::ControlMessage::try_new("drasi.graph-control.v1", Bytes::new())
                    .unwrap(),
            ),
        );
        assert!(matches!(
            SourceTransactionCodec::decode(&controlled, limits(8, 4096)),
            Err(SourceTransactionError::Envelope)
        ));
        let due = GraphChangeCodec::encode_futures_due(stream(), 7, timestamp()).unwrap();
        let due = envelope_with_marker(&due, "source");
        assert!(SourceTransactionCodec::decode(&due, limits(8, 4096)).is_err());

        // The commit position cannot be rebound or removed, and a marker cannot repeat.
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
            SystemMetadata::new(stream(), 7),
        )
        .append_context(marker(&input))
        .unwrap();
        assert!(matches!(
            SourceTransactionCodec::decode(&missing, limits(1, 4096)),
            Err(SourceTransactionError::Position)
        ));
        let mut duplicate = input.clone();
        duplicate.append_annotation(marker(&input)).unwrap();
        assert!(matches!(
            SourceTransactionCodec::decode(&duplicate, limits(1, 4096)),
            Err(SourceTransactionError::Identity)
        ));
        let mut separated = input.clone();
        separated
            .append_annotation(marker_with(&input, |value| {
                value["change_set_namespace"] = "historical".into();
            }))
            .unwrap();
        separated.append_annotation(marker(&input)).unwrap();
        assert!(matches!(
            SourceTransactionCodec::decode(&separated, limits(1, 4096)),
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

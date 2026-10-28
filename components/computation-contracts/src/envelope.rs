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

use bytes::Bytes;
use chrono::{DateTime, Utc};

pub use drasi_core_types::models::ChangeOrdering;

use super::{
    data::validate_identifier, ChangeSetRef, ComponentId, ContractError, EnvelopeId, Result,
    StreamId,
};
use crate::internal::change::{computation_bridge, ProcessingContext as SharedContext};

/// Ordinary immutable data only. Opaque transactions, acknowledgements, and other
/// local resource capabilities belong outside the envelope, never in context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContextValue {
    Bool(bool),
    Signed(i64),
    Unsigned(u64),
    String(Arc<str>),
    Bytes(Arc<[u8]>),
}

/// One validated append-only contribution. Repeated keys do not overwrite history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextEntry {
    inner: Arc<ContextContribution>,
}

#[derive(Debug, PartialEq, Eq)]
struct ContextContribution {
    contributor: ComponentId,
    key: Arc<str>,
    value: ContextValue,
}

impl ContextEntry {
    pub fn try_new(
        contributor: ComponentId,
        key: impl Into<Arc<str>>,
        value: ContextValue,
    ) -> Result<Self> {
        let key = key.into();
        validate_identifier("context key", &key)?;
        Ok(Self {
            inner: Arc::new(ContextContribution {
                contributor,
                key,
                value,
            }),
        })
    }

    pub fn contributor(&self) -> &str {
        self.inner.contributor.as_str()
    }

    pub fn key(&self) -> &str {
        &self.inner.key
    }

    /// Scalars are copied; string/byte allocations remain shared.
    pub fn value(&self) -> ContextValue {
        self.inner.value.clone()
    }

    pub(super) fn value_ref(&self) -> &ContextValue {
        &self.inner.value
    }
}

/// Read-only view of an envelope's branch-local annotation list.
/// Appending through the envelope shares the existing immutable A2 chain.
#[derive(Debug, Clone)]
pub struct ProcessingContext {
    inner: SharedContext<ContextEntry>,
}

impl ProcessingContext {
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// Iterate newest first, retaining duplicate keys and sharing data allocations.
    pub fn entries(&self) -> impl Iterator<Item = ContextEntry> {
        self.inner.entries()
    }
}

/// Opaque control directive scoped to one immutable event. The kind is checked;
/// the payload is uninterpreted bytes and may be empty. It is bounded only by
/// the enclosing envelope frame limit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControlMessage {
    kind: Arc<str>,
    payload: Bytes,
}

impl ControlMessage {
    pub fn try_new(kind: impl Into<Arc<str>>, payload: Bytes) -> Result<Self> {
        let kind = kind.into();
        validate_identifier("control kind", &kind)?;
        Ok(Self { kind, payload })
    }

    pub fn kind(&self) -> &str {
        &self.kind
    }

    pub fn payload(&self) -> &Bytes {
        &self.payload
    }
}

/// Immutable producer metadata. Sequence, not timestamp, is authoritative.
/// Each stream belongs to one producer/output port. Successful new emissions
/// have strictly increasing sequences (gaps are legal); the future host enforces
/// ownership and ordering. No order is promised across distinct streams.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemMetadata {
    stream: StreamId,
    sequence: u64,
    timestamp: Option<DateTime<Utc>>,
    source_position: Option<Bytes>,
    control: Option<ControlMessage>,
    ordering: ChangeOrdering,
}

impl SystemMetadata {
    pub fn new(stream: StreamId, sequence: u64) -> Self {
        Self {
            stream,
            sequence,
            timestamp: None,
            source_position: None,
            control: None,
            ordering: ChangeOrdering::EventTime,
        }
    }

    /// Observational for FIFO transports. Explicit native ranked inputs may use
    /// this as their declared event time; SourceEvent adapters preserve raw time separately.
    pub fn with_timestamp(mut self, timestamp: DateTime<Utc>) -> Self {
        self.timestamp = Some(timestamp);
        self
    }

    /// Opaque producer position, not proof of durability or a checkpoint.
    pub fn with_source_position(mut self, position: Bytes) -> Self {
        self.source_position = Some(position);
        self
    }

    /// Event-local control. Unlike annotations, derived outputs do not inherit
    /// it; a producer must attach a fresh control to the output metadata.
    pub fn with_control(mut self, control: ControlMessage) -> Self {
        self.control = Some(control);
        self
    }

    /// Immutable stale-admission policy for this event. New metadata defaults to
    /// `EventTime`; `ProducerSequence` is only valid when the receiver has
    /// externally validated the producer identity/epoch and accepted sequence.
    /// Derived outputs do not inherit it: the producer states the policy again.
    pub fn with_ordering(mut self, ordering: ChangeOrdering) -> Self {
        self.ordering = ordering;
        self
    }

    pub fn stream(&self) -> &StreamId {
        &self.stream
    }

    pub const fn sequence(&self) -> u64 {
        self.sequence
    }

    pub const fn timestamp(&self) -> Option<DateTime<Utc>> {
        self.timestamp
    }

    pub fn source_position(&self) -> Option<&Bytes> {
        self.source_position.as_ref()
    }

    pub fn control(&self) -> Option<&ControlMessage> {
        self.control.as_ref()
    }

    pub const fn ordering(&self) -> ChangeOrdering {
        self.ordering
    }
}

/// Input identity and metadata retained by a derived output, without retaining
/// the input payload. Prior ancestry is shared. This is single-input lineage,
/// not a multi-input transactional or causal graph.
pub struct Lineage {
    envelope_id: EnvelopeId,
    system: Arc<SystemMetadata>,
    parent: Option<Arc<Lineage>>,
    depth: usize,
}

/// Maximum ancestry retained by a published or encoded envelope.
pub const MAX_LINEAGE_DEPTH: usize = 1024;

impl std::fmt::Debug for Lineage {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Lineage")
            .field("envelope_id", &self.envelope_id)
            .field("system", &self.system)
            .field("depth", &self.depth)
            .finish_non_exhaustive()
    }
}

impl Drop for Lineage {
    fn drop(&mut self) {
        let mut parent = self.parent.take();
        while let Some(shared) = parent {
            match Arc::try_unwrap(shared) {
                Ok(mut owned) => parent = owned.parent.take(),
                Err(_) => break,
            }
        }
    }
}

impl Lineage {
    pub(super) fn validate_depth(depth: usize) -> Result<()> {
        if depth > MAX_LINEAGE_DEPTH {
            return Err(ContractError::LineageTooDeep {
                limit: MAX_LINEAGE_DEPTH,
            });
        }
        Ok(())
    }

    pub(super) fn restore(
        envelope_id: EnvelopeId,
        system: SystemMetadata,
        parent: Option<Arc<Lineage>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            envelope_id,
            system: Arc::new(system),
            depth: parent
                .as_ref()
                .map_or(1, |parent| parent.depth.saturating_add(1)),
            parent,
        })
    }

    pub fn envelope_id(&self) -> &EnvelopeId {
        &self.envelope_id
    }

    pub fn system(&self) -> &Arc<SystemMetadata> {
        &self.system
    }

    pub fn parent(&self) -> Option<&Arc<Lineage>> {
        self.parent.as_ref()
    }
}

/// Immutable description of a change to a set at a time.
///
/// A processing component cannot change this event or its records. A transformer
/// emits a new event through [`ChangeEnvelope::derive`] instead.
#[derive(Debug)]
pub struct ChangeEvent {
    id: EnvelopeId,
    changes: ChangeSetRef,
    system: Arc<SystemMetadata>,
    lineage: Option<Arc<Lineage>>,
}

impl ChangeEvent {
    pub fn new(id: EnvelopeId, changes: ChangeSetRef, system: SystemMetadata) -> Self {
        Self {
            id,
            changes,
            system: Arc::new(system),
            lineage: None,
        }
    }

    pub fn id(&self) -> &EnvelopeId {
        &self.id
    }

    pub fn changes(&self) -> &ChangeSetRef {
        &self.changes
    }

    pub fn system(&self) -> &Arc<SystemMetadata> {
        &self.system
    }

    pub fn lineage(&self) -> Option<&Arc<Lineage>> {
        self.lineage.as_ref()
    }
}

/// The computation graph's data-plane currency: a shared immutable change event
/// and a branch-owned appendable list of immutable annotations.
///
/// Cloning shares the event and existing entries. Appending changes only this
/// envelope's list; it cannot alter another branch's history or the event.
#[derive(Debug, Clone)]
pub struct ChangeEnvelope {
    event: Arc<ChangeEvent>,
    context: ProcessingContext,
}

/// Compatibility name for the original v1 envelope contract.
pub type Envelope = ChangeEnvelope;

impl ChangeEnvelope {
    /// Replay committed logical content without changing its provenance.
    /// Event metadata, including any control, is retained.
    pub fn reemit(&self, sequence: u64) -> Result<Self> {
        let mut system = self.system().as_ref().clone();
        system.sequence = sequence;
        Ok(Self {
            event: Arc::new(ChangeEvent {
                id: super::emission_id(system.stream(), sequence)?,
                changes: self.changes().clone(),
                system: Arc::new(system),
                lineage: self.lineage().cloned(),
            }),
            context: self.context.clone(),
        })
    }

    /// Identity of the envelope's shared annotation root.
    pub fn context_identity(&self) -> u64 {
        computation_bridge::context_identity(&self.context.inner)
    }

    #[doc(hidden)]
    pub fn restore(
        id: EnvelopeId,
        changes: ChangeSetRef,
        system: SystemMetadata,
        lineage: Option<Arc<Lineage>>,
        context_identity: u64,
        annotations: Vec<ContextEntry>,
    ) -> Result<Self> {
        let mut envelope = Self {
            event: Arc::new(ChangeEvent {
                id,
                changes,
                system: Arc::new(system),
                lineage,
            }),
            context: ProcessingContext {
                inner: computation_bridge::restored_context(context_identity),
            },
        };
        for entry in annotations {
            envelope.append_annotation(entry)?;
        }
        Ok(envelope)
    }

    /// Construct a new root event. Producers must not reuse an identity for
    /// different events. IDs are supplied identities, not content fingerprints.
    pub fn new(id: EnvelopeId, changes: ChangeSetRef, system: SystemMetadata) -> Self {
        Self::from_event(Arc::new(ChangeEvent::new(id, changes, system)))
    }

    /// Start an annotation list for an already constructed immutable event.
    pub fn from_event(event: Arc<ChangeEvent>) -> Self {
        let context = ProcessingContext {
            inner: computation_bridge::new_context(event.id().namespace(), event.id().value()),
        };
        Self { event, context }
    }

    /// Append one immutable annotation to this branch. Earlier entries remain
    /// immutable and shared; repeated keys retain their complete history.
    pub fn append_annotation(&mut self, entry: ContextEntry) -> Result<()> {
        self.context.inner = computation_bridge::append_context(&self.context.inner, entry)
            .ok_or(ContractError::ContextOverflow)?;
        Ok(())
    }

    /// Extend only this branch. Logical event identity and metadata are preserved.
    pub fn append_context(&self, entry: ContextEntry) -> Result<Self> {
        let mut branch = self.clone();
        branch.append_annotation(entry)?;
        Ok(branch)
    }

    /// Emit a new event from this input, preserving context and input metadata
    /// in lineage. The producer supplies its own output-stream identity/sequence.
    /// An input sequence must not be reused as the sequence of multiple outputs.
    /// Context is inherited, but the input's control is not: the output has only
    /// the supplied metadata, while the input's control remains historical lineage.
    /// Graph publication and codecs reject ancestry deeper than [`MAX_LINEAGE_DEPTH`].
    /// Use [`Self::try_derive`] to reject that limit before constructing an output.
    pub fn derive(&self, id: EnvelopeId, changes: ChangeSetRef, system: SystemMetadata) -> Self {
        Self {
            event: Arc::new(ChangeEvent {
                id,
                changes,
                system: Arc::new(system),
                lineage: Some(Arc::new(Lineage {
                    envelope_id: self.id().clone(),
                    system: self.system().clone(),
                    parent: self.lineage().cloned(),
                    depth: self.lineage_depth().saturating_add(1),
                })),
            }),
            context: self.context.clone(),
        }
    }

    /// Derive an output within the shared graph/wire ancestry limit.
    pub fn try_derive(
        &self,
        id: EnvelopeId,
        changes: ChangeSetRef,
        system: SystemMetadata,
    ) -> Result<Self> {
        Lineage::validate_depth(self.lineage_depth().saturating_add(1))?;
        Ok(self.derive(id, changes, system))
    }

    pub fn lineage_depth(&self) -> usize {
        self.lineage().map_or(0, |lineage| lineage.depth)
    }

    /// Validate the constant-time ancestry bound before publication or encoding.
    pub fn validate_lineage(&self) -> Result<()> {
        Lineage::validate_depth(self.lineage_depth())
    }

    pub fn event(&self) -> &Arc<ChangeEvent> {
        &self.event
    }

    pub fn id(&self) -> &EnvelopeId {
        self.event.id()
    }

    pub fn changes(&self) -> &ChangeSetRef {
        self.event.changes()
    }

    pub fn system(&self) -> &Arc<SystemMetadata> {
        self.event.system()
    }

    pub fn annotations(&self) -> &ProcessingContext {
        &self.context
    }

    pub fn context(&self) -> &ProcessingContext {
        &self.context
    }

    pub fn lineage(&self) -> Option<&Arc<Lineage>> {
        self.event.lineage()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ChangeSet, RecordId, RecordImage, RecordValidationError, RecordValidator, Schema,
        SchemaDescriptor, SchemaId, SchemaVersion,
    };

    struct NoRecords;

    impl RecordValidator for NoRecords {
        fn validate_identity(
            &self,
            _: &SchemaDescriptor,
            _: &RecordId,
        ) -> std::result::Result<(), RecordValidationError> {
            Err(RecordValidationError::new("identity", "no records"))
        }

        fn validate(
            &self,
            schema: &SchemaDescriptor,
            id: &RecordId,
            _: RecordImage,
            _: &[u8],
        ) -> std::result::Result<(), RecordValidationError> {
            self.validate_identity(schema, id)
        }
    }

    fn fixture_changes() -> ChangeSetRef {
        let schema = Schema::new(
            SchemaDescriptor::try_new(
                SchemaId::try_new("test.control").unwrap(),
                SchemaVersion::try_new(1).unwrap(),
                "none",
                Bytes::from_static(b"empty"),
            )
            .unwrap(),
            Arc::new(NoRecords),
        );
        ChangeSet::try_new(
            crate::ChangeSetId::try_new("changes", Bytes::from_static(b"empty")).unwrap(),
            schema.descriptor().clone(),
            Vec::new(),
        )
        .unwrap()
    }

    fn control(kind: &str, payload: &'static [u8]) -> ControlMessage {
        ControlMessage::try_new(kind, Bytes::from_static(payload)).unwrap()
    }

    fn metadata(stream: &str, sequence: u64) -> SystemMetadata {
        SystemMetadata::new(StreamId::try_new(stream).unwrap(), sequence)
    }

    #[test]
    fn lineage_limit_is_exact_and_deep_shared_chains_drop_on_a_small_stack() {
        std::thread::Builder::new()
            .stack_size(2 * 1024 * 1024)
            .spawn(|| {
                let mut envelope = ChangeEnvelope::new(
                    EnvelopeId::try_new("root", Bytes::from_static(b"root")).unwrap(),
                    fixture_changes(),
                    metadata("root", 0),
                );
                for sequence in 1..=MAX_LINEAGE_DEPTH {
                    envelope = envelope
                        .try_derive(
                            EnvelopeId::try_new(
                                "bounded",
                                Bytes::copy_from_slice(&sequence.to_be_bytes()),
                            )
                            .unwrap(),
                            envelope.changes().clone(),
                            metadata("bounded", sequence as u64),
                        )
                        .unwrap();
                }
                assert_eq!(envelope.lineage_depth(), MAX_LINEAGE_DEPTH);
                assert!(matches!(
                    envelope.try_derive(
                        envelope.id().clone(),
                        envelope.changes().clone(),
                        metadata("over", 1),
                    ),
                    Err(ContractError::LineageTooDeep { .. })
                ));
                let shared = envelope.clone();
                for sequence in MAX_LINEAGE_DEPTH..100_000 {
                    envelope = envelope.derive(
                        EnvelopeId::try_new(
                            "deep",
                            Bytes::copy_from_slice(&sequence.to_be_bytes()),
                        )
                        .unwrap(),
                        envelope.changes().clone(),
                        metadata("deep", sequence as u64),
                    );
                }
                assert!(envelope.validate_lineage().is_err());
                assert!(format!("{:?}", envelope.lineage().unwrap()).len() < 1024);
                drop(envelope);
                assert_eq!(shared.lineage_depth(), MAX_LINEAGE_DEPTH);
                drop(shared);
            })
            .unwrap()
            .join()
            .unwrap();
    }

    #[test]
    fn control_messages_check_kind_and_keep_arbitrary_payloads() {
        for kind in ["", " ", "a b", "a\tb", "a\nb", "a\u{0}b", "\u{7f}"] {
            assert!(
                matches!(
                    ControlMessage::try_new(kind, Bytes::new()),
                    Err(ContractError::InvalidIdentifier {
                        kind: "control kind",
                        ..
                    })
                ),
                "{kind:?}"
            );
        }
        let empty = ControlMessage::try_new("custom/reevaluate", Bytes::new()).unwrap();
        assert_eq!(empty.kind(), "custom/reevaluate");
        assert!(empty.payload().is_empty());
        let bytes = control("opaque", &[255, 0, 128]);
        assert_eq!(bytes.payload().as_ref(), [255, 0, 128]);
        assert_eq!(bytes.clone(), bytes);
        assert_ne!(bytes, control("opaque", &[255, 0]));
        assert_ne!(bytes, control("other", &[255, 0, 128]));
    }

    #[test]
    fn metadata_control_is_absent_by_default_and_reemit_retains_it() {
        assert!(metadata("source", 1).control().is_none());
        let expected = control("custom/due", &[0, 1, 2]);
        let original = ChangeEnvelope::new(
            EnvelopeId::try_new("source", Bytes::from_static(b"event")).unwrap(),
            fixture_changes(),
            metadata("source", 1).with_control(expected.clone()),
        );
        assert_eq!(original.system().control(), Some(&expected));
        let replay = original.reemit(9).unwrap();
        assert_eq!(replay.system().sequence(), 9);
        assert_eq!(replay.system().control(), Some(&expected));
        assert_eq!(original.system().sequence(), 1);
    }

    #[test]
    fn derive_uses_fresh_metadata_and_keeps_input_control_only_in_lineage() {
        let input_control = control("custom/due", &[7]);
        let mut input = ChangeEnvelope::new(
            EnvelopeId::try_new("source", Bytes::from_static(b"event")).unwrap(),
            fixture_changes(),
            metadata("source", 1).with_control(input_control.clone()),
        );
        input
            .append_annotation(
                ContextEntry::try_new(
                    ComponentId::try_new("source").unwrap(),
                    "inherited",
                    ContextValue::Bool(true),
                )
                .unwrap(),
            )
            .unwrap();
        let output = input.derive(
            EnvelopeId::try_new("output", Bytes::from_static(b"event")).unwrap(),
            input.changes().clone(),
            metadata("output", 1),
        );
        assert!(output.system().control().is_none());
        assert_eq!(output.annotations().len(), 1);
        let lineage = output.lineage().unwrap();
        assert_eq!(lineage.system().control(), Some(&input_control));

        let replacement = control("custom/replacement", &[]);
        let next = output.derive(
            EnvelopeId::try_new("next", Bytes::from_static(b"event")).unwrap(),
            output.changes().clone(),
            metadata("next", 1).with_control(replacement.clone()),
        );
        assert_eq!(next.system().control(), Some(&replacement));
        let parent = next.lineage().unwrap();
        assert!(parent.system().control().is_none());
        assert_eq!(
            parent.parent().unwrap().system().control(),
            Some(&input_control)
        );
        let replay = next.reemit(2).unwrap();
        assert_eq!(replay.system().control(), Some(&replacement));
        assert!(replay.lineage().unwrap().system().control().is_none());
    }

    #[test]
    fn ordering_defaults_to_event_time_and_is_replay_retained_but_not_derived() {
        assert_eq!(metadata("source", 1).ordering(), ChangeOrdering::EventTime);
        let input = ChangeEnvelope::new(
            EnvelopeId::try_new("source", Bytes::from_static(b"event")).unwrap(),
            fixture_changes(),
            metadata("source", 1).with_ordering(ChangeOrdering::ProducerSequence),
        );
        assert_eq!(input.system().ordering(), ChangeOrdering::ProducerSequence);
        let replay = input.reemit(9).unwrap();
        assert_eq!(replay.system().ordering(), ChangeOrdering::ProducerSequence);
        assert_eq!(input.system().ordering(), ChangeOrdering::ProducerSequence);

        let output = input.derive(
            EnvelopeId::try_new("output", Bytes::from_static(b"event")).unwrap(),
            input.changes().clone(),
            metadata("output", 1),
        );
        assert_eq!(output.system().ordering(), ChangeOrdering::EventTime);
        assert_eq!(
            output.lineage().unwrap().system().ordering(),
            ChangeOrdering::ProducerSequence
        );
        let explicit = input.derive(
            EnvelopeId::try_new("explicit", Bytes::from_static(b"event")).unwrap(),
            input.changes().clone(),
            metadata("explicit", 1).with_ordering(input.system().ordering()),
        );
        assert_eq!(
            explicit.system().ordering(),
            ChangeOrdering::ProducerSequence
        );
    }

    #[test]
    fn copied_annotation_entries_share_immutable_storage() {
        let entry = ContextEntry::try_new(
            ComponentId::try_new("source").unwrap(),
            "visited",
            ContextValue::Bool(true),
        )
        .unwrap();
        let context = computation_bridge::new_context("test", b"event");
        let first = computation_bridge::append_context(&context, entry.clone()).unwrap();
        let left = computation_bridge::append_context(&first, entry.clone()).unwrap();
        let right = first.clone();
        assert!(Arc::ptr_eq(
            &left.entries().last().unwrap().inner,
            &right.entries().next().unwrap().inner
        ));
        assert!(Arc::ptr_eq(
            &entry.inner,
            &left.entries().next().unwrap().inner
        ));
    }
}

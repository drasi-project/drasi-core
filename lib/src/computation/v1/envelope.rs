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

use super::{
    data::validate_identifier, ChangeSetRef, ComponentId, ContractError, EnvelopeId, Result,
    StreamId,
};
use crate::computation::internal::change::{
    computation_bridge, ProcessingContext as SharedContext,
};

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
}

impl SystemMetadata {
    pub fn new(stream: StreamId, sequence: u64) -> Self {
        Self {
            stream,
            sequence,
            timestamp: None,
            source_position: None,
        }
    }

    /// Observational timestamp, never a scheduling/order key.
    pub fn with_timestamp(mut self, timestamp: DateTime<Utc>) -> Self {
        self.timestamp = Some(timestamp);
        self
    }

    /// Opaque producer position, not proof of durability or a checkpoint.
    pub fn with_source_position(mut self, position: Bytes) -> Self {
        self.source_position = Some(position);
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
}

/// Input identity and metadata retained by a derived output, without retaining
/// the input payload. Prior ancestry is shared. This is single-input lineage,
/// not a multi-input transactional or causal graph.
#[derive(Debug)]
pub struct Lineage {
    envelope_id: EnvelopeId,
    system: Arc<SystemMetadata>,
    parent: Option<Arc<Lineage>>,
}

impl Lineage {
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
                })),
            }),
            context: self.context.clone(),
        }
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

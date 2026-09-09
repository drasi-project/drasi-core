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
use crate::change::{self, computation_bridge};

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
    inner: change::ContextContribution,
}

impl ContextEntry {
    pub fn try_new(
        contributor: ComponentId,
        key: impl Into<Arc<str>>,
        value: ContextValue,
    ) -> Result<Self> {
        let key = key.into();
        validate_identifier("context key", &key)?;
        let value = match value {
            ContextValue::Bool(value) => change::ContextValue::Bool(value),
            ContextValue::Signed(value) => change::ContextValue::Signed(value),
            ContextValue::Unsigned(value) => change::ContextValue::Unsigned(value),
            ContextValue::String(value) => change::ContextValue::String(value),
            ContextValue::Bytes(value) => change::ContextValue::Bytes(value),
        };
        Ok(Self {
            inner: change::ContextContribution::new(
                change::ContextContributor::new(
                    change::ContextContributorKind::Runtime,
                    contributor.as_str(),
                ),
                key,
                value,
            ),
        })
    }

    pub fn contributor(&self) -> &str {
        self.inner.contributor().component_id()
    }

    pub fn key(&self) -> &str {
        self.inner.key()
    }

    /// Scalars are copied; string/byte allocations remain shared.
    pub fn value(&self) -> ContextValue {
        match self.inner.value() {
            change::ContextValue::Bool(value) => ContextValue::Bool(*value),
            change::ContextValue::Signed(value) => ContextValue::Signed(*value),
            change::ContextValue::Unsigned(value) => ContextValue::Unsigned(*value),
            change::ContextValue::String(value) => ContextValue::String(value.clone()),
            change::ContextValue::Bytes(value) => ContextValue::Bytes(value.clone()),
        }
    }
}

/// Persistent immutable context chain, sharing the A2 root and linked nodes.
#[derive(Debug, Clone)]
pub struct ProcessingContext {
    inner: change::ProcessingContext,
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
        let mut head = self.inner.head().cloned();
        std::iter::from_fn(move || {
            let node = head.take()?;
            head = node.parent().cloned();
            Some(ContextEntry {
                inner: node.contribution().clone(),
            })
        })
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

/// Immutable logical event with shared data and branch-local context history.
/// Cloning or appending context does not deep-clone records or system metadata.
#[derive(Debug, Clone)]
pub struct Envelope {
    id: EnvelopeId,
    changes: ChangeSetRef,
    system: Arc<SystemMetadata>,
    context: ProcessingContext,
    lineage: Option<Arc<Lineage>>,
}

impl Envelope {
    /// Construct a new root event. Producers must not reuse an identity for
    /// different events. IDs are supplied identities, not content fingerprints.
    pub fn new(id: EnvelopeId, changes: ChangeSetRef, system: SystemMetadata) -> Self {
        let context = ProcessingContext {
            inner: computation_bridge::new_context(id.namespace(), id.value()),
        };
        Self {
            id,
            changes,
            system: Arc::new(system),
            context,
            lineage: None,
        }
    }

    /// Extend only this branch. Logical event identity and metadata are preserved.
    pub fn append_context(&self, entry: ContextEntry) -> Result<Self> {
        self.context
            .len()
            .checked_add(1)
            .ok_or(ContractError::ContextOverflow)?;
        Ok(Self {
            context: ProcessingContext {
                inner: computation_bridge::append_context(&self.context.inner, entry.inner),
            },
            ..self.clone()
        })
    }

    /// Emit a new event from this input, preserving context and input metadata
    /// in lineage. The producer supplies its own output-stream identity/sequence.
    /// An input sequence must not be reused as the sequence of multiple outputs.
    pub fn derive(&self, id: EnvelopeId, changes: ChangeSetRef, system: SystemMetadata) -> Self {
        Self {
            id,
            changes,
            system: Arc::new(system),
            context: self.context.clone(),
            lineage: Some(Arc::new(Lineage {
                envelope_id: self.id.clone(),
                system: self.system.clone(),
                parent: self.lineage.clone(),
            })),
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

    pub fn context(&self) -> &ProcessingContext {
        &self.context
    }

    pub fn lineage(&self) -> Option<&Arc<Lineage>> {
        self.lineage.as_ref()
    }
}

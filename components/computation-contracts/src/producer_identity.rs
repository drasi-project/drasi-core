// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use crate::{ChangeEnvelope, ComponentId, ContextEntry, ContextValue, StreamId};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

const PROGRESS: &str = "drasi.graph-producer-progress.v1";

/// The owner of a graph-change producer's logical output sequence.
///
/// A durable incarnation is persisted with its configuration, not recreated at
/// process startup. Transport emissions may replay the same logical sequence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GraphProducerIdentity {
    construction_scope: String,
    graph_id: String,
    component_id: ComponentId,
    stream: StreamId,
    incarnation: uuid::Uuid,
    persistent: bool,
}

impl GraphProducerIdentity {
    /// A new incarnation for a source whose position is lost on reconstruction.
    /// Persistent consumers must not treat this as a restart-stable stream.
    pub fn volatile(
        scope: String,
        graph_id: String,
        component_id: ComponentId,
        stream: StreamId,
    ) -> anyhow::Result<Self> {
        Self::new(scope, graph_id, component_id, stream, false)
    }

    pub fn new(
        scope: String,
        graph_id: String,
        component_id: ComponentId,
        stream: StreamId,
        persistent: bool,
    ) -> anyhow::Result<Self> {
        Self::for_epoch(
            scope,
            graph_id,
            component_id,
            stream,
            uuid::Uuid::new_v4(),
            persistent,
        )
    }

    /// Reconstruct the actual output epoch supplied by its state owner.
    pub fn for_epoch(
        scope: String,
        graph_id: String,
        component_id: ComponentId,
        stream: StreamId,
        incarnation: uuid::Uuid,
        persistent: bool,
    ) -> anyhow::Result<Self> {
        let identity = Self {
            construction_scope: scope,
            graph_id,
            component_id,
            stream,
            incarnation,
            persistent,
        };
        identity.validate()?;
        Ok(identity)
    }

    pub fn construction_scope(&self) -> &str {
        &self.construction_scope
    }
    pub fn graph_id(&self) -> &str {
        &self.graph_id
    }
    pub fn component_id(&self) -> &ComponentId {
        &self.component_id
    }
    pub fn stream(&self) -> &StreamId {
        &self.stream
    }
    pub fn incarnation(&self) -> uuid::Uuid {
        self.incarnation
    }
    pub fn persistent(&self) -> bool {
        self.persistent
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        super::data::validate_identifier("producer construction scope", &self.construction_scope)?;
        super::data::validate_identifier("producer graph", &self.graph_id)?;
        anyhow::ensure!(!self.incarnation.is_nil(), "nil graph producer incarnation");
        Ok(())
    }
}

/// Versioned graph-only progress, separate from raw source provenance and the
/// strictly increasing transport emission sequence. Does not change legacy FFI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GraphProducerProgress {
    version: u32,
    identity: GraphProducerIdentity,
    sequence: u64,
}

impl GraphProducerProgress {
    pub fn identity(&self) -> &GraphProducerIdentity {
        &self.identity
    }
    pub fn sequence(&self) -> u64 {
        self.sequence
    }

    /// Attach the immediate producer's logical position. This preserves its
    /// declared volatility and validates that it owns this output stream.
    pub fn annotate(
        envelope: &mut ChangeEnvelope,
        identity: &GraphProducerIdentity,
        sequence: u64,
    ) -> anyhow::Result<()> {
        identity.validate()?;
        anyhow::ensure!(
            sequence > 0 && identity.stream == *envelope.system().stream(),
            "invalid graph producer logical progress"
        );
        envelope.append_annotation(ContextEntry::try_new(
            identity.component_id.clone(),
            PROGRESS,
            ContextValue::Bytes(Arc::from(serde_json::to_vec(&Self {
                version: 1,
                identity: identity.clone(),
                sequence,
            })?)),
        )?)?;
        Ok(())
    }

    pub fn from_envelope(envelope: &ChangeEnvelope) -> anyhow::Result<Option<Self>> {
        Self::read(envelope, false)
    }

    /// Query results retain input annotations as history. Only a marker for the
    /// current stream supersedes the query's own recovery identity.
    pub fn from_query_envelope(envelope: &ChangeEnvelope) -> anyhow::Result<Option<Self>> {
        Self::read(envelope, true)
    }

    fn read(envelope: &ChangeEnvelope, allow_query_ancestor: bool) -> anyhow::Result<Option<Self>> {
        Self::read_scoped(envelope, allow_query_ancestor).map(|(progress, _)| progress)
    }

    pub fn read_scoped(
        envelope: &ChangeEnvelope,
        allow_query_ancestor: bool,
    ) -> anyhow::Result<(Option<Self>, bool)> {
        Self::read_at(
            envelope,
            envelope.system().stream(),
            envelope.lineage(),
            allow_query_ancestor,
        )
    }

    /// Read a producer boundary already selected from this envelope's lineage.
    /// The caller must separately validate the identity-preserving graph path.
    /// Later producer markers cannot be mistaken for this boundary's progress.
    pub fn read_boundary(
        envelope: &ChangeEnvelope,
        boundary: &Arc<crate::SystemMetadata>,
    ) -> anyhow::Result<(Option<Self>, bool)> {
        let mut system = envelope.system();
        let mut ancestor = envelope.lineage();
        while !Arc::ptr_eq(system, boundary) {
            let parent = ancestor
                .ok_or_else(|| anyhow::anyhow!("producer boundary is outside envelope lineage"))?;
            system = parent.system();
            ancestor = parent.parent();
        }
        Self::read_at(envelope, system.stream(), ancestor, false)
    }

    fn read_at(
        envelope: &ChangeEnvelope,
        stream: &StreamId,
        ancestor: Option<&Arc<crate::Lineage>>,
        allow_query_ancestor: bool,
    ) -> anyhow::Result<(Option<Self>, bool)> {
        let Some(entry) = envelope
            .annotations()
            .entries()
            .find(|entry| entry.key() == PROGRESS)
        else {
            return Ok((None, false));
        };
        let ContextValue::Bytes(bytes) = entry.value() else {
            anyhow::bail!("graph producer progress is not bytes");
        };
        let progress: Self = serde_json::from_slice(&bytes)?;
        anyhow::ensure!(
            progress.version == 1,
            "unsupported graph producer progress version"
        );
        progress.identity.validate()?;
        anyhow::ensure!(
            progress.sequence > 0 && progress.identity.component_id.as_str() == entry.contributor(),
            "graph producer progress does not identify the immediate producer"
        );
        if progress.identity.stream != *stream
            && (allow_query_ancestor || has_ancestor_stream(ancestor, &progress.identity.stream))
        {
            return Ok((None, !progress.identity.persistent));
        }
        anyhow::ensure!(
            progress.identity.stream == *stream,
            "graph producer progress does not identify the immediate producer"
        );
        Ok((Some(progress), false))
    }
}

fn has_ancestor_stream(mut ancestor: Option<&Arc<crate::Lineage>>, stream: &StreamId) -> bool {
    while let Some(entry) = ancestor {
        if entry.system().stream() == stream {
            return true;
        }
        ancestor = entry.parent();
    }
    false
}

pub fn has_distinct_producer_lineage(envelope: &ChangeEnvelope) -> bool {
    let mut ancestor = envelope.lineage();
    while let Some(entry) = ancestor {
        if entry.system().stream() != envelope.system().stream() {
            return true;
        }
        ancestor = entry.parent();
    }
    false
}

// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Initialization of a volatile transformer from its upstream outputs' snapshots.
//!
//! A continuous query keeps its bootstrap results to itself, so a transformer
//! fed by a query (or by any output that existed before it started) would
//! otherwise see only rows that change after it starts. A transformer factory
//! that opts in to `output-snapshots-v1` (`supports_output_snapshots`,
//! `create_with_output_snapshots`) and sets `Capabilities::binds_inputs`
//! receives its incoming edges in `CreateRequest::inputs` and, when the host
//! binds an `output_snapshots` dependency, a [`NativeOutputSnapshots`] reader.
//! [`UpstreamSnapshots`] then:
//!
//! 1. reads every edge's snapshot at start ([`UpstreamSnapshots::load_all`])
//!    and returns its elements as full-image `Insert` changes;
//! 2. skips live envelopes the snapshot already covers (same producer epoch,
//!    sequence at or below the snapshot's) ([`UpstreamSnapshots::admit`]);
//! 3. reads an edge again when its producer restarts (a new epoch), deleting
//!    the elements it contributed that the new snapshot no longer has.
//!
//! Positions and contributed elements change only through
//! [`UpstreamSnapshots::commit`], so a component that rejects a batch can
//! leave them unchanged. Envelopes without producer progress, or from a
//! producer that is not a bound edge, are applied as they are. Inputs must use
//! [`GraphChangeCodec`].

use anyhow::{ensure, Result};
use async_trait::async_trait;
use drasi_computation_contracts::{
    ChangeEnvelope, GraphChangeCodec, GraphProducerProgress, InputBinding, OutputCursor,
    OutputEndpoint, PortId, Record,
};
use drasi_core_types::models::{Element, ElementMetadata, ElementReference, SourceChange};
use futures_util::StreamExt;
use std::{collections::HashSet, sync::Arc};

use crate::NativeOutputSnapshots;

/// Reads one upstream output's snapshot: its position and full-image records.
#[async_trait]
pub trait SnapshotSource: Send + Sync {
    async fn read(&self, output: &OutputEndpoint) -> Result<(OutputCursor, Vec<Record>)>;
}

#[async_trait]
impl SnapshotSource for NativeOutputSnapshots {
    async fn read(&self, output: &OutputEndpoint) -> Result<(OutputCursor, Vec<Record>)> {
        let mut snapshot = self.snapshot(output, &GraphChangeCodec::schema()).await?;
        let mut records = Vec::new();
        while let Some(record) = snapshot.records.next().await {
            records.push(record?);
        }
        Ok((snapshot.cursor, records))
    }
}

struct Edge {
    binding: InputBinding,
    cursor: Option<OutputCursor>,
    members: HashSet<ElementReference>,
}

/// The upstream edges of one input port and the positions their rows reflect.
pub struct UpstreamSnapshots {
    edges: Vec<Edge>,
}

struct EdgeUpdate {
    edge: usize,
    cursor: OutputCursor,
    membership: Vec<(ElementReference, bool)>,
}

/// Changes to apply for a snapshot load or one live envelope, and the edge
/// positions to adopt once the component accepts them.
#[must_use = "commit the batch after the component accepts its changes"]
pub struct SnapshotBatch {
    /// Snapshot changes to apply first: full images, then deletions.
    pub changes: Vec<SourceChange>,
    /// Whether the live envelope's own changes still need applying (false
    /// when the edge's snapshot already covers it).
    pub apply_envelope: bool,
    updates: Vec<EdgeUpdate>,
}

impl SnapshotBatch {
    /// Records the envelope's decoded changes, once applied, as contributed by
    /// its edge. Call it with the changes applied in addition to `changes`.
    pub fn include(&mut self, applied: &[SourceChange]) {
        if let Some(update) = self.updates.last_mut() {
            update.membership.extend(membership(applied));
        }
    }
}

impl UpstreamSnapshots {
    /// The bound edges of `port`.
    pub fn new(bindings: &[InputBinding], port: &PortId) -> Self {
        Self {
            edges: bindings
                .iter()
                .filter(|binding| &binding.input_port == port)
                .map(|binding| Edge {
                    binding: binding.clone(),
                    cursor: None,
                    members: HashSet::new(),
                })
                .collect(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.edges.is_empty()
    }

    /// The position each edge's rows reflect, once loaded.
    pub fn cursors(&self) -> impl Iterator<Item = (&InputBinding, Option<&OutputCursor>)> {
        self.edges
            .iter()
            .map(|edge| (&edge.binding, edge.cursor.as_ref()))
    }

    /// Reads every edge's snapshot. Deletions of elements an edge no longer
    /// has carry `deleted_from` as their effective time.
    pub async fn load_all(
        &self,
        source: &dyn SnapshotSource,
        deleted_from: u64,
    ) -> Result<SnapshotBatch> {
        let mut batch = SnapshotBatch {
            changes: Vec::new(),
            apply_envelope: false,
            updates: Vec::with_capacity(self.edges.len()),
        };
        for edge in 0..self.edges.len() {
            let (cursor, changes) = self.read(edge, source, deleted_from).await?;
            batch.updates.push(EdgeUpdate {
                edge,
                cursor,
                membership: membership(&changes),
            });
            batch.changes.extend(changes);
        }
        Ok(batch)
    }

    /// Decides how to apply one live envelope. An edge seen for the first
    /// time in this producer epoch is read again first.
    pub async fn admit(
        &self,
        source: &dyn SnapshotSource,
        envelope: &ChangeEnvelope,
        deleted_from: u64,
    ) -> Result<SnapshotBatch> {
        let untracked = || SnapshotBatch {
            changes: Vec::new(),
            apply_envelope: true,
            updates: Vec::new(),
        };
        let Some(progress) = GraphProducerProgress::from_envelope(envelope)? else {
            return Ok(untracked());
        };
        let identity = progress.identity();
        let Some(edge) = self.edges.iter().position(|edge| {
            &edge.binding.output.component == identity.component_id()
                && &edge.binding.stream == identity.stream()
        }) else {
            return Ok(untracked());
        };
        let cursor = OutputCursor::try_new(identity.clone(), progress.sequence())?;
        let (position, changes) = match &self.edges[edge].cursor {
            Some(saved) if saved.ensure_same_epoch(&cursor).is_ok() => (saved.clone(), Vec::new()),
            _ => self.read(edge, source, deleted_from).await?,
        };
        let apply_envelope = !(position.ensure_same_epoch(&cursor).is_ok()
            && cursor.sequence() <= position.sequence());
        let adopted = if apply_envelope { cursor } else { position };
        Ok(SnapshotBatch {
            updates: vec![EdgeUpdate {
                edge,
                cursor: adopted,
                membership: membership(&changes),
            }],
            changes,
            apply_envelope,
        })
    }

    /// Adopts the positions and contributed elements of an accepted batch.
    pub fn commit(&mut self, batch: SnapshotBatch) {
        for update in batch.updates {
            let edge = &mut self.edges[update.edge];
            edge.cursor = Some(update.cursor);
            for (reference, present) in update.membership {
                if present {
                    edge.members.insert(reference);
                } else {
                    edge.members.remove(&reference);
                }
            }
        }
    }

    async fn read(
        &self,
        edge: usize,
        source: &dyn SnapshotSource,
        deleted_from: u64,
    ) -> Result<(OutputCursor, Vec<SourceChange>)> {
        let edge = &self.edges[edge];
        let (cursor, records) = source.read(&edge.binding.output).await?;
        ensure!(
            cursor.producer().stream() == &edge.binding.stream,
            "snapshot of {} is for another stream",
            edge.binding.output
        );
        let mut changes = Vec::with_capacity(records.len());
        let mut present = HashSet::with_capacity(records.len());
        for record in &records {
            let element = Element::try_from(GraphChangeCodec::decode_element(record)?)?;
            present.insert(element.get_reference().clone());
            changes.push(SourceChange::Insert { element });
        }
        for reference in edge.members.difference(&present) {
            changes.push(SourceChange::Delete {
                metadata: ElementMetadata {
                    reference: reference.clone(),
                    labels: Arc::from([]),
                    effective_from: deleted_from,
                },
            });
        }
        Ok((cursor, changes))
    }
}

fn membership(changes: &[SourceChange]) -> Vec<(ElementReference, bool)> {
    changes
        .iter()
        .filter_map(|change| match change {
            SourceChange::Insert { element }
            | SourceChange::Replace { element }
            | SourceChange::Update { element, .. } => Some((element.get_reference().clone(), true)),
            SourceChange::Delete { metadata } => Some((metadata.reference.clone(), false)),
            SourceChange::Future { .. } => None,
        })
        .collect()
}

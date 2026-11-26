// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! A transformer's materialized output: its current rows, committed sequence,
//! retained history and live follow leases. Snapshot, snapshot-and-follow and
//! replay readers all read this one state; there is no second copy of the rows.

use std::{
    collections::VecDeque,
    num::NonZeroUsize,
    sync::{Arc, RwLock, RwLockReadGuard},
};

use anyhow::{Context, Result};
use async_trait::async_trait;
use bytes::Bytes;
use drasi_computation_contracts::{
    ChangeEnvelope, ChangeOperation, ComponentId, ElementImage, GraphChangeCodec,
    GraphProducerIdentity, GraphProducerProgress, OutputCursor, OutputEnvelope, OutputFollowHub,
    OutputFollowLimits, OutputReadError, OutputReplay, OutputSnapshot, OutputSnapshotFollow,
    Record, RecordId, ReplayProvider, SnapshotFollowProvider, SnapshotProvider, StreamId,
    UpdateSemantics,
};
use drasi_core_types::models::{Element, SourceChange};

use super::observations::Observations;

/// The current rows of one output, keyed by record identity.
pub type Rows = im::HashMap<RecordId, Record>;

type ReadResult<T> = std::result::Result<T, OutputReadError>;

/// The logical position a published output occupies in its producer's history.
pub fn logical_sequence(envelope: &ChangeEnvelope) -> Result<u64> {
    Ok(GraphProducerProgress::from_envelope(envelope)?
        .context("transformer output omitted producer progress")?
        .sequence())
}

/// Apply committed operations to current rows. Patches merge into the
/// existing image; a patch of an absent row becomes that row.
pub fn apply_operations(rows: &mut Rows, operations: &[ChangeOperation]) -> Result<()> {
    for operation in operations {
        match operation {
            ChangeOperation::Added { after, .. }
            | ChangeOperation::Updated {
                after,
                semantics: UpdateSemantics::Replace,
                ..
            } => {
                rows.insert(after.identity().clone(), after.clone());
            }
            ChangeOperation::Updated {
                after,
                semantics: UpdateSemantics::Patch,
                ..
            } => {
                let patch = GraphChangeCodec::decode_patch(after)?;
                let mut element = Element::try_from(patch.image().clone())?;
                if let Some(previous) = rows.get(after.identity()) {
                    let previous = Element::try_from(GraphChangeCodec::decode_element(previous)?)?;
                    anyhow::ensure!(
                        std::mem::discriminant(&element) == std::mem::discriminant(&previous),
                        "an update changed an element's kind"
                    );
                    element.merge_missing_properties_except(&previous, patch.removed_properties());
                }
                let record = GraphChangeCodec::element_record(&ElementImage::try_from(element)?)?;
                rows.insert(record.identity().clone(), record);
            }
            ChangeOperation::Deleted { identity, .. } => {
                rows.remove(identity.identity());
            }
        }
    }
    Ok(())
}

/// A stable storage key for a record identity (64-bit FNV-1a). Restoring a
/// row verifies that its identity still derives the key it was stored under.
pub fn row_key(identity: &RecordId) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in identity
        .namespace()
        .as_bytes()
        .iter()
        .chain([0u8].iter())
        .chain(identity.value().iter())
    {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

/// Storage mutations for the rows one output changed: the current full image,
/// or `None` for a removed row.
pub fn row_mutations(output: &ChangeEnvelope, after: &Rows) -> Vec<(u64, Option<Bytes>)> {
    let mut seen = std::collections::HashSet::new();
    output
        .changes()
        .operations()
        .iter()
        .map(ChangeOperation::identity)
        .filter(|identity| seen.insert((*identity).clone()))
        .map(|identity| {
            (
                row_key(identity),
                after.get(identity).map(|row| row.payload().clone()),
            )
        })
        .collect()
}

/// Rebuild a stored graph-element row.
pub fn restore_row(key: u64, payload: Bytes) -> Result<Record> {
    let image = ElementImage::decode(&payload, payload.len())?;
    let record = GraphChangeCodec::element_record(&image)?;
    anyhow::ensure!(
        row_key(record.identity()) == key && record.payload() == &payload,
        "stored row does not match its storage key"
    );
    Ok(record)
}

/// The keyed difference from `old` to `new` as graph changes, or `None` when
/// they are equal.
pub fn difference(
    old: &Rows,
    new: &Rows,
    stream: StreamId,
    sequence: u64,
) -> Result<Option<ChangeEnvelope>> {
    let mut keys: Vec<_> = old.keys().chain(new.keys()).collect();
    keys.sort_unstable_by(|a, b| (a.namespace(), a.value()).cmp(&(b.namespace(), b.value())));
    keys.dedup();
    let mut changes = Vec::new();
    for key in keys {
        let element = |row: &Record| -> Result<Element> {
            Ok(Element::try_from(GraphChangeCodec::decode_element(row)?)?)
        };
        changes.push(match (old.get(key), new.get(key)) {
            (None, Some(after)) => SourceChange::Insert {
                element: element(after)?,
            },
            (Some(before), Some(after)) if before.payload() == after.payload() => continue,
            (Some(_), Some(after)) => SourceChange::Replace {
                element: element(after)?,
            },
            (Some(before), None) => SourceChange::Delete {
                metadata: element(before)?.get_metadata().clone(),
            },
            (None, None) => unreachable!("key came from one of the two views"),
        });
    }
    if changes.is_empty() {
        return Ok(None);
    }
    Ok(Some(GraphChangeCodec::encode_changes(
        &changes, stream, sequence, None,
    )?))
}

pub struct OutputState {
    pub dataset: Arc<str>,
    pub rows: Rows,
    pub sequence: u64,
    pub outbox: VecDeque<ChangeEnvelope>,
    pub capacity: NonZeroUsize,
    pub ready: bool,
    /// A clean stop retains committed history; reconstruction fences it.
    pub recovery_ready: bool,
    pub generation: u64,
    pub producer: Option<GraphProducerIdentity>,
    follow: OutputFollowHub,
}

impl OutputState {
    fn new(dataset: Arc<str>, capacity: NonZeroUsize) -> Self {
        Self {
            dataset,
            rows: Rows::new(),
            sequence: 0,
            outbox: VecDeque::new(),
            capacity,
            ready: false,
            recovery_ready: false,
            generation: 0,
            producer: None,
            follow: OutputFollowHub::new(
                NonZeroUsize::new(64).expect("nonzero lease capacity"),
                OutputFollowLimits {
                    max_envelopes: capacity,
                    max_bytes: NonZeroUsize::new(16 * 1024 * 1024).expect("nonzero byte capacity"),
                    lifetime: std::time::Duration::from_secs(300),
                },
            )
            .expect("valid transformer output follow bounds"),
        }
    }

    pub fn cursor(&self) -> ReadResult<OutputCursor> {
        let producer = self.producer.clone().ok_or_else(|| {
            OutputReadError::NotReady("transformer output has no producer identity yet".into())
        })?;
        OutputCursor::try_new(producer, self.sequence)
    }

    pub fn head(&self) -> ReadResult<OutputCursor> {
        if !self.ready {
            return Err(OutputReadError::NotReady(
                "transformer output is not ready".into(),
            ));
        }
        self.cursor()
    }

    pub fn snapshot(&self) -> ReadResult<OutputSnapshot> {
        OutputSnapshot::try_new(
            self.head()?,
            self.dataset.to_string(),
            GraphChangeCodec::schema().descriptor().clone(),
            self.rows.clone(),
        )
    }

    /// Retained outputs after `after`, or the typed gap when retention or the
    /// committed head cannot supply them.
    pub fn replay_after(&self, after: &OutputCursor) -> ReadResult<OutputReplay> {
        let head = self.head()?;
        after.ensure_same_epoch(&head)?;
        let cursor = after.clone();
        let after = after.sequence();
        let oldest = self
            .outbox
            .front()
            .map(|envelope| envelope.system().sequence());
        if after > self.sequence
            || (after < self.sequence
                && oldest.is_none_or(|oldest| after < oldest.saturating_sub(1)))
        {
            return Err(OutputReadError::Gap {
                requested: after,
                oldest: Some(oldest.unwrap_or(self.sequence)),
                latest: self.sequence,
            });
        }
        let envelopes = self
            .outbox
            .iter()
            .filter(|envelope| envelope.system().sequence() > after)
            .cloned()
            .collect();
        OutputReplay::try_new(cursor, head, envelopes)
    }

    fn apply_committed(&mut self, envelope: ChangeEnvelope) -> Result<()> {
        anyhow::ensure!(
            Some(envelope.system().sequence()) == self.sequence.checked_add(1),
            "committed output does not match the next in-memory sequence"
        );
        self.follow.publish(&envelope)?;
        apply_operations(&mut self.rows, envelope.changes().operations())?;
        self.sequence = envelope.system().sequence();
        if self.outbox.len() == self.capacity.get() {
            self.outbox.pop_front();
        }
        self.outbox.push_back(envelope);
        Ok(())
    }

    fn reset_to_sequence(&mut self, sequence: u64, generation: u64) -> Result<()> {
        self.follow.retire()?;
        self.rows.clear();
        self.outbox.clear();
        self.sequence = sequence;
        self.generation = generation;
        Ok(())
    }
}

/// Validates an output envelope against the logic's own description of it.
pub type Verify<'a> =
    &'a (dyn Fn(&ChangeEnvelope, &GraphProducerIdentity, u64) -> Result<()> + Sync);

/// Refuses discarding an output that belongs to another owner.
pub type VerifyOwner<'a> =
    &'a (dyn Fn(&ChangeEnvelope, &GraphProducerIdentity) -> Result<()> + Sync);

/// The executing transformer's output. It owns mutation; its readers share it.
pub struct TransformerOutput {
    instance: String,
    graph: String,
    component: ComponentId,
    stream: StreamId,
    state: RwLock<OutputState>,
    notify: tokio::sync::Notify,
    observations: Observations,
}

impl TransformerOutput {
    pub(crate) fn new(
        instance: String,
        graph: String,
        component: ComponentId,
        stream: StreamId,
        dataset: Arc<str>,
        capacity: NonZeroUsize,
        observations: Observations,
    ) -> Self {
        Self {
            instance,
            graph,
            component,
            stream,
            state: RwLock::new(OutputState::new(dataset, capacity)),
            notify: tokio::sync::Notify::new(),
            observations,
        }
    }

    pub fn component(&self) -> &ComponentId {
        &self.component
    }

    pub fn stream(&self) -> &StreamId {
        &self.stream
    }

    /// Read the current state. Hold the guard only briefly.
    pub fn read(&self) -> Result<RwLockReadGuard<'_, OutputState>> {
        self.state
            .read()
            .map_err(|_| anyhow::anyhow!("transformer output state poisoned"))
    }

    fn write(&self) -> Result<std::sync::RwLockWriteGuard<'_, OutputState>> {
        self.state
            .write()
            .map_err(|_| anyhow::anyhow!("transformer output state poisoned"))
    }

    /// Resolves after the next state or readiness change.
    pub fn changed(&self) -> tokio::sync::futures::Notified<'_> {
        self.notify.notified()
    }

    pub async fn wait_ready(&self) -> Result<()> {
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.read()?.ready {
                return Ok(());
            }
            notified.await;
        }
    }

    pub(crate) fn increase_retention(&self, capacity: NonZeroUsize) -> Result<()> {
        let mut state = self.write()?;
        anyhow::ensure!(
            capacity >= state.capacity,
            "an update cannot discard retained output"
        );
        state.follow.increase_envelope_limit(capacity)?;
        state.capacity = capacity;
        Ok(())
    }

    pub(crate) fn next_volatile_epoch(&self) -> Result<(GraphProducerIdentity, u64)> {
        let generation = {
            let state = self.read()?;
            if state.producer.is_some() {
                state
                    .generation
                    .checked_add(1)
                    .context("transformer output generation exhausted")?
            } else {
                state.generation
            }
        };
        let producer = GraphProducerIdentity::volatile(
            self.instance.clone(),
            self.graph.clone(),
            self.component.clone(),
            self.stream.clone(),
        )?;
        Ok((producer, generation))
    }

    pub(crate) fn validate_producer(&self, producer: &GraphProducerIdentity) -> Result<()> {
        producer.validate()?;
        anyhow::ensure!(
            producer.construction_scope() == self.instance
                && producer.graph_id() == self.graph
                && producer.component_id() == &self.component
                && producer.stream() == &self.stream,
            "restored transformer output belongs to another output scope"
        );
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn restore(
        &self,
        producer: GraphProducerIdentity,
        sequence: u64,
        rows: Rows,
        available: bool,
        retained: Vec<ChangeEnvelope>,
        generation: u64,
        verify: Verify<'_>,
    ) -> Result<()> {
        self.validate_producer(&producer)?;
        let retained = retained
            .into_iter()
            .map(|envelope| {
                let progress = GraphProducerProgress::from_envelope(&envelope)?
                    .context("retained output omitted producer progress")?;
                anyhow::ensure!(
                    progress.identity() == &producer,
                    "retained output belongs to another producer"
                );
                verify(&envelope, &producer, generation)?;
                Ok(envelope.reemit(progress.sequence())?)
            })
            .collect::<Result<Vec<_>>>()?;
        let mut state = self.write()?;
        state.follow.retire()?;
        state.rows = rows;
        state.generation = generation;
        state.sequence = sequence;
        let skip = retained.len().saturating_sub(state.capacity.get());
        state.outbox = retained.into_iter().skip(skip).collect();
        state.producer = Some(producer);
        state.ready = available;
        state.recovery_ready = available;
        drop(state);
        self.notify.notify_waiters();
        self.observe_state()
    }

    pub(crate) fn activate(&self) -> Result<()> {
        let mut state = self.write()?;
        anyhow::ensure!(
            state.producer.is_some(),
            "transformer output has not initialized"
        );
        state.ready = true;
        state.recovery_ready = true;
        drop(state);
        self.notify.notify_waiters();
        Ok(())
    }

    pub(crate) fn begin_activation(&self) -> Result<()> {
        let mut state = self.write()?;
        state.ready = false;
        state.recovery_ready = false;
        state.follow.retire()?;
        drop(state);
        self.notify.notify_waiters();
        Ok(())
    }

    pub(crate) fn suspend(&self) -> Result<()> {
        let mut state = self.write()?;
        state.ready = false;
        state.follow.retire()?;
        drop(state);
        self.notify.notify_waiters();
        Ok(())
    }

    pub(crate) fn apply_bootstrap(&self, envelope: &ChangeEnvelope) -> Result<()> {
        let mut state = self.write()?;
        anyhow::ensure!(!state.ready, "bootstrap rows must remain unpublished");
        anyhow::ensure!(
            state.producer.is_some(),
            "transformer output has not initialized"
        );
        apply_operations(&mut state.rows, envelope.changes().operations())?;
        drop(state);
        self.observe_state()
    }

    pub(crate) fn rows_at(&self, cut: &OutputCursor) -> Result<Rows> {
        let state = self.read()?;
        anyhow::ensure!(
            state.producer.as_ref() == Some(cut.producer()) && state.sequence == cut.sequence(),
            "transformer output changed before rebuild"
        );
        Ok(state.rows.clone())
    }

    pub(crate) fn row_count(&self) -> Result<u64> {
        let state = self.read()?;
        anyhow::ensure!(
            state.producer.is_some(),
            "transformer output is not initialized"
        );
        Ok(u64::try_from(state.rows.len())?)
    }

    /// The rows after applying `envelope`, without publishing it.
    pub(crate) fn rows_after(&self, envelope: &ChangeEnvelope) -> Result<Rows> {
        let state = self.read()?;
        anyhow::ensure!(
            state.producer.is_some(),
            "transformer output is not initialized"
        );
        let mut rows = state.rows.clone();
        drop(state);
        apply_operations(&mut rows, envelope.changes().operations())?;
        Ok(rows)
    }

    pub(crate) fn clear(&self) -> Result<()> {
        let mut state = self.write()?;
        state.ready = false;
        state.recovery_ready = false;
        state.reset_to_sequence(0, 0)?;
        state.producer = None;
        drop(state);
        self.notify.notify_waiters();
        self.observe_state()
    }

    pub(crate) fn processing(self: &Arc<Self>) -> PublicationGuard {
        PublicationGuard {
            output: self.clone(),
            complete: false,
        }
    }

    /// Describe an output: the logic's own annotations, then the producer
    /// position downstream readers resume from.
    pub(crate) fn stamp(
        &self,
        envelope: &mut ChangeEnvelope,
        sequence: u64,
        decorate: &dyn Fn(&mut ChangeEnvelope, &GraphProducerIdentity, u64) -> Result<()>,
    ) -> Result<()> {
        let state = self.read()?;
        let producer = state
            .producer
            .clone()
            .context("transformer output has no producer identity")?;
        let generation = state.generation;
        drop(state);
        anyhow::ensure!(
            producer.component_id() == &self.component
                && producer.stream() == envelope.system().stream(),
            "transformer output belongs to another producer output"
        );
        decorate(envelope, &producer, generation)?;
        GraphProducerProgress::annotate(envelope, &producer, sequence)?;
        Ok(())
    }

    /// Validate acknowledgements without advancing durable progress. The
    /// processing owner commits the returned position before publishing it.
    pub(crate) fn validate_delivery(
        &self,
        outputs: &[OutputEnvelope],
        mut confirmed: u64,
        pending: Option<&ChangeEnvelope>,
        verify: Verify<'_>,
    ) -> Result<u64> {
        let state = self.read()?;
        let producer = state
            .producer
            .as_ref()
            .context("transformer output has no producer identity")?;
        for output in outputs {
            let progress = GraphProducerProgress::from_envelope(&output.envelope)?;
            let sequence = progress.as_ref().map_or(0, GraphProducerProgress::sequence);
            anyhow::ensure!(
                output.port.as_str() == super::OUTPUT_PORT
                    && output.envelope.system().stream() == &self.stream
                    && progress
                        .as_ref()
                        .is_some_and(|progress| progress.identity() == producer),
                "delivery confirmation belongs to another output lifetime"
            );
            verify(&output.envelope, producer, state.generation)?;
            if sequence > confirmed {
                anyhow::ensure!(
                    confirmed.checked_add(1) == Some(sequence) && sequence <= state.sequence,
                    "delivery confirmation would skip pending output"
                );
                if let Some(pending) = pending {
                    anyhow::ensure!(
                        output.envelope.id() == pending.id()
                            && output.envelope.system().sequence() == pending.system().sequence()
                            && output.envelope.changes().schema() == pending.changes().schema()
                            && output.envelope.changes().operations()
                                == pending.changes().operations(),
                        "confirmation does not match pending output"
                    );
                }
                confirmed = sequence;
            }
        }
        Ok(confirmed)
    }

    /// Commit a stamped output to the in-memory view and its followers.
    pub(crate) fn publish(&self, envelope: &ChangeEnvelope, verify: Verify<'_>) -> Result<()> {
        let mut state = self.write()?;
        let producer = state
            .producer
            .clone()
            .context("transformer output has no producer identity")?;
        verify(envelope, &producer, state.generation)?;
        let progress = GraphProducerProgress::from_envelope(envelope)?
            .context("transformer output omitted producer progress")?;
        anyhow::ensure!(
            progress.identity() == &producer,
            "committed output belongs to another producer identity"
        );
        let fresh = progress.sequence() > state.sequence;
        state.apply_committed(envelope.reemit(progress.sequence())?)?;
        drop(state);
        self.notify.notify_waiters();
        if fresh {
            self.observations.results(envelope)?;
        }
        self.observe_state()
    }

    fn observe_state(&self) -> Result<()> {
        if !self.observations.enabled() {
            return Ok(());
        }
        let state = self.read()?;
        self.observations
            .state(state.outbox.len(), state.rows.len())
    }

    fn read_state(&self) -> ReadResult<RwLockReadGuard<'_, OutputState>> {
        self.state.read().map_err(|_| {
            OutputReadError::Failed(anyhow::anyhow!("transformer output state poisoned"))
        })
    }
}

/// A failed or cancelled publication cannot leave ready readers or live leases.
pub(crate) struct PublicationGuard {
    output: Arc<TransformerOutput>,
    complete: bool,
}

impl PublicationGuard {
    pub(crate) fn complete(mut self) {
        self.complete = true;
    }
}

impl Drop for PublicationGuard {
    fn drop(&mut self) {
        if self.complete {
            return;
        }
        match self.output.state.write() {
            Ok(mut state) => {
                state.ready = false;
                state.recovery_ready = false;
                if let Err(error) = state.follow.retire() {
                    log::error!("Failed to retire transformer output readers: {error:#}");
                }
            }
            Err(error) => log::error!("Failed to fence poisoned transformer output: {error}"),
        }
        self.output.notify.notify_waiters();
    }
}

#[async_trait]
impl SnapshotProvider for TransformerOutput {
    async fn snapshot(&self) -> ReadResult<OutputSnapshot> {
        self.read_state()?.snapshot()
    }
}

#[async_trait]
impl SnapshotFollowProvider for TransformerOutput {
    async fn snapshot_and_follow(
        &self,
        limits: OutputFollowLimits,
    ) -> ReadResult<OutputSnapshotFollow> {
        let mut state = self.state.write().map_err(|_| {
            OutputReadError::Failed(anyhow::anyhow!("transformer output state poisoned"))
        })?;
        let snapshot = state.snapshot()?;
        state.follow.open(snapshot, limits)
    }
}

#[async_trait]
impl ReplayProvider for TransformerOutput {
    async fn committed_head(&self) -> ReadResult<OutputCursor> {
        self.read_state()?.head()
    }

    async fn replay_after(&self, after: &OutputCursor) -> ReadResult<OutputReplay> {
        self.read_state()?.replay_after(after)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use drasi_core_types::models::{
        ElementMetadata, ElementPropertyMap, ElementReference, ElementValue,
    };
    use std::collections::BTreeSet;

    fn node(id: &str, properties: &[(&str, i64)]) -> Element {
        let mut map = ElementPropertyMap::new();
        for (name, value) in properties {
            map.insert(name, ElementValue::Integer(*value));
        }
        Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new("source", id),
                labels: Arc::from(vec![Arc::from("Item")]),
                effective_from: 1,
            },
            properties: map,
        }
    }

    fn envelope(changes: &[SourceChange], sequence: u64) -> ChangeEnvelope {
        GraphChangeCodec::encode_changes(
            changes,
            StreamId::try_new("items").unwrap(),
            sequence,
            None,
        )
        .unwrap()
    }

    #[test]
    fn patches_merge_into_the_current_row_and_keys_restore() {
        let mut rows = Rows::new();
        let insert = envelope(
            &[SourceChange::Insert {
                element: node("a", &[("x", 1), ("y", 2)]),
            }],
            1,
        );
        apply_operations(&mut rows, insert.changes().operations()).unwrap();
        let patch = envelope(
            &[SourceChange::Update {
                element: node("a", &[("x", 5)]),
                removed_properties: BTreeSet::from(["y".to_owned()]),
            }],
            2,
        );
        apply_operations(&mut rows, patch.changes().operations()).unwrap();
        let [row] = rows
            .values()
            .cloned()
            .collect::<Vec<_>>()
            .try_into()
            .unwrap();
        let Element::Node { properties, .. } =
            Element::try_from(GraphChangeCodec::decode_element(&row).unwrap()).unwrap()
        else {
            panic!("node")
        };
        assert_eq!(properties.get("x"), Some(&ElementValue::Integer(5)));
        assert_eq!(properties.get("y"), None);

        let mutations = row_mutations(&patch, &rows);
        let [(key, Some(payload))] = mutations.as_slice() else {
            panic!("one stored row")
        };
        let (key, payload) = (*key, payload.clone());
        assert_eq!(restore_row(key, payload).unwrap(), row);
    }

    #[test]
    fn difference_is_keyed_and_omits_unchanged_rows() {
        let mut old = Rows::new();
        let mut new = Rows::new();
        apply_operations(
            &mut old,
            envelope(
                &[
                    SourceChange::Insert {
                        element: node("same", &[("x", 1)]),
                    },
                    SourceChange::Insert {
                        element: node("gone", &[("x", 1)]),
                    },
                    SourceChange::Insert {
                        element: node("changed", &[("x", 1)]),
                    },
                ],
                1,
            )
            .changes()
            .operations(),
        )
        .unwrap();
        apply_operations(
            &mut new,
            envelope(
                &[
                    SourceChange::Insert {
                        element: node("same", &[("x", 1)]),
                    },
                    SourceChange::Insert {
                        element: node("changed", &[("x", 2)]),
                    },
                    SourceChange::Insert {
                        element: node("new", &[("x", 1)]),
                    },
                ],
                1,
            )
            .changes()
            .operations(),
        )
        .unwrap();
        let delta = difference(&old, &new, StreamId::try_new("items").unwrap(), 7)
            .unwrap()
            .unwrap();
        assert_eq!(delta.changes().operations().len(), 3);
        let mut applied = old.clone();
        apply_operations(&mut applied, delta.changes().operations()).unwrap();
        assert_eq!(applied, new);
        assert!(
            difference(&new, &new, StreamId::try_new("items").unwrap(), 8)
                .unwrap()
                .is_none()
        );
    }
}

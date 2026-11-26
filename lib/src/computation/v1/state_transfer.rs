// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Delivery of a captured output view. Only the host sees the transport.

use super::{
    emission_id, BinaryEnvelopeCodec, ChangeEnvelope, ChangeOperation, ChangeSet, ChangeSetId,
    OutputReadError, OutputSnapshot, Record, SchemaDescriptor, StreamId, SystemMetadata,
};
use bytes::Bytes;
use std::{
    collections::VecDeque,
    iter::{FusedIterator, Peekable},
    num::{NonZeroU64, NonZeroUsize},
};

mod file;
pub(super) use file::{file_scope, FileTransfer};
mod lane;

pub(super) use lane::file_lane;

type ReadResult<T> = std::result::Result<T, OutputReadError>;

/// Hard per-envelope limits. The byte limit is the *complete* native binary
/// envelope, not the sum of the record payload lengths.
#[derive(Clone, Copy, Debug)]
pub struct SnapshotBatchLimits {
    pub max_records: NonZeroUsize,
    pub max_bytes: NonZeroUsize,
}

impl Default for SnapshotBatchLimits {
    fn default() -> Self {
        Self {
            max_records: const { NonZeroUsize::new(128).expect("nonzero batch size") },
            max_bytes: const { NonZeroUsize::new(1024 * 1024).expect("nonzero byte limit") },
        }
    }
}

/// How the host delivers a baseline. This does not select whether to follow.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub enum StateTransfer {
    /// Emit bounded batches on demand, without spooling the view.
    #[default]
    Stream,
    /// Seal the entire view before import. The per-read disk quota includes
    /// all framing. Exceeding it fails the read; there is no streaming fallback.
    TemporaryFile { max_file_bytes: NonZeroU64 },
}

/// Options common to one-shot and snapshot-plus-follow reads.
#[derive(Clone, Copy, Debug, Default)]
pub struct CurrentStateOptions {
    pub batches: SnapshotBatchLimits,
    pub transfer: StateTransfer,
}

type Records = Peekable<Box<dyn Iterator<Item = Record> + Send>>;

/// Transport-independent, lazy iterator of ordinary full-image additions.
/// Iterates the immutable view in its own traversal order; it does not sort,
/// collect all keys, or impose an order on independently constructed maps.
/// The caller supplies a fresh session stream: envelope sequences on it are
/// not positions on the producer's live stream. The cut is `snapshot.cursor()`.
pub struct SnapshotBatches {
    records: Option<Records>,
    pending: VecDeque<Record>,
    stream: StreamId,
    schema: SchemaDescriptor,
    limits: SnapshotBatchLimits,
    sequence: u64,
}

impl SnapshotBatches {
    /// Clones the persistent map root, not its records. The iterator can be
    /// moved to a distributor task and outlive the borrowed snapshot.
    pub fn new(snapshot: &OutputSnapshot, stream: StreamId, limits: SnapshotBatchLimits) -> Self {
        Self::from_records(
            snapshot.records().clone().into_iter().map(|(_, row)| row),
            stream,
            snapshot.schema().clone(),
            limits,
            0,
        )
    }

    fn from_records(
        records: impl Iterator<Item = Record> + Send + 'static,
        stream: StreamId,
        schema: SchemaDescriptor,
        limits: SnapshotBatchLimits,
        sequence: u64,
    ) -> Self {
        let records: Box<dyn Iterator<Item = Record> + Send> = Box::new(records);
        Self {
            records: Some(records.peekable()),
            pending: VecDeque::new(),
            stream,
            schema,
            limits,
            sequence,
        }
    }

    fn batch(&mut self) -> ReadResult<Option<ChangeEnvelope>> {
        let Some(records) = &mut self.records else {
            return Ok(None);
        };
        // A cheap lower bound avoids pulling a count-sized batch of huge
        // records. The codec, not this estimate, decides what fits below.
        if self.pending.is_empty() {
            let mut remaining = self.limits.max_bytes.get();
            while self.pending.len() < self.limits.max_records.get() {
                let Some(record) = records.peek() else {
                    break;
                };
                let id = record.identity();
                let minimum = record
                    .payload()
                    .len()
                    .saturating_add(id.value().len())
                    .saturating_add(id.namespace().len());
                if !self.pending.is_empty() && minimum > remaining {
                    break;
                }
                remaining = remaining.saturating_sub(minimum);
                if let Some(record) = records.next() {
                    self.pending.push_back(record);
                }
            }
        }
        let mut count = self.pending.len();
        if count == 0 {
            return Ok(None);
        }
        self.sequence = self
            .sequence
            .checked_add(1)
            .ok_or_else(|| OutputReadError::invalid("snapshot batch sequence exhausted"))?;
        loop {
            let envelope = self.envelope(count).map_err(OutputReadError::Failed)?;
            let bytes = BinaryEnvelopeCodec::encoded_size(&envelope)
                .map_err(|error| OutputReadError::Failed(error.into()))?;
            if bytes <= self.limits.max_bytes.get() {
                self.pending.drain(..count);
                return Ok(Some(envelope));
            }
            if count == 1 {
                return Err(OutputReadError::invalid(format!(
                    "one snapshot record requires a {bytes}-byte envelope; batch limit is {}",
                    self.limits.max_bytes
                )));
            }
            // Split only this bounded batch; keep its ordered suffix for
            // subsequent calls. Never encode growing prefixes per added row.
            count /= 2;
        }
    }

    fn envelope(&self, count: usize) -> anyhow::Result<ChangeEnvelope> {
        let operations = self
            .pending
            .iter()
            .take(count)
            .enumerate()
            .map(|(ordinal, after)| ChangeOperation::Added {
                ordinal: ordinal as u64,
                after: after.clone(),
            })
            .collect();
        let changes = ChangeSet::try_new(
            ChangeSetId::try_new(
                self.stream.as_str(),
                Bytes::copy_from_slice(&self.sequence.to_be_bytes()),
            )?,
            self.schema.clone(),
            operations,
        )?;
        Ok(ChangeEnvelope::new(
            emission_id(&self.stream, self.sequence)?,
            changes,
            SystemMetadata::new(self.stream.clone(), self.sequence),
        ))
    }
}

impl Iterator for SnapshotBatches {
    type Item = ReadResult<ChangeEnvelope>;

    fn next(&mut self) -> Option<Self::Item> {
        let result = self.batch();
        if !matches!(result, Ok(Some(_))) {
            self.records = None;
            self.pending.clear();
        }
        result.transpose()
    }
}

impl FusedIterator for SnapshotBatches {}

/// Reuse the materialized batch encoder one provider page at a time. Page
/// ordinals belong to this read, never to the live producer's history.
pub(super) fn scan_batches(
    scan: super::StateScan,
    stream: StreamId,
    schema: SchemaDescriptor,
    limits: SnapshotBatchLimits,
) -> super::StateScan {
    use futures::StreamExt;
    futures::stream::try_unfold(
        (scan, None::<SnapshotBatches>, 0),
        move |(mut scan, mut page, mut sequence)| {
            let stream = stream.clone();
            let schema = schema.clone();
            async move {
                loop {
                    if let Some(batch) = page.as_mut().and_then(Iterator::next) {
                        let batch = batch?;
                        sequence = batch.system().sequence();
                        return Ok(Some((batch, (scan, page, sequence))));
                    }
                    let Some(frame) = scan.next().await.transpose()? else {
                        return Ok(None);
                    };
                    drasi_computation_contracts::validate_state_batch(&frame, &schema)?;
                    let count = frame.changes().operations().len();
                    let rows =
                        (0..count).map(move |index| match &frame.changes().operations()[index] {
                            ChangeOperation::Added { after, .. } => after.clone(),
                            _ => unreachable!("validated snapshot page"),
                        });
                    page = Some(SnapshotBatches::from_records(
                        rows,
                        stream.clone(),
                        schema.clone(),
                        limits,
                        sequence,
                    ));
                }
            }
        },
    )
    .boxed()
}

// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::{
    binlog::Position,
    progress::{self, ConsumerBinding, Starting},
};
use anyhow::{Context, Result};
use bytes::Bytes;
use drasi_computation_contracts::{
    exchange::{SourceEvent, SourceEventWrapper},
    ComponentId, ConsumerProgressSnapshot, ConsumerProgressUpdates, GraphChangeCodec,
    InputPositionComparator, OutputEnvelope, PortId, SourceProgressKey, SourceProgressSnapshot,
    SourceTransactionCodec, SourceTransactionLimits, StreamId, MAX_SOURCE_POSITION_BYTES,
};
use serde::{Deserialize, Serialize};
use std::{
    cmp::Ordering,
    collections::BTreeMap,
    sync::{
        atomic::{AtomicU64, Ordering as AtomicOrdering},
        Arc,
    },
};

#[derive(Clone, Copy, Debug, Default)]
pub struct Options {
    pub persistent: bool,
}

/// Row coverage is distinct from the existing transaction cursor. Counters are
/// transport/logical allocation, never a substitute for physical binlog order.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RowPosition {
    pub version: u32,
    pub position: Position,
    pub offset: Option<u32>,
    pub count: u32,
    pub sequence: u64,
}

impl RowPosition {
    pub fn validate(&self) -> Result<()> {
        self.position.sequence()?;
        anyhow::ensure!(self.version == 1, "unsupported MySQL row cursor version");
        match self.offset {
            Some(offset) => anyhow::ensure!(
                self.position.version == 1
                    && self.count > 0
                    && offset < self.count
                    && self.sequence > 0,
                "invalid MySQL transaction row offset/count"
            ),
            None => anyhow::ensure!(
                self.position.version == 2 && self.count == 0 && self.sequence == 0,
                "invalid MySQL after-commit row snapshot boundary"
            ),
        }
        Ok(())
    }
    pub fn completed(&self) -> bool {
        self.offset
            .is_none_or(|offset| offset.checked_add(1) == Some(self.count))
    }
    pub fn encode(&self) -> Result<Bytes> {
        self.validate()?;
        let bytes = serde_json::to_vec(self)?;
        anyhow::ensure!(
            bytes.len() <= MAX_SOURCE_POSITION_BYTES,
            "MySQL row cursor exceeds position limit"
        );
        Ok(Bytes::from(bytes))
    }
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        anyhow::ensure!(
            !bytes.is_empty() && bytes.len() <= MAX_SOURCE_POSITION_BYTES,
            "MySQL row cursor exceeds position limit"
        );
        let value: Self = serde_json::from_slice(bytes)?;
        value.validate()?;
        Ok(value)
    }
}

pub fn compare(left: &RowPosition, right: &RowPosition) -> Result<Ordering> {
    left.validate()?;
    right.validate()?;
    let physical = progress::compare(&left.position, &right.position)?;
    if physical != Ordering::Equal {
        return Ok(physical);
    }
    if let (Some(left_offset), Some(right_offset)) = (left.offset, right.offset) {
        anyhow::ensure!(
            left.count == right.count,
            "MySQL row cursors disagree on transaction size"
        );
        return Ok(left_offset.cmp(&right_offset));
    }
    Ok(match (left.completed(), right.completed()) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Greater,
        (false, true) => Ordering::Less,
        (false, false) => unreachable!("two partial rows compared above"),
    })
}

pub fn checkpoint(
    state: &SourceProgressSnapshot,
    source: &ComponentId,
    binding: [u8; 32],
) -> Result<Option<RowPosition>> {
    let Some(checkpoint) = state
        .checkpoints
        .get(&SourceProgressKey::Source(source.to_string()))
    else {
        return Ok(None);
    };
    let position = RowPosition::decode(
        checkpoint
            .source_position
            .as_deref()
            .context("MySQL row checkpoint has no cursor")?,
    )?;
    anyhow::ensure!(
        position.position.binding == binding && position.sequence == checkpoint.sequence,
        "MySQL row checkpoint binding or sequence mismatch"
    );
    Ok(Some(position))
}

pub struct Positions;
impl InputPositionComparator for Positions {
    fn is_after(&self, event: &[u8], handled: &[u8]) -> Result<bool> {
        Ok(
            compare(&RowPosition::decode(event)?, &RowPosition::decode(handled)?)?
                == Ordering::Greater,
        )
    }
}

/// Keep one bounded transaction's decoded rows, not an additional envelope
/// buffer. Row envelopes are allocated only as the owned worker admits them.
pub struct Emission {
    position: Position,
    changes: std::iter::Enumerate<std::vec::IntoIter<drasi_core_types::models::SourceChange>>,
    count: u32,
    source: ComponentId,
    stream: StreamId,
    sequence: Arc<AtomicU64>,
    timestamp: chrono::DateTime<chrono::Utc>,
}

impl Emission {
    pub fn new(
        group: OutputEnvelope,
        source: ComponentId,
        stream: StreamId,
        sequence: Arc<AtomicU64>,
        limits: SourceTransactionLimits,
    ) -> Result<Self> {
        let transaction = SourceTransactionCodec::decode(&group.envelope, limits)?;
        let position = serde_json::from_slice(transaction.position())?;
        let changes = transaction.into_changes();
        let count = changes
            .len()
            .try_into()
            .context("too many MySQL transaction rows")?;
        Ok(Self {
            position,
            changes: changes.into_iter().enumerate(),
            count,
            source,
            stream,
            sequence,
            timestamp: group
                .envelope
                .system()
                .timestamp()
                .context("MySQL transaction timestamp missing")?,
        })
    }
}
impl Iterator for Emission {
    type Item = Result<OutputEnvelope>;
    fn next(&mut self) -> Option<Self::Item> {
        let (offset, change) = self.changes.next()?;
        Some((|| {
            let sequence = self
                .sequence
                .fetch_update(AtomicOrdering::Relaxed, AtomicOrdering::Relaxed, |value| {
                    value.checked_add(1)
                })
                .map_err(|_| anyhow::anyhow!("MySQL row sequence exhausted"))?
                + 1;
            let position = RowPosition {
                version: 1,
                position: self.position.clone(),
                offset: Some(offset.try_into()?),
                count: self.count,
                sequence,
            };
            let event = SourceEventWrapper::new(
                self.source.to_string(),
                SourceEvent::Change(change),
                self.timestamp,
                sequence,
            )
            .with_source_position(position.encode()?);
            Ok(OutputEnvelope {
                port: PortId::try_new("out")?,
                envelope: GraphChangeCodec::encode_source_event(
                    Arc::new(event),
                    &self.source,
                    self.stream.clone(),
                    sequence,
                    None,
                )?,
            })
        })())
    }
}

pub fn validate(state: &SourceProgressSnapshot, options: Options) -> Result<()> {
    anyhow::ensure!(
        state.ready
            && state.recovered
            && state.admitting
            && state.failure.is_none()
            && (!options.persistent || state.persistent),
        "MySQL row consumer is unready, failed or missing required persistence"
    );
    Ok(())
}

pub fn minimum<'a>(
    initial: &'a Starting,
    source: &ComponentId,
    binding: [u8; 32],
    options: Options,
) -> Result<Option<&'a SourceProgressSnapshot>> {
    let mut selected: Option<(&SourceProgressSnapshot, Option<RowPosition>)> = None;
    let mut history = None;
    let mut boundaries = BTreeMap::new();
    for state in initial.states() {
        validate(state, options)?;
        let position = checkpoint(state, source, binding)?;
        if let Some(position) = &position {
            if let Some(history) = &history {
                compare(position, history)?;
            } else {
                history = Some(position.clone());
            }
            if let Some(previous) = boundaries.insert(
                (position.position.file.clone(), position.position.end),
                position.clone(),
            ) {
                compare(position, &previous)?;
            }
        }
        let earlier = match (&selected, &position) {
            (None, _) | (Some((_, Some(_))), None) => true,
            (Some((_, Some(previous))), Some(position)) => {
                compare(position, previous)? == Ordering::Less
            }
            _ => false,
        };
        if earlier {
            selected = Some((state, position));
        }
    }
    Ok(selected.map(|(state, _)| state))
}

struct Member {
    generation: u64,
    initial: u64,
    position: Option<RowPosition>,
}

pub struct Updates {
    updates: Box<dyn ConsumerProgressUpdates>,
    members: BTreeMap<ComponentId, Member>,
    options: Options,
}

impl Updates {
    pub fn new(
        binding: &ConsumerBinding,
        initial: &ConsumerProgressSnapshot,
        source: &ComponentId,
        identity: [u8; 32],
        options: Options,
    ) -> Result<Self> {
        anyhow::ensure!(
            binding.members.iter().eq(initial.consumers.keys()),
            "MySQL row membership changed"
        );
        let members = initial
            .consumers
            .iter()
            .map(|(id, state)| {
                validate(state, options)?;
                let position = checkpoint(state, source, identity)?;
                Ok((
                    id.clone(),
                    Member {
                        generation: state.reset_generation,
                        initial: position.as_ref().map_or(0, |position| position.sequence),
                        position,
                    },
                ))
            })
            .collect::<Result<_>>()?;
        Ok(Self {
            updates: binding.view.subscribe()?,
            members,
            options,
        })
    }

    pub fn validate(
        &mut self,
        source: &ComponentId,
        identity: [u8; 32],
        admitted: u64,
    ) -> Result<()> {
        let group = self.updates.snapshot()?;
        anyhow::ensure!(
            self.members.keys().eq(group.consumers.keys()),
            "MySQL row membership changed"
        );
        for (id, state) in group.consumers {
            validate(&state, self.options)?;
            let member = self
                .members
                .get_mut(&id)
                .context("missing MySQL row consumer")?;
            anyhow::ensure!(
                state.reset_generation == member.generation,
                "MySQL row consumer reset requires ordered source cleanup and reinitialization"
            );
            let position = checkpoint(&state, source, identity)?;
            if let Some(previous) = &member.position {
                let next = position
                    .as_ref()
                    .context("MySQL row checkpoint disappeared")?;
                anyhow::ensure!(
                    compare(next, previous)? != Ordering::Less
                        && next.sequence >= previous.sequence,
                    "MySQL row progress moved backwards"
                );
            }
            anyhow::ensure!(
                position.as_ref().map_or(0, |value| value.sequence) <= admitted.max(member.initial),
                "MySQL row progress exceeds actual source admission"
            );
            member.position = position;
        }
        Ok(())
    }
    pub async fn changed(&mut self) -> Result<()> {
        self.updates.changed().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(offset: u32, sequence: u64) -> RowPosition {
        RowPosition {
            version: 1,
            position: Position {
                version: 1,
                binding: [1; 32],
                file: "mysql-bin.999999".into(),
                start: 4,
                end: 100,
                digest: [3; 32],
            },
            offset: Some(offset),
            count: 3,
            sequence,
        }
    }
    fn boundary() -> RowPosition {
        let mut value = row(0, 0);
        value.position.version = 2;
        value.position.digest = [7; 32];
        value.offset = None;
        value.count = 0;
        value
    }
    #[test]
    fn offsets_and_completed_boundaries_ignore_independent_counters() -> Result<()> {
        assert_eq!(compare(&row(0, 1000), &row(1, 1))?, Ordering::Less);
        assert_eq!(compare(&row(1, 1000), &boundary())?, Ordering::Less);
        assert_eq!(compare(&row(2, 1), &boundary())?, Ordering::Equal);
        assert_eq!(compare(&row(0, 500), &row(0, 1))?, Ordering::Equal);
        let mut rotated = row(0, 1);
        rotated.position.file = "mysql-bin.1000000".into();
        assert_eq!(compare(&rotated, &row(2, 9999))?, Ordering::Greater);
        Ok(())
    }
    #[test]
    fn incompatible_history_and_transaction_shapes_never_compare_as_duplicates() {
        for change in ["binding", "digest", "count", "offset", "history", "kind"] {
            let mut invalid = row(0, 1);
            match change {
                "binding" => invalid.position.binding = [2; 32],
                "digest" => invalid.position.digest = [8; 32],
                "count" => invalid.count = 4,
                "offset" => invalid.offset = Some(3),
                "history" => invalid.position.file = "other-bin.999999".into(),
                "kind" => invalid.position.version = 2,
                _ => unreachable!(),
            }
            assert!(compare(&invalid, &row(0, 1)).is_err(), "{change}");
        }
        let old = serde_json::to_vec(&row(0, 1).position).unwrap();
        assert!(
            RowPosition::decode(&old).is_err(),
            "transaction state must not become row state"
        );
    }
}

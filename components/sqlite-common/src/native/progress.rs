// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use anyhow::Result;
use drasi_computation_contracts::{
    ComponentId, ConsumerProgressSnapshot, ConsumerProgressUpdates, ConsumerProgressView,
    SourceProgressKey, SourceProgressSnapshot, SourceProgressUpdates, SourceProgressView,
};
use std::{collections::BTreeMap, sync::Arc};

#[derive(Clone)]
pub enum Reader {
    Single(SourceProgressView),
    Group(ConsumerProgressView),
}

#[derive(Clone)]
pub enum Observed {
    Single(Arc<SourceProgressSnapshot>),
    Group(ConsumerProgressSnapshot),
}

pub enum Generations {
    Single(u64),
    Group(BTreeMap<ComponentId, u64>),
}

impl Observed {
    pub fn generations(&self) -> Generations {
        match self {
            Self::Single(state) => Generations::Single(state.reset_generation),
            Self::Group(states) => Generations::Group(
                states
                    .consumers
                    .iter()
                    .map(|(id, state)| (id.clone(), state.reset_generation))
                    .collect(),
            ),
        }
    }

    pub fn has_checkpoint(&self, source: &ComponentId) -> bool {
        let key = SourceProgressKey::Source(source.clone());
        match self {
            Self::Single(state) => state.checkpoints.contains_key(&key),
            Self::Group(states) => states
                .consumers
                .values()
                .any(|state| state.checkpoints.contains_key(&key)),
        }
    }

    pub fn ready(&self) -> bool {
        match self {
            Self::Single(state) => state.ready,
            Self::Group(states) => states.ready(),
        }
    }
}

impl Reader {
    pub fn subscribe(&self) -> Result<Updates> {
        Ok(match self {
            Self::Single(reader) => Updates::Single(reader.subscribe()?),
            Self::Group(reader) => Updates::Group(reader.subscribe()?),
        })
    }
}

pub enum Updates {
    Single(Box<dyn SourceProgressUpdates>),
    Group(Box<dyn ConsumerProgressUpdates>),
}

impl Updates {
    pub fn snapshot(&mut self) -> Result<Observed> {
        Ok(match self {
            Self::Single(updates) => Observed::Single(updates.snapshot()?),
            Self::Group(updates) => Observed::Group(updates.snapshot()?),
        })
    }

    pub async fn changed(&mut self) -> Result<()> {
        match self {
            Self::Single(updates) => updates.changed().await,
            Self::Group(updates) => updates.changed().await,
        }
    }
}

/// Waits for the single owner and returns its reader and first
/// snapshot, advancing `sequence` past the owner's last transport sequence.
pub async fn prepare_single(
    reader: SourceProgressView,
    stream: &drasi_computation_contracts::StreamId,
    sequence: &std::sync::atomic::AtomicU64,
) -> Result<(Reader, Observed)> {
    // The factory declares that this progress must be durable; the graph
    // refuses a volatile binding before the source is constructed.
    let snapshot = reader.wait_ready().await?;
    sequence.fetch_max(
        snapshot
            .transport_sequences
            .get(stream)
            .copied()
            .unwrap_or(0),
        std::sync::atomic::Ordering::Relaxed,
    );
    Ok((Reader::Single(reader), Observed::Single(snapshot)))
}

/// Waits for every consumer of the fixed group and returns the group reader
/// and its first snapshot. When `members` is given, the recovered membership
/// must equal it.
pub async fn prepare_group(
    reader: ConsumerProgressView,
    members: Option<&[ComponentId]>,
    stream: &drasi_computation_contracts::StreamId,
    sequence: &std::sync::atomic::AtomicU64,
) -> Result<(Reader, Observed)> {
    let snapshot = reader.wait_ready().await?;
    if let Some(members) = members {
        anyhow::ensure!(
            snapshot.consumers.keys().eq(members.iter()),
            "SQLite consumer membership changed"
        );
    }
    for (consumer, state) in &snapshot.consumers {
        anyhow::ensure!(
            state.recovered,
            "native SQLite replay requires recovered progress for consumer {consumer}"
        );
        sequence.fetch_max(
            state.transport_sequences.get(stream).copied().unwrap_or(0),
            std::sync::atomic::Ordering::Relaxed,
        );
    }
    Ok((Reader::Group(reader), Observed::Group(snapshot)))
}

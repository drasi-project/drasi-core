// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::{binlog::Position, source};
use anyhow::Result;
use drasi_computation_contracts::{
    ComponentId, ConsumerProgressSnapshot, ConsumerProgressUpdates, ConsumerProgressView,
    InputPositionComparator, SourceProgressSnapshot, MAX_CONSUMER_PROGRESS_MEMBERS,
};
use std::{
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

#[cfg(test)]
mod tests;

pub fn compare(left: &Position, right: &Position) -> Result<Ordering> {
    let left_sequence = left.sequence()?;
    let right_sequence = right.sequence()?;
    anyhow::ensure!(
        left.binding == right.binding
            && left.file.rsplit_once('.').map(|part| part.0)
                == right.file.rsplit_once('.').map(|part| part.0),
        "MySQL positions belong to different source histories"
    );
    let order = left_sequence.cmp(&right_sequence);
    if order == Ordering::Equal {
        anyhow::ensure!(
            left.file == right.file && (left.version != right.version || left == right),
            "MySQL positions disagree about the same committed boundary"
        );
    }
    Ok(order)
}

pub struct Positions;

/// Fixed membership supplied by the actual host capability, never a derived
/// aggregate of whichever members currently happen to be ready.
#[derive(Clone)]
pub struct ConsumerBinding {
    pub view: ConsumerProgressView,
    pub members: BTreeSet<ComponentId>,
}

impl ConsumerBinding {
    pub fn new(
        producer: &ComponentId,
        view: ConsumerProgressView,
        members: &[ComponentId],
    ) -> Result<Self> {
        let expected: BTreeSet<_> = members.iter().cloned().collect();
        anyhow::ensure!(
            view.producer_id() == producer
                && !view.graph_id().is_empty()
                && !expected.is_empty()
                && expected.len() == members.len()
                && expected.len() <= MAX_CONSUMER_PROGRESS_MEMBERS
                && !expected.contains(producer),
            "invalid MySQL consumer binding"
        );
        Ok(Self {
            view,
            members: expected,
        })
    }
    pub fn graph_id(&self) -> &str {
        self.view.graph_id()
    }
    pub fn producer_id(&self) -> &ComponentId {
        self.view.producer_id()
    }
    pub fn read_only(&self) -> ConsumerProgressView {
        self.view.clone()
    }
}

impl InputPositionComparator for Positions {
    fn is_after(&self, event: &[u8], handled: &[u8]) -> Result<bool> {
        let event: Position = serde_json::from_slice(event)?;
        let handled: Position = serde_json::from_slice(handled)?;
        Ok(compare(&event, &handled)? == Ordering::Greater)
    }
}

pub enum Starting {
    Untracked,
    Single(Arc<SourceProgressSnapshot>),
    Consumers(ConsumerProgressSnapshot),
}

impl Starting {
    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            !matches!(self, Self::Consumers(group) if group.consumers.is_empty()),
            "MySQL consumer membership is empty"
        );
        for state in self.states() {
            validate(state)?;
        }
        Ok(())
    }

    pub fn states(&self) -> impl Iterator<Item = &Arc<SourceProgressSnapshot>> {
        let single = match self {
            Self::Single(state) => Some(state),
            _ => None,
        };
        let consumers = match self {
            Self::Consumers(states) => Some(&states.consumers),
            _ => None,
        };
        single
            .into_iter()
            .chain(consumers.into_iter().flat_map(|states| states.values()))
    }

    pub fn minimum(
        &self,
        producer: &ComponentId,
        binding: [u8; 32],
    ) -> Result<Option<&SourceProgressSnapshot>> {
        self.validate()?;
        let mut selected: Option<(&SourceProgressSnapshot, Option<Position>)> = None;
        let mut history = None;
        for state in self.states() {
            let position = source::checkpoint(state, producer, binding)?;
            if let Some(position) = &position {
                if let Some(history) = &history {
                    compare(position, history)?;
                } else {
                    history = Some(position.clone());
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
}

fn validate(state: &SourceProgressSnapshot) -> Result<()> {
    anyhow::ensure!(
        state.ready
            && state.recovered
            && state.admitting
            && state.persistent
            && state.failure.is_none(),
        "MySQL replay requires ready, admitting, persistent and unfenced consumer progress"
    );
    Ok(())
}

struct Member {
    generation: u64,
    initial: u64,
    confirmed: u64,
}

pub struct ConsumerUpdates {
    updates: Box<dyn ConsumerProgressUpdates>,
    members: BTreeMap<ComponentId, Member>,
}

impl ConsumerUpdates {
    pub fn new(
        group: &ConsumerBinding,
        initial: &ConsumerProgressSnapshot,
        producer: &ComponentId,
        binding: [u8; 32],
    ) -> Result<Self> {
        anyhow::ensure!(
            group.members.iter().eq(initial.consumers.keys()),
            "MySQL initial consumer membership changed"
        );
        let members = initial
            .consumers
            .iter()
            .map(|(id, state)| {
                validate(state)?;
                let confirmed = source::checkpoint(state, producer, binding)?
                    .as_ref()
                    .map(Position::sequence)
                    .transpose()?
                    .unwrap_or(0);
                Ok((
                    id.clone(),
                    Member {
                        generation: state.reset_generation,
                        initial: confirmed,
                        confirmed,
                    },
                ))
            })
            .collect::<Result<_>>()?;
        Ok(Self {
            updates: group.read_only().subscribe()?,
            members,
        })
    }

    pub fn validate(
        &mut self,
        producer: &ComponentId,
        binding: [u8; 32],
        admitted: u64,
    ) -> Result<u64> {
        let snapshot = self.updates.snapshot()?;
        anyhow::ensure!(
            self.members.keys().eq(snapshot.consumers.keys()),
            "MySQL active consumer membership changed"
        );
        for (id, state) in snapshot.consumers {
            validate(&state)?;
            let member = self.members.get_mut(&id).expect("validated membership");
            anyhow::ensure!(
                state.reset_generation == member.generation,
                "MySQL consumer was reset while replication was active"
            );
            let sequence = source::checkpoint(&state, producer, binding)?
                .as_ref()
                .map(Position::sequence)
                .transpose()?
                .unwrap_or(0);
            anyhow::ensure!(
                sequence >= member.confirmed && sequence <= admitted.max(member.initial),
                "MySQL consumer progress moved outside its admitted range"
            );
            member.confirmed = sequence;
        }
        self.members
            .values()
            .map(|member| member.confirmed)
            .min()
            .ok_or_else(|| anyhow::anyhow!("MySQL consumer membership is empty"))
    }

    pub async fn changed(&mut self) -> Result<()> {
        self.updates.changed().await
    }
}

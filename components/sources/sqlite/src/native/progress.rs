// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use anyhow::Result;
use drasi_computation_contracts::{
    ComponentId, ConsumerProgressSnapshot, ConsumerProgressUpdates, ConsumerProgressView,
    SourceProgressKey, SourceProgressSnapshot, SourceProgressUpdates, SourceProgressView,
};
use std::{collections::BTreeMap, sync::Arc};

#[derive(Clone)]
pub(super) enum Reader {
    Single(SourceProgressView),
    Group(ConsumerProgressView),
}

#[derive(Clone)]
pub(super) enum Observed {
    Single(Arc<SourceProgressSnapshot>),
    Group(ConsumerProgressSnapshot),
}

pub(super) enum Generations {
    Single(u64),
    Group(BTreeMap<ComponentId, u64>),
}

impl Observed {
    pub(super) fn generations(&self) -> Generations {
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

    pub(super) fn has_checkpoint(&self, source: &ComponentId) -> bool {
        let key = SourceProgressKey::Source(source.to_string());
        match self {
            Self::Single(state) => state.checkpoints.contains_key(&key),
            Self::Group(states) => states
                .consumers
                .values()
                .any(|state| state.checkpoints.contains_key(&key)),
        }
    }

    pub(super) fn ready(&self) -> bool {
        match self {
            Self::Single(state) => state.ready,
            Self::Group(states) => states.ready(),
        }
    }
}

impl Reader {
    pub(super) fn subscribe(&self) -> Result<Updates> {
        Ok(match self {
            Self::Single(reader) => Updates::Single(reader.subscribe()?),
            Self::Group(reader) => Updates::Group(reader.subscribe()?),
        })
    }
}

pub(super) enum Updates {
    Single(Box<dyn SourceProgressUpdates>),
    Group(Box<dyn ConsumerProgressUpdates>),
}

impl Updates {
    pub(super) fn snapshot(&mut self) -> Result<Observed> {
        Ok(match self {
            Self::Single(updates) => Observed::Single(updates.snapshot()?),
            Self::Group(updates) => Observed::Group(updates.snapshot()?),
        })
    }

    pub(super) async fn changed(&mut self) -> Result<()> {
        match self {
            Self::Single(updates) => updates.changed().await,
            Self::Group(updates) => updates.changed().await,
        }
    }
}

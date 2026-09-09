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

//! Migrated A6/A7 output projection, sequence and retained-history ownership.

use std::{
    collections::VecDeque,
    num::NonZeroUsize,
    sync::{Arc, RwLock},
};

use crate::computation::v1::{ChangeEnvelope, ChangeOperation, Record, RecordId};

#[derive(Clone)]
pub struct QuerySnapshot {
    pub as_of_sequence: u64,
    pub rows: im::HashMap<RecordId, Record>,
}

#[derive(Debug, thiserror::Error)]
pub enum QueryHistoryError {
    #[error("query history after {requested} is unavailable; oldest retained sequence {oldest}, latest {latest}")]
    Unavailable {
        requested: u64,
        oldest: u64,
        latest: u64,
    },
    #[error("query output state is poisoned")]
    Poisoned,
}

pub(crate) struct QueryOutputState {
    pub(crate) rows: im::HashMap<RecordId, Record>,
    pub(crate) sequence: u64,
    pub(crate) outbox: VecDeque<ChangeEnvelope>,
    pub(crate) capacity: NonZeroUsize,
}

impl QueryOutputState {
    pub(crate) fn new(capacity: NonZeroUsize) -> Self {
        Self {
            rows: im::HashMap::new(),
            sequence: 0,
            outbox: VecDeque::new(),
            capacity,
        }
    }

    pub(crate) fn next_sequence(&self) -> anyhow::Result<u64> {
        self.sequence
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("query output sequence exhausted"))
    }

    pub(crate) fn apply(&mut self, envelope: ChangeEnvelope) -> anyhow::Result<()> {
        if envelope.system().sequence() != self.next_sequence()? {
            anyhow::bail!("committed output does not match the next in-memory sequence");
        }
        for operation in envelope.changes().operations() {
            match operation {
                ChangeOperation::Added { after, .. } | ChangeOperation::Updated { after, .. } => {
                    self.rows.insert(after.identity().clone(), after.clone());
                }
                ChangeOperation::Deleted { identity, .. } => {
                    self.rows.remove(identity.identity());
                }
            }
        }
        self.sequence = envelope.system().sequence();
        if self.outbox.len() == self.capacity.get() {
            self.outbox.pop_front();
        }
        self.outbox.push_back(envelope);
        Ok(())
    }

    pub(crate) fn hydrate(
        &mut self,
        rows: im::HashMap<RecordId, Record>,
        sequence: u64,
        outbox: Vec<ChangeEnvelope>,
    ) {
        self.rows = rows;
        self.sequence = sequence;
        let skip = outbox.len().saturating_sub(self.capacity.get());
        self.outbox = outbox.into_iter().skip(skip).collect();
    }

    pub(crate) fn reset_to_sequence(&mut self, sequence: u64) {
        self.rows.clear();
        self.outbox.clear();
        self.sequence = sequence;
    }
}

/// Read-only access to one graph-owned CQ's typed snapshot and retained outputs.
#[derive(Clone)]
pub struct QueryResults {
    pub(crate) state: Arc<RwLock<QueryOutputState>>,
}

impl QueryResults {
    pub fn snapshot(&self) -> Result<QuerySnapshot, QueryHistoryError> {
        let state = self.state.read().map_err(|_| QueryHistoryError::Poisoned)?;
        Ok(QuerySnapshot {
            as_of_sequence: state.sequence,
            rows: state.rows.clone(),
        })
    }

    pub fn replay(&self, after: u64) -> Result<Vec<ChangeEnvelope>, QueryHistoryError> {
        let state = self.state.read().map_err(|_| QueryHistoryError::Poisoned)?;
        let oldest = state
            .outbox
            .front()
            .map(|envelope| envelope.system().sequence());
        if after > state.sequence
            || (after < state.sequence
                && oldest.map_or(true, |oldest| after < oldest.saturating_sub(1)))
        {
            return Err(QueryHistoryError::Unavailable {
                requested: after,
                oldest: oldest.unwrap_or(state.sequence),
                latest: state.sequence,
            });
        }
        Ok(state
            .outbox
            .iter()
            .filter(|envelope| envelope.system().sequence() > after)
            .cloned()
            .collect())
    }
}

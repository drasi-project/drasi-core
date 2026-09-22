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

use crate::computation::v1::{
    ChangeEnvelope, ChangeOperation, QueryRecoveryIdentity, Record, RecordId,
};

#[derive(Clone)]
pub struct QuerySnapshot {
    pub as_of_sequence: u64,
    pub generation: u64,
    pub rows: im::HashMap<RecordId, Record>,
}

/// One coherent recovery observation. Rows share the immutable snapshot tree;
/// retained envelopes are copied only when a caller requests a replay suffix.
#[derive(Clone)]
pub struct QueryRecoveryView {
    pub identity: QueryRecoveryIdentity,
    pub snapshot: QuerySnapshot,
    pub oldest_sequence: Option<u64>,
    pub retained: Option<Vec<ChangeEnvelope>>,
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
    #[error("query output is not ready for recovery")]
    NotReady,
    #[error("query output has no verified producer recovery identity")]
    MissingIdentity,
}

pub(crate) struct QueryOutputState {
    pub(crate) rows: im::HashMap<RecordId, Record>,
    pub(crate) sequence: u64,
    pub(crate) outbox: VecDeque<ChangeEnvelope>,
    pub(crate) capacity: NonZeroUsize,
    pub(crate) ready: bool,
    // A clean stop retains committed history; reconstruction/bootstrap fences it.
    pub(crate) recovery_ready: bool,
    pub(crate) generation: u64,
    pub(crate) identity: Option<QueryRecoveryIdentity>,
}

impl QueryOutputState {
    pub(crate) fn new(capacity: NonZeroUsize) -> Self {
        Self {
            rows: im::HashMap::new(),
            sequence: 0,
            outbox: VecDeque::new(),
            capacity,
            ready: false,
            recovery_ready: false,
            generation: 0,
            identity: None,
        }
    }

    pub(crate) fn next_sequence(&self) -> anyhow::Result<u64> {
        self.sequence
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("query output sequence exhausted"))
    }

    pub(crate) fn apply(&mut self, envelope: ChangeEnvelope) -> anyhow::Result<()> {
        if self.identity.as_ref() != Some(&QueryRecoveryIdentity::from_envelope(&envelope)?)
            || crate::computation::v1::QueryChangeCodec::query_generation(&envelope)?
                != self.generation
        {
            anyhow::bail!("committed output belongs to another producer identity or generation");
        }
        if envelope.system().sequence() != self.next_sequence()? {
            anyhow::bail!("committed output does not match the next in-memory sequence");
        }
        self.apply_rows(&envelope);
        self.sequence = envelope.system().sequence();
        if self.outbox.len() == self.capacity.get() {
            self.outbox.pop_front();
        }
        self.outbox.push_back(envelope);
        Ok(())
    }

    pub(crate) fn apply_rows(&mut self, envelope: &ChangeEnvelope) {
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
    }

    pub(crate) fn hydrate(
        &mut self,
        rows: im::HashMap<RecordId, Record>,
        sequence: u64,
        outbox: Vec<ChangeEnvelope>,
        generation: u64,
        identity: QueryRecoveryIdentity,
    ) {
        self.rows = rows;
        self.generation = generation;
        self.identity = Some(identity);
        self.sequence = sequence;
        let skip = outbox.len().saturating_sub(self.capacity.get());
        self.outbox = outbox.into_iter().skip(skip).collect();
    }

    pub(crate) fn reset_to_sequence(&mut self, sequence: u64, generation: u64) {
        self.rows.clear();
        self.outbox.clear();
        self.sequence = sequence;
        self.generation = generation;
    }
}

/// Read-only access to one graph-owned CQ's typed snapshot and retained outputs.
#[derive(Clone)]
pub struct QueryResults {
    pub(crate) state: Arc<RwLock<QueryOutputState>>,
    pub(crate) notify: Arc<tokio::sync::Notify>,
}

impl QueryResults {
    pub(crate) async fn wait_recovery_ready(&self) -> Result<(), QueryHistoryError> {
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self
                .state
                .read()
                .map_err(|_| QueryHistoryError::Poisoned)?
                .recovery_ready
            {
                return Ok(());
            }
            notified.await;
        }
    }

    pub async fn wait_ready(&self) -> Result<(), QueryHistoryError> {
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self
                .state
                .read()
                .map_err(|_| QueryHistoryError::Poisoned)?
                .ready
            {
                return Ok(());
            }
            notified.await;
        }
    }

    pub fn snapshot(&self) -> Result<QuerySnapshot, QueryHistoryError> {
        let state = self.state.read().map_err(|_| QueryHistoryError::Poisoned)?;
        Ok(QuerySnapshot {
            as_of_sequence: state.sequence,
            generation: state.generation,
            rows: state.rows.clone(),
        })
    }

    pub fn recovery_view(
        &self,
        after: Option<u64>,
    ) -> Result<QueryRecoveryView, QueryHistoryError> {
        let state = self.state.read().map_err(|_| QueryHistoryError::Poisoned)?;
        if !state.recovery_ready {
            return Err(QueryHistoryError::NotReady);
        }
        Ok(QueryRecoveryView {
            identity: state
                .identity
                .clone()
                .ok_or(QueryHistoryError::MissingIdentity)?,
            snapshot: QuerySnapshot {
                as_of_sequence: state.sequence,
                generation: state.generation,
                rows: state.rows.clone(),
            },
            oldest_sequence: state
                .outbox
                .front()
                .map(|envelope| envelope.system().sequence()),
            retained: after.map(|after| {
                state
                    .outbox
                    .iter()
                    .filter(|envelope| envelope.system().sequence() > after)
                    .cloned()
                    .collect()
            }),
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

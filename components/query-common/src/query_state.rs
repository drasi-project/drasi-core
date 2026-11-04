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

//! The query's canonical output projection, sequence and retained-history ownership.

use std::{
    collections::VecDeque,
    num::NonZeroUsize,
    sync::{Arc, RwLock},
};

use drasi_computation_contracts::{
    ChangeEnvelope, ComponentId, GraphProducerIdentity, GraphProducerProgress, OutputEnvelope,
    OutputFollowHub, OutputFollowLimits, QueryChangeCodec, QueryRecoveryIdentity, Record, RecordId,
    ResultNodeProjector, StreamId,
};

pub use drasi_computation_contracts::{
    QueryHistoryError, QueryRecoveryHead, QueryRecoveryView, QueryResultSnapshot,
};

pub struct QueryOutputState {
    pub projector: ResultNodeProjector,
    pub rows: im::HashMap<RecordId, Record>,
    pub sequence: u64,
    pub outbox: VecDeque<ChangeEnvelope>,
    pub capacity: NonZeroUsize,
    pub ready: bool,
    // A clean stop retains committed history; reconstruction/bootstrap fences it.
    pub recovery_ready: bool,
    pub generation: u64,
    pub identity: Option<QueryRecoveryIdentity>,
    pub producer: Option<GraphProducerIdentity>,
    pub follow: OutputFollowHub,
}

impl QueryOutputState {
    pub fn recovery_head(&self) -> Result<QueryRecoveryHead, QueryHistoryError> {
        if !self.recovery_ready {
            return Err(QueryHistoryError::NotReady);
        }
        Ok(QueryRecoveryHead {
            identity: self
                .identity
                .clone()
                .ok_or(QueryHistoryError::MissingIdentity)?,
            sequence: self.sequence,
            generation: self.generation,
        })
    }

    pub fn recovery_view(
        &self,
        after: Option<u64>,
    ) -> Result<QueryRecoveryView, QueryHistoryError> {
        let head = self.recovery_head()?;
        Ok(QueryRecoveryView {
            identity: head.identity,
            snapshot: QueryResultSnapshot {
                projector: self.projector.clone(),
                as_of_sequence: head.sequence,
                generation: head.generation,
                rows: self.rows.clone(),
            },
            oldest_sequence: self
                .outbox
                .front()
                .map(|envelope| envelope.system().sequence()),
            retained: after.map(|after| {
                self.outbox
                    .iter()
                    .filter(|envelope| envelope.system().sequence() > after)
                    .cloned()
                    .collect()
            }),
        })
    }

    pub fn new(capacity: NonZeroUsize, projector: ResultNodeProjector) -> Self {
        Self {
            projector,
            rows: im::HashMap::new(),
            sequence: 0,
            outbox: VecDeque::new(),
            capacity,
            ready: false,
            recovery_ready: false,
            generation: 0,
            identity: None,
            producer: None,
            follow: OutputFollowHub::new(
                NonZeroUsize::new(64).expect("nonzero lease capacity"),
                OutputFollowLimits {
                    max_envelopes: capacity,
                    max_bytes: NonZeroUsize::new(16 * 1024 * 1024).expect("nonzero byte capacity"),
                    lifetime: std::time::Duration::from_secs(300),
                },
            )
            .expect("valid query output follow bounds"),
        }
    }

    pub fn next_sequence(&self) -> anyhow::Result<u64> {
        self.sequence
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("query output sequence exhausted"))
    }

    pub fn apply(&mut self, envelope: ChangeEnvelope) -> anyhow::Result<()> {
        self.validate_identity(&envelope)?;
        self.apply_committed(envelope)
    }

    fn validate_identity(&self, envelope: &ChangeEnvelope) -> anyhow::Result<()> {
        if self.identity.as_ref() != Some(&QueryRecoveryIdentity::from_envelope(envelope)?)
            || QueryChangeCodec::query_generation(envelope)? != self.generation
        {
            anyhow::bail!("committed output belongs to another producer identity or generation");
        }
        Ok(())
    }

    /// The native adapter retains the actual host-validated producer epoch.
    /// Its transport emission counter is not the query's result sequence.
    pub fn apply_producer(&mut self, envelope: ChangeEnvelope) -> anyhow::Result<()> {
        self.validate_identity(&envelope)?;
        let progress = GraphProducerProgress::from_envelope(&envelope)?
            .ok_or_else(|| anyhow::anyhow!("query output omitted producer progress"))?;
        let sequence = QueryChangeCodec::query_sequence(&envelope)?;
        anyhow::ensure!(
            self.producer.as_ref() == Some(progress.identity()) && progress.sequence() == sequence,
            "committed output belongs to another producer identity or position"
        );
        self.apply_committed(envelope.reemit(sequence)?)
    }

    fn apply_committed(&mut self, envelope: ChangeEnvelope) -> anyhow::Result<()> {
        if envelope.system().sequence() != self.next_sequence()? {
            anyhow::bail!("committed output does not match the next in-memory sequence");
        }
        self.follow.publish(&envelope)?;
        self.apply_rows(&envelope);
        self.sequence = envelope.system().sequence();
        if self.outbox.len() == self.capacity.get() {
            self.outbox.pop_front();
        }
        self.outbox.push_back(envelope);
        Ok(())
    }

    /// Retained outputs after `after`, or the typed gap when retention (or the
    /// committed head) cannot supply them.
    pub fn replay_after(&self, after: u64) -> Result<Vec<ChangeEnvelope>, QueryHistoryError> {
        let oldest = self
            .outbox
            .front()
            .map(|envelope| envelope.system().sequence());
        if after > self.sequence
            || (after < self.sequence
                && oldest.map_or(true, |oldest| after < oldest.saturating_sub(1)))
        {
            return Err(QueryHistoryError::Unavailable {
                requested: after,
                oldest: oldest.unwrap_or(self.sequence),
                latest: self.sequence,
            });
        }
        Ok(self
            .outbox
            .iter()
            .filter(|envelope| envelope.system().sequence() > after)
            .cloned()
            .collect())
    }

    pub fn apply_rows(&mut self, envelope: &ChangeEnvelope) {
        QueryChangeCodec::apply_result_operations(&mut self.rows, envelope.changes().operations());
    }

    pub fn hydrate(
        &mut self,
        rows: im::HashMap<RecordId, Record>,
        sequence: u64,
        outbox: Vec<ChangeEnvelope>,
        generation: u64,
        identity: QueryRecoveryIdentity,
    ) -> anyhow::Result<()> {
        self.restore_projection(rows, sequence, outbox, generation)?;
        self.identity = Some(identity);
        self.producer = None;
        Ok(())
    }

    pub fn hydrate_producer(
        &mut self,
        rows: im::HashMap<RecordId, Record>,
        sequence: u64,
        outbox: Vec<ChangeEnvelope>,
        producer: GraphProducerIdentity,
        identity: QueryRecoveryIdentity,
        generation: u64,
    ) -> anyhow::Result<()> {
        producer.validate()?;
        let outbox = outbox
            .into_iter()
            .map(|envelope| {
                let progress = GraphProducerProgress::from_envelope(&envelope)?
                    .ok_or_else(|| anyhow::anyhow!("retained output omitted producer progress"))?;
                let logical = QueryChangeCodec::query_sequence(&envelope)?;
                anyhow::ensure!(
                    progress.identity() == &producer && progress.sequence() == logical,
                    "retained output belongs to another query producer"
                );
                anyhow::ensure!(
                    QueryRecoveryIdentity::from_envelope(&envelope)? == identity
                        && QueryChangeCodec::query_generation(&envelope)? == generation,
                    "retained output belongs to another query configuration or generation"
                );
                Ok(envelope.reemit(logical)?)
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        self.hydrate(rows, sequence, outbox, generation, identity)?;
        self.producer = Some(producer);
        Ok(())
    }

    fn restore_projection(
        &mut self,
        rows: im::HashMap<RecordId, Record>,
        sequence: u64,
        outbox: Vec<ChangeEnvelope>,
        generation: u64,
    ) -> anyhow::Result<()> {
        self.follow.retire()?;
        self.rows = rows;
        self.generation = generation;
        self.sequence = sequence;
        let skip = outbox.len().saturating_sub(self.capacity.get());
        self.outbox = outbox.into_iter().skip(skip).collect();
        Ok(())
    }

    pub fn reset_to_sequence(&mut self, sequence: u64, generation: u64) -> anyhow::Result<()> {
        self.follow.retire()?;
        self.rows.clear();
        self.outbox.clear();
        self.sequence = sequence;
        self.generation = generation;
        Ok(())
    }
}

/// Read-only access to one graph-owned CQ's typed snapshot and retained outputs.
///
/// ```compile_fail
/// fn cannot_mutate(results: drasi_query_common::QueryResults) {
///     results.state.write().unwrap().rows.clear();
/// }
/// ```
#[derive(Clone)]
pub struct QueryResults {
    pub(crate) state: Arc<RwLock<QueryOutputState>>,
    pub(crate) notify: Arc<tokio::sync::Notify>,
}

/// Mutation authority retained by the executing query, never by its read facade.
#[derive(Clone)]
pub struct QueryOutputOwner {
    pub state: Arc<RwLock<QueryOutputState>>,
    pub notify: Arc<tokio::sync::Notify>,
    reader: QueryResults,
}

impl QueryOutputOwner {
    pub fn new(capacity: NonZeroUsize, projector: ResultNodeProjector) -> Self {
        let state = Arc::new(RwLock::new(QueryOutputState::new(capacity, projector)));
        let notify = Arc::new(tokio::sync::Notify::new());
        Self {
            reader: QueryResults {
                state: state.clone(),
                notify: notify.clone(),
            },
            state,
            notify,
        }
    }

    pub fn reader(&self) -> QueryResults {
        self.reader.clone()
    }

    pub fn begin_activation(&self) -> anyhow::Result<()> {
        let mut state = self
            .state
            .write()
            .map_err(|_| QueryHistoryError::Poisoned)?;
        state.ready = false;
        state.recovery_ready = false;
        state.follow.retire()?;
        drop(state);
        self.notify.notify_waiters();
        Ok(())
    }

    pub fn activate(&self) -> anyhow::Result<()> {
        let mut state = self
            .state
            .write()
            .map_err(|_| QueryHistoryError::Poisoned)?;
        anyhow::ensure!(
            state.identity.is_some(),
            "query output has not initialized its identity"
        );
        state.ready = true;
        state.recovery_ready = true;
        drop(state);
        self.notify.notify_waiters();
        Ok(())
    }

    pub fn suspend(&self) -> anyhow::Result<()> {
        let mut state = self
            .state
            .write()
            .map_err(|_| QueryHistoryError::Poisoned)?;
        state.ready = false;
        state.follow.retire()?;
        drop(state);
        self.notify.notify_waiters();
        Ok(())
    }

    pub fn stamp(
        &self,
        output: &mut ChangeEnvelope,
        component: &ComponentId,
    ) -> anyhow::Result<()> {
        let state = self.state.read().map_err(|_| QueryHistoryError::Poisoned)?;
        let identity = state
            .identity
            .as_ref()
            .ok_or(QueryHistoryError::MissingIdentity)?;
        anyhow::ensure!(
            identity.query_id() == component.as_str(),
            "query output belongs to another component"
        );
        QueryChangeCodec::set_generation(output, component, state.generation)?;
        identity.annotate(output, component)?;
        let producer = match &state.producer {
            Some(producer) => producer.clone(),
            None => identity.output_producer(output.system().stream().clone(), state.generation)?,
        };
        anyhow::ensure!(
            producer.component_id() == component && producer.stream() == output.system().stream(),
            "query output belongs to another producer output"
        );
        let sequence = QueryChangeCodec::query_sequence(output)?;
        GraphProducerProgress::annotate(output, &producer, sequence)?;
        Ok(())
    }

    /// Validate acknowledgements without advancing durable progress. The
    /// processing owner must commit the returned position before publishing it.
    pub fn validate_delivery(
        &self,
        stream: &StreamId,
        outputs: &[OutputEnvelope],
        mut confirmed: u64,
        pending: Option<&ChangeEnvelope>,
    ) -> anyhow::Result<u64> {
        let state = self.state.read().map_err(|_| QueryHistoryError::Poisoned)?;
        let identity = state
            .identity
            .as_ref()
            .ok_or(QueryHistoryError::MissingIdentity)?;
        let producer = match &state.producer {
            Some(producer) => producer.clone(),
            None => identity.output_producer(stream.clone(), state.generation)?,
        };
        for output in outputs {
            let sequence = QueryChangeCodec::query_sequence(&output.envelope)?;
            let progress = GraphProducerProgress::from_envelope(&output.envelope)?;
            anyhow::ensure!(
                output.port.as_str() == "out"
                    && output.envelope.system().stream() == stream
                    && QueryRecoveryIdentity::from_envelope(&output.envelope)? == *identity
                    && QueryChangeCodec::query_generation(&output.envelope)? == state.generation
                    && progress
                        .as_ref()
                        .is_some_and(|progress| progress.identity() == &producer
                            && progress.sequence() == sequence),
                "query delivery confirmation belongs to another output lifetime"
            );
            if sequence > confirmed {
                anyhow::ensure!(
                    confirmed.checked_add(1) == Some(sequence) && sequence <= state.sequence,
                    "query delivery confirmation would skip pending output"
                );
                if let Some(pending) = pending {
                    anyhow::ensure!(
                        output.envelope.id() == pending.id()
                            && output.envelope.system().sequence() == pending.system().sequence()
                            && output.envelope.changes().schema() == pending.changes().schema()
                            && output.envelope.changes().operations()
                                == pending.changes().operations(),
                        "query confirmation does not match pending output"
                    );
                }
                confirmed = sequence;
            }
        }
        Ok(confirmed)
    }

    pub fn processing(&self) -> QueryPublicationGuard {
        QueryPublicationGuard {
            results: self.clone(),
            complete: false,
        }
    }
}

impl std::ops::Deref for QueryOutputOwner {
    type Target = QueryResults;
    fn deref(&self) -> &Self::Target {
        &self.reader
    }
}

pub struct QueryOutputHead {
    pub sequence: u64,
    pub generation: u64,
    pub ready: bool,
}

impl QueryResults {
    pub fn head(&self) -> Result<QueryOutputHead, QueryHistoryError> {
        let state = self.state.read().map_err(|_| QueryHistoryError::Poisoned)?;
        Ok(QueryOutputHead {
            sequence: state.sequence,
            generation: state.generation,
            ready: state.ready,
        })
    }

    pub fn same_owner(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.state, &other.state)
    }

    pub fn changed(&self) -> tokio::sync::futures::Notified<'_> {
        self.notify.notified()
    }

    /// Cleanup observations retain the last generation even after a poisoned operation.
    pub fn observed_generation(&self, query: &str) -> u64 {
        self.state
            .read()
            .unwrap_or_else(|poisoned| {
                log::error!(
                    "Reading stored output generation for '{query}' from poisoned query state"
                );
                poisoned.into_inner()
            })
            .generation
    }

    pub async fn wait_recovery_ready(&self) -> Result<(), QueryHistoryError> {
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

    pub fn snapshot(&self) -> Result<QueryResultSnapshot, QueryHistoryError> {
        let state = self.state.read().map_err(|_| QueryHistoryError::Poisoned)?;
        Ok(QueryResultSnapshot {
            projector: state.projector.clone(),
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
        state.recovery_view(after)
    }

    pub fn recovery_head(&self) -> Result<QueryRecoveryHead, QueryHistoryError> {
        self.state
            .read()
            .map_err(|_| QueryHistoryError::Poisoned)?
            .recovery_head()
    }

    pub fn replay(&self, after: u64) -> Result<Vec<ChangeEnvelope>, QueryHistoryError> {
        self.state
            .read()
            .map_err(|_| QueryHistoryError::Poisoned)?
            .replay_after(after)
    }
}

#[async_trait::async_trait]
impl drasi_computation_contracts::QueryRecoveryReader for QueryResults {
    async fn recovery_head(
        &self,
    ) -> Result<QueryRecoveryHead, drasi_computation_contracts::OutputReadError> {
        self.recovery_head().map_err(Into::into)
    }

    async fn recovery_view(
        &self,
        after: Option<u64>,
    ) -> Result<QueryRecoveryView, drasi_computation_contracts::OutputReadError> {
        self.recovery_view(after).map_err(Into::into)
    }

    async fn wait_recovery(
        &self,
        after: Option<&QueryRecoveryHead>,
    ) -> Result<QueryRecoveryHead, drasi_computation_contracts::OutputReadError> {
        loop {
            let changed = self.changed();
            tokio::pin!(changed);
            changed.as_mut().enable();
            match self.recovery_head() {
                Ok(head) if after != Some(&head) => return Ok(head),
                Ok(_) | Err(QueryHistoryError::NotReady) => {}
                Err(error) => return Err(error.into()),
            }
            changed.await;
        }
    }
}

/// A failed or cancelled publication cannot leave ready readers or live leases.
pub struct QueryPublicationGuard {
    results: QueryOutputOwner,
    complete: bool,
}

impl QueryPublicationGuard {
    pub fn complete(mut self) {
        self.complete = true;
    }
}

impl Drop for QueryPublicationGuard {
    fn drop(&mut self) {
        if self.complete {
            return;
        }
        match self.results.state.write() {
            Ok(mut state) => {
                state.ready = false;
                state.recovery_ready = false;
                if let Err(error) = state.follow.retire() {
                    log::error!("Failed to retire query result readers: {error:#}");
                }
            }
            Err(error) => log::error!("Failed to fence poisoned query output state: {error}"),
        }
        self.results.notify.notify_waiters();
    }
}

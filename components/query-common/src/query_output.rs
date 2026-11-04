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

//! The query's generic output-read providers.
//!
//! They read the query's own output state (canonical rows, retained outbox and
//! committed sequence); there is no second copy of the rows. Their epoch is the
//! same one the query stamps on its published output.

use crate::{QueryHistoryError, QueryOutputState, QueryResults};
use async_trait::async_trait;
use drasi_computation_contracts::*;
use std::sync::Arc;

type ReadResult<T> = std::result::Result<T, OutputReadError>;

/// The query's declared output port.
pub const QUERY_OUTPUT_PORT: &str = "out";

pub struct QueryOutputProvider {
    results: QueryResults,
    stream: StreamId,
}

impl QueryOutputProvider {
    pub fn new(results: &QueryResults, stream: &StreamId) -> Arc<Self> {
        Arc::new(Self {
            results: results.clone(),
            stream: stream.clone(),
        })
    }

    pub fn results(&self) -> QueryResults {
        self.results.clone()
    }

    pub fn readers(self: &Arc<Self>) -> OutputReaders {
        let port = PortId::try_new(QUERY_OUTPUT_PORT).expect("static query output port");
        OutputReaders::new()
            .with_snapshot(port.clone(), self.clone())
            .with_replay(port.clone(), self.clone())
            .with_snapshot_follow(port, self.clone())
    }

    fn state(&self) -> ReadResult<std::sync::RwLockReadGuard<'_, QueryOutputState>> {
        self.results
            .state
            .read()
            .map_err(|_| OutputReadError::Failed(anyhow::anyhow!("query output state poisoned")))
    }

    // The epoch is derived exactly as the query stamps it on published output.
    fn head(&self, state: &QueryOutputState) -> ReadResult<OutputCursor> {
        if !state.ready {
            return Err(OutputReadError::NotReady(
                "query output is not ready".into(),
            ));
        }
        self.cursor(state)
    }

    fn cursor(&self, state: &QueryOutputState) -> ReadResult<OutputCursor> {
        if let Some(producer) = &state.producer {
            return OutputCursor::try_new(producer.clone(), state.sequence);
        }
        let identity = state.identity.as_ref().ok_or_else(|| {
            OutputReadError::NotReady("query output has no producer identity yet".into())
        })?;
        let producer = identity
            .output_producer(self.stream.clone(), state.generation)
            .map_err(OutputReadError::Failed)?;
        OutputCursor::try_new(producer, state.sequence)
    }

    fn snapshot_at(&self, state: &QueryOutputState) -> ReadResult<OutputSnapshot> {
        OutputSnapshot::try_new(
            self.head(state)?,
            state.projector.dataset(),
            GraphChangeCodec::schema().descriptor().clone(),
            state.rows.clone(),
        )
    }

    pub fn query_head(&self) -> ReadResult<(OutputCursor, u64)> {
        let state = self.state()?;
        Ok((self.head(&state)?, state.generation))
    }

    pub fn query_observation(&self) -> ReadResult<QueryOutputObservation> {
        let state = self.state()?;
        self.observation(&state, self.head(&state)?)
    }

    pub fn query_recovery_observation(&self) -> ReadResult<QueryOutputObservation> {
        let state = self.state()?;
        state.recovery_head()?;
        self.observation(&state, self.cursor(&state)?)
    }

    fn observation(
        &self,
        state: &QueryOutputState,
        cursor: OutputCursor,
    ) -> ReadResult<QueryOutputObservation> {
        Ok(QueryOutputObservation {
            cursor,
            generation: state.generation,
            config_hash: state
                .identity
                .as_ref()
                .ok_or_else(|| OutputReadError::NotReady("query has no recovery identity".into()))?
                .configuration_hash(),
            live_results: state.rows.len(),
            outbox_size: state.outbox.len(),
            oldest_sequence: state
                .outbox
                .front()
                .map(|output| output.system().sequence()),
        })
    }

    pub fn query_snapshot(&self) -> ReadResult<(OutputSnapshot, u64)> {
        let state = self.state()?;
        Ok((self.snapshot_at(&state)?, state.generation))
    }

    pub fn query_replay(&self, after: &OutputCursor) -> ReadResult<(OutputReplay, u64)> {
        let state = self.state()?;
        let head = self.head(&state)?;
        after.ensure_same_epoch(&head)?;
        let envelopes = state
            .replay_after(after.sequence())
            .map_err(|error| match error {
                QueryHistoryError::Unavailable {
                    requested,
                    oldest,
                    latest,
                } => OutputReadError::Gap {
                    requested,
                    oldest: Some(oldest),
                    latest,
                },
                other => OutputReadError::Failed(other.into()),
            })?;
        Ok((
            OutputReplay::try_new(after.clone(), head, envelopes)?,
            state.generation,
        ))
    }

    /// Query-facing metadata is captured alongside the same canonical snapshot
    /// and history used by the generic providers, never from a shadow row store.
    pub fn query_view(&self) -> ReadResult<QueryOutputView> {
        self.query_view_with_cursor().map(|(_, view)| view)
    }

    pub fn query_view_with_cursor(&self) -> ReadResult<(OutputCursor, QueryOutputView)> {
        let state = self.state()?;
        let snapshot = self.snapshot_at(&state)?;
        let identity = state.identity.as_ref().ok_or_else(|| {
            OutputReadError::NotReady("query output has no producer identity yet".into())
        })?;
        let view = QueryOutputView {
            snapshot: QueryResultSnapshot {
                projector: state.projector.clone(),
                as_of_sequence: snapshot.cursor().sequence(),
                generation: state.generation,
                rows: snapshot.records().clone(),
            },
            retained: state.outbox.iter().cloned().collect(),
            config_hash: identity.configuration_hash(),
            incarnation: identity.incarnation(),
        };
        Ok((snapshot.cursor().clone(), view))
    }

    pub fn query_recovery_view(&self) -> ReadResult<(OutputCursor, QueryOutputView)> {
        let state = self.state()?;
        let view = state.recovery_view(Some(0))?;
        Ok((
            self.cursor(&state)?,
            QueryOutputView {
                snapshot: view.snapshot,
                retained: view.retained.expect("requested retained history"),
                config_hash: view.identity.configuration_hash(),
                incarnation: view.identity.incarnation(),
            },
        ))
    }
}

pub use drasi_computation_contracts::QueryOutputView;

#[async_trait]
impl SnapshotProvider for QueryOutputProvider {
    async fn snapshot(&self) -> ReadResult<OutputSnapshot> {
        let state = self.state()?;
        self.snapshot_at(&state)
    }
}

#[async_trait]
impl SnapshotFollowProvider for QueryOutputProvider {
    async fn snapshot_and_follow(
        &self,
        limits: OutputFollowLimits,
    ) -> ReadResult<OutputSnapshotFollow> {
        let mut state =
            self.results.state.write().map_err(|_| {
                OutputReadError::Failed(anyhow::anyhow!("query output state poisoned"))
            })?;
        let snapshot = self.snapshot_at(&state)?;
        state.follow.open(snapshot, limits)
    }
}

#[async_trait]
impl ReplayProvider for QueryOutputProvider {
    async fn committed_head(&self) -> ReadResult<OutputCursor> {
        self.query_head().map(|(head, _)| head)
    }

    async fn replay_after(&self, after: &OutputCursor) -> ReadResult<OutputReplay> {
        self.query_replay(after).map(|(replay, _)| replay)
    }
}

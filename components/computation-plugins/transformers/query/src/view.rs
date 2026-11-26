// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Query-specific reads of the shared transformer output: result rows with
//! their projector, configuration identity and recovery observations.

use std::sync::{Arc, RwLockReadGuard};

use async_trait::async_trait;
use drasi_computation_contracts::{
    OutputCursor, OutputReadError, OutputReplay, OutputSnapshot, QueryDescription,
    QueryHistoryError, QueryOutputObservation, QueryOutputView, QueryRecoveryHead,
    QueryRecoveryIdentity, QueryRecoveryReader, QueryRecoveryView, QueryResultSnapshot,
    QueryViewProvider, ResultNodeProjector,
};
use drasi_computation_plugin_sdk::transformer::{OutputState, TransformerOutput};

type ReadResult<T> = std::result::Result<T, OutputReadError>;

pub(super) struct QueryView {
    output: Arc<TransformerOutput>,
    projector: ResultNodeProjector,
    description: QueryDescription,
}

impl QueryView {
    pub(super) fn new(
        output: Arc<TransformerOutput>,
        projector: ResultNodeProjector,
        description: QueryDescription,
    ) -> Self {
        Self {
            output,
            projector,
            description,
        }
    }

    fn state(&self) -> ReadResult<RwLockReadGuard<'_, OutputState>> {
        self.output.read().map_err(OutputReadError::Failed)
    }

    fn identity(&self, state: &OutputState) -> Result<QueryRecoveryIdentity, QueryHistoryError> {
        let producer = state
            .producer
            .as_ref()
            .ok_or(QueryHistoryError::MissingIdentity)?;
        QueryRecoveryIdentity::for_producer(producer, self.description.configuration_hash)
            .map_err(|_| QueryHistoryError::MissingIdentity)
    }

    fn result_snapshot(&self, state: &OutputState) -> QueryResultSnapshot {
        QueryResultSnapshot {
            projector: self.projector.clone(),
            as_of_sequence: state.sequence,
            generation: state.generation,
            rows: state.rows.clone(),
        }
    }

    fn observation(
        &self,
        state: &OutputState,
        cursor: OutputCursor,
    ) -> ReadResult<QueryOutputObservation> {
        Ok(QueryOutputObservation {
            cursor,
            generation: state.generation,
            config_hash: self.description.configuration_hash,
            live_results: state.rows.len(),
            outbox_size: state.outbox.len(),
            oldest_sequence: state
                .outbox
                .front()
                .map(|output| output.system().sequence()),
        })
    }

    fn recovery_head_of(
        &self,
        state: &OutputState,
    ) -> Result<QueryRecoveryHead, QueryHistoryError> {
        if !state.recovery_ready {
            return Err(QueryHistoryError::NotReady);
        }
        Ok(QueryRecoveryHead {
            identity: self.identity(state)?,
            sequence: state.sequence,
            generation: state.generation,
        })
    }

    fn recovery_view_of(
        &self,
        state: &OutputState,
        after: Option<u64>,
    ) -> Result<QueryRecoveryView, QueryHistoryError> {
        let head = self.recovery_head_of(state)?;
        Ok(QueryRecoveryView {
            identity: head.identity,
            snapshot: self.result_snapshot(state),
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

    async fn ready(&self) -> ReadResult<()> {
        self.output
            .wait_ready()
            .await
            .map_err(OutputReadError::Failed)
    }
}

#[async_trait]
impl QueryViewProvider for QueryView {
    fn description(&self) -> QueryDescription {
        let mut description = self.description.clone();
        if let Ok(state) = self.output.read() {
            description.outbox_capacity = state.capacity;
        }
        description
    }

    fn supports_recovery_views(&self) -> bool {
        true
    }

    fn recovery_reader(self: Arc<Self>) -> Option<Arc<dyn QueryRecoveryReader>> {
        Some(self)
    }

    async fn query_recovery_observation(&self) -> ReadResult<QueryOutputObservation> {
        let state = self.state()?;
        self.recovery_head_of(&state)?;
        self.observation(&state, state.cursor()?)
    }

    async fn query_recovery_view(&self) -> ReadResult<(OutputCursor, QueryOutputView)> {
        let state = self.state()?;
        let view = self.recovery_view_of(&state, Some(0))?;
        Ok((
            state.cursor()?,
            QueryOutputView {
                snapshot: view.snapshot,
                retained: view.retained.expect("requested retained history"),
                config_hash: view.identity.configuration_hash(),
                incarnation: view.identity.incarnation(),
            },
        ))
    }

    async fn query_observation(&self) -> ReadResult<QueryOutputObservation> {
        self.ready().await?;
        let state = self.state()?;
        self.observation(&state, state.head()?)
    }

    async fn query_head(&self) -> ReadResult<(OutputCursor, u64)> {
        self.ready().await?;
        let state = self.state()?;
        Ok((state.head()?, state.generation))
    }

    async fn query_snapshot(&self) -> ReadResult<(OutputSnapshot, u64)> {
        self.ready().await?;
        let state = self.state()?;
        Ok((state.snapshot()?, state.generation))
    }

    async fn query_view(&self) -> ReadResult<(OutputCursor, QueryOutputView)> {
        self.ready().await?;
        let state = self.state()?;
        let snapshot = state.snapshot()?;
        let identity = self.identity(&state)?;
        let view = QueryOutputView {
            snapshot: QueryResultSnapshot {
                projector: self.projector.clone(),
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

    async fn query_replay(&self, after: &OutputCursor) -> ReadResult<(OutputReplay, u64)> {
        self.ready().await?;
        let state = self.state()?;
        Ok((state.replay_after(after)?, state.generation))
    }
}

#[async_trait]
impl QueryRecoveryReader for QueryView {
    async fn recovery_head(&self) -> ReadResult<QueryRecoveryHead> {
        Ok(self.recovery_head_of(&*self.state()?)?)
    }

    async fn recovery_view(&self, after: Option<u64>) -> ReadResult<QueryRecoveryView> {
        Ok(self.recovery_view_of(&*self.state()?, after)?)
    }

    async fn wait_recovery(
        &self,
        after: Option<&QueryRecoveryHead>,
    ) -> ReadResult<QueryRecoveryHead> {
        loop {
            let changed = self.output.changed();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let head = self.recovery_head_of(&*self.state()?);
            match head {
                Ok(head) if after != Some(&head) => return Ok(head),
                Ok(_) | Err(QueryHistoryError::NotReady) => {}
                Err(error) => return Err(error.into()),
            }
            changed.await;
        }
    }
}

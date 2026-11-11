// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::{num::NonZeroUsize, sync::Arc};

use async_trait::async_trait;
use chrono::Utc;
use drasi_computation_contracts::{
    ChangeEnvelope, ComponentId, GraphProducerIdentity, OutputCursor, OutputEnvelope,
    OutputFollowLimits, OutputReadError, OutputReplay, OutputSnapshot, OutputSnapshotFollow,
    QueryChangeCodec, QueryDescription, QueryOutputView, QueryRecoveryIdentity, QueryViewProvider,
    Record, RecordId, ReplayProvider, ResultNodeProjector, SnapshotFollowProvider,
    SnapshotProvider, StreamId,
};
use drasi_computation_plugin_sdk::Scope;
use drasi_core::evaluation::context::QueryPartEvaluationContext;
use drasi_query_common::{QueryOutputOwner, QueryOutputProvider, QueryPublicationGuard};

pub(super) type Rows = im::HashMap<RecordId, Record>;

pub(super) struct QueryOutput {
    instance: String,
    graph: String,
    component: ComponentId,
    stream: StreamId,
    projector: ResultNodeProjector,
    results: QueryOutputOwner,
    reader: Arc<QueryOutputProvider>,
    configuration_hash: u64,
    description: QueryDescription,
}

impl QueryOutput {
    pub(super) fn increase_retention(&self, capacity: NonZeroUsize) -> anyhow::Result<()> {
        let mut state = self
            .results
            .state
            .write()
            .map_err(|_| anyhow::anyhow!("query output state poisoned"))?;
        anyhow::ensure!(
            capacity >= state.capacity,
            "query update cannot discard retained output"
        );
        state.follow.increase_envelope_limit(capacity)?;
        state.capacity = capacity;
        Ok(())
    }

    pub(super) fn new(
        scope: Option<&Scope>,
        component: ComponentId,
        stream: StreamId,
        projector: ResultNodeProjector,
        capacity: NonZeroUsize,
        configuration_hash: u64,
        description: QueryDescription,
    ) -> Self {
        let results = QueryOutputOwner::new(capacity, projector.clone());
        let reader = QueryOutputProvider::new(&results, &stream);
        Self {
            instance: scope
                .map_or("native-standalone", |scope| scope.instance_id.as_str())
                .into(),
            graph: scope
                .map_or("native-standalone", |scope| scope.graph_id.as_str())
                .into(),
            component,
            stream,
            projector,
            results,
            reader,
            configuration_hash,
            description,
        }
    }

    pub(super) fn apply_initial(
        &self,
        rows: &mut Rows,
        results: &[QueryPartEvaluationContext],
    ) -> anyhow::Result<()> {
        let timestamp = u64::try_from(Utc::now().timestamp_millis())?;
        let (operations, _) = self
            .projector
            .project_evaluation(results, timestamp)?
            .into_parts();
        QueryChangeCodec::apply_result_operations(rows, &operations);
        Ok(())
    }

    pub(super) fn initialize(&self, rows: Rows) -> anyhow::Result<()> {
        let (producer, generation) = self.next_volatile_epoch()?;
        self.restore(producer, 0, rows, true, Vec::new(), generation)
    }

    pub(super) fn next_volatile_epoch(&self) -> anyhow::Result<(GraphProducerIdentity, u64)> {
        let generation = {
            let state = self
                .results
                .state
                .read()
                .map_err(|_| anyhow::anyhow!("query output state poisoned"))?;
            if state.identity.is_some() {
                state
                    .generation
                    .checked_add(1)
                    .ok_or_else(|| anyhow::anyhow!("query output generation exhausted"))?
            } else {
                state.generation
            }
        };
        let producer = GraphProducerIdentity::volatile(
            self.instance.clone(),
            self.graph.clone(),
            self.component.clone(),
            self.stream.clone(),
        )?;
        Ok((producer, generation))
    }

    pub(super) fn restore(
        &self,
        producer: GraphProducerIdentity,
        sequence: u64,
        rows: Rows,
        available: bool,
        retained: Vec<ChangeEnvelope>,
        generation: u64,
    ) -> anyhow::Result<()> {
        let identity = self.recovery_identity(&producer)?;
        let mut state = self
            .results
            .state
            .write()
            .map_err(|_| anyhow::anyhow!("query output state poisoned"))?;
        state.hydrate_producer(rows, sequence, retained, producer, identity, generation)?;
        state.ready = available;
        state.recovery_ready = available;
        drop(state);
        self.results.notify.notify_waiters();
        Ok(())
    }

    pub(super) fn recovery_identity(
        &self,
        producer: &GraphProducerIdentity,
    ) -> anyhow::Result<QueryRecoveryIdentity> {
        anyhow::ensure!(
            producer.construction_scope() == self.instance
                && producer.graph_id() == self.graph
                && producer.component_id() == &self.component
                && producer.stream() == &self.stream,
            "restored query snapshot belongs to another output scope"
        );
        Ok(QueryRecoveryIdentity::for_producer(
            producer,
            self.configuration_hash,
        )?)
    }

    pub(super) fn activate(&self) -> anyhow::Result<()> {
        let state = self
            .results
            .state
            .read()
            .map_err(|_| anyhow::anyhow!("query output state poisoned"))?;
        anyhow::ensure!(
            state.producer.is_some(),
            "query snapshot has not initialized"
        );
        drop(state);
        self.results.activate()
    }

    pub(super) fn begin_activation(&self) -> anyhow::Result<()> {
        self.results.begin_activation()
    }

    pub(super) fn suspend(&self) -> anyhow::Result<()> {
        self.results.suspend()
    }

    pub(super) fn apply_bootstrap(&self, envelope: &ChangeEnvelope) -> anyhow::Result<()> {
        let mut state = self
            .results
            .state
            .write()
            .map_err(|_| anyhow::anyhow!("query output state poisoned"))?;
        anyhow::ensure!(!state.ready, "bootstrap rows must remain unpublished");
        anyhow::ensure!(
            state.producer.is_some(),
            "query snapshot has not initialized"
        );
        state.apply_rows(envelope);
        Ok(())
    }

    pub(super) fn restore_row(&self, signature: u64, bytes: Vec<u8>) -> anyhow::Result<Record> {
        Ok(QueryChangeCodec::restore_live_row(
            &self.projector,
            signature,
            bytes.into(),
        )?)
    }

    pub(super) fn row_count(&self) -> anyhow::Result<u64> {
        let state = self
            .results
            .state
            .read()
            .map_err(|_| anyhow::anyhow!("query output state poisoned"))?;
        anyhow::ensure!(
            state.producer.is_some(),
            "query snapshot is not initialized"
        );
        Ok(u64::try_from(state.rows.len())?)
    }

    pub(super) fn count_after(&self, envelope: &ChangeEnvelope) -> anyhow::Result<u64> {
        let state = self
            .results
            .state
            .read()
            .map_err(|_| anyhow::anyhow!("query output state poisoned"))?;
        anyhow::ensure!(
            state.producer.is_some(),
            "query snapshot is not initialized"
        );
        let mut rows = state.rows.clone();
        drop(state);
        QueryChangeCodec::apply_result_operations(&mut rows, envelope.changes().operations());
        Ok(u64::try_from(rows.len())?)
    }

    pub(super) fn clear(&self) -> anyhow::Result<()> {
        let mut state = self
            .results
            .state
            .write()
            .map_err(|_| anyhow::anyhow!("query output state poisoned"))?;
        state.ready = false;
        state.recovery_ready = false;
        state.reset_to_sequence(0, 0)?;
        state.identity = None;
        state.producer = None;
        drop(state);
        self.results.notify.notify_waiters();
        Ok(())
    }

    pub(super) fn processing(&self) -> QueryPublicationGuard {
        self.results.processing()
    }

    pub(super) fn stamp(&self, envelope: &mut ChangeEnvelope) -> anyhow::Result<()> {
        self.results.stamp(envelope, &self.component)
    }

    pub(super) fn validate_delivery(
        &self,
        outputs: &[OutputEnvelope],
        confirmed: u64,
        pending: Option<&ChangeEnvelope>,
    ) -> anyhow::Result<u64> {
        self.results
            .validate_delivery(&self.stream, outputs, confirmed, pending)
    }

    pub(super) fn publish(&self, envelope: &mut ChangeEnvelope) -> anyhow::Result<()> {
        let persistent = {
            let state = self
                .results
                .state
                .read()
                .map_err(|_| anyhow::anyhow!("query output state poisoned"))?;
            state
                .producer
                .as_ref()
                .is_some_and(GraphProducerIdentity::persistent)
        };
        if !persistent
            && drasi_computation_contracts::GraphProducerProgress::from_envelope(envelope)?
                .is_none()
        {
            self.stamp(envelope)?;
        }
        let mut state = self
            .results
            .state
            .write()
            .map_err(|_| anyhow::anyhow!("query output state poisoned"))?;
        state.apply_producer(envelope.clone())?;
        drop(state);
        self.results.notify.notify_waiters();
        Ok(())
    }
}

#[async_trait]
impl SnapshotProvider for QueryOutput {
    async fn snapshot(&self) -> Result<OutputSnapshot, OutputReadError> {
        self.reader.snapshot().await
    }
}

#[async_trait]
impl ReplayProvider for QueryOutput {
    async fn committed_head(&self) -> Result<OutputCursor, OutputReadError> {
        self.reader.committed_head().await
    }

    async fn replay_after(&self, after: &OutputCursor) -> Result<OutputReplay, OutputReadError> {
        self.reader.replay_after(after).await
    }
}

#[async_trait]
impl SnapshotFollowProvider for QueryOutput {
    async fn snapshot_and_follow(
        &self,
        limits: OutputFollowLimits,
    ) -> Result<OutputSnapshotFollow, OutputReadError> {
        self.reader.snapshot_and_follow(limits).await
    }
}

#[async_trait]
impl QueryViewProvider for QueryOutput {
    fn supports_recovery_views(&self) -> bool {
        true
    }
    fn recovery_reader(
        self: Arc<Self>,
    ) -> Option<Arc<dyn drasi_computation_contracts::QueryRecoveryReader>> {
        Some(Arc::new(self.reader.results()))
    }
    async fn query_recovery_observation(
        &self,
    ) -> Result<drasi_computation_contracts::QueryOutputObservation, OutputReadError> {
        self.reader.query_recovery_observation()
    }
    async fn query_recovery_view(
        &self,
    ) -> Result<(OutputCursor, QueryOutputView), OutputReadError> {
        self.reader.query_recovery_view()
    }
    fn description(&self) -> QueryDescription {
        let mut description = self.description.clone();
        description.outbox_capacity = self
            .results
            .state
            .read()
            .expect("query output state poisoned")
            .capacity;
        description
    }

    async fn query_observation(
        &self,
    ) -> Result<drasi_computation_contracts::QueryOutputObservation, OutputReadError> {
        self.results
            .wait_ready()
            .await
            .map_err(|error| OutputReadError::Failed(error.into()))?;
        self.reader.query_observation()
    }

    async fn query_head(&self) -> Result<(OutputCursor, u64), OutputReadError> {
        self.results
            .wait_ready()
            .await
            .map_err(|error| OutputReadError::Failed(error.into()))?;
        self.reader.query_head()
    }

    async fn query_snapshot(&self) -> Result<(OutputSnapshot, u64), OutputReadError> {
        self.results
            .wait_ready()
            .await
            .map_err(|error| OutputReadError::Failed(error.into()))?;
        self.reader.query_snapshot()
    }

    async fn query_view(&self) -> Result<(OutputCursor, QueryOutputView), OutputReadError> {
        self.results
            .wait_ready()
            .await
            .map_err(|error| OutputReadError::Failed(error.into()))?;
        self.reader.query_view_with_cursor()
    }

    async fn query_replay(
        &self,
        after: &OutputCursor,
    ) -> Result<(OutputReplay, u64), OutputReadError> {
        self.results
            .wait_ready()
            .await
            .map_err(|error| OutputReadError::Failed(error.into()))?;
        self.reader.query_replay(after)
    }
}

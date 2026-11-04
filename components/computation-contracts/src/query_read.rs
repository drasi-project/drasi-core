// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::num::NonZeroUsize;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::{
    ChangeEnvelope, ComputationQueryLanguage, OutputCursor, OutputReadError, OutputReplay,
    OutputSnapshot, QueryExecutionSettings, QueryResultSnapshot, StreamId,
};

/// Immutable authoring metadata supplied by the actual query component.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QueryDescription {
    pub graph_id: String,
    pub query: String,
    pub language: ComputationQueryLanguage,
    pub execution: QueryExecutionSettings,
    pub output_stream: StreamId,
    pub outbox_capacity: NonZeroUsize,
    pub bootstrap: bool,
    pub auto_reset: bool,
    pub volatile: bool,
    pub configuration_hash: u64,
}

/// One coherent read of the query's canonical rows and retained output.
#[derive(Clone)]
pub struct QueryOutputView {
    pub snapshot: QueryResultSnapshot,
    pub retained: Vec<ChangeEnvelope>,
    pub config_hash: u64,
    pub incarnation: Option<uuid::Uuid>,
}

/// One metadata-only observation of the query's canonical output state.
#[derive(Debug, Clone)]
pub struct QueryOutputObservation {
    pub cursor: OutputCursor,
    pub generation: u64,
    pub config_hash: u64,
    pub live_results: usize,
    pub outbox_size: usize,
    pub oldest_sequence: Option<u64>,
}

impl QueryOutputObservation {
    pub fn validate(
        &self,
        query: &crate::ComponentId,
        description: &QueryDescription,
    ) -> Result<(), OutputReadError> {
        let producer = self.cursor.producer();
        if producer.component_id() != query
            || producer.graph_id() != description.graph_id
            || producer.stream() != &description.output_stream
            || producer.persistent() == description.volatile
            || self.config_hash != description.configuration_hash
            || self.outbox_size > description.outbox_capacity.get()
        {
            return Err(OutputReadError::invalid(
                "query observation disagrees with its definition",
            ));
        }
        let expected = self
            .oldest_sequence
            .and_then(|oldest| self.cursor.sequence().checked_sub(oldest)?.checked_add(1));
        if self.oldest_sequence == Some(0)
            || self.outbox_size == 0 && self.oldest_sequence.is_some()
            || self.outbox_size != 0 && expected != u64::try_from(self.outbox_size).ok()
        {
            return Err(OutputReadError::invalid(
                "query observation has inconsistent history bounds",
            ));
        }
        Ok(())
    }
}

/// Read-only query semantics over an output. It does not own or run an evaluator.
#[async_trait]
pub trait QueryViewProvider: Send + Sync {
    fn description(&self) -> QueryDescription;
    fn supports_recovery_views(&self) -> bool {
        false
    }
    fn recovery_reader(
        self: std::sync::Arc<Self>,
    ) -> Option<std::sync::Arc<dyn crate::QueryRecoveryReader>> {
        None
    }
    async fn query_recovery_observation(&self) -> Result<QueryOutputObservation, OutputReadError> {
        Err(OutputReadError::invalid(
            "query recovery observations are not supported",
        ))
    }
    async fn query_recovery_view(
        &self,
    ) -> Result<(OutputCursor, QueryOutputView), OutputReadError> {
        Err(OutputReadError::invalid(
            "query recovery views are not supported",
        ))
    }
    fn supports_observations(&self) -> bool {
        true
    }
    async fn query_observation(&self) -> Result<QueryOutputObservation, OutputReadError> {
        let (cursor, view) = self.query_view().await?;
        if cursor.sequence() != view.snapshot.as_of_sequence
            || view.incarnation
                != (!cursor.producer().persistent()).then_some(cursor.producer().incarnation())
        {
            return Err(OutputReadError::invalid(
                "query observation is not one coherent view",
            ));
        }
        Ok(QueryOutputObservation {
            cursor,
            generation: view.snapshot.generation,
            config_hash: view.config_hash,
            live_results: view.snapshot.rows.len(),
            outbox_size: view.retained.len(),
            oldest_sequence: view
                .retained
                .first()
                .map(|output| output.system().sequence()),
        })
    }
    async fn query_head(&self) -> Result<(OutputCursor, u64), OutputReadError>;
    async fn query_snapshot(&self) -> Result<(OutputSnapshot, u64), OutputReadError>;
    /// Capture the producer cursor, rows and retained history in one observation.
    async fn query_view(&self) -> Result<(OutputCursor, QueryOutputView), OutputReadError>;
    async fn query_replay(
        &self,
        after: &OutputCursor,
    ) -> Result<(OutputReplay, u64), OutputReadError>;
}

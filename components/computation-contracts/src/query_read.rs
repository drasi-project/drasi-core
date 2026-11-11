// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

#![deny(missing_docs, clippy::unwrap_used, clippy::expect_used)]

//! Query metadata and coherent read-only output observations.

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
    /// Identity of the graph that owns the query.
    pub graph_id: String,
    /// Query text interpreted by the selected language.
    pub query: String,
    /// Language used to parse the query.
    pub language: ComputationQueryLanguage,
    /// Input, middleware and evaluation settings.
    pub execution: QueryExecutionSettings,
    /// Producer stream for query-result changes.
    pub output_stream: StreamId,
    /// Maximum number of retained output envelopes.
    pub outbox_capacity: NonZeroUsize,
    /// Whether bootstrap was requested, not whether it has completed.
    pub bootstrap: bool,
    /// Whether eligible recovery failures may reset the query.
    pub auto_reset: bool,
    /// Whether the query is configured for volatile output.
    pub volatile: bool,
    /// Configuration identity associated with canonical output.
    pub configuration_hash: u64,
}

/// One coherent read of the query's canonical rows and retained output.
#[derive(Clone)]
pub struct QueryOutputView {
    /// Current rows and their canonical output position.
    pub snapshot: QueryResultSnapshot,
    /// History retained at the same observation as the snapshot.
    pub retained: Vec<ChangeEnvelope>,
    /// Configuration identity associated with this view.
    pub config_hash: u64,
    /// Volatile producer incarnation, absent for persistent output.
    pub incarnation: Option<uuid::Uuid>,
}

/// One metadata-only observation of the query's canonical output state.
#[derive(Debug, Clone)]
pub struct QueryOutputObservation {
    /// Current producer identity and output position.
    pub cursor: OutputCursor,
    /// Canonical result generation, not a graph activation generation.
    pub generation: u64,
    /// Configuration identity associated with this observation.
    pub config_hash: u64,
    /// Number of current result rows.
    pub live_results: usize,
    /// Number of retained output envelopes.
    pub outbox_size: usize,
    /// First retained output sequence, absent when history is empty.
    pub oldest_sequence: Option<u64>,
}

impl QueryOutputObservation {
    /// Reject mismatched ownership, configuration or retained-history bounds.
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
    /// Return immutable authoring metadata from the actual query owner.
    fn description(&self) -> QueryDescription;
    /// Whether separately retained recovery observations are implemented.
    fn supports_recovery_views(&self) -> bool {
        false
    }
    /// Return the provider's recovery reader when supported.
    fn recovery_reader(
        self: std::sync::Arc<Self>,
    ) -> Option<std::sync::Arc<dyn crate::QueryRecoveryReader>> {
        None
    }
    /// Observe retained recovery metadata under the recovery reader's lifecycle contract.
    async fn query_recovery_observation(&self) -> Result<QueryOutputObservation, OutputReadError> {
        Err(OutputReadError::invalid(
            "query recovery observations are not supported",
        ))
    }
    /// Read a coherent retained recovery view after the required cleanup.
    async fn query_recovery_view(
        &self,
    ) -> Result<(OutputCursor, QueryOutputView), OutputReadError> {
        Err(OutputReadError::invalid(
            "query recovery views are not supported",
        ))
    }
    /// Whether canonical output observations are available.
    fn supports_observations(&self) -> bool {
        true
    }
    /// Read coherent counts and bounds, falling back to a full view when necessary.
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
    /// Read the producer cursor and canonical result generation.
    async fn query_head(&self) -> Result<(OutputCursor, u64), OutputReadError>;
    /// Read an immutable snapshot and its canonical result generation.
    async fn query_snapshot(&self) -> Result<(OutputSnapshot, u64), OutputReadError>;
    /// Capture the producer cursor, rows and retained history in one observation.
    async fn query_view(&self) -> Result<(OutputCursor, QueryOutputView), OutputReadError>;
    /// Read output after the supplied cursor together with its result generation.
    async fn query_replay(
        &self,
        after: &OutputCursor,
    ) -> Result<(OutputReplay, u64), OutputReadError>;
}

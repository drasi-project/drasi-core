// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::collections::HashMap;

use chrono::Utc;
use drasi_computation_contracts::{
    profiling::{timestamp_ns, ProfilingMetadata},
    ChangeEnvelope, ComponentId, GraphChangeCodec, GraphProducerProgress, QueryChangeCodec,
    QueryCodecError, QueryOutputMetadata, ResultNodeProjector, SourceTransactionCodec,
    SourceTransactionLimits, StreamId, SystemMetadata,
};
use drasi_core::{
    computation::ComputationFutureResult, evaluation::context::QueryPartEvaluationContext,
    interface::IndexError, models::SourceChange,
};

use crate::ContinuousQueryDefinition;

pub struct PreparedQueryInput {
    changes: Option<Vec<SourceChange>>,
    pub source_id: Option<String>,
}

impl PreparedQueryInput {
    pub fn into_changes(self, input: &ChangeEnvelope) -> anyhow::Result<Vec<SourceChange>> {
        match self.changes {
            Some(changes) => Ok(changes),
            None => Ok(GraphChangeCodec::decode_changes(input)?),
        }
    }
}

/// Select the existing ordinary-batch or complete-source-transaction contract.
pub fn prepare_query_input<const SOURCE_TRANSACTION: bool>(
    input: &ChangeEnvelope,
    limits: Option<SourceTransactionLimits>,
) -> anyhow::Result<PreparedQueryInput> {
    if input.system().ordering() == drasi_core::models::ChangeOrdering::ProducerSequence {
        anyhow::ensure!(
            GraphProducerProgress::from_envelope(input)?.is_some(),
            "producer-sequence ordering requires an explicit current output identity"
        );
    }
    if SOURCE_TRANSACTION {
        let limits =
            limits.ok_or_else(|| anyhow::anyhow!("source transaction limits are missing"))?;
        let transaction = SourceTransactionCodec::decode(input, limits)?;
        let source_id = Some(transaction.source_id().to_owned());
        Ok(PreparedQueryInput {
            changes: Some(transaction.into_changes()),
            source_id,
        })
    } else {
        Ok(PreparedQueryInput {
            changes: None,
            source_id: None,
        })
    }
}

/// Existing per-input query execution metadata. Transactions remain owned by
/// the hosting adapter; completion timing is added only after its commit.
pub struct QueryOperation<'a> {
    input: &'a ChangeEnvelope,
    source_id: String,
    profiling: ProfilingMetadata,
}

impl<'a> QueryOperation<'a> {
    pub fn output_sequence(
        current: u64,
        results: &[QueryPartEvaluationContext],
    ) -> anyhow::Result<u64> {
        if results
            .iter()
            .any(|result| !matches!(result, QueryPartEvaluationContext::Noop))
        {
            current
                .checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("query output sequence exhausted"))
        } else {
            Ok(0)
        }
    }

    pub fn new(input: &'a ChangeEnvelope) -> anyhow::Result<Self> {
        let metadata = GraphChangeCodec::source_metadata(input)?;
        let source_id = metadata
            .as_ref()
            .map(|metadata| metadata.source_id.clone())
            .unwrap_or_else(|| input.system().stream().as_str().to_owned());
        Ok(Self::from_metadata(
            input,
            source_id,
            metadata.and_then(|metadata| metadata.profiling),
        ))
    }

    pub fn from_metadata(
        input: &'a ChangeEnvelope,
        source_id: String,
        profiling: Option<ProfilingMetadata>,
    ) -> Self {
        let mut profiling = profiling.unwrap_or_default();
        profiling.query_core_return_ns = None;
        profiling.query_send_ns = None;
        profiling.query_receive_ns = Some(timestamp_ns());
        profiling.query_core_call_ns = Some(timestamp_ns());
        Self {
            input,
            source_id,
            profiling,
        }
    }

    pub fn with_source_id(mut self, source_id: String) -> Self {
        self.source_id = source_id;
        self
    }

    pub fn project(
        &self,
        definition: &ContinuousQueryDefinition,
        projector: &ResultNodeProjector,
        sequence: u64,
        results: &[QueryPartEvaluationContext],
    ) -> Result<Option<ChangeEnvelope>, QueryCodecError> {
        QueryChangeCodec::encode_evaluation(
            Some(self.input),
            &definition.id,
            SystemMetadata::new(definition.output_stream.clone(), sequence)
                .with_timestamp(self.input.system().timestamp().unwrap_or_else(Utc::now)),
            projector,
            results,
            QueryOutputMetadata {
                query_id: definition.id.to_string(),
                source_id: Some(self.source_id.clone()),
                timestamp: Utc::now(),
                metadata: HashMap::new(),
                profiling: Some(self.profiling.clone()),
            },
        )
    }

    pub fn committed(
        output: &mut ChangeEnvelope,
        component: &ComponentId,
        core_return_ns: u64,
    ) -> anyhow::Result<()> {
        QueryChangeCodec::append_post_commit_profiling(
            output,
            component,
            core_return_ns,
            timestamp_ns(),
        )?;
        Ok(())
    }
}

pub fn project_due_query_output(
    definition: &ContinuousQueryDefinition,
    projector: &ResultNodeProjector,
    sequence: u64,
    due: &ComputationFutureResult,
) -> Result<Option<ChangeEnvelope>, IndexError> {
    let timestamp = chrono::DateTime::from_timestamp_millis(
        i64::try_from(due.future.due_time).map_err(IndexError::other)?,
    )
    .ok_or(IndexError::CorruptedData)?;
    let trigger = GraphChangeCodec::encode_change(
        SourceChange::Future {
            future_ref: due.future.clone(),
        },
        StreamId::try_new(format!("{}/future", definition.id)).map_err(IndexError::other)?,
        sequence,
        Some(timestamp),
    )
    .map_err(IndexError::other)?;
    QueryChangeCodec::encode_evaluation(
        Some(&trigger),
        &definition.id,
        SystemMetadata::new(definition.output_stream.clone(), sequence).with_timestamp(timestamp),
        projector,
        &due.results,
        QueryOutputMetadata {
            query_id: definition.id.to_string(),
            source_id: Some(due.source_id.to_string()),
            timestamp: Utc::now(),
            metadata: HashMap::new(),
            profiling: Some(ProfilingMetadata::new()),
        },
    )
    .map_err(IndexError::other)
}

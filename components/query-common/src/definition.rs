// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::num::NonZeroUsize;

use bytes::Bytes;
use drasi_computation_contracts::{
    ComponentDescriptor, ComponentId, ComponentSemanticKind, ComputationQueryLanguage,
    GraphChangeCodec, OutputProgress, PipeRequirements, PortDescriptor, PortDirection, PortId,
    QueryExecutionSettings, ResultNodeProjector, StreamId,
};
use serde::{Deserialize, Serialize};

/// The existing query definition, independent of how its component is hosted.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContinuousQueryDefinition {
    pub graph_id: String,
    pub id: ComponentId,
    pub query: String,
    pub language: ComputationQueryLanguage,
    pub output_stream: StreamId,
    pub outbox_capacity: NonZeroUsize,
}

impl ContinuousQueryDefinition {
    pub fn descriptor(&self) -> ComponentDescriptor {
        query_descriptor(self.id.clone())
    }

    pub fn projector(
        &self,
        instance: &str,
        execution: &QueryExecutionSettings,
    ) -> anyhow::Result<ResultNodeProjector> {
        Ok(ResultNodeProjector::try_new(
            &drasi_computation_contracts::output_identity::dataset_namespace(
                instance,
                &self.graph_id,
                &self.id,
                &PortId::try_new("out")?,
            )?,
            execution.result.label.as_deref(),
        )?)
    }

    pub fn configuration_bytes(&self, execution: &QueryExecutionSettings) -> anyhow::Result<Bytes> {
        let execution = execution.fingerprint_settings();
        if execution.source_transactions.is_some() {
            // Admission limits may increase without changing transaction semantics.
            let mut semantics = execution;
            semantics.source_transactions = None;
            return Ok(Bytes::from(serde_json::to_vec(&(
                6u32,
                &self.query,
                self.language,
                self.output_stream.as_str(),
                &semantics,
                "complete-source-transactions-v1",
                drasi_core::query::SOURCE_CHANGE_SEMANTICS_VERSION,
            ))?));
        }
        Ok(Bytes::from(serde_json::to_vec(&(
            5u32,
            &self.query,
            self.language,
            self.output_stream.as_str(),
            execution,
            drasi_core::query::SOURCE_CHANGE_SEMANTICS_VERSION,
        ))?))
    }

    pub fn configuration_hash(&self, execution: &QueryExecutionSettings) -> anyhow::Result<u64> {
        use std::hash::Hasher;
        let mut hasher = fnv::FnvHasher::default();
        hasher.write(&self.configuration_bytes(execution)?);
        Ok(hasher.finish())
    }
}

pub fn query_descriptor(id: ComponentId) -> ComponentDescriptor {
    ComponentDescriptor::try_new(
        id,
        vec![
            PortDescriptor::new(
                PortId::try_new("in").expect("port"),
                PortDirection::Input,
                GraphChangeCodec::schema().descriptor().clone(),
                PipeRequirements::default(),
            ),
            PortDescriptor::new(
                PortId::try_new("out").expect("port"),
                PortDirection::Output,
                GraphChangeCodec::schema().descriptor().clone(),
                PipeRequirements::default(),
            ),
        ],
    )
    .expect("typed query descriptor")
    .with_semantic_kind(ComponentSemanticKind::Query)
    .with_output_progress(OutputProgress::Origin)
}

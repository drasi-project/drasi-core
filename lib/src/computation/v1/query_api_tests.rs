// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use drasi_computation_contracts::{
    ComputationQueryLanguage, GraphProducerIdentity, GraphProducerProgress, OutputCursor,
    OutputEnvelope, OutputReplay, OutputSnapshot, PortId, QueryChangeCodec, QueryDescription,
    QueryExecutionSettings, QueryOutputMetadata, QueryRecoveryIdentity, StreamId, SystemMetadata,
};
use std::{
    num::NonZeroUsize,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
};

struct Reader {
    description: QueryDescription,
    cursor: OutputCursor,
    active: AtomicBool,
    observations: bool,
    capacity: AtomicUsize,
}

#[async_trait::async_trait]
impl QueryViewProvider for Reader {
    fn supports_observations(&self) -> bool {
        self.observations
    }
    fn description(&self) -> QueryDescription {
        let mut description = self.description.clone();
        description.outbox_capacity =
            NonZeroUsize::new(self.capacity.load(Ordering::Acquire)).unwrap();
        description
    }
    async fn query_head(&self) -> Result<(OutputCursor, u64), OutputReadError> {
        if self.active.load(Ordering::Acquire) {
            Ok((self.cursor.clone(), 9))
        } else {
            Err(OutputReadError::NotReady("stopped".into()))
        }
    }
    async fn query_observation(&self) -> Result<QueryOutputObservation, OutputReadError> {
        let (cursor, generation) = self.query_head().await?;
        Ok(QueryOutputObservation {
            cursor,
            generation,
            config_hash: self.description.configuration_hash,
            live_results: 13,
            outbox_size: 2,
            oldest_sequence: Some(6),
        })
    }
    async fn query_snapshot(&self) -> Result<(OutputSnapshot, u64), OutputReadError> {
        Err(OutputReadError::NotReady("no active snapshot".into()))
    }
    async fn query_view(&self) -> Result<(OutputCursor, QueryOutputView), OutputReadError> {
        Err(OutputReadError::NotReady("no active query view".into()))
    }
    async fn query_replay(&self, _: &OutputCursor) -> Result<(OutputReplay, u64), OutputReadError> {
        Err(OutputReadError::NotReady("no active replay".into()))
    }
}

fn reader(observations: bool) -> anyhow::Result<(ComponentId, Arc<Reader>)> {
    let id = ComponentId::generate();
    let stream = StreamId::try_new("query/out")?;
    let execution = QueryExecutionSettings::default();
    let definition = drasi_query_common::ContinuousQueryDefinition {
        graph_id: "graph".into(),
        id: id.clone(),
        query: "MATCH (n) RETURN n".into(),
        language: ComputationQueryLanguage::Cypher,
        output_stream: stream.clone(),
        outbox_capacity: NonZeroUsize::new(8).expect("positive capacity"),
    };
    let reader = Arc::new(Reader {
        description: QueryDescription {
            graph_id: definition.graph_id.clone(),
            query: definition.query.clone(),
            language: definition.language,
            configuration_hash: definition.configuration_hash(&execution)?,
            execution,
            output_stream: stream.clone(),
            outbox_capacity: definition.outbox_capacity,
            bootstrap: false,
            auto_reset: false,
            volatile: false,
        },
        cursor: OutputCursor::try_new(
            GraphProducerIdentity::new(
                "instance".into(),
                "graph".into(),
                id.clone(),
                stream,
                true,
            )?,
            7,
        )?,
        active: AtomicBool::new(true),
        observations,
        capacity: AtomicUsize::new(8),
    });
    Ok((id, reader))
}

fn emission(reader: &Reader, sequence: u64, transport: u64) -> anyhow::Result<OutputEnvelope> {
    let id = reader.cursor.producer().component_id();
    let mut envelope = QueryChangeCodec::encode_evaluation(
        None,
        id,
        SystemMetadata::new(reader.description.output_stream.clone(), sequence),
        &ResultNodeProjector::try_new("query-results", None)?,
        &[drasi_core::evaluation::context::QueryPartEvaluationContext::Adding {
            after: Default::default(),
            row_signature: sequence,
        }],
        QueryOutputMetadata {
            query_id: id.as_str().into(),
            source_id: None,
            timestamp: chrono::Utc::now(),
            metadata: Default::default(),
            profiling: None,
        },
    )?
    .ok_or_else(|| anyhow::anyhow!("expected a result"))?;
    QueryChangeCodec::set_generation(&mut envelope, id, 9)?;
    QueryRecoveryIdentity::try_new(
        "graph",
        id.clone(),
        reader.description.configuration_hash,
        None,
    )?
    .with_construction_scope("instance")?
    .annotate(&mut envelope, id)?;
    GraphProducerProgress::annotate(&mut envelope, reader.cursor.producer(), sequence)?;
    Ok(OutputEnvelope {
        port: PortId::try_new("out")?,
        envelope: envelope.reemit(transport)?,
    })
}

#[tokio::test]
async fn retained_native_reader_uses_updated_capacity_without_replacing_api_or_metrics(
) -> anyhow::Result<()> {
    let (id, reader) = reader(true)?;
    let api = QueryApi::from_native_reader(id, reader.clone(), None)?;
    api.initialize_native_progress().await?;
    let output = api.output.clone();
    let metrics = api.output_metrics().unwrap();
    let progress = api.native_progress.clone();
    let mut observation = reader.query_observation().await?;
    observation.cursor = observation.cursor.at_sequence(20);
    observation.outbox_size = 20;
    observation.oldest_sequence = Some(1);
    let emitted = [emission(&reader, 20, 20)?];
    assert!(api
        .observe_native_progress(&emitted, Some(&observation))
        .is_err());
    reader.capacity.store(32, Ordering::Release);
    api.observe_native_progress(&emitted, Some(&observation))?;
    assert_eq!(metrics.snapshot().outbox_size, 20);
    assert_eq!(api.generation().await?, 9);
    assert!(Arc::ptr_eq(&output, &api.output));
    assert!(Arc::ptr_eq(&metrics, &api.output_metrics().unwrap()));
    assert!(Arc::ptr_eq(&progress, &api.native_progress));
    Ok(())
}

#[tokio::test]
async fn native_generation_remains_observable_without_reopening_stopped_read_services(
) -> anyhow::Result<()> {
    let (id, reader) = reader(false)?;
    let api = QueryApi::from_native_reader(id, reader.clone(), None)?;
    api.initialize_native_progress().await?;
    assert!(api.output_metrics().is_none());
    assert_eq!(api.generation().await?, 9);
    reader.active.store(false, Ordering::Release);
    assert_eq!(api.generation().await?, 9);
    assert!(api.output_frontier().await.is_err());
    assert!(api.snapshot(Duration::from_millis(10)).await.is_err());
    assert!(api
        .native_progress
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .input
        .is_none());
    Ok(())
}

#[tokio::test]
async fn native_initial_gauges_use_canonical_metadata_without_fetching_rows() -> anyhow::Result<()>
{
    let (id, reader) = reader(true)?;
    let api = QueryApi::from_native_reader(id, reader.clone(), None)?;
    api.initialize_native_progress().await?;
    let metrics = api.output_metrics().expect("negotiated statistics");
    let initialized = metrics.snapshot();
    assert_eq!(initialized.live_results_count, 13);
    assert_eq!(initialized.outbox_size, 2);
    assert_eq!(initialized.outbox_earliest_seq, 6);
    assert_eq!(initialized.outbox_latest_seq, 7);
    assert_eq!(initialized.result_seq_advances, 0);
    assert_eq!(initialized.snapshot_fetch_count, 0);
    reader.active.store(false, Ordering::Release);
    assert_eq!(metrics.snapshot(), initialized);
    assert_eq!(api.generation().await?, 9);
    assert!(api.output_frontier().await.is_err());
    Ok(())
}

#[tokio::test]
async fn replay_and_noop_do_not_advance_or_regress_native_metrics() -> anyhow::Result<()> {
    let (id, reader) = reader(true)?;
    let api = QueryApi::from_native_reader(id, reader.clone(), None)?;
    api.initialize_native_progress().await?;
    let mut observation = reader.query_observation().await?;
    api.observe_native_progress(
        &[emission(&reader, 6, 50)?, emission(&reader, 7, 51)?],
        Some(&observation),
    )?;
    assert_eq!(api.metrics.snapshot().result_seq_advances, 0);
    assert_eq!(
        api.native_progress
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .cursor
            .sequence(),
        7
    );
    observation.cursor = observation.cursor.at_sequence(8);
    observation.oldest_sequence = Some(7);
    let output = emission(&reader, 8, 52)?;
    api.observe_native_progress(std::slice::from_ref(&output), Some(&observation))?;
    api.observe_native_progress(&[output], Some(&observation))?;
    api.observe_native_progress(&[], None)?;
    let current = api.metrics.snapshot();
    assert_eq!(current.result_seq_advances, 1);
    assert_eq!(current.outbox_latest_seq, 8);
    assert_eq!(current.outbox_earliest_seq, 7);
    assert_eq!(current.outbox_size, 2);
    assert_eq!(current.live_results_count, 13);
    Ok(())
}

#[tokio::test]
async fn malformed_native_observations_cannot_change_metrics() -> anyhow::Result<()> {
    let (id, reader) = reader(true)?;
    let api = QueryApi::from_native_reader(id.clone(), reader.clone(), None)?;
    api.initialize_native_progress().await?;
    let before = api.metrics.snapshot();
    let observation = reader.query_observation().await?;
    let output = [emission(&reader, 7, 50)?];
    let mut invalid = Vec::new();
    let mut value = observation.clone();
    value.config_hash ^= 1;
    invalid.push(value);
    let mut value = observation.clone();
    value.oldest_sequence = Some(0);
    invalid.push(value);
    let mut value = observation.clone();
    value.outbox_size = 9;
    invalid.push(value);
    let mut value = observation.clone();
    value.oldest_sequence = None;
    invalid.push(value);
    let mut value = observation.clone();
    value.cursor = value.cursor.at_sequence(6);
    value.oldest_sequence = Some(5);
    invalid.push(value);
    let mut value = observation.clone();
    value.generation = 8;
    invalid.push(value);
    let mut value = observation.clone();
    value.cursor = OutputCursor::try_new(
        GraphProducerIdentity::new(
            "another-instance".into(),
            "graph".into(),
            id,
            reader.description.output_stream.clone(),
            true,
        )?,
        7,
    )?;
    invalid.push(value);
    for invalid in invalid {
        assert!(api
            .observe_native_progress(&output, Some(&invalid))
            .is_err());
        assert_eq!(api.metrics.snapshot(), before);
    }
    assert!(api.observe_native_progress(&output, None).is_err());
    assert!(api
        .observe_native_progress(&[], Some(&observation))
        .is_err());
    assert_eq!(api.metrics.snapshot(), before);
    Ok(())
}

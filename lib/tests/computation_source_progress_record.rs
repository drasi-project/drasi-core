// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! A source's progress-only record (`GraphChangeCodec::encode_source_progress`)
//! is committed by a query as that source's checkpoint, without results.

#![cfg(test)]

#[path = "support/mod.rs"]
mod fixture_support;

use std::{num::NonZeroUsize, sync::Arc};

use bytes::Bytes;
use drasi_core::{
    computation::InMemoryComputationProvider,
    models::{Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange},
};
use drasi_lib::computation::v1::*;

fn scope() -> OperationScope {
    OperationScope::new("test", "component", 1, None, CancellationToken::new())
}

fn stream() -> StreamId {
    StreamId::try_new("source/out").unwrap()
}

#[tokio::test(flavor = "current_thread")]
async fn a_query_commits_a_source_progress_record_as_that_sources_checkpoint_without_results() {
    let source = fixture_support::component_id(4);
    let query_id = fixture_support::component_id(1);
    let owner = Arc::new(QuerySourceProgress::new("query-graph", query_id.clone()).unwrap());
    let mut query = ContinuousQueryTransformer::new(
        ContinuousQueryDefinition {
            graph_id: "query-graph".into(),
            id: query_id,
            query: "MATCH (n:Person) RETURN n.name AS name".into(),
            language: ComputationQueryLanguage::Cypher,
            output_stream: StreamId::try_new("query/out").unwrap(),
            outbox_capacity: NonZeroUsize::new(8).unwrap(),
        },
        Arc::new(InMemoryComputationProvider),
    )
    .await
    .unwrap()
    .with_source_progress(owner.clone())
    .unwrap();
    query.start(&StartContext::new(scope())).await.unwrap();
    let key = SourceProgressKey::Source(source.clone());
    let at = chrono::DateTime::from_timestamp_millis(1000).unwrap();

    let change = drasi_lib::channels::SourceEventWrapper::new(
        source.clone(),
        drasi_lib::channels::SourceEvent::Change(SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("people", "one"),
                    labels: Arc::from([Arc::from("Person")]),
                    effective_from: 1000,
                },
                properties: ElementPropertyMap::from(serde_json::json!({"name": "Alice"})),
            },
        }),
        at,
        1,
    )
    .with_source_position(Bytes::from_static(b"position-1"));
    let input = GraphChangeCodec::encode_source_event(Arc::new(change), &source, stream(), 1, None)
        .unwrap();
    let output = query
        .transform(InputEnvelope {
            execution_context: Default::default(),
            port: PortId::try_new("in").unwrap(),
            envelope: input,
        })
        .await
        .unwrap();
    query.delivery_completed(&output).await.unwrap();
    let rows = query.results().snapshot().unwrap().rows.len();
    assert_eq!(rows, 1);

    let record = GraphChangeCodec::encode_source_progress(
        &source,
        stream(),
        2,
        Bytes::from_static(b"position-2"),
        at,
    )
    .unwrap();
    assert!(record.changes().operations().is_empty());
    let output = query
        .transform(InputEnvelope {
            execution_context: Default::default(),
            port: PortId::try_new("in").unwrap(),
            envelope: record,
        })
        .await
        .unwrap();
    assert!(
        output
            .iter()
            .all(|event| event.envelope.changes().operations().is_empty()),
        "a progress record produced results"
    );
    query.delivery_completed(&output).await.unwrap();
    assert_eq!(query.results().snapshot().unwrap().rows.len(), rows);
    let checkpoint = owner.snapshot().checkpoints[&key].clone();
    assert_eq!(checkpoint.sequence, 2);
    assert_eq!(
        checkpoint.source_position.as_deref(),
        Some(&b"position-2"[..])
    );
    query
        .stop(&StopContext::new(scope(), StopMode::Drain))
        .await
        .unwrap();
}

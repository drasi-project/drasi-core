// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::{num::NonZeroUsize, sync::Arc};

use bytes::Bytes;
use drasi_core::{
    computation::InMemoryComputationProvider,
    models::{Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange},
};
use drasi_lib::{
    channels::{SourceEvent, SourceEventWrapper},
    computation::v1::*,
};

fn change(id: &str, label: &str, value: i64) -> SourceChange {
    SourceChange::Insert {
        element: Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new("records", id),
                labels: Arc::from([Arc::from(label)]),
                effective_from: 1000,
            },
            properties: ElementPropertyMap::from(serde_json::json!({"value":value})),
        },
    }
}

fn raw(emission: u64) -> ChangeEnvelope {
    GraphChangeCodec::encode_source_event(
        Arc::new(SourceEventWrapper {
            source_id: "postgres".into(),
            event: SourceEvent::Change(change("raw", "Raw", 1)),
            timestamp: chrono::DateTime::from_timestamp(1_700_000_000, 0).expect("timestamp"),
            sequence: 42,
            source_position: Some(Bytes::from_static(b"source-position")),
            profiling: None,
        }),
        &ComponentId::try_new("postgres").expect("source"),
        StreamId::try_new("postgres/out").expect("stream"),
        emission,
        None,
    )
    .expect("raw source event")
}

async fn query(id: &str, text: &str) -> anyhow::Result<ContinuousQueryTransformer> {
    let mut query = ContinuousQueryTransformer::new(
        ContinuousQueryDefinition {
            graph_id: "provenance-diamond".into(),
            id: ComponentId::try_new(id)?,
            query: text.into(),
            language: ComputationQueryLanguage::Cypher,
            output_stream: StreamId::try_new(format!("{id}/out"))?,
            outbox_capacity: NonZeroUsize::new(8).expect("capacity"),
        },
        Arc::new(InMemoryComputationProvider),
    )
    .await?;
    query.start().await?;
    Ok(query)
}

async fn apply(
    query: &mut ContinuousQueryTransformer,
    envelope: ChangeEnvelope,
) -> anyhow::Result<Vec<OutputEnvelope>> {
    let output = query
        .transform(InputEnvelope {
            port: PortId::try_new("in")?,
            envelope,
        })
        .await?;
    query.delivery_completed(&output).await?;
    Ok(output)
}

#[tokio::test]
async fn raw_and_derived_diamond_inputs_keep_distinct_progress_without_losing_provenance(
) -> anyhow::Result<()> {
    for marker in ["none", "immediate-volatile", "ancestor-volatile"] {
        let mut upstream = query("upstream", "MATCH (n:Raw) RETURN n.value AS value").await?;
        let mut downstream =
            query("downstream", "MATCH (n:Derived) RETURN n.value AS value").await?;
        let source = raw(1);
        let provenance = GraphChangeCodec::source_metadata(&source)?;
        assert!(apply(&mut downstream, source.clone()).await?.is_empty());
        let upstream_output = apply(&mut upstream, source.clone()).await?;
        assert_eq!(upstream_output.len(), 1);
        let mut cause = upstream_output[0].envelope.clone();
        if marker == "ancestor-volatile" {
            let identity = GraphProducerIdentity::volatile(
                "scope".into(),
                "provenance-diamond".into(),
                ComponentId::try_new("upstream")?,
                StreamId::try_new("upstream/out")?,
            )?;
            GraphProducerProgress::annotate(&mut cause, &identity, 1)?;
        }
        let identity = GraphProducerIdentity::volatile(
            "scope".into(),
            "provenance-diamond".into(),
            ComponentId::try_new("derived")?,
            StreamId::try_new("derived/out")?,
        )?;
        let mut first = None;
        for sequence in 1..=2 {
            let mut derived = GraphChangeCodec::derive_changes(
                &cause,
                &[change(&format!("derived-{sequence}"), "Derived", sequence as i64)],
                StreamId::try_new("derived/out")?,
                sequence,
            )?;
            if marker == "immediate-volatile" {
                GraphProducerProgress::annotate(&mut derived, &identity, sequence)?;
            }
            assert_eq!(GraphChangeCodec::source_metadata(&derived)?, provenance);
            assert_eq!(
                derived.system().source_position(),
                cause.system().source_position()
            );
            assert!(derived.lineage().is_some());
            let mut codec = EnvelopeCodec::new(NonZeroUsize::new(8 << 20).expect("codec limit"));
            codec.register_schema(GraphChangeCodec::schema())?;
            let derived = codec.decode(&codec.encode(&derived)?)?;
            if sequence == 1 {
                first = Some(derived.clone());
            }
            assert_eq!(
                apply(&mut downstream, derived).await?.len(),
                1,
                "{marker}: derived emission was suppressed by causal source progress"
            );
            assert_eq!(
                downstream.results().snapshot()?.rows.len(),
                sequence as usize
            );
        }
        assert!(
            apply(&mut downstream, first.expect("first derived emission"))
                .await?
                .is_empty()
        );
        assert!(apply(&mut downstream, raw(2)).await?.is_empty());
        assert_eq!(downstream.results().snapshot()?.rows.len(), 2);
        upstream.stop().await?;
        downstream.stop().await?;
    }
    Ok(())
}

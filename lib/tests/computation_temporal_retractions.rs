// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::{num::NonZeroUsize, sync::Arc};

use drasi_core::{
    computation::{ComputationIndexProvider, InMemoryComputationProvider},
    models::{Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange},
};
use drasi_lib::computation::v1::*;
use serde_json::{json, Value};

const QUERY: &str = "
MATCH (f:Config) MATCH (s:Sample)
WITH f,s,drasi.trueNowOrLater(
  datetime.realtime().epochMillis >= s.report_time + 1000,
  s.report_time + 1000) AS expired
WITH f,s,CASE WHEN expired THEN 0 ELSE 100-f.load END AS available
RETURN collect({gpu:s.gpu_id,available:available}) AS capacities,
  sum(CASE WHEN available > 0 THEN 1 ELSE 0 END) AS fresh
";

async fn open(
    provider: Arc<dyn ComputationIndexProvider>,
) -> anyhow::Result<ContinuousQueryTransformer> {
    let mut query = ContinuousQueryTransformer::new(
        ContinuousQueryDefinition {
            graph_id: "temporal-retraction".into(),
            id: ComponentId::try_new("capacity")?,
            query: QUERY.into(),
            language: ComputationQueryLanguage::Cypher,
            output_stream: StreamId::try_new("capacity/out")?,
            outbox_capacity: NonZeroUsize::new(8).expect("capacity"),
        },
        provider,
    )
    .await?;
    query.start().await?;
    Ok(query)
}

async fn apply(
    query: &mut ContinuousQueryTransformer,
    sequence: u64,
    id: &str,
    label: &str,
    time: u64,
    properties: Value,
    update: bool,
) -> anyhow::Result<()> {
    let element = Element::Node {
        metadata: ElementMetadata {
            reference: ElementReference::new("source", id),
            labels: Arc::from([Arc::from(label)]),
            effective_from: time,
        },
        properties: ElementPropertyMap::from(properties),
    };
    let change = if update {
        SourceChange::Update { element }
    } else {
        SourceChange::Insert { element }
    };
    let output = query
        .transform(InputEnvelope {
            port: PortId::try_new("in")?,
            envelope: GraphChangeCodec::encode_change(
                change,
                StreamId::try_new("source/out")?,
                sequence,
                None,
            )?,
        })
        .await?;
    query.delivery_completed(&output).await
}

fn assert_state(query: &ContinuousQueryTransformer, available: i64, fresh: f64) {
    let snapshot = query.results().snapshot().expect("snapshot");
    assert_eq!(snapshot.rows.len(), 1);
    let row = QueryChangeCodec::decode_row(snapshot.rows.values().next().expect("row"))
        .expect("typed row");
    let value = serde_json::to_value(row.values).expect("JSON");
    let capacities = value["capacities"].as_array().expect("capacities");
    assert_eq!(capacities.len(), 2, "{value}");
    for gpu in ["gpu-a", "gpu-b"] {
        assert_eq!(
            capacities
                .iter()
                .filter(|row| row["gpu"] == gpu && row["available"] == available)
                .count(),
            1,
            "{value}"
        );
    }
    assert_eq!(value["fresh"].as_f64(), Some(fresh), "{value}");
}

async fn exercise(provider: Arc<dyn ComputationIndexProvider>, reopen: bool) -> anyhow::Result<()> {
    let mut query = open(provider.clone()).await?;
    for (sequence, id, label, time, properties, update) in [
        (1, "config", "Config", 1000, json!({"load":0}), false),
        (
            2,
            "sample-a",
            "Sample",
            1000,
            json!({"gpu_id":"gpu-a","report_time":1000}),
            false,
        ),
        (
            3,
            "sample-b",
            "Sample",
            1000,
            json!({"gpu_id":"gpu-b","report_time":1000}),
            false,
        ),
        (4, "config", "Config", 1500, json!({"load":10}), true),
    ] {
        apply(&mut query, sequence, id, label, time, properties, update).await?;
    }
    assert_state(&query, 90, 2.0);
    if reopen {
        query.stop().await?;
        drop(query);
        query = open(provider.clone()).await?;
        assert_state(&query, 90, 2.0);
    }
    let output = query.on_wakeup().await?;
    query.delivery_completed(&output).await?;
    assert_state(&query, 0, 0.0);
    if reopen {
        query.stop().await?;
        drop(query);
        query = open(provider).await?;
        assert_state(&query, 0, 0.0);
    }
    for _ in 0..3 {
        let output = query.on_wakeup().await?;
        query.delivery_completed(&output).await?;
        assert_state(&query, 0, 0.0);
    }
    apply(
        &mut query,
        5,
        "config",
        "Config",
        2500,
        json!({"load":20}),
        true,
    )
    .await?;
    assert_state(&query, 0, 0.0);
    query.stop().await
}

#[tokio::test]
async fn scheduled_native_retractions_use_the_last_applied_clock() -> anyhow::Result<()> {
    exercise(Arc::new(InMemoryComputationProvider), false).await
}

#[cfg(feature = "computation-rocksdb-tests")]
#[tokio::test]
async fn temporal_retraction_clocks_survive_native_query_reconstruction() -> anyhow::Result<()> {
    use drasi_index_rocksdb::{
        computation::RocksDbComputationProvider, RocksDbMemoryBudget, RocksIndexOptions,
    };
    let directory = tempfile::tempdir()?;
    exercise(
        Arc::new(RocksDbComputationProvider::new(
            directory.path(),
            RocksIndexOptions::new(
                false,
                false,
                RocksDbMemoryBudget::from_total_budget_bytes(32 << 20)?,
            ),
        )),
        true,
    )
    .await
}

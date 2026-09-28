// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

mod mock_source;

use std::{io::Write, num::NonZeroUsize, sync::Arc, time::Instant};

use drasi_core::{
    computation::{ComputationIndexProvider, InMemoryComputationProvider},
    models::{Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange},
};
use drasi_lib::{computation::v1::*, DrasiLib};
use serde_json::{json, Value};

const TEXT: &str = "MATCH (w:Workload) OPTIONAL MATCH (p:Plan) OPTIONAL MATCH (r:Readiness)
WITH w, r.ready = true AND p.version > 0 AS confirmed
RETURN w.id AS id, sum(CASE WHEN confirmed THEN w.replicas ELSE 0 END) AS ready";

async fn submit(
    query: &mut ContinuousQueryTransformer,
    sequence: u64,
    id: &str,
    label: &str,
    properties: Value,
    update: bool,
) -> anyhow::Result<()> {
    let element = Element::Node {
        metadata: ElementMetadata {
            reference: ElementReference::new("source", id),
            labels: Arc::from([Arc::from(label)]),
            effective_from: 1000 + sequence,
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

async fn measure(name: &str, provider: Arc<dyn ComputationIndexProvider>) -> anyhow::Result<Value> {
    assert!(
        provider.is_volatile(),
        "the demo's query-state comparison must be in memory"
    );
    let mut query = ContinuousQueryTransformer::new_with_options(
        ContinuousQueryDefinition {
            graph_id: "memory-provider-isolation".into(),
            id: ComponentId::try_new(name)?,
            query: TEXT.into(),
            language: ComputationQueryLanguage::Cypher,
            output_stream: StreamId::try_new(format!("{name}/out"))?,
            outbox_capacity: NonZeroUsize::new(8).expect("capacity"),
        },
        provider,
        QueryOptions {
            recovery: QueryRecoveryPolicy::Strict,
            publication: QueryPublicationMode::NonAtomic,
        },
    )
    .await?;
    query.start().await?;
    let mut sequence = 0;
    for number in 0..12 {
        sequence += 1;
        submit(
            &mut query,
            sequence,
            &format!("workload-{number}"),
            "Workload",
            json!({"id":number,"replicas":2}),
            false,
        )
        .await?;
    }
    sequence += 1;
    submit(
        &mut query,
        sequence,
        "plan",
        "Plan",
        json!({"version":1}),
        false,
    )
    .await?;
    sequence += 1;
    submit(
        &mut query,
        sequence,
        "ready",
        "Readiness",
        json!({"ready":false}),
        false,
    )
    .await?;
    let mut times = Vec::new();
    let start = Instant::now();
    for iteration in 0..40 {
        sequence += 1;
        let ready = iteration % 2 == 0;
        let tick = Instant::now();
        submit(
            &mut query,
            sequence,
            "ready",
            "Readiness",
            json!({"ready":ready}),
            true,
        )
        .await?;
        times.push(tick.elapsed().as_nanos());
        let snapshot = query.results().snapshot()?;
        assert_eq!(snapshot.rows.len(), 12);
        for record in snapshot.rows.values() {
            let row = QueryChangeCodec::decode_row(record)?;
            let value = serde_json::to_value(row.values)?;
            assert_eq!(value["ready"].as_f64(), Some(if ready { 2.0 } else { 0.0 }));
        }
    }
    let wall_ns = start.elapsed().as_nanos();
    times.sort_unstable();
    let report = json!({
        "provider":name,"updates":40,"validated_rows_per_update":12,
        "wall_ns":wall_ns.to_string(),"p50_ns":times[19].to_string(),
        "p95_ns":times[37].to_string(),"max_ns":times[39].to_string(),
        "scope":"query transform and delivery completion, not live graph queue latency",
    });
    query.stop().await?;
    Ok(report)
}

#[tokio::test]
async fn bare_memory_provider_processes_validated_context_updates() -> anyhow::Result<()> {
    write_report(&measure("bare-memory", Arc::new(InMemoryComputationProvider)).await?)
}

fn write_report(report: &Value) -> anyhow::Result<()> {
    let mut output = std::io::stdout().lock();
    serde_json::to_writer(&mut output, report)?;
    output.write_all(b"\n")?;
    Ok(())
}

#[tokio::test]
async fn default_pipeline_memory_provider_processes_the_same_context_updates() -> anyhow::Result<()>
{
    let (source, _handle) = mock_source::MockSource::new("source")?;
    let core = DrasiLib::builder().with_source(source).build().await?;
    let batch = core
        .computation_pipeline()?
        .source(
            core.borrow_computation_source("source").await?,
            SourceSubscriptionOptions::default(),
        )?
        .query(
            drasi_lib::Query::cypher("probe")
                .query(TEXT)
                .from_source("source")
                .enable_bootstrap(false)
                .build(),
        )
        .build()?;
    let provider = batch
        .bindings
        .resources
        .values()
        .find(|resource| resource.role() == ResourceRole::IndexBackend)
        .ok_or_else(|| anyhow::anyhow!("pipeline omitted its index provider"))?
        .get::<QueryIndexProviderResource>()?
        .0
        .clone();
    let indexes = provider
        .create_indexes("memory-provider-isolation", "capabilities")
        .await?;
    assert!(
        indexes.cleanup().is_none(),
        "default native memory indexes must not spawn plugin I/O tasks"
    );
    assert!(indexes.checkpoint_store().is_none());
    assert!(indexes.outbox_writer().is_none());
    drop(indexes);
    write_report(&measure("pipeline-memory", provider).await?)?;
    drop(batch);
    core.shutdown().await?;
    Ok(())
}

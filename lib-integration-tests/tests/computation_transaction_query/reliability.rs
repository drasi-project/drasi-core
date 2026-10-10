// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use drasi_lib::config::{QueryJoinConfig, QueryJoinKeyConfig};

async fn joined_query(
    root: &Path,
    progress: Arc<QuerySourceProgress>,
) -> Result<TransactionTransformer> {
    let config = drasi_lib::Query::cypher("query")
        .query("MATCH (o:Order)-[:CUSTOMER]->(c:Customer) RETURN c.region AS region, sum(o.amount) AS total")
        .from_source("orders").from_source("customers")
        .with_joins(vec![QueryJoinConfig {
            id: "CUSTOMER".into(),
            keys: vec![
                QueryJoinKeyConfig { label: "Order".into(), property: "customerId".into() },
                QueryJoinKeyConfig { label: "Customer".into(), property: "id".into() },
            ],
        }]).build();
    let mut query = TransactionTransformer::query(
        ContinuousQueryDefinition {
            graph_id: "mixed-source".into(),
            id: ComponentId::try_new("query")?,
            query: config.query.clone(),
            language: ComputationQueryLanguage::Cypher,
            output_stream: StreamId::try_new("query/out")?,
            outbox_capacity: NonZeroUsize::new(8).expect("nonzero capacity"),
        },
        provider(root),
        QueryOptions::default(),
        QueryExecutionSettings::from_legacy_config(&config),
        None,
    )
    .await?
    .with_source_progress(progress)?;
    query.start().await?;
    Ok(query)
}

fn source_input(
    source: &str,
    key: u64,
    sequence: u64,
    properties: serde_json::Value,
    insert: bool,
) -> Result<InputEnvelope> {
    let element = Element::Node {
        metadata: ElementMetadata {
            reference: ElementReference::new(source, &key.to_string()),
            labels: Arc::from([Arc::from(if source == "orders" {
                "Order"
            } else {
                "Customer"
            })]),
            // Deliberately disagree with arrival order across and within producers.
            effective_from: 10_000 - sequence,
        },
        properties: ElementPropertyMap::from(properties),
    };
    Ok(InputEnvelope {
        port: PortId::try_new("in")?,
        envelope: GraphChangeCodec::encode_change(
            if insert {
                SourceChange::Insert { element }
            } else {
                SourceChange::Update { element }
            },
            StreamId::try_new(format!("{source}/out"))?,
            sequence,
            None,
        )?,
    })
}

fn assert_joined(
    query: &TransactionTransformer,
    orders: &BTreeMap<u64, i64>,
    customers: &BTreeMap<u64, String>,
) -> Result<()> {
    let mut expected = BTreeMap::<String, f64>::new();
    for (customer, amount) in orders {
        if let Some(region) = customers.get(customer) {
            *expected.entry(region.clone()).or_default() += *amount as f64;
        }
    }
    let mut actual = BTreeMap::new();
    for encoded in query
        .query_results()
        .context("results")?
        .snapshot()?
        .rows
        .values()
    {
        let row = QueryChangeCodec::decode_row(encoded)?;
        let json = serde_json::to_value(row.values)?;
        let region = json["region"].as_str().context("region")?.to_owned();
        let total = json["total"].as_f64().context("numeric sum")?;
        anyhow::ensure!(
            actual.insert(region, total).is_none(),
            "duplicate aggregate group"
        );
    }
    assert_eq!(actual, expected);
    Ok(())
}

async fn mixed_source_recovery() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let progress = Arc::new(QuerySourceProgress::new(
        "mixed-source",
        ComponentId::try_new("query")?,
    )?);
    let mut query = joined_query(directory.path(), progress.clone()).await?;
    let mut orders = BTreeMap::new();
    let mut customers = BTreeMap::new();
    let mut last = BTreeMap::<&str, InputEnvelope>::new();
    for cycle in 0..64_u64 {
        let sequence = cycle + 1;
        let key = cycle % 4;
        let order = source_input(
            "orders",
            key,
            sequence,
            serde_json::json!({
                "customerId":key, "amount":sequence
            }),
            !orders.contains_key(&key),
        )?;
        let region = format!("region-{}", (cycle / 4 + key) % 3);
        let customer = source_input(
            "customers",
            key,
            sequence,
            serde_json::json!({
                "id":key, "region":region
            }),
            !customers.contains_key(&key),
        )?;
        let arrivals = if cycle % 2 == 0 {
            [("orders", order), ("customers", customer)]
        } else {
            [("customers", customer), ("orders", order)]
        };
        for (source, input) in arrivals {
            if source == "orders" {
                orders.insert(key, sequence as i64);
            } else {
                customers.insert(key, region.clone());
            }
            let output = query.transform(input.clone()).await?;
            query.delivery_completed(&output).await?;
            assert_joined(&query, &orders, &customers)?;
            last.insert(source, input);
        }
        if cycle % 8 == 7 {
            query.stop().await?;
            drop(query);
            query = joined_query(directory.path(), progress.clone()).await?;
            assert_joined(&query, &orders, &customers)?;
            for (source, input) in &last {
                assert!(
                    query.transform(input.clone()).await?.is_empty(),
                    "replayed {source} duplicated output"
                );
                assert_eq!(
                    progress.snapshot().checkpoints
                        [&SourceProgressKey::Stream(StreamId::try_new(format!("{source}/out"))?)]
                        .sequence,
                    sequence
                );
            }
            assert_joined(&query, &orders, &customers)?;
        }
    }
    query.stop().await?;
    Ok(())
}

#[tokio::test]
async fn mixed_source_join_aggregates_and_checkpoints_survive_reordered_updates_current_thread(
) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(60), mixed_source_recovery()).await?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mixed_source_join_aggregates_and_checkpoints_survive_reordered_updates_multi_thread(
) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(60), mixed_source_recovery()).await?
}

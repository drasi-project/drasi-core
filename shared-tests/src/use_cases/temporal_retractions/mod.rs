// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::{collections::BTreeMap, sync::Arc};

use drasi_core::{
    evaluation::{context::QueryPartEvaluationContext, functions::FunctionRegistry},
    models::{
        Element, ElementMetadata, ElementPropertyMap, ElementReference, QueryJoin, QueryJoinKey,
        SourceChange,
    },
    query::QueryBuilder,
};
use drasi_functions_cypher::CypherFunctionSet;
use drasi_query_cypher::CypherParser;
use serde_json::{json, Value};

use crate::QueryTestConfig;

fn node(id: &str, label: &str, time: u64, properties: Value, update: bool) -> SourceChange {
    let element = Element::Node {
        metadata: ElementMetadata {
            reference: ElementReference::new("temporal-retraction", id),
            labels: Arc::from([Arc::from(label)]),
            effective_from: time,
        },
        properties: ElementPropertyMap::from(properties),
    };
    if update {
        SourceChange::Update { element }
    } else {
        SourceChange::Insert { element }
    }
}

fn apply(rows: &mut BTreeMap<u64, Value>, changes: Vec<QueryPartEvaluationContext>) {
    for change in changes {
        match change {
            QueryPartEvaluationContext::Aggregation {
                after,
                row_signature,
                ..
            } => {
                rows.insert(
                    row_signature,
                    serde_json::to_value(after).expect("aggregate JSON"),
                );
            }
            other => panic!("unexpected result {other:?}"),
        }
    }
}

fn assert_capacities(rows: &BTreeMap<u64, Value>, available: i64, fresh: f64) {
    assert_eq!(rows.len(), 1);
    let row = rows.values().next().expect("aggregate");
    let capacities = row["capacities"].as_array().expect("capacities");
    assert_eq!(
        capacities.len(),
        2,
        "expired inputs must not add duplicate GPU rows: {row}"
    );
    assert_eq!(
        capacities
            .iter()
            .filter(|capacity| capacity["gpu"] == "gpu-a")
            .count(),
        1
    );
    assert_eq!(
        capacities
            .iter()
            .filter(|capacity| capacity["gpu"] == "gpu-b")
            .count(),
        1
    );
    assert!(
        capacities
            .iter()
            .all(|capacity| capacity["available"] == available),
        "{row}"
    );
    assert_eq!(row["fresh"].as_f64(), Some(fresh), "{row}");
}

pub async fn shared_context_future_hints_retract_actual_contributions(
    config: &(impl QueryTestConfig + Send),
) {
    let functions = Arc::new(FunctionRegistry::new()).with_cypher_function_set();
    let query = config.config_query(QueryBuilder::new(
        "MATCH (f:Config) MATCH (g:Gpu) OPTIONAL MATCH (g)-[:SAMPLE]->(s:Sample) WITH f,g,s, CASE WHEN s IS NULL THEN true ELSE drasi.trueNowOrLater(datetime.realtime().epochMillis >= s.report_time + 1000, s.report_time + 1000) END AS expired WITH f,g, CASE WHEN expired THEN 0 ELSE 100 - f.load END AS available RETURN collect({gpu:g.gpu_id,available:available}) AS capacities, sum(CASE WHEN available > 0 THEN 1 ELSE 0 END) AS fresh",
        Arc::new(CypherParser::new(functions.clone())),
    ).with_function_registry(functions).with_joins(vec![QueryJoin {
        id: "SAMPLE".into(),
        keys: ["Gpu", "Sample"].into_iter().map(|label| QueryJoinKey { label: label.into(), property: "gpu_id".into() }).collect(),
    }])).await.build().await;
    let mut rows = BTreeMap::new();
    for input in [
        node("config", "Config", 1000, json!({"load":0}), false),
        node("gpu-a", "Gpu", 1000, json!({"gpu_id":"gpu-a"}), false),
        node("gpu-b", "Gpu", 1000, json!({"gpu_id":"gpu-b"}), false),
        node(
            "sample-a",
            "Sample",
            1000,
            json!({"gpu_id":"gpu-a","report_time":1000}),
            false,
        ),
        node(
            "sample-b",
            "Sample",
            1000,
            json!({"gpu_id":"gpu-b","report_time":1000}),
            false,
        ),
        node("config", "Config", 1500, json!({"load":10}), true),
    ] {
        apply(
            &mut rows,
            query
                .process_source_change(input)
                .await
                .expect("source change"),
        );
    }
    assert_capacities(&rows, 90, 2.0);
    let mut due = 0;
    while let Some(result) = query.process_due_futures().await.expect("due future") {
        due += 1;
        assert!(due <= 10, "future processing must drain its scheduled work");
        apply(&mut rows, result.results);
        assert_capacities(&rows, 0, 0.0);
    }
    assert!(due > 0, "the actual future queue must have been exercised");
    apply(
        &mut rows,
        query
            .process_source_change(node("config", "Config", 2500, json!({"load":20}), true))
            .await
            .expect("later context"),
    );
    assert_capacities(&rows, 0, 0.0);
}

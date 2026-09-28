// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::{collections::BTreeMap, sync::Arc};

use drasi_core::{
    evaluation::{
        context::{QueryPartEvaluationContext, QueryVariables},
        functions::FunctionRegistry,
    },
    models::{Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange},
    query::QueryBuilder,
};
use drasi_functions_cypher::CypherFunctionSet;
use drasi_query_cypher::CypherParser;
use serde_json::{json, Value};

use crate::QueryTestConfig;

fn node(id: &str, label: &str, value: &str, update: bool) -> SourceChange {
    let element = Element::Node {
        metadata: ElementMetadata {
            reference: ElementReference::new("disconnected", id),
            labels: Arc::from([Arc::from(label)]),
            effective_from: if update { 2000 } else { 1000 },
        },
        properties: ElementPropertyMap::from(json!({"value":value})),
    };
    if update {
        SourceChange::Update { element }
    } else {
        SourceChange::Insert { element }
    }
}

fn delete(id: &str) -> SourceChange {
    SourceChange::Delete {
        metadata: ElementMetadata {
            reference: ElementReference::new("disconnected", id),
            labels: Arc::from([]),
            effective_from: 3000,
        },
    }
}

fn apply(
    rows: &mut BTreeMap<u64, QueryVariables>,
    changes: Vec<QueryPartEvaluationContext>,
    mut expected: Vec<Value>,
) {
    for change in changes {
        match change {
            QueryPartEvaluationContext::Adding {
                after,
                row_signature,
            } => {
                assert!(
                    rows.insert(row_signature, after).is_none(),
                    "duplicate added identity"
                );
            }
            QueryPartEvaluationContext::Updating {
                before,
                after,
                row_signature,
            } => {
                assert_eq!(
                    rows.get(&row_signature),
                    Some(&before),
                    "update of an unknown identity"
                );
                rows.insert(row_signature, after);
            }
            QueryPartEvaluationContext::Removing {
                before,
                row_signature,
            } => {
                assert_eq!(
                    rows.remove(&row_signature),
                    Some(before),
                    "removal of an unknown identity"
                );
            }
            other => panic!("unexpected nonaggregate result {other:?}"),
        }
    }
    let mut actual: Vec<_> = rows
        .values()
        .map(|row| serde_json::to_value(row).expect("row JSON"))
        .collect();
    actual.sort_by_key(ToString::to_string);
    expected.sort_by_key(ToString::to_string);
    assert_eq!(actual, expected);
}

async fn context_lifecycle(
    config: &(impl QueryTestConfig + Send),
    optional: bool,
    context_first: bool,
) {
    let text = if optional {
        "MATCH (w:Workload) OPTIONAL MATCH (a:AppliedPlan) RETURN w.value AS workload, a.value AS fleet"
    } else {
        "MATCH (w:Workload) MATCH (a:AppliedPlan) RETURN w.value AS workload, a.value AS fleet"
    };
    let query = config
        .config_query(QueryBuilder::new(
            text,
            Arc::new(CypherParser::new(Arc::new(FunctionRegistry::new()))),
        ))
        .await
        .build()
        .await;
    let mut rows = BTreeMap::new();
    let expected = |workloads: &[&str], contexts: &[&str]| -> Vec<Value> {
        workloads
            .iter()
            .flat_map(|workload| {
                if contexts.is_empty() && optional {
                    vec![json!({"workload":workload,"fleet":null})]
                } else {
                    contexts
                        .iter()
                        .map(|fleet| json!({"workload":workload,"fleet":fleet}))
                        .collect()
                }
            })
            .collect()
    };
    if context_first {
        apply(
            &mut rows,
            query
                .process_source_change(node("plan", "AppliedPlan", "demo", false))
                .await
                .expect("context insert"),
            Vec::new(),
        );
    }
    let initial_context: &[&str] = if context_first { &["demo"] } else { &[] };
    apply(
        &mut rows,
        query
            .process_source_change(node("one", "Workload", "one", false))
            .await
            .expect("first workload"),
        expected(&["one"], initial_context),
    );
    apply(
        &mut rows,
        query
            .process_source_change(node("two", "Workload", "two", false))
            .await
            .expect("second workload"),
        expected(&["one", "two"], initial_context),
    );
    if !context_first {
        apply(
            &mut rows,
            query
                .process_source_change(node("plan", "AppliedPlan", "demo", false))
                .await
                .expect("context insert"),
            expected(&["one", "two"], &["demo"]),
        );
    }
    apply(
        &mut rows,
        query
            .process_source_change(node("plan", "AppliedPlan", "updated", true))
            .await
            .expect("context update"),
        expected(&["one", "two"], &["updated"]),
    );
    apply(
        &mut rows,
        query
            .process_source_change(node("another-plan", "AppliedPlan", "updated", false))
            .await
            .expect("equal-valued context"),
        expected(&["one", "two"], &["updated", "updated"]),
    );
    apply(
        &mut rows,
        query
            .process_source_change(delete("plan"))
            .await
            .expect("first context removal"),
        expected(&["one", "two"], &["updated"]),
    );
    apply(
        &mut rows,
        query
            .process_source_change(delete("another-plan"))
            .await
            .expect("last context removal"),
        expected(&["one", "two"], &[]),
    );
    apply(
        &mut rows,
        query
            .process_source_change(node("one", "Workload", "renamed", true))
            .await
            .expect("workload update"),
        expected(&["renamed", "two"], &[]),
    );
    apply(
        &mut rows,
        query
            .process_source_change(delete("one"))
            .await
            .expect("workload removal"),
        expected(&["two"], &[]),
    );
    apply(
        &mut rows,
        query
            .process_source_change(delete("two"))
            .await
            .expect("last workload removal"),
        Vec::new(),
    );
}

pub async fn disconnected_optional_lifecycle(config: &(impl QueryTestConfig + Send)) {
    context_lifecycle(config, true, false).await;
    context_lifecycle(config, true, true).await;
}

pub async fn disconnected_mandatory_lifecycle(config: &(impl QueryTestConfig + Send)) {
    context_lifecycle(config, false, false).await;
    context_lifecycle(config, false, true).await;
}

pub async fn disconnected_matching_reads_current_transaction_writes(
    config: &(impl QueryTestConfig + Send),
) {
    let query = config
        .config_query(QueryBuilder::new(
            "MATCH (a:Item) MATCH (b:Item) RETURN a.value AS left, b.value AS right",
            Arc::new(CypherParser::new(Arc::new(FunctionRegistry::new()))),
        ))
        .await
        .build()
        .await;
    let mut rows = BTreeMap::new();
    apply(
        &mut rows,
        query
            .process_source_change(node("one", "Item", "one", false))
            .await
            .expect("first item"),
        vec![json!({"left":"one","right":"one"})],
    );
    apply(
        &mut rows,
        query
            .process_source_change(node("two", "Item", "two", false))
            .await
            .expect("second item"),
        vec![
            json!({"left":"one","right":"one"}),
            json!({"left":"one","right":"two"}),
            json!({"left":"two","right":"one"}),
            json!({"left":"two","right":"two"}),
        ],
    );
    apply(
        &mut rows,
        query
            .process_source_change(delete("one"))
            .await
            .expect("remove first item"),
        vec![json!({"left":"two","right":"two"})],
    );
    apply(
        &mut rows,
        query
            .process_source_change(delete("two"))
            .await
            .expect("remove last item"),
        Vec::new(),
    );
}

pub async fn disconnected_optional_contexts_aggregate_coherently(
    config: &(impl QueryTestConfig + Send),
) {
    let functions = Arc::new(FunctionRegistry::new()).with_cypher_function_set();
    let query = config.config_query(QueryBuilder::new(
        "MATCH (w:Workload) OPTIONAL MATCH (a:AppliedPlan) OPTIONAL MATCH (s:Status) RETURN count(w) AS total, sum(CASE WHEN a.value = 'ready' AND s.value = 'ready' THEN 1 ELSE 0 END) AS confirmed",
        Arc::new(CypherParser::new(functions.clone())),
    ).with_function_registry(functions)).await.build().await;
    let mut rows = BTreeMap::new();
    for (change, total, confirmed) in [
        (node("one", "Workload", "one", false), 1.0, 0.0),
        (node("two", "Workload", "two", false), 2.0, 0.0),
        (node("plan", "AppliedPlan", "ready", false), 2.0, 0.0),
        (node("status", "Status", "ready", false), 2.0, 2.0),
        (node("second-plan", "AppliedPlan", "ready", false), 4.0, 4.0),
        (delete("plan"), 2.0, 2.0),
        (node("status", "Status", "stopped", true), 2.0, 0.0),
        (delete("status"), 2.0, 0.0),
        (delete("second-plan"), 2.0, 0.0),
    ] {
        for change in query
            .process_source_change(change)
            .await
            .expect("aggregate input")
        {
            let QueryPartEvaluationContext::Aggregation {
                after,
                row_signature,
                ..
            } = change
            else {
                panic!("expected aggregate update");
            };
            rows.insert(
                row_signature,
                serde_json::to_value(after).expect("aggregate row"),
            );
        }
        assert_eq!(rows.len(), 1);
        let row = rows.values().next().expect("aggregate result");
        assert_eq!(row["total"].as_f64(), Some(total));
        assert_eq!(row["confirmed"].as_f64(), Some(confirmed));
    }
}

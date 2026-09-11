//! Extra variable-length MATCH coverage on Garnet.

#![allow(clippy::unwrap_used)]

use std::{collections::BTreeSet, sync::Arc};

use drasi_core::{
    evaluation::{
        context::QueryPartEvaluationContext,
        functions::{FunctionRegistry, RegisterAggregationFunctions},
        QueryExecutionError,
    },
    models::{Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange},
    query::{QueryBuilder, VariableLengthMatchLimits},
};
use drasi_index_garnet::{
    element_index::GarnetElementIndex, future_queue::GarnetFutureQueue,
    result_index::GarnetResultIndex, GarnetSessionControl, GarnetSessionState,
};
use drasi_query_cypher::CypherParser;
use serde_json::json;
use shared_tests::redis_helpers::setup_redis;
use uuid::Uuid;

const SOURCE: &str = "g";

fn metadata(id: &str, labels: &[&str], time: u64) -> ElementMetadata {
    ElementMetadata {
        reference: ElementReference::new(SOURCE, id),
        labels: labels.iter().map(|label| Arc::from(*label)).collect(),
        effective_from: time,
    }
}

fn node(id: &str, label: &str) -> SourceChange {
    SourceChange::Insert {
        element: Element::Node {
            metadata: metadata(id, &[label], 1),
            properties: ElementPropertyMap::from(json!({"id": id})),
        },
    }
}

fn rel(id: &str, from: &str, to: &str) -> SourceChange {
    SourceChange::Insert {
        element: Element::Relation {
            metadata: metadata(id, &["R"], 1),
            properties: ElementPropertyMap::from(json!({"id": id})),
            in_node: ElementReference::new(SOURCE, from),
            out_node: ElementReference::new(SOURCE, to),
        },
    }
}

fn trail_count(edges: &[(String, String, String)]) -> usize {
    let mut rows = 0;
    fn walk(
        edges: &[(String, String, String)],
        current: &str,
        used: &mut BTreeSet<String>,
        hops: usize,
        rows: &mut usize,
    ) {
        if (1..=2).contains(&hops) {
            *rows += 1;
        }
        if hops == 2 {
            return;
        }
        for (id, from, to) in edges {
            if used.contains(id) || from != current {
                continue;
            }
            used.insert(id.clone());
            walk(edges, to, used, hops + 1, rows);
            used.remove(id);
        }
    }
    for start in ["a", "b", "c", "d"] {
        walk(edges, start, &mut BTreeSet::new(), 0, &mut rows);
    }
    rows
}

async fn build(
    url: &str,
    query: &str,
    aggregate: bool,
    limits: Option<VariableLengthMatchLimits>,
) -> drasi_core::query::ContinuousQuery {
    let registry = Arc::new(FunctionRegistry::new());
    if aggregate {
        registry.register_aggregation_functions();
    }
    let parser = Arc::new(CypherParser::new(registry.clone()));
    let query_id = format!("test-{}", Uuid::new_v4());
    let client = redis::Client::open(url).unwrap();
    let connection = client.get_multiplexed_async_connection().await.unwrap();
    let session_state = Arc::new(GarnetSessionState::new(connection.clone()));
    let session_control = Arc::new(GarnetSessionControl::new(session_state.clone()));
    let element_index =
        GarnetElementIndex::new(&query_id, connection.clone(), true, session_state.clone());
    let ari = GarnetResultIndex::new(&query_id, connection.clone(), session_state.clone());
    let fq = GarnetFutureQueue::new(&query_id, connection, session_state);
    let element_index = Arc::new(element_index);
    let mut builder = QueryBuilder::new(query, parser)
        .with_function_registry(registry)
        .with_element_index(element_index.clone())
        .with_archive_index(element_index)
        .with_result_index(Arc::new(ari))
        .with_future_queue(Arc::new(fq))
        .with_session_control(session_control);
    if let Some(limits) = limits {
        builder = builder.with_variable_length_match_limits(limits);
    }
    builder.try_build().await.unwrap()
}

#[tokio::test(flavor = "current_thread")]
async fn garnet_variable_length_extra() {
    let redis = setup_redis().await;
    let url = redis.url().to_string();
    let mut failures = Vec::new();
    let changes = [
        node("a", "N"),
        node("b", "N"),
        node("c", "N"),
        node("d", "N"),
        rel("ab", "a", "b"),
        rel("bc", "b", "c"),
        rel("cd", "c", "d"),
        rel("ad", "a", "d"),
    ];

    let agg = build(
        &url,
        "MATCH (a:N)-[rs:R*1..2]->(b:N) RETURN count(b) AS paths",
        true,
        None,
    )
    .await;
    let mut count = 0i64;
    let mut edges = Vec::new();
    for (step, change) in changes.iter().enumerate() {
        if let SourceChange::Insert {
            element:
                Element::Relation {
                    metadata,
                    in_node,
                    out_node,
                    ..
                },
        } = change
        {
            edges.push((
                metadata.reference.element_id.to_string(),
                in_node.element_id.to_string(),
                out_node.element_id.to_string(),
            ));
        }
        match agg.process_source_change(change.clone()).await {
            Ok(contexts) => {
                for context in contexts {
                    if let QueryPartEvaluationContext::Aggregation { after, .. } = context {
                        count = after
                            .get("paths")
                            .and_then(|value| value.as_i64())
                            .unwrap_or(-1);
                    }
                }
            }
            Err(err) => failures.push(format!("agg step {step}: {err}")),
        }
        let expected = trail_count(&edges) as i64;
        if count != expected {
            failures.push(format!("agg step {step}: {count} expected {expected}"));
            break;
        }
    }

    let timed = build(
        &url,
        "MATCH (a:N)-[rs:R*1..2]->(b:N) WHERE drasi.trueLater(true, 10) RETURN count(b) AS paths",
        true,
        None,
    )
    .await;
    for change in &changes {
        timed.process_source_change(change.clone()).await.unwrap();
    }
    let mut due_count = 0i64;
    let mut wakes = 0;
    while let Some(due) = timed.process_due_futures().await.unwrap() {
        wakes += 1;
        if wakes > 16 {
            failures.push("timer too many wakes".into());
            break;
        }
        for context in due.results {
            if let QueryPartEvaluationContext::Aggregation { after, .. } = context {
                due_count = after
                    .get("paths")
                    .and_then(|value| value.as_i64())
                    .unwrap_or(-1);
            }
        }
    }
    let expected = trail_count(&[
        ("ab".into(), "a".into(), "b".into()),
        ("bc".into(), "b".into(), "c".into()),
        ("cd".into(), "c".into(), "d".into()),
        ("ad".into(), "a".into(), "d".into()),
    ]) as i64;
    if due_count != expected {
        failures.push(format!("timer count {due_count} expected {expected}"));
    }

    let limited = build(
        &url,
        "MATCH (a:N)-[rs:R*1]-(b:N) RETURN a.id AS start",
        false,
        Some(VariableLengthMatchLimits {
            max_matches: 1,
            ..Default::default()
        }),
    )
    .await;
    limited.process_source_change(node("a", "N")).await.unwrap();
    limited.process_source_change(node("b", "N")).await.unwrap();
    match limited.process_source_change(rel("ab", "a", "b")).await {
        Err(err)
            if matches!(
                err.execution_error(),
                Some(QueryExecutionError::MatchResourceLimit {
                    resource: "buffered matches",
                    limit: 1
                })
            ) => {}
        other => failures.push(format!("garnet max_matches: {other:?}")),
    }

    println!("garnet extra failures: {}", failures.len());
    for failure in &failures {
        println!("FAIL {failure}");
    }
    assert!(failures.is_empty(), "{failures:?}");
}

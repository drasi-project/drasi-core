//! GQL variable-length MATCH gauntlet.

#![allow(clippy::unwrap_used)]

use std::{collections::BTreeSet, sync::Arc};

use drasi_core::{
    evaluation::{context::QueryPartEvaluationContext, functions::FunctionRegistry},
    models::{Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange},
    query::QueryBuilder,
};
use drasi_functions_cypher::CypherFunctionSet;
use drasi_query_gql::GQLParser;
use serde_json::json;

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

fn trail_count(edges: &[(String, String, String)], min: usize, max: usize) -> usize {
    let mut rows = 0;
    fn walk(
        edges: &[(String, String, String)],
        current: &str,
        used: &mut BTreeSet<String>,
        hops: usize,
        min: usize,
        max: usize,
        rows: &mut usize,
    ) {
        if hops >= min && hops <= max {
            *rows += 1;
        }
        if hops == max {
            return;
        }
        for (id, from, to) in edges {
            if used.contains(id) || from != current {
                continue;
            }
            used.insert(id.clone());
            walk(edges, to, used, hops + 1, min, max, rows);
            used.remove(id);
        }
    }
    for start in ["a", "b", "c", "d"] {
        walk(edges, start, &mut BTreeSet::new(), 0, min, max, &mut rows);
    }
    rows
}

async fn build(query: &str) -> Result<drasi_core::query::ContinuousQuery, String> {
    let registry = Arc::new(FunctionRegistry::new()).with_cypher_function_set();
    let parser = Arc::new(GQLParser::new(registry.clone()));
    QueryBuilder::new(query, parser)
        .with_function_registry(registry)
        .try_build()
        .await
        .map_err(|err| format!("build: {err}"))
}

#[tokio::test(flavor = "current_thread")]
async fn gql_variable_length_gauntlet() {
    let mut failures = Vec::new();
    let queries = [
        (
            "MATCH (a:N)-[rs:R*1..2]->(b:N) RETURN a.id AS start, b.id AS end, [r IN rs | r.id] AS edges",
            1usize,
            2usize,
        ),
        (
            "MATCH (a:N)-[rs:R*2]->(b:N) RETURN a.id AS start, b.id AS end, [r IN rs | r.id] AS edges",
            2,
            2,
        ),
        (
            "MATCH (a:N)-[rs:R*0..1]->(b:N) RETURN a.id AS start, b.id AS end, [r IN rs | r.id] AS edges",
            0,
            1,
        ),
        (
            "MATCH (a:N)-[rs:R*1..2]->(b:N) FILTER a.id <> b.id RETURN a.id AS start, b.id AS end, [r IN rs | r.id] AS edges",
            1,
            2,
        ),
    ];
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

    for (query_text, min, max) in queries {
        let query = match build(query_text).await {
            Ok(query) => query,
            Err(err) => {
                failures.push(format!("build `{query_text}`: {err}"));
                continue;
            }
        };
        let mut live = 0usize;
        let mut edges = Vec::new();
        let mut nodes = 0usize;
        for (step, change) in changes.iter().enumerate() {
            match change {
                SourceChange::Insert {
                    element: Element::Node { .. },
                } => nodes += 1,
                SourceChange::Insert {
                    element:
                        Element::Relation {
                            metadata,
                            in_node,
                            out_node,
                            ..
                        },
                } => edges.push((
                    metadata.reference.element_id.to_string(),
                    in_node.element_id.to_string(),
                    out_node.element_id.to_string(),
                )),
                _ => {}
            }
            match query.process_source_change(change.clone()).await {
                Ok(contexts) => {
                    for context in contexts {
                        match context {
                            QueryPartEvaluationContext::Adding { .. } => live += 1,
                            QueryPartEvaluationContext::Removing { .. } => {
                                live = live.saturating_sub(1)
                            }
                            QueryPartEvaluationContext::Noop => {}
                            QueryPartEvaluationContext::Updating { .. } => {}
                            other => failures.push(format!("{query_text} step {step}: {other:?}")),
                        }
                    }
                }
                Err(err) => {
                    failures.push(format!("{query_text} step {step}: {err}"));
                    break;
                }
            }
            let expected = if min == 0 {
                nodes + trail_count(&edges, 1, max)
            } else {
                trail_count(&edges, min, max)
            };
            if live != expected {
                failures.push(format!(
                    "{query_text} step {step}: live {live} expected {expected}"
                ));
                break;
            }
        }
    }

    println!("gql failures: {}", failures.len());
    for failure in &failures {
        println!("FAIL {failure}");
    }
    assert!(failures.is_empty(), "{failures:?}");
}

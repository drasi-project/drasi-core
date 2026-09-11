// Copyright 2026 The Drasi Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use crate::evaluation::QueryExecutionError;

use std::sync::Arc;

use drasi_query_ast::{
    api::{QueryParseError, QueryParser},
    ast,
};
use drasi_query_cypher::CypherParser;
use serde_json::json;

use crate::{
    evaluation::functions::FunctionRegistry,
    in_memory_index::in_memory_element_index::InMemoryElementIndex,
    interface::{ElementIndex, QueryBuilderError},
    middleware::MiddlewareTypeRegistry,
    models::{
        Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange,
        SourceMiddlewareConfig,
    },
    path_solver::variable_length::VariableLengthMatchPlan,
    query::{QueryBuilder, VariableLengthMatchLimits},
};

fn parser() -> Arc<CypherParser> {
    Arc::new(CypherParser::new(Arc::new(FunctionRegistry::new())))
}

fn node(id: &str) -> Element {
    Element::Node {
        metadata: ElementMetadata {
            reference: ElementReference::new("test", id),
            labels: Arc::new([Arc::from("N")]),
            effective_from: 1,
        },
        properties: ElementPropertyMap::from(json!({"id": id})),
    }
}

fn edge(id: &str, from: &str, to: &str) -> Element {
    Element::Relation {
        metadata: ElementMetadata {
            reference: ElementReference::new("test", id),
            labels: Arc::new([Arc::from("R")]),
            effective_from: 1,
        },
        in_node: ElementReference::new("test", from),
        out_node: ElementReference::new("test", to),
        properties: ElementPropertyMap::from(json!({"id": id})),
    }
}

#[tokio::test]
async fn empty_source_pipelines_do_not_enable_middleware() {
    let text = "MATCH (a)-[:R*2]->(b) RETURN b";
    for builder in [
        QueryBuilder::new(text, parser()),
        QueryBuilder::new(text, parser())
            .with_middleware_registry(Arc::new(MiddlewareTypeRegistry::new())),
        QueryBuilder::new(text, parser()).with_source_pipeline("test", &[]),
        QueryBuilder::new(text, parser())
            .with_middleware_registry(Arc::new(MiddlewareTypeRegistry::new()))
            .with_source_pipeline("test", &[])
            .with_source_pipeline("other", &[]),
    ] {
        assert!(builder.try_build().await.is_ok());
    }
    for builder in [
        QueryBuilder::new(text, parser()).with_source_pipeline("test", &["configured".into()]),
        QueryBuilder::new(text, parser()).with_source_middleware(Arc::new(
            SourceMiddlewareConfig {
                name: "configured".into(),
                kind: "test".into(),
                config: serde_json::Map::new(),
            },
        )),
    ] {
        assert!(builder
            .try_build()
            .await
            .is_err_and(|error| error.to_string().contains("source middleware")));
    }
}

#[test]
fn variable_length_candidate_slots_do_not_grow_with_bounds() {
    for bound in [0, 1, 2, 64] {
        let parsed = parser()
            .parse_scoped(&format!("MATCH (a)-[:R*{bound}]->(b) RETURN b"))
            .unwrap();
        let plan = VariableLengthMatchPlan::compile(
            &parsed,
            false,
            false,
            VariableLengthMatchLimits::default(),
        )
        .unwrap()
        .unwrap();
        assert!(plan.affinity(&node("transit")).is_empty());
        let expected_slots = if bound == 0 { Vec::new() } else { vec![0] };
        assert_eq!(plan.affinity(&edge("r", "a", "b")), expected_slots);
    }
}

#[tokio::test]
async fn variable_length_does_not_allocate_a_fixed_solver() {
    for (pattern, expected_references) in [
        ("MATCH (a)-[:R]->(b) RETURN a", 3),
        ("MATCH (a)-[:R*1]->(b) RETURN a", 2),
    ] {
        let index = Arc::new(InMemoryElementIndex::new());
        let query = QueryBuilder::new(pattern, parser())
            .with_element_index(index.clone())
            .build()
            .await;
        assert_eq!(Arc::strong_count(&index), expected_references, "{pattern}");
        drop(query);
        assert_eq!(Arc::strong_count(&index), 1, "{pattern}");
    }
}

#[tokio::test]
async fn variable_length_limits_fail_before_writes_and_allow_retry() {
    let index = Arc::new(InMemoryElementIndex::new());
    let query = QueryBuilder::new("MATCH (a)-[rs:R*1]-(b) RETURN rs", parser())
        .with_element_index(index.clone())
        .with_variable_length_match_limits(VariableLengthMatchLimits {
            max_matches: 1,
            ..Default::default()
        })
        .build()
        .await;
    for element in [node("a"), node("b")] {
        query
            .process_source_change(SourceChange::Insert { element })
            .await
            .unwrap();
    }
    let relation = edge("r", "a", "b");
    assert!(matches!(
        query
            .process_source_change(SourceChange::Insert {
                element: relation.clone()
            })
            .await,
        Err(error) if matches!(error.execution_error(), Some(QueryExecutionError::MatchResourceLimit {
            resource: "buffered matches",
            limit: 1
        }))
    ));
    assert!(index
        .get_element(relation.get_reference())
        .await
        .unwrap()
        .is_none());
    assert!(query
        .process_source_change(SourceChange::Insert { element: node("c") })
        .await
        .is_ok());
}

struct LegacyParser;

impl QueryParser for LegacyParser {
    fn parse(&self, input: &str) -> Result<ast::Query, QueryParseError> {
        parser().parse(input)
    }
}

#[tokio::test]
async fn variable_length_requires_scope_metadata_but_fixed_queries_do_not() {
    assert!(
        QueryBuilder::new("MATCH (a)-[:R]->(b) RETURN a", Arc::new(LegacyParser))
            .try_build()
            .await
            .is_ok()
    );
    let error = QueryBuilder::new("MATCH (a)-[:R*1]->(b) RETURN a", Arc::new(LegacyParser))
        .try_build()
        .await
        .unwrap_err();
    assert!(error.to_string().contains("parse_scoped"));
}

#[tokio::test]
async fn variable_length_rejects_unsupported_semantics_at_build() {
    for (query, diagnostic) in [
        ("MATCH (a)-[:R*]->(b) RETURN a", "unbounded"),
        ("MATCH (a)-[:R*-1]->(b) RETURN a", "nonnegative"),
        ("MATCH (a)-[:R*3..2]->(b) RETURN a", "ordered"),
        ("MATCH (a)-[:R*65]->(b) RETURN a", "max_hops"),
        ("OPTIONAL MATCH (a)-[:R*1]->(b) RETURN a", "OPTIONAL"),
        (
            "MATCH (a)-[:R*1]->(b) WITH a MATCH (a)-[:R]->(b) RETURN b",
            "later",
        ),
        ("MATCH (a)-[:R*1]->(b), (c) RETURN a", "disconnected"),
        (
            "MATCH (a)-[rs:R*1]->(b)-[rs:R]->(c) RETURN a",
            "more than once",
        ),
        ("MATCH (a)-[a:R*1]->(b) RETURN a", "both a node"),
        ("MATCH (a)-[:R*1 {x: a.x}]->(b) RETURN a", "literal"),
        ("MATCH (a)-[:R*1 {x: rand()}]->(b) RETURN a", "literal"),
        ("MATCH (a)-[:R*1]->(b) SET a.x = 1 RETURN a", "SET"),
        ("MATCH (a)-[:R*1]->(b) DELETE a RETURN b", "DELETE"),
    ] {
        let error = QueryBuilder::new(query, parser())
            .try_build()
            .await
            .unwrap_err();
        assert!(
            matches!(&error, QueryBuilderError::EvaluationError(error) if matches!(error.execution_error(), Some(QueryExecutionError::InvalidVariableLengthMatch(_)))),
            "{query}: {error}"
        );
        assert!(error.to_string().contains(diagnostic), "{query}: {error}");
    }
}

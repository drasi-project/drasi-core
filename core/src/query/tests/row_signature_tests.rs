// Copyright 2024 The Drasi Authors.
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

use std::sync::Arc;

use drasi_query_cypher::CypherParser;
use serde_json::json;

use crate::{
    evaluation::{
        context::QueryPartEvaluationContext,
        functions::{Function, FunctionRegistry, Sum},
    },
    in_memory_index::{
        in_memory_element_index::InMemoryElementIndex, in_memory_future_queue::InMemoryFutureQueue,
        in_memory_result_index::InMemoryResultIndex,
    },
    models::{Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange},
    query::QueryBuilder,
};

fn create_registry_with_sum() -> Arc<FunctionRegistry> {
    let registry = Arc::new(FunctionRegistry::new());
    registry.register_function("sum", Function::Aggregating(Arc::new(Sum {})));
    registry
}

async fn build_simple_query(query_str: &str) -> crate::query::ContinuousQuery {
    let function_registry = Arc::new(FunctionRegistry::new());
    let parser = Arc::new(CypherParser::new(function_registry.clone()));
    let builder = QueryBuilder::new(query_str, parser);
    builder.build().await
}

async fn build_aggregating_query(query_str: &str) -> crate::query::ContinuousQuery {
    let function_registry = create_registry_with_sum();
    let parser = Arc::new(CypherParser::new(function_registry.clone()));
    let builder =
        QueryBuilder::new(query_str, parser).with_function_registry(function_registry.clone());
    builder.build().await
}

async fn build_query_with_indexes(query_str: &str) -> crate::query::ContinuousQuery {
    let function_registry = Arc::new(FunctionRegistry::new());
    let parser = Arc::new(CypherParser::new(function_registry.clone()));
    let element_index = Arc::new(InMemoryElementIndex::new());
    let result_index = Arc::new(InMemoryResultIndex::new());
    let future_queue = Arc::new(InMemoryFutureQueue::new());
    let builder = QueryBuilder::new(query_str, parser)
        .with_element_index(element_index.clone())
        .with_archive_index(element_index)
        .with_result_index(result_index)
        .with_future_queue(future_queue);
    builder.build().await
}

async fn build_aggregating_query_with_indexes(query_str: &str) -> crate::query::ContinuousQuery {
    let function_registry = create_registry_with_sum();
    let parser = Arc::new(CypherParser::new(function_registry.clone()));
    let element_index = Arc::new(InMemoryElementIndex::new());
    let result_index = Arc::new(InMemoryResultIndex::new());
    let future_queue = Arc::new(InMemoryFutureQueue::new());
    let builder = QueryBuilder::new(query_str, parser)
        .with_function_registry(function_registry.clone())
        .with_element_index(element_index.clone())
        .with_archive_index(element_index)
        .with_result_index(result_index)
        .with_future_queue(future_queue);
    builder.build().await
}

fn make_node(source: &str, id: &str, label: &str, props: serde_json::Value) -> SourceChange {
    SourceChange::Insert {
        element: Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new(source, id),
                labels: Arc::new([Arc::from(label)]),
                effective_from: 1000,
            },
            properties: ElementPropertyMap::from(props),
        },
    }
}

fn make_update(source: &str, id: &str, label: &str, props: serde_json::Value) -> SourceChange {
    SourceChange::Update {
        element: Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new(source, id),
                labels: Arc::new([Arc::from(label)]),
                effective_from: 2000,
            },
            properties: ElementPropertyMap::from(props),
        },
    }
}

fn make_delete(source: &str, id: &str) -> SourceChange {
    SourceChange::Delete {
        metadata: ElementMetadata {
            reference: ElementReference::new(source, id),
            labels: Arc::new([]),
            effective_from: 3000,
        },
    }
}

#[tokio::test]
async fn optional_match_transitions_preserve_the_complete_identity_keyed_row_set() {
    use crate::models::{QueryJoin, QueryJoinKey};
    use std::collections::BTreeMap;

    for synthetic in [false, true] {
        let parser = Arc::new(CypherParser::new(Arc::new(FunctionRegistry::new())));
        let mut builder = QueryBuilder::new(
            "MATCH (d:DecisionExplanation) OPTIONAL MATCH (d)-[:DECISION_WRITE]->(w:PlanWriteOutcome) RETURN d.decision_id AS decision_id, w.outcome AS outcome",
            parser,
        );
        if synthetic {
            builder = builder.with_joins(vec![QueryJoin {
                id: "DECISION_WRITE".into(),
                keys: ["DecisionExplanation", "PlanWriteOutcome"]
                    .into_iter()
                    .map(|label| QueryJoinKey {
                        label: label.into(),
                        property: "decision_id".into(),
                    })
                    .collect(),
            }]);
        }
        let query = builder.build().await;
        let mut rows = BTreeMap::new();
        let mut apply = |changes: Vec<QueryPartEvaluationContext>| {
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
                            "update must identify its actual previous row"
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
                            "delete must identify its actual previous row"
                        );
                    }
                    other => panic!("unexpected non-aggregate change: {other:?}"),
                }
            }
            let mut values: Vec<_> = rows
                .values()
                .map(|row| serde_json::to_value(row).unwrap())
                .collect();
            values.sort_by_key(ToString::to_string);
            values
        };
        assert_eq!(
            apply(
                query
                    .process_source_change(make_node(
                        "test",
                        "left",
                        "DecisionExplanation",
                        json!({"decision_id":"decision"})
                    ))
                    .await
                    .unwrap()
            ),
            vec![json!({"decision_id":"decision","outcome":null})],
        );
        if !synthetic {
            for right in ["first", "second"] {
                assert_eq!(
                    apply(
                        query
                            .process_source_change(SourceChange::Insert {
                                element: Element::Relation {
                                    metadata: ElementMetadata {
                                        reference: ElementReference::new(
                                            "test",
                                            &format!("edge-{right}")
                                        ),
                                        labels: Arc::from([Arc::from("DECISION_WRITE")]),
                                        effective_from: 1000,
                                    },
                                    in_node: ElementReference::new("test", "left"),
                                    out_node: ElementReference::new("test", right),
                                    properties: ElementPropertyMap::new(),
                                },
                            })
                            .await
                            .unwrap()
                    ),
                    vec![json!({"decision_id":"decision","outcome":null})],
                );
            }
        }
        for (id, count) in [("first", 1), ("second", 2)] {
            let result = apply(
                query
                    .process_source_change(make_node(
                        "test",
                        id,
                        "PlanWriteOutcome",
                        json!({"decision_id":"decision","outcome":"rejected"}),
                    ))
                    .await
                    .unwrap(),
            );
            assert_eq!(
                result,
                vec![json!({"decision_id":"decision","outcome":"rejected"}); count]
            );
        }
        assert_eq!(
            apply(
                query
                    .process_source_change(make_delete("test", "first"))
                    .await
                    .unwrap()
            ),
            vec![json!({"decision_id":"decision","outcome":"rejected"})],
        );
        assert_eq!(
            apply(
                query
                    .process_source_change(make_delete("test", "second"))
                    .await
                    .unwrap()
            ),
            vec![json!({"decision_id":"decision","outcome":null})],
        );
        assert!(apply(
            query
                .process_source_change(make_delete("test", "left"))
                .await
                .unwrap()
        )
        .is_empty());
    }
}

#[tokio::test]
async fn non_aggregating_insert_has_nonzero_row_signature() {
    let query = build_simple_query("MATCH (n:Sensor) RETURN n.name").await;

    let change = make_node("test", "s1", "Sensor", json!({"name": "temp_1"}));
    let result = query.process_source_change(change).await.unwrap();

    assert_eq!(result.len(), 1);
    assert!(matches!(
        &result[0],
        QueryPartEvaluationContext::Adding { .. }
    ));
    assert_ne!(
        result[0].row_signature(),
        0,
        "Adding result should have a non-zero row_signature"
    );
}

#[tokio::test]
async fn non_aggregating_update_preserves_row_signature() {
    let query = build_query_with_indexes("MATCH (n:Sensor) RETURN n.name").await;

    let insert = make_node("test", "s1", "Sensor", json!({"name": "temp_1"}));
    let insert_result = query.process_source_change(insert).await.unwrap();
    assert_eq!(insert_result.len(), 1);
    let insert_row_signature = insert_result[0].row_signature();

    let update = make_update("test", "s1", "Sensor", json!({"name": "temp_1_updated"}));
    let update_result = query.process_source_change(update).await.unwrap();
    assert_eq!(update_result.len(), 1);
    assert!(matches!(
        &update_result[0],
        QueryPartEvaluationContext::Updating { .. }
    ));
    assert_eq!(
        update_result[0].row_signature(),
        insert_row_signature,
        "Update of same node should preserve row_signature"
    );
}

#[tokio::test]
async fn non_aggregating_delete_preserves_row_signature() {
    let query = build_query_with_indexes("MATCH (n:Sensor) RETURN n.name").await;

    let insert = make_node("test", "s1", "Sensor", json!({"name": "temp_1"}));
    let insert_result = query.process_source_change(insert).await.unwrap();
    assert_eq!(insert_result.len(), 1);
    let insert_row_signature = insert_result[0].row_signature();

    let delete = make_delete("test", "s1");
    let delete_result = query.process_source_change(delete).await.unwrap();
    assert_eq!(delete_result.len(), 1);
    assert!(matches!(
        &delete_result[0],
        QueryPartEvaluationContext::Removing { .. }
    ));
    assert_eq!(
        delete_result[0].row_signature(),
        insert_row_signature,
        "Delete of same node should preserve row_signature"
    );
}

#[tokio::test]
async fn different_solutions_get_different_row_signatures() {
    let query = build_simple_query("MATCH (n:Sensor) RETURN n.name").await;

    let change1 = make_node("test", "s1", "Sensor", json!({"name": "temp_1"}));
    let result1 = query.process_source_change(change1).await.unwrap();
    assert_eq!(result1.len(), 1);

    let change2 = make_node("test", "s2", "Sensor", json!({"name": "temp_1"}));
    let result2 = query.process_source_change(change2).await.unwrap();
    assert_eq!(result2.len(), 1);

    assert_ne!(
        result1[0].row_signature(),
        result2[0].row_signature(),
        "Different graph solutions should produce different row_signatures even with identical projected values"
    );
}

#[tokio::test]
async fn aggregating_query_has_nonzero_row_signature() {
    let query = build_aggregating_query(
        "MATCH (n:Sensor) RETURN n.region AS region, sum(n.value) AS total",
    )
    .await;

    let change = make_node(
        "test",
        "s1",
        "Sensor",
        json!({"region": "west", "value": 10}),
    );
    let result = query.process_source_change(change).await.unwrap();

    assert_eq!(result.len(), 1);
    assert!(matches!(
        &result[0],
        QueryPartEvaluationContext::Aggregation { .. }
    ));
    assert_ne!(
        result[0].row_signature(),
        0,
        "Aggregation result should have a non-zero row_signature"
    );
}

#[tokio::test]
async fn aggregating_same_group_preserves_row_signature() {
    let query = build_aggregating_query_with_indexes(
        "MATCH (n:Sensor) RETURN n.region AS region, sum(n.value) AS total",
    )
    .await;

    let change1 = make_node(
        "test",
        "s1",
        "Sensor",
        json!({"region": "west", "value": 10}),
    );
    let result1 = query.process_source_change(change1).await.unwrap();
    assert_eq!(result1.len(), 1);
    let first_row_signature = result1[0].row_signature();

    let change2 = make_node(
        "test",
        "s2",
        "Sensor",
        json!({"region": "west", "value": 20}),
    );
    let result2 = query.process_source_change(change2).await.unwrap();
    assert_eq!(result2.len(), 1);
    assert_eq!(
        result2[0].row_signature(),
        first_row_signature,
        "Adding to same aggregation group should preserve row_signature"
    );
}

#[tokio::test]
async fn aggregating_different_groups_get_different_row_signatures() {
    let query = build_aggregating_query(
        "MATCH (n:Sensor) RETURN n.region AS region, sum(n.value) AS total",
    )
    .await;

    let change1 = make_node(
        "test",
        "s1",
        "Sensor",
        json!({"region": "west", "value": 10}),
    );
    let result1 = query.process_source_change(change1).await.unwrap();
    assert_eq!(result1.len(), 1);

    let change2 = make_node(
        "test",
        "s2",
        "Sensor",
        json!({"region": "east", "value": 20}),
    );
    let result2 = query.process_source_change(change2).await.unwrap();
    assert_eq!(result2.len(), 1);

    assert_ne!(
        result1[0].row_signature(),
        result2[0].row_signature(),
        "Different GROUP BY values should produce different row_signatures"
    );
}

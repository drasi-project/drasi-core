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
    evaluation::{context::QueryPartEvaluationContext, functions::FunctionRegistry},
    in_memory_index::{
        in_memory_element_index::InMemoryElementIndex, in_memory_future_queue::InMemoryFutureQueue,
        in_memory_result_index::InMemoryResultIndex,
    },
    models::{Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange},
    query::QueryBuilder,
};

async fn build_query_with_indexes(query_str: &str) -> crate::query::ContinuousQuery {
    let function_registry = Arc::new(FunctionRegistry::new());
    let parser = Arc::new(CypherParser::new(function_registry.clone()));
    let element_index = Arc::new(InMemoryElementIndex::new());
    let result_index = Arc::new(InMemoryResultIndex::new());
    let future_queue = Arc::new(InMemoryFutureQueue::new());

    QueryBuilder::new(query_str, parser)
        .with_element_index(element_index.clone())
        .with_archive_index(element_index)
        .with_result_index(result_index)
        .with_future_queue(future_queue)
        .build()
        .await
}

fn make_node(source: &str, id: &str, label: &str, timestamp: u64, name: &str) -> SourceChange {
    SourceChange::Insert {
        element: Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new(source, id),
                labels: Arc::new([Arc::from(label)]),
                effective_from: timestamp,
            },
            properties: ElementPropertyMap::from(json!({"name": name})),
        },
    }
}

fn make_update(source: &str, id: &str, label: &str, timestamp: u64, name: &str) -> SourceChange {
    SourceChange::Update {
        element: Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new(source, id),
                labels: Arc::new([Arc::from(label)]),
                effective_from: timestamp,
            },
            properties: ElementPropertyMap::from(json!({"name": name})),
        },
    }
}

fn make_delete(source: &str, id: &str, timestamp: u64) -> SourceChange {
    SourceChange::Delete {
        metadata: ElementMetadata {
            reference: ElementReference::new(source, id),
            labels: Arc::new([]),
            effective_from: timestamp,
        },
    }
}

#[tokio::test]
async fn stale_update_is_rejected_and_state_remains_latest() {
    let query = build_query_with_indexes("MATCH (n:Sensor) RETURN n.name AS name").await;

    let insert_result = query
        .process_source_change(make_node("test", "s1", "Sensor", 10, "A"))
        .await
        .unwrap();
    assert_eq!(insert_result.len(), 1);

    let fresh_update_result = query
        .process_source_change(make_update("test", "s1", "Sensor", 30, "C"))
        .await
        .unwrap();
    assert_eq!(fresh_update_result.len(), 1);

    let stale_update_result = query
        .process_source_change(make_update("test", "s1", "Sensor", 20, "B"))
        .await
        .unwrap();
    assert!(stale_update_result.is_empty());

    let delete_result = query
        .process_source_change(make_delete("test", "s1", 40))
        .await
        .unwrap();
    assert_eq!(delete_result.len(), 1);

    match &delete_result[0] {
        QueryPartEvaluationContext::Removing { before, .. } => {
            assert_eq!(before.get("name").and_then(|value| value.as_str()), Some("C"));
        }
        _ => panic!("Expected remove result after deleting latest state"),
    }
}

#[tokio::test]
async fn stale_delete_is_rejected_and_element_remains_available() {
    let query = build_query_with_indexes("MATCH (n:Sensor) RETURN n.name AS name").await;

    query.process_source_change(make_node("test", "s1", "Sensor", 10, "A"))
        .await
        .unwrap();

    query.process_source_change(make_update("test", "s1", "Sensor", 30, "C"))
        .await
        .unwrap();

    let stale_delete_result = query
        .process_source_change(make_delete("test", "s1", 20))
        .await
        .unwrap();
    assert!(stale_delete_result.is_empty());

    let fresh_update_result = query
        .process_source_change(make_update("test", "s1", "Sensor", 40, "D"))
        .await
        .unwrap();
    assert_eq!(fresh_update_result.len(), 1);

    match &fresh_update_result[0] {
        QueryPartEvaluationContext::Updating { before, after, .. } => {
            assert_eq!(before.get("name").and_then(|value| value.as_str()), Some("C"));
            assert_eq!(after.get("name").and_then(|value| value.as_str()), Some("D"));
        }
        _ => panic!("Expected update result after stale delete was rejected"),
    }
}

#[tokio::test]
async fn equal_timestamp_update_is_allowed() {
    let query = build_query_with_indexes("MATCH (n:Sensor) RETURN n.name AS name").await;

    query.process_source_change(make_node("test", "s1", "Sensor", 10, "A"))
        .await
        .unwrap();

    query.process_source_change(make_update("test", "s1", "Sensor", 30, "C"))
        .await
        .unwrap();

    let equal_timestamp_update_result = query
        .process_source_change(make_update("test", "s1", "Sensor", 30, "E"))
        .await
        .unwrap();
    assert_eq!(equal_timestamp_update_result.len(), 1);

    match &equal_timestamp_update_result[0] {
        QueryPartEvaluationContext::Updating { before, after, .. } => {
            assert_eq!(before.get("name").and_then(|value| value.as_str()), Some("C"));
            assert_eq!(after.get("name").and_then(|value| value.as_str()), Some("E"));
        }
        _ => panic!("Expected equal timestamp update to be accepted"),
    }
}

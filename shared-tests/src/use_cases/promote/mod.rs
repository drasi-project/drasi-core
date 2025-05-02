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

use drasi_middleware::promote::PromoteMiddlewareFactory;
use serde_json::json;

use drasi_core::{
    evaluation::{
        context::QueryPartEvaluationContext,
        variable_value::{float::Float, integer::Integer, VariableValue},
    },
    middleware::MiddlewareTypeRegistry,
    models::{
        Element, ElementMetadata, ElementPropertyMap, ElementReference, ElementValue, SourceChange,
    },
    query::QueryBuilder,
};

use crate::QueryTestConfig;

mod queries;

// Helper macros and functions for test data creation
macro_rules! variablemap {
    ($( $key: expr => $val: expr ),*) => {{
         let mut map = ::std::collections::BTreeMap::new();
         $( map.insert($key.to_string().into(), $val); )*
         map
    }}
}

fn null_vv() -> VariableValue {
    VariableValue::Null
}

fn create_node_insert_change(id: &str, props_json: serde_json::Value) -> SourceChange {
    let mut props = ElementPropertyMap::default();
    if let serde_json::Value::Object(obj) = props_json {
        for (k, v) in obj {
            props.insert(&k, ElementValue::from(&v));
        }
    }
    SourceChange::Insert {
        element: Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new("test_source", id),
                labels: vec![Arc::from("TestNode")].into(),
                effective_from: 0,
            },
            properties: props,
        },
    }
}

fn create_node_update_change(id: &str, props_json: serde_json::Value) -> SourceChange {
    let mut props = ElementPropertyMap::default();
    if let serde_json::Value::Object(obj) = props_json {
        for (k, v) in obj {
            props.insert(&k, ElementValue::from(&v));
        }
    }
    SourceChange::Update {
        element: Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new("test_source", id),
                labels: vec![Arc::from("TestNode")].into(),
                effective_from: 1,
            },
            properties: props,
        },
    }
}

fn create_node_delete_change(id: &str) -> SourceChange {
    SourceChange::Delete {
        metadata: ElementMetadata {
            reference: ElementReference::new("test_source", id),
            labels: vec![Arc::from("TestNode")].into(),
            effective_from: 2,
        },
    }
}

fn create_middleware_registry() -> Arc<MiddlewareTypeRegistry> {
    let mut registry = MiddlewareTypeRegistry::new();
    registry.register(Arc::new(PromoteMiddlewareFactory::new()));
    Arc::new(registry)
}

async fn setup_query(
    config: &(impl QueryTestConfig + Send),
    middleware_name: &str,
    mappings: serde_json::Value,
    on_conflict: Option<&str>,
    on_error: Option<&str>,
) -> drasi_core::query::ContinuousQuery {
    let registry = create_middleware_registry();
    let mut builder = QueryBuilder::new(queries::promote_observer_query());
    builder = config.config_query(builder).await;
    builder = builder.with_middleware_registry(registry);

    let mw_config =
        queries::create_promote_middleware(middleware_name, mappings, on_conflict, on_error);

    builder = builder.with_source_middleware(mw_config);
    builder =
        builder.with_source_pipeline("test_source", &queries::source_pipeline(middleware_name));

    builder.build().await
}

#[allow(clippy::unwrap_used)]
async fn test_basic_promotion(config: &(impl QueryTestConfig + Send)) {
    let query = setup_query(
        config,
        "promote_basic",
        json!([
            { "path": "$.nested.value", "target_name": "promoted_value" }
        ]),
        None,
        None,
    )
    .await;

    let insert = create_node_insert_change(
        "node1",
        json!({
            "id": "node1",
            "nested": { "value": "hello", "other": 123 }
        }),
    );

    let result = query.process_source_change(insert).await.unwrap();
    assert_eq!(result.len(), 1);

    let nested_map = variablemap!(
        "value" => VariableValue::String("hello".to_string()),
        "other" => VariableValue::Integer(Integer::from(123))
    );

    assert!(result.contains(&QueryPartEvaluationContext::Adding {
        after: variablemap!(
            "id" => VariableValue::String("node1".to_string()),
            "nested" => VariableValue::Object(nested_map),
            "promoted_value" => VariableValue::String("hello".to_string()),
            "original_prop" => null_vv(),
            "another_promoted" => null_vv()
        )
    }));
}

#[allow(clippy::unwrap_used)]
async fn test_multiple_mappings(config: &(impl QueryTestConfig + Send)) {
    let query = setup_query(
        config,
        "promote_multi",
        json!([
            { "path": "$.nested.value", "target_name": "promoted_value" },
            { "path": "$.nested.other", "target_name": "another_promoted" }
        ]),
        None,
        None,
    )
    .await;

    let insert = create_node_insert_change(
        "node2",
        json!({
            "id": "node2",
            "nested": { "value": true, "other": 99.5 }
        }),
    );

    let result = query.process_source_change(insert).await.unwrap();
    assert_eq!(result.len(), 1);

    let nested_map = variablemap!(
        "value" => VariableValue::Bool(true),
        "other" => VariableValue::Float(Float::from(99.5))
    );

    assert!(result.contains(&QueryPartEvaluationContext::Adding {
        after: variablemap!(
            "id" => VariableValue::String("node2".to_string()),
            "nested" => VariableValue::Object(nested_map),
            "promoted_value" => VariableValue::Bool(true),
            "another_promoted" => VariableValue::Float(Float::from(99.5)),
            "original_prop" => null_vv()
        )
    }));
}

#[allow(clippy::unwrap_used)]
async fn test_conflict_overwrite(config: &(impl QueryTestConfig + Send)) {
    let query = setup_query(
        config,
        "promote_overwrite",
        json!([
            { "path": "$.nested.value", "target_name": "promoted_value" }
        ]),
        None, // Default: overwrite
        None,
    )
    .await;

    let insert = create_node_insert_change(
        "node3",
        json!({
            "id": "node3",
            "nested": { "value": "new_value" },
            "promoted_value": "original_value" // Existing property
        }),
    );

    let result = query.process_source_change(insert).await.unwrap();
    assert_eq!(result.len(), 1);

    let nested_map = variablemap!(
        "value" => VariableValue::String("new_value".to_string())
    );

    assert!(result.contains(&QueryPartEvaluationContext::Adding {
        after: variablemap!(
            "id" => VariableValue::String("node3".to_string()),
            "nested" => VariableValue::Object(nested_map),
            "promoted_value" => VariableValue::String("new_value".to_string()), // Overwritten
            "original_prop" => null_vv(),
            "another_promoted" => null_vv()
        )
    }));
}

#[allow(clippy::unwrap_used)]
async fn test_conflict_skip(config: &(impl QueryTestConfig + Send)) {
    let query = setup_query(
        config,
        "promote_conflict_skip",
        json!([
            { "path": "$.nested.value", "target_name": "promoted_value" }
        ]),
        Some("skip"), // on_conflict: skip
        None,
    )
    .await;

    let insert = create_node_insert_change(
        "node4",
        json!({
            "id": "node4",
            "nested": { "value": "new_value" },
            "promoted_value": "original_value" // Existing property
        }),
    );

    let result = query.process_source_change(insert).await.unwrap();
    assert_eq!(result.len(), 1);

    let nested_map = variablemap!(
        "value" => VariableValue::String("new_value".to_string())
    );

    assert!(result.contains(&QueryPartEvaluationContext::Adding {
        after: variablemap!(
            "id" => VariableValue::String("node4".to_string()),
            "nested" => VariableValue::Object(nested_map),
            "promoted_value" => VariableValue::String("original_value".to_string()), // Kept original
            "original_prop" => null_vv(),
            "another_promoted" => null_vv()
        )
    }));
}

#[allow(clippy::unwrap_used)]
async fn test_conflict_fail(config: &(impl QueryTestConfig + Send)) {
    let query = setup_query(
        config,
        "promote_conflict_fail",
        json!([
            { "path": "$.nested.value", "target_name": "promoted_value" }
        ]),
        Some("fail"), // on_conflict: fail
        None,
    )
    .await;

    let insert = create_node_insert_change(
        "node5",
        json!({
            "id": "node5",
            "nested": { "value": "new_value" },
            "promoted_value": "original_value" // Existing property
        }),
    );

    let result = query.process_source_change(insert).await;
    assert!(result.is_err());
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("already exists and conflict strategy is 'fail'"));
}

#[allow(clippy::unwrap_used)]
async fn test_error_no_match_skip(config: &(impl QueryTestConfig + Send)) {
    let query = setup_query(
        config,
        "promote_error_skip",
        json!([
            { "path": "$.nonexistent.path", "target_name": "promoted_value" }
        ]),
        None,
        Some("skip"), // on_error: skip
    )
    .await;

    let insert = create_node_insert_change(
        "node6",
        json!({
            "id": "node6",
            "original_prop": "data"
        }),
    );

    let result = query.process_source_change(insert).await.unwrap();
    assert_eq!(result.len(), 1);

    // No promotion should happen
    assert!(result.contains(&QueryPartEvaluationContext::Adding {
        after: variablemap!(
            "id" => VariableValue::String("node6".to_string()),
            "original_prop" => VariableValue::String("data".to_string()),
            "nested" => null_vv(),
            "promoted_value" => null_vv(), // Not promoted
            "another_promoted" => null_vv()
        )
    }));
}

#[allow(clippy::unwrap_used)]
async fn test_error_no_match_fail(config: &(impl QueryTestConfig + Send)) {
    let query = setup_query(
        config,
        "promote_error_fail",
        json!([
            { "path": "$.nested.value", "target_name": "promoted_value" }
        ]),
        None,
        None, // Default: fail
    )
    .await;

    let insert = create_node_insert_change(
        "node7",
        json!({
            "id": "node7",
            "original_prop": "data"
            // Missing "nested" field needed by mapping
        }),
    );

    let result = query.process_source_change(insert).await;
    assert!(result.is_err());
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("selected no values"));
}

#[allow(clippy::unwrap_used)]
async fn test_error_multi_match_fail(config: &(impl QueryTestConfig + Send)) {
    let query = setup_query(
        config,
        "promote_multi_match_fail",
        json!([
            // This path selects multiple values
            { "path": "$.items[*].value", "target_name": "promoted_value" }
        ]),
        None,
        Some("fail"), // on_error: fail
    )
    .await;

    let insert = create_node_insert_change(
        "node8",
        json!({
            "id": "node8",
            "items": [ { "value": "a" }, { "value": "b" } ]
        }),
    );

    let result = query.process_source_change(insert).await;
    assert!(result.is_err());
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("selected multiple values"));
}

#[allow(clippy::unwrap_used)]
async fn test_update_change(config: &(impl QueryTestConfig + Send)) {
    let query = setup_query(
        config,
        "promote_update",
        json!([
            { "path": "$.nested.value", "target_name": "promoted_value" }
        ]),
        None,
        None,
    )
    .await;

    let node_id = "node9";

    // Initial insert
    let insert_initial = create_node_insert_change(
        node_id,
        json!({ "id": node_id, "nested": { "value": "initial" } }),
    );
    let result_initial = query.process_source_change(insert_initial).await.unwrap();

    let initial_nested = variablemap!("value" => VariableValue::String("initial".to_string()));
    let expected_initial_state = variablemap!(
        "id" => VariableValue::String(node_id.to_string()),
        "nested" => VariableValue::Object(initial_nested),
        "promoted_value" => VariableValue::String("initial".to_string()),
        "original_prop" => null_vv(),
        "another_promoted" => null_vv()
    );

    assert!(
        result_initial.contains(&QueryPartEvaluationContext::Adding {
            after: expected_initial_state.clone()
        })
    );

    // Perform update
    let update = create_node_update_change(
        node_id,
        json!({ "id": node_id, "nested": { "value": "updated" } }),
    );

    let result_update = query.process_source_change(update).await.unwrap();
    assert_eq!(result_update.len(), 1);

    let updated_nested = variablemap!("value" => VariableValue::String("updated".to_string()));
    assert!(
        result_update.contains(&QueryPartEvaluationContext::Updating {
            before: expected_initial_state,
            after: variablemap!(
                "id" => VariableValue::String(node_id.to_string()),
                "nested" => VariableValue::Object(updated_nested),
                "promoted_value" => VariableValue::String("updated".to_string()),
                "original_prop" => null_vv(),
                "another_promoted" => null_vv()
            )
        })
    );
}

#[allow(clippy::unwrap_used)]
async fn test_delete_change(config: &(impl QueryTestConfig + Send)) {
    let query = setup_query(
        config,
        "promote_delete",
        json!([
            { "path": "$.nested.value", "target_name": "promoted_value" }
        ]),
        None,
        None,
    )
    .await;

    let node_id = "node10";

    // Initial insert
    let insert_initial = create_node_insert_change(
        node_id,
        json!({ "id": node_id, "nested": { "value": "initial_for_delete" } }),
    );
    query.process_source_change(insert_initial).await.unwrap();

    // Update
    let update = create_node_update_change(
        node_id,
        json!({ "id": node_id, "nested": { "value": "updated_for_delete" } }),
    );
    query.process_source_change(update).await.unwrap();

    let updated_nested =
        variablemap!("value" => VariableValue::String("updated_for_delete".to_string()));
    let expected_state_before_delete = variablemap!(
        "id" => VariableValue::String(node_id.to_string()),
        "nested" => VariableValue::Object(updated_nested),
        "promoted_value" => VariableValue::String("updated_for_delete".to_string()),
        "original_prop" => null_vv(),
        "another_promoted" => null_vv()
    );

    // Perform delete
    let delete = create_node_delete_change(node_id);
    let result_delete = query.process_source_change(delete).await.unwrap();
    assert_eq!(result_delete.len(), 1);

    assert!(
        result_delete.contains(&QueryPartEvaluationContext::Removing {
            before: expected_state_before_delete,
        })
    );
}

#[allow(clippy::unwrap_used)]
pub async fn promote_test(config: &(impl QueryTestConfig + Send)) {
    test_basic_promotion(config).await;
    test_multiple_mappings(config).await;
    test_conflict_overwrite(config).await;
    test_conflict_skip(config).await;
    test_conflict_fail(config).await;
    test_error_no_match_skip(config).await;
    test_error_no_match_fail(config).await;
    test_error_multi_match_fail(config).await;
    test_update_change(config).await;
    test_delete_change(config).await;
}

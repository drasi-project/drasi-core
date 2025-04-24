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

//! # Promote Middleware Tests

use std::sync::Arc;

use crate::promote::PromoteMiddlewareFactory;
use drasi_core::{
    in_memory_index::in_memory_element_index::InMemoryElementIndex,
    interface::{FutureElementRef, MiddlewareError, MiddlewareSetupError, SourceMiddlewareFactory},
    models::{
        Element, ElementMetadata, ElementPropertyMap, ElementReference, ElementTimestamp,
        ElementValue, SourceChange, SourceMiddlewareConfig,
    },
};
use serde_json::{json, Value};

// --- Test Helpers ---

fn count_properties(props: &ElementPropertyMap) -> usize {
    props.map_iter(|_, _| ()).count()
}

fn create_mw_config(config_json: Value) -> SourceMiddlewareConfig {
    SourceMiddlewareConfig {
        name: "test_promote".into(),
        kind: "promote".into(),
        config: config_json
            .as_object()
            .expect("Config JSON must be an object")
            .clone(),
    }
}

fn create_node_insert_change(props: Value) -> SourceChange {
    SourceChange::Insert {
        element: Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new("test_source", "node1"),
                labels: vec!["TestNode".into()].into(),
                effective_from: 0,
            },
            properties: props.into(),
        },
    }
}

fn create_node_update_change(props: Value) -> SourceChange {
    SourceChange::Update {
        element: Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new("test_source", "node1"),
                labels: vec!["TestNode".into()].into(),
                effective_from: 1, // Different time from insert
            },
            properties: props.into(),
        },
    }
}

fn create_relation_insert_change(props: Value) -> SourceChange {
    SourceChange::Insert {
        element: Element::Relation {
            metadata: ElementMetadata {
                reference: ElementReference::new("test_source", "rel1"),
                labels: vec!["CONNECTS".into()].into(),
                effective_from: 0,
            },
            properties: props.into(),
            in_node: ElementReference::new("test_source", "node1"),
            out_node: ElementReference::new("test_source", "node2"),
        },
    }
}

fn create_delete_change() -> SourceChange {
    SourceChange::Delete {
        metadata: ElementMetadata {
            reference: ElementReference::new("test_source", "node1"),
            labels: vec!["TestNode".into()].into(),
            effective_from: 2,
        },
    }
}

fn create_future_change() -> SourceChange {
    SourceChange::Future {
        future_ref: FutureElementRef {
            element_ref: ElementReference::new("test_source", "node1"),
            original_time: 0 as ElementTimestamp,
            due_time: 100 as ElementTimestamp,
            group_signature: 0,
        },
    }
}

fn get_props_from_change(change: &SourceChange) -> &ElementPropertyMap {
    match change {
        SourceChange::Insert { element } | SourceChange::Update { element } => {
            element.get_properties()
        }
        _ => panic!("Expected Insert or Update change to get properties"),
    }
}

// --- Test Modules ---

mod process {
    use super::*;

    #[tokio::test]
    /// Promotes a nested `user.id` field into a top‑level property.
    async fn test_basic_promote_from_nested_object() {
        let factory = PromoteMiddlewareFactory::new();
        let config = json!({
            "mappings": [
                {
                    "path": "$.user.id",
                    "target_name": "id"
                }
            ]
        });
        let mw_config = create_mw_config(config);
        let subject = factory.create(&mw_config).unwrap();
        let element_index = Arc::new(InMemoryElementIndex::new());

        let source_change = create_node_insert_change(json!({
            "user": {
                "id": "user123",
                "name": "John Doe"
            }
        }));

        let result = subject
            .process(source_change, element_index.as_ref())
            .await
            .unwrap();

        assert_eq!(result.len(), 1);
        let props = get_props_from_change(&result[0]);

        // Original data should be preserved
        assert_eq!(
            props.get("user"),
            Some(&ElementValue::from(
                &json!({"id": "user123", "name": "John Doe"})
            ))
        );

        // Id should be promoted to top level
        assert_eq!(
            props.get("id"),
            Some(&ElementValue::String("user123".into()))
        );
    }

    #[tokio::test]
    /// Uses a custom `target_name` for the promoted field.
    async fn test_promote_with_custom_target_name() {
        let factory = PromoteMiddlewareFactory::new();
        let config = json!({
            "mappings": [
                {
                    "path": "$.device.id",
                    "target_name": "device_id"
                }
            ]
        });
        let mw_config = create_mw_config(config);
        let subject = factory.create(&mw_config).unwrap();
        let element_index = Arc::new(InMemoryElementIndex::new());

        let source_change = create_node_insert_change(json!({
            "device": {
                "id": "dev-789",
                "type": "sensor"
            }
        }));

        let result = subject
            .process(source_change, element_index.as_ref())
            .await
            .unwrap();

        assert_eq!(result.len(), 1);
        let props = get_props_from_change(&result[0]);

        // Should promote to the specified name
        assert_eq!(
            props.get("device_id"),
            Some(&ElementValue::String("dev-789".into()))
        );
    }

    #[tokio::test]
    /// Promotes multiple values in a single change.
    async fn test_promote_multiple_properties() {
        let factory = PromoteMiddlewareFactory::new();
        let config = json!({
            "mappings": [
                {
                    "path": "$.user.id",
                    "target_name": "id"
                },
                {
                    "path": "$.user.name",
                    "target_name": "username"
                },
                {
                    "path": "$.metadata.created_at",
                    "target_name": "created_at"
                }
            ]
        });
        let mw_config = create_mw_config(config);
        let subject = factory.create(&mw_config).unwrap();
        let element_index = Arc::new(InMemoryElementIndex::new());

        let source_change = create_node_insert_change(json!({
            "user": {
                "id": "user456",
                "name": "Jane Smith"
            },
            "metadata": {
                "created_at": "2023-01-01T00:00:00Z",
                "updated_at": "2023-01-02T00:00:00Z"
            }
        }));

        let result = subject
            .process(source_change, element_index.as_ref())
            .await
            .unwrap();

        assert_eq!(result.len(), 1);
        let props = get_props_from_change(&result[0]);

        // Check all promoted properties
        assert_eq!(
            props.get("id"),
            Some(&ElementValue::String("user456".into()))
        );
        assert_eq!(
            props.get("username"),
            Some(&ElementValue::String("Jane Smith".into()))
        );
        assert_eq!(
            props.get("created_at"),
            Some(&ElementValue::String("2023-01-01T00:00:00Z".into()))
        );

        // Original structure should remain
        assert!(props.get("user").is_some());
        assert!(props.get("metadata").is_some());
    }

    #[tokio::test]
    /// Promotes a value selected from an array.
    async fn test_promote_from_array() {
        let factory = PromoteMiddlewareFactory::new();
        let config = json!({
            "mappings": [
                {
                    "path": "$.tags[0]",
                    "target_name": "primary_tag"
                }
            ]
        });
        let mw_config = create_mw_config(config);
        let subject = factory.create(&mw_config).unwrap();
        let element_index = Arc::new(InMemoryElementIndex::new());

        let source_change = create_node_insert_change(json!({
            "tags": ["important", "urgent", "follow-up"]
        }));

        let result = subject
            .process(source_change, element_index.as_ref())
            .await
            .unwrap();

        assert_eq!(result.len(), 1);
        let props = get_props_from_change(&result[0]);

        assert_eq!(
            props.get("primary_tag"),
            Some(&ElementValue::String("important".into()))
        );
    }

    #[tokio::test]
    /// Promotes using JSONPath bracket notation.
    async fn test_promote_with_bracket_notation() {
        let factory = PromoteMiddlewareFactory::new();
        let config = json!({
            "mappings": [
                {
                    "path": "$['user-data']['user-id']",
                    "target_name": "user_id"
                }
            ]
        });
        let mw_config = create_mw_config(config);
        let subject = factory.create(&mw_config).unwrap();
        let element_index = Arc::new(InMemoryElementIndex::new());

        let source_change = create_node_insert_change(json!({
            "user-data": {
                "user-id": "user789"
            }
        }));

        let result = subject
            .process(source_change, element_index.as_ref())
            .await
            .unwrap();

        assert_eq!(result.len(), 1);
        let props = get_props_from_change(&result[0]);

        assert_eq!(
            props.get("user_id"),
            Some(&ElementValue::String("user789".into()))
        );
    }

    #[tokio::test]
    /// Skips promotion when JSONPath matches nothing and `on_error = skip`.
    async fn test_jsonpath_no_match() {
        let factory = PromoteMiddlewareFactory::new();
        let config = json!({
            "mappings": [
                {
                    "path": "$.non_existent.path",
                    "target_name": "extracted_value"
                }
            ],
            "on_error": "skip"
        });
        let mw_config = create_mw_config(config);
        let subject = factory.create(&mw_config).unwrap();
        let element_index = Arc::new(InMemoryElementIndex::new());

        let source_change = create_node_insert_change(json!({
            "user": {
                "id": "user123"
            }
        }));

        // Should not fail because on_error is skip
        let result = subject
            .process(source_change.clone(), element_index.as_ref())
            .await
            .unwrap();

        assert_eq!(result.len(), 1);

        // Original data should be unchanged - no new properties
        let original_props = match &source_change {
            SourceChange::Insert { element } => element.get_properties(),
            _ => panic!("Expected Insert change"),
        };

        let result_props = get_props_from_change(&result[0]);

        // The result should have the same number of keys as the original
        assert_eq!(
            count_properties(original_props),
            count_properties(result_props)
        );

        // The extracted_value property should not exist
        assert!(result_props.get("extracted_value").is_none());
    }

    #[tokio::test]
    /// Fails when JSONPath matches nothing and `on_error = fail`.
    async fn test_jsonpath_no_match_fail() {
        let factory = PromoteMiddlewareFactory::new();
        let config = json!({
            "mappings": [
                {
                    "path": "$.non_existent.path",
                    "target_name": "extracted_value"
                }
            ],
            "on_error": "fail"
        });
        let mw_config = create_mw_config(config);
        let subject = factory.create(&mw_config).unwrap();
        let element_index = Arc::new(InMemoryElementIndex::new());

        let source_change = create_node_insert_change(json!({
            "user": {
                "id": "user123"
            }
        }));

        let result = subject.process(source_change, element_index.as_ref()).await;

        // Should fail because on_error is fail
        assert!(result.is_err());
        match result {
            Err(MiddlewareError::SourceChangeError(msg)) => {
                assert!(msg.contains("JSONPath"));
                assert!(msg.contains("selected no values"));
            }
            _ => panic!("Expected SourceChangeError"),
        }
    }

    #[tokio::test]
    /// Fails when JSONPath selects multiple values and `on_error = fail`.
    async fn test_jsonpath_multiple_values() {
        let factory = PromoteMiddlewareFactory::new();
        let config = json!({
            "mappings": [
                {
                    "path": "$..id", // Select all 'id' properties at any level
                    "target_name": "some_id"
                }
            ],
            "on_error": "fail"
        });
        let mw_config = create_mw_config(config);
        let subject = factory.create(&mw_config).unwrap();
        let element_index = Arc::new(InMemoryElementIndex::new());

        let source_change = create_node_insert_change(json!({
            "user": {
                "id": "user123"
            },
            "device": {
                "id": "device456"
            }
        }));

        let result = subject.process(source_change, element_index.as_ref()).await;

        // Should fail because JSONPath selects multiple values
        assert!(result.is_err());
        match result {
            Err(MiddlewareError::SourceChangeError(msg)) => {
                assert!(msg.contains("JSONPath"));
                assert!(msg.contains("selected multiple values"));
            }
            _ => panic!("Expected SourceChangeError"),
        }
    }

    #[tokio::test]
    /// Overwrites an existing property on conflict.
    async fn test_conflict_overwrite() {
        let factory = PromoteMiddlewareFactory::new();
        let config = json!({
            "mappings": [
                {
                    "path": "$.user.name",
                    "target_name": "name"
                }
            ],
            "on_conflict": "overwrite"
        });
        let mw_config = create_mw_config(config);
        let subject = factory.create(&mw_config).unwrap();
        let element_index = Arc::new(InMemoryElementIndex::new());

        let source_change = create_node_insert_change(json!({
            "user": {
                "name": "John Doe"
            },
            "name": "Original Name" // Existing top-level property that will be overwritten
        }));

        let result = subject
            .process(source_change, element_index.as_ref())
            .await
            .unwrap();

        assert_eq!(result.len(), 1);
        let props = get_props_from_change(&result[0]);

        // Name should be overwritten with the nested value
        assert_eq!(
            props.get("name"),
            Some(&ElementValue::String("John Doe".into()))
        );
    }

    #[tokio::test]
    /// Skips promotion when a conflict occurs and `on_conflict = skip`.
    async fn test_conflict_skip() {
        let factory = PromoteMiddlewareFactory::new();
        let config = json!({
            "mappings": [
                {
                    "path": "$.user.name",
                    "target_name": "name"
                }
            ],
            "on_conflict": "skip"
        });
        let mw_config = create_mw_config(config);
        let subject = factory.create(&mw_config).unwrap();
        let element_index = Arc::new(InMemoryElementIndex::new());

        let source_change = create_node_insert_change(json!({
            "user": {
                "name": "John Doe"
            },
            "name": "Original Name" // Existing top-level property that will NOT be overwritten
        }));

        let result = subject
            .process(source_change, element_index.as_ref())
            .await
            .unwrap();

        assert_eq!(result.len(), 1);
        let props = get_props_from_change(&result[0]);

        // Name should still have the original value
        assert_eq!(
            props.get("name"),
            Some(&ElementValue::String("Original Name".into()))
        );
    }

    #[tokio::test]
    /// Fails when a conflict occurs and `on_conflict = fail`.
    async fn test_conflict_fail() {
        let factory = PromoteMiddlewareFactory::new();
        let config = json!({
            "mappings": [
                {
                    "path": "$.user.name",
                    "target_name": "name"
                }
            ],
            "on_conflict": "fail"
        });
        let mw_config = create_mw_config(config);
        let subject = factory.create(&mw_config).unwrap();
        let element_index = Arc::new(InMemoryElementIndex::new());

        let source_change = create_node_insert_change(json!({
            "user": {
                "name": "John Doe"
            },
            "name": "Original Name" // Existing property will cause failure
        }));

        let result = subject.process(source_change, element_index.as_ref()).await;

        // Should fail because of conflict
        assert!(result.is_err());
        match result {
            Err(MiddlewareError::SourceChangeError(msg)) => {
                assert!(msg.contains("Property 'name' already exists"));
                assert!(msg.contains("conflict strategy is 'fail'"));
            }
            _ => panic!("Expected SourceChangeError"),
        }
    }

    #[tokio::test]
    /// Promotes values of different JSON types.
    async fn test_promote_different_value_types() {
        let factory = PromoteMiddlewareFactory::new();
        let config = json!({
            "mappings": [
                {
                    "path": "$.string_val",
                    "target_name": "promoted_string"
                },
                {
                    "path": "$.number_val",
                    "target_name": "promoted_number"
                },
                {
                    "path": "$.bool_val",
                    "target_name": "promoted_bool"
                },
                {
                    "path": "$.object_val",
                    "target_name": "promoted_object"
                },
                {
                    "path": "$.array_val",
                    "target_name": "promoted_array"
                },
                {
                    "path": "$.null_val",
                    "target_name": "promoted_null"
                }
            ]
        });
        let mw_config = create_mw_config(config);
        let subject = factory.create(&mw_config).unwrap();
        let element_index = Arc::new(InMemoryElementIndex::new());

        let source_change = create_node_insert_change(json!({
            "nested": {
                "deep": "value"
            },
            "string_val": "test string",
            "number_val": 12345,
            "bool_val": true,
            "object_val": {"key": "value"},
            "array_val": [1, 2, 3],
            "null_val": null
        }));

        let result = subject
            .process(source_change, element_index.as_ref())
            .await
            .unwrap();

        assert_eq!(result.len(), 1);
        let props = get_props_from_change(&result[0]);

        // Check all promoted values have the correct type
        assert_eq!(
            props.get("promoted_string"),
            Some(&ElementValue::String("test string".into()))
        );
        assert_eq!(
            props.get("promoted_number"),
            Some(&ElementValue::Integer(12345))
        );
        assert_eq!(props.get("promoted_bool"), Some(&ElementValue::Bool(true)));
        assert_eq!(
            props.get("promoted_object"),
            Some(&ElementValue::from(&json!({"key": "value"})))
        );
        assert_eq!(
            props.get("promoted_array"),
            Some(&ElementValue::from(&json!([1, 2, 3])))
        );
        assert_eq!(props.get("promoted_null"), Some(&ElementValue::Null));
    }

    #[tokio::test]
    /// Promotes a value inside a Relation element.
    async fn test_process_relation_element() {
        let factory = PromoteMiddlewareFactory::new();
        let config = json!({
            "mappings": [
                {
                    "path": "$.metadata.type",
                    "target_name": "relationship_type"
                }
            ]
        });
        let mw_config = create_mw_config(config);
        let subject = factory.create(&mw_config).unwrap();
        let element_index = Arc::new(InMemoryElementIndex::new());

        let source_change = create_relation_insert_change(json!({
            "metadata": {
                "type": "friendship",
                "since": "2023-01-01"
            }
        }));

        let result = subject
            .process(source_change, element_index.as_ref())
            .await
            .unwrap();

        assert_eq!(result.len(), 1);
        match &result[0] {
            SourceChange::Insert {
                element: Element::Relation { properties, .. },
            } => {
                assert_eq!(
                    properties.get("relationship_type"),
                    Some(&ElementValue::String("friendship".into()))
                );
                assert!(properties.get("metadata").is_some());
            }
            _ => panic!("Expected Insert Relation change"),
        }
    }

    #[tokio::test]
    /// Promotes values in an `Update` change.
    async fn test_process_update_change() {
        let factory = PromoteMiddlewareFactory::new();
        let config = json!({
            "mappings": [
                {
                    "path": "$.user.role",
                    "target_name": "user_role"
                }
            ]
        });
        let mw_config = create_mw_config(config);
        let subject = factory.create(&mw_config).unwrap();
        let element_index = Arc::new(InMemoryElementIndex::new());

        let source_change = create_node_update_change(json!({
            "user": {
                "role": "admin"
            }
        }));

        let result = subject
            .process(source_change, element_index.as_ref())
            .await
            .unwrap();

        assert_eq!(result.len(), 1);
        match &result[0] {
            SourceChange::Update { element } => {
                let props = element.get_properties();
                assert_eq!(
                    props.get("user_role"),
                    Some(&ElementValue::String("admin".into()))
                );
            }
            _ => panic!("Expected Update change"),
        }
    }

    #[tokio::test]
    /// Pass‑through behaviour for `Delete` changes.
    async fn test_process_delete_change_passthrough() {
        let factory = PromoteMiddlewareFactory::new();
        let config = json!({
            "mappings": [
                {
                    "path": "$.user.id",
                    "target_name": "user_id"
                }
            ]
        });
        let mw_config = create_mw_config(config);
        let subject = factory.create(&mw_config).unwrap();
        let element_index = Arc::new(InMemoryElementIndex::new());

        let source_change = create_delete_change();

        let result = subject
            .process(source_change.clone(), element_index.as_ref())
            .await
            .unwrap();

        assert_eq!(result.len(), 1);
        assert_eq!(result[0], source_change); // Should pass through unchanged
    }

    #[tokio::test]
    /// Pass‑through behaviour for `Future` changes.
    async fn test_process_future_change_passthrough() {
        let factory = PromoteMiddlewareFactory::new();
        let config = json!({
            "mappings": [
                {
                    "path": "$.user.id",
                    "target_name": "user_id"
                }
            ]
        });
        let mw_config = create_mw_config(config);
        let subject = factory.create(&mw_config).unwrap();
        let element_index = Arc::new(InMemoryElementIndex::new());

        let source_change = create_future_change();

        let result = subject
            .process(source_change.clone(), element_index.as_ref())
            .await
            .unwrap();

        assert_eq!(result.len(), 1);
        assert_eq!(result[0], source_change); // Should pass through unchanged
    }

    #[tokio::test]
    /// Skips mappings that select multiple values when `on_error = skip`.
    async fn test_jsonpath_multiple_values_skip() {
        let factory = PromoteMiddlewareFactory::new();
        let config = json!({
            "mappings": [
                // This path will match multiple values
                { "path": "$.values[*]", "target_name": "extracted_value" },
                // This path will match a single value and should succeed
                { "path": "$.id", "target_name": "identifier" }
            ],
            "on_error": "skip"
        });
        let mw_config = create_mw_config(config);
        let subject = factory.create(&mw_config).unwrap();
        let element_index = Arc::new(InMemoryElementIndex::new());

        let source_change = create_node_insert_change(json!({
            "id": "test-123",
            "values": [1, 2, 3, 4]
        }));

        // Should not fail because on_error is skip
        let result = subject
            .process(source_change.clone(), element_index.as_ref())
            .await
            .unwrap();

        assert_eq!(result.len(), 1);
        let props = get_props_from_change(&result[0]);

        // The multiple-match path should be skipped
        assert!(props.get("extracted_value").is_none());

        // The valid single-match path should still be processed
        assert_eq!(
            props.get("identifier").unwrap(),
            &ElementValue::String("test-123".into())
        );

        // Original properties should remain
        assert!(props.get("id").is_some());
        assert!(props.get("values").is_some());
    }
}

mod factory {
    use super::*;

    #[test]
    /// Successfully constructs middleware with minimal configuration.
    fn construct_promote_middleware_minimal() {
        let subject = PromoteMiddlewareFactory::new();
        let config = json!({
            "mappings": [
                {
                    "path": "$.user.id",
                    "target_name": "user_id"
                }
            ]
        });
        let mw_config = create_mw_config(config);
        assert!(subject.create(&mw_config).is_ok());
    }

    #[test]
    /// Successfully constructs middleware with full configuration.
    fn construct_promote_middleware_full() {
        let subject = PromoteMiddlewareFactory::new();
        let config = json!({
            "mappings": [
                {
                    "path": "$.user.id",
                    "target_name": "user_id"
                },
                {
                    "path": "$.metadata.created_at",
                    "target_name": "created_at"
                }
            ],
            "on_conflict": "skip",
            "on_error": "skip"
        });
        let mw_config = create_mw_config(config);
        assert!(subject.create(&mw_config).is_ok());
    }

    #[test]
    /// Fails when `mappings` are missing.
    fn fail_missing_mappings() {
        let subject = PromoteMiddlewareFactory::new();
        let config = json!({
            // No mappings array
        });
        let mw_config = create_mw_config(config);
        let result = subject.create(&mw_config);
        assert!(result.is_err());
        match result {
            Err(MiddlewareSetupError::InvalidConfiguration(msg)) => {
                assert!(msg.contains("missing field `mappings`"));
            }
            _ => panic!("Expected InvalidConfiguration error"),
        }
    }

    #[test]
    /// Fails when `mappings` array is empty.
    fn fail_empty_mappings() {
        let subject = PromoteMiddlewareFactory::new();
        let config = json!({
            "mappings": []
        });
        let mw_config = create_mw_config(config);
        let result = subject.create(&mw_config);
        assert!(result.is_err());
        match result {
            Err(MiddlewareSetupError::InvalidConfiguration(msg)) => {
                assert!(msg.contains("At least one mapping must be specified"));
            }
            _ => panic!("Expected InvalidConfiguration error"),
        }
    }

    #[test]
    /// Fails when JSONPath string is empty.
    fn fail_empty_path() {
        let subject = PromoteMiddlewareFactory::new();
        let config = json!({
            "mappings": [
                {
                    "path": "", // Empty path
                    "target_name": "id"
                }
            ]
        });
        let mw_config = create_mw_config(config);
        let result = subject.create(&mw_config);
        assert!(result.is_err());
        match result {
            Err(MiddlewareSetupError::InvalidConfiguration(msg)) => {
                // Check for the specific error message from the custom deserializer
                assert!(msg.contains("Empty JSONPath"), "Error message was: {}", msg);
            }
            _ => panic!("Expected InvalidConfiguration error"),
        }
    }

    #[test]
    /// Fails when JSONPath syntax is invalid.
    fn fail_invalid_jsonpath_syntax() {
        let subject = PromoteMiddlewareFactory::new();
        let config = json!({
            "mappings": [
                {
                    "path": "$.[invalid", // Invalid syntax
                    "target_name": "user_id"
                }
            ]
        });
        let mw_config = create_mw_config(config);
        let result = subject.create(&mw_config);
        assert!(result.is_err());
        match result {
            Err(MiddlewareSetupError::InvalidConfiguration(msg)) => {
                // Check for a message indicating a parse failure
                assert!(
                    msg.contains("Failed to parse rule"),
                    "Error message was: {}",
                    msg
                );
            }
            _ => panic!("Expected InvalidConfiguration error for invalid JSONPath syntax"),
        }
    }

    #[test]
    /// Fails when `target_name` is empty.
    fn fail_empty_target_name() {
        let subject = PromoteMiddlewareFactory::new();
        let config = json!({
            "mappings": [
                {
                    "path": "$.user.id",
                    "target_name": ""
                }
            ]
        });
        let mw_config = create_mw_config(config);
        let result = subject.create(&mw_config);
        assert!(result.is_err());
        match result {
            Err(MiddlewareSetupError::InvalidConfiguration(msg)) => {
                assert!(msg.contains("Empty target_name"));
            }
            _ => panic!("Expected InvalidConfiguration error"),
        }
    }

    #[test]
    /// Fails when `path` field is missing.
    fn fail_missing_path() {
        let subject = PromoteMiddlewareFactory::new();
        let config = json!({
            "mappings": [
                {
                    "target_name": "user_id" // Missing path
                }
            ]
        });
        let mw_config = create_mw_config(config);
        let result = subject.create(&mw_config);
        assert!(result.is_err());
        match result {
            Err(MiddlewareSetupError::InvalidConfiguration(msg)) => {
                assert!(msg.contains("missing field `path`"));
            }
            _ => panic!("Expected InvalidConfiguration error"),
        }
    }

    #[test]
    /// Fails when `target_name` field is missing.
    fn fail_missing_target_name() {
        let subject = PromoteMiddlewareFactory::new();
        let config = json!({
            "mappings": [
                {
                    "path": "$.user.id"  // Missing target_name
                }
            ]
        });
        let mw_config = create_mw_config(config);
        let result = subject.create(&mw_config);
        assert!(result.is_err());
        match result {
            Err(MiddlewareSetupError::InvalidConfiguration(msg)) => {
                assert!(msg.contains("missing field `target_name`"));
            }
            _ => panic!("Expected InvalidConfiguration error"),
        }
    }

    #[test]
    fn fail_invalid_conflict_strategy() {
        let subject = PromoteMiddlewareFactory::new();
        let config = json!({
            "mappings": [
                {
                    "path": "$.user.id",
                    "target_name": "user_id"
                }
            ],
            "on_conflict": "invalid_strategy"
        });
        let mw_config = create_mw_config(config);
        let result = subject.create(&mw_config);
        assert!(result.is_err());
        match result {
            Err(MiddlewareSetupError::InvalidConfiguration(msg)) => {
                assert!(msg.contains("unknown variant `invalid_strategy`"));
            }
            _ => panic!("Expected InvalidConfiguration error"),
        }
    }

    #[test]
    fn fail_invalid_error_handling() {
        let subject = PromoteMiddlewareFactory::new();
        let config = json!({
            "mappings": [
                {
                    "path": "$.user.id",
                    "target_name": "user_id"
                }
            ],
            "on_error": "invalid_strategy"
        });
        let mw_config = create_mw_config(config);
        let result = subject.create(&mw_config);
        assert!(result.is_err());
        match result {
            Err(MiddlewareSetupError::InvalidConfiguration(msg)) => {
                assert!(msg.contains("unknown variant `invalid_strategy`"));
            }
            _ => panic!("Expected InvalidConfiguration error"),
        }
    }

    #[test]
    fn fail_unknown_config_field() {
        let subject = PromoteMiddlewareFactory::new();
        let config = json!({
            "mappings": [
                {
                    "path": "$.user.id",
                    "target_name": "user_id"
                }
            ],
            "unknown_field": "value" // Unknown field
        });
        let mw_config = create_mw_config(config);
        let result = subject.create(&mw_config);
        assert!(result.is_err());
        match result {
            Err(MiddlewareSetupError::InvalidConfiguration(msg)) => {
                assert!(msg.contains("unknown field `unknown_field`"));
            }
            _ => panic!("Expected InvalidConfiguration error"),
        }
    }

    #[test]
    fn test_default_values() {
        let subject = PromoteMiddlewareFactory::new();
        // Only provide the required fields
        let config = json!({
            "mappings": [
                {
                    "path": "$.user.id",
                    "target_name": "user_id"
                }
            ]
        });
        let mw_config = create_mw_config(config);
        assert!(subject.create(&mw_config).is_ok());
        // Defaults will be applied for:
        // - on_conflict: "overwrite"
        // - on_error: "fail"
    }

    #[test]
    fn test_factory_default_creation() {
        let subject = PromoteMiddlewareFactory::default(); // Use default constructor
        let config = json!({
            "mappings": [
                {
                    "path": "$.user.id",
                    "target_name": "user_id"
                }
            ]
        });
        let mw_config = create_mw_config(config);
        assert!(subject.create(&mw_config).is_ok());
    }

    #[test]
    fn test_all_conflict_strategy_values() {
        let subject = PromoteMiddlewareFactory::new();
        let strategies = ["overwrite", "skip", "fail"];

        for strategy in strategies {
            let config = json!({
                "mappings": [
                    {
                        "path": "$.user.id",
                        "target_name": "user_id"
                    }
                ],
                "on_conflict": strategy
            });
            let mw_config = create_mw_config(config);
            assert!(
                subject.create(&mw_config).is_ok(),
                "Failed for on_conflict: {}",
                strategy
            );
        }
    }

    #[test]
    fn test_all_error_handling_values() {
        let subject = PromoteMiddlewareFactory::new();
        let values = ["skip", "fail"];

        for value in values {
            let config = json!({
                "mappings": [
                    {
                        "path": "$.user.id",
                        "target_name": "user_id"
                    }
                ],
                "on_error": value
            });
            let mw_config = create_mw_config(config);
            assert!(
                subject.create(&mw_config).is_ok(),
                "Failed for on_error: {}",
                value
            );
        }
    }
}

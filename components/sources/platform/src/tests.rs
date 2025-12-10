// Copyright 2025 The Drasi Authors.
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

//! Unit tests for the platform source plugin.
//!
//! Tests are organized into the following modules:
//! - `construction`: Tests for creating platform sources
//! - `properties`: Tests for source properties and type name
//! - `builder`: Tests for the `PlatformSourceBuilder`
//! - `config`: Tests for configuration serialization and validation
//! - `event_transformation`: Tests for CloudEvent transformation
//! - `control_events`: Tests for control message handling
//! - `redis_helpers`: Tests for Redis data extraction

use super::*;
use drasi_core::models::{Element, SourceChange};
use serde_json::json;

// ============================================================================
// Construction Tests
// ============================================================================

mod construction {
    use super::*;

    #[test]
    fn test_builder_with_valid_config() {
        let source = PlatformSourceBuilder::new("test-source")
            .with_redis_url("redis://localhost:6379")
            .with_stream_key("test-stream")
            .build();

        assert!(source.is_ok());
    }

    #[test]
    fn test_builder_with_custom_config() {
        let source = PlatformSourceBuilder::new("custom-source")
            .with_redis_url("redis://127.0.0.1:6380")
            .with_stream_key("custom-stream")
            .with_consumer_group("custom-group")
            .with_consumer_name("consumer-1")
            .with_batch_size(50)
            .with_block_ms(10000)
            .build()
            .unwrap();

        assert_eq!(source.id(), "custom-source");
    }
}

// ============================================================================
// Properties Tests
// ============================================================================

mod properties {
    use super::*;
    use drasi_lib::plugin_core::Source;

    #[test]
    fn test_id_returns_correct_value() {
        let source = PlatformSourceBuilder::new("my-platform-source")
            .with_redis_url("redis://localhost:6379")
            .with_stream_key("test-stream")
            .build()
            .unwrap();

        assert_eq!(source.id(), "my-platform-source");
    }

    #[test]
    fn test_type_name_returns_platform() {
        let source = PlatformSourceBuilder::new("test")
            .with_redis_url("redis://localhost:6379")
            .with_stream_key("test-stream")
            .build()
            .unwrap();

        assert_eq!(source.type_name(), "platform");
    }

    #[test]
    fn test_properties_contains_redis_url_and_stream_key() {
        let source = PlatformSourceBuilder::new("test")
            .with_redis_url("redis://192.168.1.1:6379")
            .with_stream_key("my-stream")
            .with_consumer_group("my-group")
            .build()
            .unwrap();
        let props = source.properties();

        assert_eq!(
            props.get("redis_url"),
            Some(&serde_json::Value::String(
                "redis://192.168.1.1:6379".to_string()
            ))
        );
        assert_eq!(
            props.get("stream_key"),
            Some(&serde_json::Value::String("my-stream".to_string()))
        );
        assert_eq!(
            props.get("consumer_group"),
            Some(&serde_json::Value::String("my-group".to_string()))
        );
    }

    #[test]
    fn test_properties_includes_batch_size() {
        let source = PlatformSourceBuilder::new("test")
            .with_redis_url("redis://localhost:6379")
            .with_stream_key("test-stream")
            .with_batch_size(200)
            .build()
            .unwrap();
        let props = source.properties();

        assert_eq!(
            props.get("batch_size"),
            Some(&serde_json::Value::Number(200.into()))
        );
    }
}

// ============================================================================
// Lifecycle Tests
// ============================================================================

mod lifecycle {
    use super::*;
    use drasi_lib::channels::ComponentStatus;
    use drasi_lib::plugin_core::Source;

    #[tokio::test]
    async fn test_initial_status_is_stopped() {
        let source = PlatformSourceBuilder::new("test")
            .with_redis_url("redis://localhost:6379")
            .with_stream_key("test-stream")
            .build()
            .unwrap();

        assert_eq!(source.status().await, ComponentStatus::Stopped);
    }
}

// ============================================================================
// Builder Tests
// ============================================================================

mod builder {
    use super::*;

    #[test]
    fn test_builder_defaults() {
        let source = PlatformSourceBuilder::new("test")
            .with_redis_url("redis://localhost:6379")
            .with_stream_key("test-stream")
            .build()
            .unwrap();

        assert_eq!(source.config.redis_url, "redis://localhost:6379");
        assert_eq!(source.config.stream_key, "test-stream");
        assert_eq!(source.config.consumer_group, "drasi-core");
        assert_eq!(source.config.consumer_name, None);
        assert_eq!(source.config.batch_size, 100);
        assert_eq!(source.config.block_ms, 5000);
    }

    #[test]
    fn test_builder_with_all_options() {
        let source = PlatformSourceBuilder::new("test")
            .with_redis_url("redis://custom:6379")
            .with_stream_key("custom-stream")
            .with_consumer_group("custom-group")
            .with_consumer_name("consumer-1")
            .with_batch_size(50)
            .with_block_ms(10000)
            .build()
            .unwrap();

        assert_eq!(source.config.redis_url, "redis://custom:6379");
        assert_eq!(source.config.stream_key, "custom-stream");
        assert_eq!(source.config.consumer_group, "custom-group");
        assert_eq!(source.config.consumer_name, Some("consumer-1".to_string()));
        assert_eq!(source.config.batch_size, 50);
        assert_eq!(source.config.block_ms, 10000);
    }

    #[test]
    fn test_builder_chaining() {
        let source = PlatformSourceBuilder::new("test")
            .with_redis_url("redis://localhost:6379")
            .with_stream_key("stream1")
            .with_stream_key("stream2") // Override
            .build()
            .unwrap();

        assert_eq!(source.config.stream_key, "stream2");
    }

    #[test]
    fn test_builder_id() {
        let source = PlatformSource::builder("my-platform-source")
            .with_redis_url("redis://localhost:6379")
            .with_stream_key("test-stream")
            .build()
            .unwrap();

        assert_eq!(source.base.id, "my-platform-source");
    }
}

// ============================================================================
// Config Tests
// ============================================================================

mod config {
    use super::*;

    #[test]
    fn test_config_serialization() {
        let config = PlatformSourceConfig {
            redis_url: "redis://localhost:6379".to_string(),
            stream_key: "test-stream".to_string(),
            consumer_group: "test-group".to_string(),
            consumer_name: Some("consumer-1".to_string()),
            batch_size: 50,
            block_ms: 10000,
        };

        let json = serde_json::to_string(&config).unwrap();
        let deserialized: PlatformSourceConfig = serde_json::from_str(&json).unwrap();

        assert_eq!(config, deserialized);
    }

    #[test]
    fn test_config_deserialization_with_defaults() {
        let json = r#"{
            "redis_url": "redis://localhost:6379",
            "stream_key": "my-stream"
        }"#;

        let config: PlatformSourceConfig = serde_json::from_str(json).unwrap();

        assert_eq!(config.redis_url, "redis://localhost:6379");
        assert_eq!(config.stream_key, "my-stream");
        assert_eq!(config.consumer_group, "drasi-core"); // default
        assert_eq!(config.consumer_name, None);
        assert_eq!(config.batch_size, 100); // default
        assert_eq!(config.block_ms, 5000); // default
    }

    #[test]
    fn test_config_validation_empty_redis_url() {
        let config = PlatformSourceConfig {
            redis_url: "".to_string(),
            stream_key: "test-stream".to_string(),
            consumer_group: "test-group".to_string(),
            consumer_name: None,
            batch_size: 100,
            block_ms: 5000,
        };

        let result = config.validate();
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("redis_url cannot be empty"));
    }

    #[test]
    fn test_config_validation_empty_stream_key() {
        let config = PlatformSourceConfig {
            redis_url: "redis://localhost:6379".to_string(),
            stream_key: "".to_string(),
            consumer_group: "test-group".to_string(),
            consumer_name: None,
            batch_size: 100,
            block_ms: 5000,
        };

        let result = config.validate();
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("stream_key cannot be empty"));
    }

    #[test]
    fn test_config_validation_empty_consumer_group() {
        let config = PlatformSourceConfig {
            redis_url: "redis://localhost:6379".to_string(),
            stream_key: "test-stream".to_string(),
            consumer_group: "".to_string(),
            consumer_name: None,
            batch_size: 100,
            block_ms: 5000,
        };

        let result = config.validate();
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("consumer_group cannot be empty"));
    }

    #[test]
    fn test_config_validation_zero_batch_size() {
        let config = PlatformSourceConfig {
            redis_url: "redis://localhost:6379".to_string(),
            stream_key: "test-stream".to_string(),
            consumer_group: "test-group".to_string(),
            consumer_name: None,
            batch_size: 0,
            block_ms: 5000,
        };

        let result = config.validate();
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("batch_size cannot be 0"));
    }

    #[test]
    fn test_config_validation_success() {
        let config = PlatformSourceConfig {
            redis_url: "redis://localhost:6379".to_string(),
            stream_key: "test-stream".to_string(),
            consumer_group: "test-group".to_string(),
            consumer_name: None,
            batch_size: 100,
            block_ms: 5000,
        };

        let result = config.validate();
        assert!(result.is_ok());
    }
}

// ============================================================================
// Event Transformation Tests
// ============================================================================

mod event_transformation {
    use super::*;

    #[test]
    fn test_transform_platform_insert_node() {
        let cloud_event = json!({
            "data": [{
                "op": "i",
                "payload": {
                    "after": {
                        "id": "node1",
                        "labels": ["Person"],
                        "properties": { "name": "Alice", "age": 30 }
                    },
                    "source": {
                        "db": "test_db",
                        "table": "node",
                        "ts_ns": 1234567890000000_u64
                    }
                }
            }],
            "id": "test-123",
            "source": "test-source",
            "type": "com.dapr.event.sent"
        });

        let results = transform_platform_event(cloud_event, "test_source").unwrap();
        assert_eq!(results.len(), 1);

        match &results[0].source_change {
            SourceChange::Insert { element } => match element {
                Element::Node {
                    metadata,
                    properties,
                } => {
                    assert_eq!(metadata.reference.element_id.as_ref(), "node1");
                    assert_eq!(metadata.reference.source_id.as_ref(), "test_source");
                    assert_eq!(metadata.labels.len(), 1);
                    assert_eq!(metadata.labels[0].as_ref(), "Person");
                    assert_eq!(metadata.effective_from, 1234567890000000);
                    assert!(properties.get("name").is_some());
                    assert!(properties.get("age").is_some());
                }
                _ => panic!("Expected Node element"),
            },
            _ => panic!("Expected Insert variant"),
        }
    }

    #[test]
    fn test_transform_platform_update_node() {
        let cloud_event = json!({
            "data": [{
                "op": "u",
                "payload": {
                    "after": {
                        "id": "node1",
                        "labels": ["Person", "Premium"],
                        "properties": { "name": "Alice Updated", "age": 31, "premium": true }
                    },
                    "source": {
                        "db": "test_db",
                        "table": "node",
                        "ts_ns": 1234567891000000_u64
                    }
                }
            }]
        });

        let results = transform_platform_event(cloud_event, "test_source").unwrap();
        assert_eq!(results.len(), 1);

        match &results[0].source_change {
            SourceChange::Update { element } => match element {
                Element::Node { metadata, .. } => {
                    assert_eq!(metadata.reference.element_id.as_ref(), "node1");
                    assert_eq!(metadata.labels.len(), 2);
                    assert_eq!(metadata.effective_from, 1234567891000000);
                }
                _ => panic!("Expected Node element"),
            },
            _ => panic!("Expected Update variant"),
        }
    }

    #[test]
    fn test_transform_platform_delete_node() {
        let cloud_event = json!({
            "data": [{
                "op": "d",
                "payload": {
                    "before": {
                        "id": "node1",
                        "labels": ["Person"],
                        "properties": { "name": "Alice" }
                    },
                    "source": {
                        "db": "test_db",
                        "table": "node",
                        "ts_ns": 1234567892000000_u64
                    }
                }
            }]
        });

        let results = transform_platform_event(cloud_event, "test_source").unwrap();
        assert_eq!(results.len(), 1);

        match &results[0].source_change {
            SourceChange::Delete { metadata } => {
                assert_eq!(metadata.reference.element_id.as_ref(), "node1");
                assert_eq!(metadata.reference.source_id.as_ref(), "test_source");
                assert_eq!(metadata.labels.len(), 1);
                assert_eq!(metadata.effective_from, 1234567892000000);
            }
            _ => panic!("Expected Delete variant"),
        }
    }

    #[test]
    fn test_transform_platform_insert_relation() {
        let cloud_event = json!({
            "data": [{
                "op": "i",
                "payload": {
                    "after": {
                        "id": "rel1",
                        "labels": ["KNOWS"],
                        "startId": "node1",
                        "endId": "node2",
                        "properties": { "since": 2020 }
                    },
                    "source": {
                        "db": "test_db",
                        "table": "rel",
                        "ts_ns": 1000000000_u64
                    }
                }
            }]
        });

        let results = transform_platform_event(cloud_event, "test_source").unwrap();
        assert_eq!(results.len(), 1);

        match &results[0].source_change {
            SourceChange::Insert { element } => match element {
                Element::Relation {
                    metadata,
                    properties,
                    out_node,
                    in_node,
                } => {
                    assert_eq!(metadata.reference.element_id.as_ref(), "rel1");
                    assert_eq!(metadata.labels.len(), 1);
                    assert_eq!(metadata.labels[0].as_ref(), "KNOWS");
                    assert_eq!(out_node.element_id.as_ref(), "node1");
                    assert_eq!(in_node.element_id.as_ref(), "node2");
                    assert_eq!(out_node.source_id.as_ref(), "test_source");
                    assert_eq!(in_node.source_id.as_ref(), "test_source");
                    assert!(properties.get("since").is_some());
                }
                _ => panic!("Expected Relation element"),
            },
            _ => panic!("Expected Insert variant"),
        }
    }

    #[test]
    fn test_transform_platform_update_relation() {
        let cloud_event = json!({
            "data": [{
                "op": "u",
                "payload": {
                    "after": {
                        "id": "rel1",
                        "labels": ["KNOWS"],
                        "startId": "node1",
                        "endId": "node2",
                        "properties": { "since": 2021, "strength": 0.8 }
                    },
                    "source": {
                        "table": "rel",
                        "ts_ns": 2000000000_u64
                    }
                }
            }]
        });

        let results = transform_platform_event(cloud_event, "test_source").unwrap();
        assert_eq!(results.len(), 1);

        match &results[0].source_change {
            SourceChange::Update { element } => match element {
                Element::Relation {
                    out_node, in_node, ..
                } => {
                    assert_eq!(out_node.element_id.as_ref(), "node1");
                    assert_eq!(in_node.element_id.as_ref(), "node2");
                }
                _ => panic!("Expected Relation element"),
            },
            _ => panic!("Expected Update variant"),
        }
    }

    #[test]
    fn test_transform_property_types() {
        let cloud_event = json!({
            "data": [{
                "op": "i",
                "payload": {
                    "after": {
                        "id": "node1",
                        "labels": ["Test"],
                        "properties": {
                            "string_prop": "hello",
                            "int_prop": 42,
                            "float_prop": 1.23456,
                            "bool_prop": true,
                            "null_prop": null,
                            "array_prop": [1, 2, 3],
                            "object_prop": { "nested": "value" }
                        }
                    },
                    "source": {
                        "table": "node",
                        "ts_ns": 1000000000_u64
                    }
                }
            }]
        });

        let results = transform_platform_event(cloud_event, "test_source").unwrap();
        assert_eq!(results.len(), 1);

        match &results[0].source_change {
            SourceChange::Insert { element } => match element {
                Element::Node { properties, .. } => {
                    assert!(properties.get("string_prop").is_some());
                    assert!(properties.get("int_prop").is_some());
                    assert!(properties.get("float_prop").is_some());
                    assert!(properties.get("bool_prop").is_some());
                    assert!(properties.get("null_prop").is_some());
                    assert!(properties.get("array_prop").is_some());
                    assert!(properties.get("object_prop").is_some());
                }
                _ => panic!("Expected Node element"),
            },
            _ => panic!("Expected Insert variant"),
        }
    }

    #[test]
    fn test_transform_empty_properties() {
        let cloud_event = json!({
            "data": [{
                "op": "i",
                "payload": {
                    "after": {
                        "id": "node1",
                        "labels": ["Empty"],
                        "properties": {}
                    },
                    "source": {
                        "table": "node",
                        "ts_ns": 1000000000_u64
                    }
                }
            }]
        });

        let results = transform_platform_event(cloud_event, "test_source").unwrap();
        assert_eq!(results.len(), 1);

        match &results[0].source_change {
            SourceChange::Insert { element } => match element {
                Element::Node { properties, .. } => {
                    assert!(
                        properties.get("name").is_none()
                            || matches!(
                                properties.get("name"),
                                Some(drasi_core::models::ElementValue::Null)
                            )
                    );
                }
                _ => panic!("Expected Node element"),
            },
            _ => panic!("Expected Insert variant"),
        }
    }

    #[test]
    fn test_transform_multiple_labels() {
        let cloud_event = json!({
            "data": [{
                "op": "i",
                "payload": {
                    "after": {
                        "id": "node1",
                        "labels": ["Person", "Employee", "Manager"],
                        "properties": {}
                    },
                    "source": {
                        "table": "node",
                        "ts_ns": 1000000000_u64
                    }
                }
            }]
        });

        let results = transform_platform_event(cloud_event, "test_source").unwrap();
        assert_eq!(results.len(), 1);

        match &results[0].source_change {
            SourceChange::Insert { element } => match element {
                Element::Node { metadata, .. } => {
                    assert_eq!(metadata.labels.len(), 3);
                    assert_eq!(metadata.labels[0].as_ref(), "Person");
                    assert_eq!(metadata.labels[1].as_ref(), "Employee");
                    assert_eq!(metadata.labels[2].as_ref(), "Manager");
                }
                _ => panic!("Expected Node element"),
            },
            _ => panic!("Expected Insert variant"),
        }
    }

    #[test]
    fn test_transform_multiple_events_in_data_array() {
        let cloud_event = json!({
            "data": [
                {
                    "op": "i",
                    "payload": {
                        "after": {
                            "id": "node1",
                            "labels": ["Person"],
                            "properties": { "name": "Alice" }
                        },
                        "source": {
                            "table": "node",
                            "ts_ns": 1000000000_u64
                        }
                    }
                },
                {
                    "op": "i",
                    "payload": {
                        "after": {
                            "id": "node2",
                            "labels": ["Person"],
                            "properties": { "name": "Bob" }
                        },
                        "source": {
                            "table": "node",
                            "ts_ns": 2000000000_u64
                        }
                    }
                }
            ]
        });

        let results = transform_platform_event(cloud_event, "test_source").unwrap();
        assert_eq!(results.len(), 2);

        match &results[0].source_change {
            SourceChange::Insert { element } => match element {
                Element::Node { metadata, .. } => {
                    assert_eq!(metadata.reference.element_id.as_ref(), "node1");
                }
                _ => panic!("Expected Node element"),
            },
            _ => panic!("Expected Insert variant"),
        }

        match &results[1].source_change {
            SourceChange::Insert { element } => match element {
                Element::Node { metadata, .. } => {
                    assert_eq!(metadata.reference.element_id.as_ref(), "node2");
                }
                _ => panic!("Expected Node element"),
            },
            _ => panic!("Expected Insert variant"),
        }
    }

    #[test]
    fn test_transform_real_world_dapr_event() {
        let cloud_event = json!({
            "data": [{
                "op": "u",
                "payload": {
                    "after": {
                        "id": "public:Message:4",
                        "labels": ["Message"],
                        "properties": {
                            "From": "David",
                            "Message": "hello",
                            "MessageId": 4
                        }
                    },
                    "source": {
                        "db": "hello-world",
                        "lsn": 26715048,
                        "table": "node",
                        "ts_ns": 1759503489836973000_u64
                    }
                },
                "reactivatorEnd_ns": 1759503491747344212_u64,
                "reactivatorStart_ns": 1759503491640055712_u64
            }],
            "datacontenttype": "application/json",
            "id": "5095316c-f4b6-43db-9887-f2730cf1dc2b",
            "pubsubname": "drasi-pubsub",
            "source": "hello-world-reactivator",
            "specversion": "1.0",
            "time": "2025-10-03T14:58:12Z",
            "topic": "hello-world-change",
            "type": "com.dapr.event.sent"
        });

        let results = transform_platform_event(cloud_event, "hello-world-source").unwrap();
        assert_eq!(results.len(), 1);

        match &results[0].source_change {
            SourceChange::Update { element } => match element {
                Element::Node {
                    metadata,
                    properties,
                } => {
                    assert_eq!(metadata.reference.element_id.as_ref(), "public:Message:4");
                    assert_eq!(metadata.reference.source_id.as_ref(), "hello-world-source");
                    assert_eq!(metadata.labels.len(), 1);
                    assert_eq!(metadata.labels[0].as_ref(), "Message");
                    assert_eq!(metadata.effective_from, 1759503489836973000);
                    assert!(properties.get("From").is_some());
                    assert!(properties.get("Message").is_some());
                    assert!(properties.get("MessageId").is_some());
                }
                _ => panic!("Expected Node element"),
            },
            _ => panic!("Expected Update variant"),
        }
    }

    #[test]
    fn test_transform_large_timestamp() {
        let cloud_event = json!({
            "data": [{
                "op": "i",
                "payload": {
                    "after": {
                        "id": "node1",
                        "labels": ["Test"],
                        "properties": {}
                    },
                    "source": {
                        "table": "node",
                        "ts_ns": 9999999999999000000_u64
                    }
                }
            }]
        });

        let results = transform_platform_event(cloud_event, "test_source").unwrap();
        assert_eq!(results.len(), 1);

        match &results[0].source_change {
            SourceChange::Insert { element } => match element {
                Element::Node { metadata, .. } => {
                    assert_eq!(metadata.effective_from, 9999999999999000000);
                }
                _ => panic!("Expected Node element"),
            },
            _ => panic!("Expected Insert variant"),
        }
    }
}

// ============================================================================
// Error Handling Tests
// ============================================================================

mod error_handling {
    use super::*;

    #[test]
    fn test_transform_missing_op_field() {
        let cloud_event = json!({
            "data": [{
                "payload": {
                    "after": {
                        "id": "node1",
                        "labels": ["Test"],
                        "properties": {}
                    },
                    "source": {
                        "table": "node",
                        "ts_ns": 1000000000_u64
                    }
                }
            }]
        });

        let result = transform_platform_event(cloud_event, "test_source");
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Missing or invalid 'op' field"));
    }

    #[test]
    fn test_transform_missing_table_field() {
        let cloud_event = json!({
            "data": [{
                "op": "i",
                "payload": {
                    "after": {
                        "id": "node1",
                        "labels": ["Test"],
                        "properties": {}
                    },
                    "source": {
                        "ts_ns": 1000000000_u64
                    }
                }
            }]
        });

        let result = transform_platform_event(cloud_event, "test_source");
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Missing or invalid 'payload.source.table' field"));
    }

    #[test]
    fn test_transform_invalid_operation_type() {
        let cloud_event = json!({
            "data": [{
                "op": "x",
                "payload": {
                    "after": {
                        "id": "node1",
                        "labels": ["Test"],
                        "properties": {}
                    },
                    "source": {
                        "table": "node",
                        "ts_ns": 1000000000_u64
                    }
                }
            }]
        });

        let result = transform_platform_event(cloud_event, "test_source");
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Unknown operation type"));
    }

    #[test]
    fn test_transform_missing_element_id() {
        let cloud_event = json!({
            "data": [{
                "op": "i",
                "payload": {
                    "after": {
                        "labels": ["Test"],
                        "properties": {}
                    },
                    "source": {
                        "table": "node",
                        "ts_ns": 1000000000_u64
                    }
                }
            }]
        });

        let result = transform_platform_event(cloud_event, "test_source");
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Missing or invalid element 'id' field"));
    }

    #[test]
    fn test_transform_missing_labels() {
        let cloud_event = json!({
            "data": [{
                "op": "i",
                "payload": {
                    "after": {
                        "id": "node1",
                        "properties": {}
                    },
                    "source": {
                        "table": "node",
                        "ts_ns": 1000000000_u64
                    }
                }
            }]
        });

        let result = transform_platform_event(cloud_event, "test_source");
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Missing or invalid 'labels' field"));
    }

    #[test]
    fn test_transform_empty_labels() {
        let cloud_event = json!({
            "data": [{
                "op": "i",
                "payload": {
                    "after": {
                        "id": "node1",
                        "labels": [],
                        "properties": {}
                    },
                    "source": {
                        "table": "node",
                        "ts_ns": 1000000000_u64
                    }
                }
            }]
        });

        let result = transform_platform_event(cloud_event, "test_source");
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Labels array is empty"));
    }

    #[test]
    fn test_transform_missing_timestamp() {
        let cloud_event = json!({
            "data": [{
                "op": "i",
                "payload": {
                    "after": {
                        "id": "node1",
                        "labels": ["Test"],
                        "properties": {}
                    },
                    "source": {
                        "table": "node"
                    }
                }
            }]
        });

        let result = transform_platform_event(cloud_event, "test_source");
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Missing or invalid 'payload.source.ts_ns' field"));
    }

    #[test]
    fn test_transform_missing_start_id_for_relation() {
        let cloud_event = json!({
            "data": [{
                "op": "i",
                "payload": {
                    "after": {
                        "id": "rel1",
                        "labels": ["KNOWS"],
                        "endId": "node2",
                        "properties": {}
                    },
                    "source": {
                        "table": "rel",
                        "ts_ns": 1000000000_u64
                    }
                }
            }]
        });

        let result = transform_platform_event(cloud_event, "test_source");
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Missing or invalid 'startId'"));
    }

    #[test]
    fn test_transform_missing_end_id_for_relation() {
        let cloud_event = json!({
            "data": [{
                "op": "i",
                "payload": {
                    "after": {
                        "id": "rel1",
                        "labels": ["KNOWS"],
                        "startId": "node1",
                        "properties": {}
                    },
                    "source": {
                        "table": "rel",
                        "ts_ns": 1000000000_u64
                    }
                }
            }]
        });

        let result = transform_platform_event(cloud_event, "test_source");
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Missing or invalid 'endId'"));
    }
}

// ============================================================================
// Redis Helper Tests
// ============================================================================

mod redis_helpers {
    use super::*;

    #[test]
    fn test_extract_event_data_with_data_key() {
        let mut entry = HashMap::new();
        entry.insert(
            "data".to_string(),
            redis::Value::Data(b"test data".to_vec()),
        );

        let result = extract_event_data(&entry).unwrap();
        assert_eq!(result, "test data");
    }

    #[test]
    fn test_extract_event_data_with_event_key() {
        let mut entry = HashMap::new();
        entry.insert(
            "event".to_string(),
            redis::Value::Data(b"event data".to_vec()),
        );

        let result = extract_event_data(&entry).unwrap();
        assert_eq!(result, "event data");
    }

    #[test]
    fn test_extract_event_data_missing_key() {
        let entry = HashMap::new();

        let result = extract_event_data(&entry);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("No event data found"));
    }
}

// ============================================================================
// Message Type Detection Tests
// ============================================================================

mod message_type_detection {
    use super::*;

    #[test]
    fn test_detect_message_type_data() {
        let cloud_event = json!({
            "data": [{
                "op": "i",
                "payload": {
                    "after": {
                        "id": "node1",
                        "labels": ["Person"],
                        "properties": { "name": "Alice" }
                    },
                    "source": {
                        "db": "mydb",
                        "table": "node",
                        "ts_ns": 1000000000_u64
                    }
                }
            }]
        });

        let msg_type = detect_message_type(&cloud_event);
        assert_eq!(msg_type, MessageType::Data);
    }

    #[test]
    fn test_detect_message_type_control_lowercase() {
        let cloud_event = json!({
            "data": [{
                "op": "i",
                "payload": {
                    "after": {
                        "queryId": "query1",
                        "queryNodeId": "default"
                    },
                    "source": {
                        "db": "drasi",
                        "table": "SourceSubscription",
                        "ts_ns": 1000000000_u64
                    }
                }
            }]
        });

        let msg_type = detect_message_type(&cloud_event);
        assert_eq!(
            msg_type,
            MessageType::Control("SourceSubscription".to_string())
        );
    }

    #[test]
    fn test_detect_message_type_control_uppercase() {
        let cloud_event = json!({
            "data": [{
                "op": "i",
                "payload": {
                    "after": {
                        "queryId": "query1",
                        "queryNodeId": "default"
                    },
                    "source": {
                        "db": "DRASI",
                        "table": "SourceSubscription",
                        "ts_ns": 1000000000_u64
                    }
                }
            }]
        });

        let msg_type = detect_message_type(&cloud_event);
        assert_eq!(
            msg_type,
            MessageType::Control("SourceSubscription".to_string())
        );
    }

    #[test]
    fn test_detect_message_type_control_mixedcase() {
        let cloud_event = json!({
            "data": [{
                "op": "i",
                "payload": {
                    "after": {
                        "queryId": "query1",
                        "queryNodeId": "default"
                    },
                    "source": {
                        "db": "DrAsI",
                        "table": "SourceSubscription",
                        "ts_ns": 1000000000_u64
                    }
                }
            }]
        });

        let msg_type = detect_message_type(&cloud_event);
        assert_eq!(
            msg_type,
            MessageType::Control("SourceSubscription".to_string())
        );
    }

    #[test]
    fn test_detect_message_type_empty_data_array() {
        let cloud_event = json!({
            "data": []
        });

        let msg_type = detect_message_type(&cloud_event);
        assert_eq!(msg_type, MessageType::Data);
    }

    #[test]
    fn test_detect_message_type_missing_data_field() {
        let cloud_event = json!({
            "id": "test-123",
            "source": "test-source"
        });

        let msg_type = detect_message_type(&cloud_event);
        assert_eq!(msg_type, MessageType::Data);
    }
}

// ============================================================================
// Control Event Tests
// ============================================================================

mod control_events {
    use super::*;

    #[test]
    fn test_transform_control_event_source_subscription_insert() {
        let cloud_event = json!({
            "data": [{
                "op": "i",
                "payload": {
                    "after": {
                        "queryId": "query1",
                        "queryNodeId": "default",
                        "nodeLabels": ["Person", "Employee"],
                        "relLabels": ["KNOWS", "WORKS_FOR"]
                    },
                    "source": {
                        "db": "Drasi",
                        "table": "SourceSubscription",
                        "ts_ns": 1000000000_u64
                    }
                }
            }]
        });

        let results = transform_control_event(cloud_event, "SourceSubscription").unwrap();
        assert_eq!(results.len(), 1);

        match &results[0] {
            SourceControl::Subscription {
                query_id,
                query_node_id,
                node_labels,
                rel_labels,
                operation,
            } => {
                assert_eq!(query_id, "query1");
                assert_eq!(query_node_id, "default");
                assert_eq!(node_labels.len(), 2);
                assert!(node_labels.contains(&"Person".to_string()));
                assert!(node_labels.contains(&"Employee".to_string()));
                assert_eq!(rel_labels.len(), 2);
                assert!(rel_labels.contains(&"KNOWS".to_string()));
                assert!(rel_labels.contains(&"WORKS_FOR".to_string()));
                assert_eq!(operation, &ControlOperation::Insert);
            }
        }
    }

    #[test]
    fn test_transform_control_event_source_subscription_update() {
        let cloud_event = json!({
            "data": [{
                "op": "u",
                "payload": {
                    "after": {
                        "queryId": "query1",
                        "queryNodeId": "default",
                        "nodeLabels": ["Person"],
                        "relLabels": []
                    },
                    "source": {
                        "db": "Drasi",
                        "table": "SourceSubscription",
                        "ts_ns": 1000000000_u64
                    }
                }
            }]
        });

        let results = transform_control_event(cloud_event, "SourceSubscription").unwrap();
        assert_eq!(results.len(), 1);

        match &results[0] {
            SourceControl::Subscription { operation, .. } => {
                assert_eq!(operation, &ControlOperation::Update);
            }
        }
    }

    #[test]
    fn test_transform_control_event_source_subscription_delete() {
        let cloud_event = json!({
            "data": [{
                "op": "d",
                "payload": {
                    "before": {
                        "queryId": "query1",
                        "queryNodeId": "default",
                        "nodeLabels": [],
                        "relLabels": []
                    },
                    "source": {
                        "db": "Drasi",
                        "table": "SourceSubscription",
                        "ts_ns": 1000000000_u64
                    }
                }
            }]
        });

        let results = transform_control_event(cloud_event, "SourceSubscription").unwrap();
        assert_eq!(results.len(), 1);

        match &results[0] {
            SourceControl::Subscription { operation, .. } => {
                assert_eq!(operation, &ControlOperation::Delete);
            }
        }
    }

    #[test]
    fn test_transform_control_event_empty_labels() {
        let cloud_event = json!({
            "data": [{
                "op": "i",
                "payload": {
                    "after": {
                        "queryId": "query1",
                        "queryNodeId": "default",
                        "nodeLabels": [],
                        "relLabels": []
                    },
                    "source": {
                        "db": "Drasi",
                        "table": "SourceSubscription",
                        "ts_ns": 1000000000_u64
                    }
                }
            }]
        });

        let results = transform_control_event(cloud_event, "SourceSubscription").unwrap();
        assert_eq!(results.len(), 1);

        match &results[0] {
            SourceControl::Subscription {
                node_labels,
                rel_labels,
                ..
            } => {
                assert!(node_labels.is_empty());
                assert!(rel_labels.is_empty());
            }
        }
    }

    #[test]
    fn test_transform_control_event_missing_labels() {
        let cloud_event = json!({
            "data": [{
                "op": "i",
                "payload": {
                    "after": {
                        "queryId": "query1",
                        "queryNodeId": "default"
                    },
                    "source": {
                        "db": "Drasi",
                        "table": "SourceSubscription",
                        "ts_ns": 1000000000_u64
                    }
                }
            }]
        });

        let results = transform_control_event(cloud_event, "SourceSubscription").unwrap();
        assert_eq!(results.len(), 1);

        match &results[0] {
            SourceControl::Subscription {
                node_labels,
                rel_labels,
                ..
            } => {
                assert!(node_labels.is_empty());
                assert!(rel_labels.is_empty());
            }
        }
    }

    #[test]
    fn test_transform_control_event_unknown_type() {
        let cloud_event = json!({
            "data": [{
                "op": "i",
                "payload": {
                    "after": {
                        "queryId": "query1",
                        "queryNodeId": "default"
                    },
                    "source": {
                        "db": "Drasi",
                        "table": "UnknownControlType",
                        "ts_ns": 1000000000_u64
                    }
                }
            }]
        });

        let results = transform_control_event(cloud_event, "UnknownControlType").unwrap();
        assert_eq!(results.len(), 0);
    }

    #[test]
    fn test_transform_control_event_multiple_events() {
        let cloud_event = json!({
            "data": [
                {
                    "op": "i",
                    "payload": {
                        "after": {
                            "queryId": "query1",
                            "queryNodeId": "default",
                            "nodeLabels": ["Person"],
                            "relLabels": []
                        },
                        "source": {
                            "db": "Drasi",
                            "table": "SourceSubscription",
                            "ts_ns": 1000000000_u64
                        }
                    }
                },
                {
                    "op": "i",
                    "payload": {
                        "after": {
                            "queryId": "query2",
                            "queryNodeId": "default",
                            "nodeLabels": ["Product"],
                            "relLabels": []
                        },
                        "source": {
                            "db": "Drasi",
                            "table": "SourceSubscription",
                            "ts_ns": 2000000000_u64
                        }
                    }
                }
            ]
        });

        let results = transform_control_event(cloud_event, "SourceSubscription").unwrap();
        assert_eq!(results.len(), 2);

        match &results[0] {
            SourceControl::Subscription { query_id, .. } => {
                assert_eq!(query_id, "query1");
            }
        }

        match &results[1] {
            SourceControl::Subscription { query_id, .. } => {
                assert_eq!(query_id, "query2");
            }
        }
    }
}

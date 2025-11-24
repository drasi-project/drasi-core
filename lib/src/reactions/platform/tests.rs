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

//! Integration tests for Platform Reaction
//!
//! These tests verify the interaction between components (transformer, publisher, CloudEvent format).
//! Individual component unit tests are in their respective module files.

#[cfg(test)]
use super::*;
#[cfg(test)]
use crate::channels::QueryResult;
#[cfg(test)]
use serde_json::json;
#[cfg(test)]
use std::collections::HashMap;

/// Helper function to create a basic test configuration
#[cfg(test)]
fn create_test_config() -> ReactionConfig {
    use crate::reactions::platform::PlatformReactionConfig;

    ReactionConfig {
        id: "test-reaction".to_string(),
        queries: vec!["test-query".to_string()],
        auto_start: true,
        config: crate::config::ReactionSpecificConfig::Platform(PlatformReactionConfig {
            redis_url: "redis://localhost:6379".to_string(),
            pubsub_name: None,
            source_name: None,
            max_stream_length: None,
            emit_control_events: false,
            batch_enabled: false,
            batch_max_size: 100,
            batch_max_wait_ms: 100,
        }),
        priority_queue_capacity: None,
    }
}

/// Helper function to create a QueryResult with test data
#[cfg(test)]
fn create_test_query_result(query_id: &str, results: Vec<serde_json::Value>) -> QueryResult {
    QueryResult {
        query_id: query_id.to_string(),
        timestamp: chrono::Utc::now(),
        results,
        metadata: HashMap::new(),
        profiling: None,
    }
}

#[cfg(test)]
mod transformer_integration_tests {
    use super::*;

    #[test]
    fn test_transformer_produces_valid_cloudevent_structure() {
        let query_result = create_test_query_result(
            "test-query",
            vec![json!({
                "type": "add",
                "data": {"id": "1", "value": "test"}
            })],
        );

        let result_event = transformer::transform_query_result(query_result.clone(), 1, 1).unwrap();

        // Wrap in CloudEvent
        let cloud_event_config = CloudEventConfig::new();
        let cloud_event = CloudEvent::new(result_event, &query_result.query_id, &cloud_event_config);

        // Verify CloudEvent structure
        assert_eq!(cloud_event.topic, "test-query-results");
        assert_eq!(cloud_event.datacontenttype, "application/json");
        assert_eq!(cloud_event.specversion, "1.0");
        assert_eq!(cloud_event.event_type, "com.dapr.event.sent");
        assert_eq!(cloud_event.pubsubname, "drasi-pubsub");
        assert_eq!(cloud_event.source, "drasi-core");

        // Verify data can be serialized
        let json_str = serde_json::to_string(&cloud_event).unwrap();
        assert!(json_str.contains("queryId"));
        assert!(json_str.contains("sequence"));
        assert!(json_str.contains("addedResults"));
    }

    #[test]
    fn test_transformer_handles_all_operation_types() {
        let query_result = create_test_query_result(
            "mixed-ops-query",
            vec![
                json!({"type": "add", "data": {"id": "1"}}),
                json!({
                    "type": "update",
                    "before": {"id": "2", "value": 10},
                    "after": {"id": "2", "value": 20}
                }),
                json!({"type": "delete", "data": {"id": "3"}}),
            ],
        );

        let result_event = transformer::transform_query_result(query_result, 1, 1).unwrap();

        match result_event {
            ResultEvent::Change(change) => {
                assert_eq!(change.added_results.len(), 1);
                assert_eq!(change.updated_results.len(), 1);
                assert_eq!(change.deleted_results.len(), 1);
            }
            _ => panic!("Expected Change event"),
        }
    }

    #[test]
    fn test_transformer_preserves_metadata_through_cloudevent() {
        let mut metadata = HashMap::new();
        metadata.insert("custom_field".to_string(), json!("custom_value"));

        let query_result = QueryResult {
            query_id: "metadata-query".to_string(),
            timestamp: chrono::Utc::now(),
            results: vec![json!({"type": "add", "data": {"id": "1"}})],
            metadata,
            profiling: None,
        };

        let result_event = transformer::transform_query_result(query_result.clone(), 1, 1).unwrap();
        let cloud_event_config = CloudEventConfig::new();
        let cloud_event = CloudEvent::new(result_event, &query_result.query_id, &cloud_event_config);

        // Serialize and verify metadata is present
        let json_str = serde_json::to_string(&cloud_event).unwrap();
        assert!(json_str.contains("custom_field"));
        assert!(json_str.contains("custom_value"));
    }
}

#[cfg(test)]
mod cloudevent_format_tests {
    use super::*;

    #[test]
    fn test_cloudevent_json_serialization_format() {
        let change_event = ResultChangeEvent {
            query_id: "format-test".to_string(),
            sequence: 42,
            source_time_ms: 1705318245123,
            added_results: vec![{
                let mut map = serde_json::Map::new();
                map.insert("id".to_string(), json!("1"));
                map
            }],
            updated_results: vec![],
            deleted_results: vec![],
            metadata: None,
        };

        let result_event = ResultEvent::Change(change_event);
        let cloud_event_config = CloudEventConfig::with_values(
            "test-pubsub".to_string(),
            "test-source".to_string(),
        );
        let cloud_event = CloudEvent::new(result_event, "format-test", &cloud_event_config);

        // Serialize to JSON
        let json_value: serde_json::Value =
            serde_json::to_value(&cloud_event).expect("Serialization should succeed");

        // Verify all required CloudEvents fields are present with correct types
        assert!(json_value["data"].is_object(), "data should be an object");
        assert!(
            json_value["datacontenttype"].is_string(),
            "datacontenttype should be a string"
        );
        assert!(json_value["id"].is_string(), "id should be a string");
        assert!(
            json_value["pubsubname"].is_string(),
            "pubsubname should be a string"
        );
        assert!(json_value["source"].is_string(), "source should be a string");
        assert!(
            json_value["specversion"].is_string(),
            "specversion should be a string"
        );
        assert!(json_value["time"].is_string(), "time should be a string");
        assert!(json_value["topic"].is_string(), "topic should be a string");
        assert!(json_value["type"].is_string(), "type should be a string");

        // Verify camelCase format for data fields
        let data = &json_value["data"];
        assert!(data["queryId"].is_string(), "queryId should use camelCase");
        assert!(data["sourceTimeMs"].is_u64(), "sourceTimeMs should use camelCase");
        assert!(
            data["addedResults"].is_array(),
            "addedResults should use camelCase"
        );
        assert!(
            data["updatedResults"].is_array(),
            "updatedResults should use camelCase"
        );
        assert!(
            data["deletedResults"].is_array(),
            "deletedResults should use camelCase"
        );
    }

    #[test]
    fn test_cloudevent_deserialization_roundtrip() {
        let original_event = ResultChangeEvent {
            query_id: "roundtrip-test".to_string(),
            sequence: 123,
            source_time_ms: 1705318245123,
            added_results: vec![],
            updated_results: vec![],
            deleted_results: vec![],
            metadata: None,
        };

        let result_event = ResultEvent::Change(original_event.clone());
        let cloud_event_config = CloudEventConfig::new();
        let cloud_event = CloudEvent::new(result_event, "roundtrip-test", &cloud_event_config);

        // Serialize and deserialize
        let json_str = serde_json::to_string(&cloud_event).unwrap();
        let deserialized: CloudEvent<ResultEvent> = serde_json::from_str(&json_str).unwrap();

        // Verify structure is preserved
        match deserialized.data {
            ResultEvent::Change(change) => {
                assert_eq!(change.query_id, original_event.query_id);
                assert_eq!(change.sequence, original_event.sequence);
                assert_eq!(change.source_time_ms, original_event.source_time_ms);
            }
            _ => panic!("Expected Change event"),
        }
    }

    #[test]
    fn test_control_event_cloudevent_format() {
        let control_event = ResultControlEvent {
            query_id: "control-test".to_string(),
            sequence: 1,
            source_time_ms: 1705318245123,
            metadata: None,
            control_signal: ControlSignal::Running,
        };

        let result_event = ResultEvent::Control(control_event);
        let cloud_event_config = CloudEventConfig::new();
        let cloud_event = CloudEvent::new(result_event, "control-test", &cloud_event_config);

        let json_value: serde_json::Value = serde_json::to_value(&cloud_event).unwrap();

        assert_eq!(json_value["data"]["kind"], "control");
        assert_eq!(json_value["data"]["controlSignal"]["kind"], "running");
        assert_eq!(json_value["topic"], "control-test-results");
    }
}

#[cfg(test)]
mod batch_processing_tests {
    use super::*;

    #[test]
    fn test_batch_configuration_validation() {
        use crate::reactions::platform::PlatformReactionConfig;

        // Valid batch configuration
        let valid_config = ReactionConfig {
            id: "batch-test".to_string(),
            queries: vec!["test-query".to_string()],
            auto_start: true,
            config: crate::config::ReactionSpecificConfig::Platform(PlatformReactionConfig {
                redis_url: "redis://localhost:6379".to_string(),
                pubsub_name: None,
                source_name: None,
                max_stream_length: None,
                emit_control_events: false,
                batch_enabled: true,
                batch_max_size: 100,
                batch_max_wait_ms: 50,
            }),
            priority_queue_capacity: None,
        };

        let (event_tx, _) = tokio::sync::mpsc::channel(100);
        let result = PlatformReaction::new(valid_config, event_tx);
        assert!(result.is_ok(), "Valid batch configuration should succeed");
    }

    #[test]
    fn test_batch_size_zero_rejected() {
        use crate::reactions::platform::PlatformReactionConfig;

        let invalid_config = ReactionConfig {
            id: "invalid-batch".to_string(),
            queries: vec!["test-query".to_string()],
            auto_start: true,
            config: crate::config::ReactionSpecificConfig::Platform(PlatformReactionConfig {
                redis_url: "redis://localhost:6379".to_string(),
                pubsub_name: None,
                source_name: None,
                max_stream_length: None,
                emit_control_events: false,
                batch_enabled: true,
                batch_max_size: 0, // Invalid: zero size
                batch_max_wait_ms: 50,
            }),
            priority_queue_capacity: None,
        };

        let (event_tx, _) = tokio::sync::mpsc::channel(100);
        let result = PlatformReaction::new(invalid_config, event_tx);
        assert!(
            result.is_err(),
            "batch_max_size of 0 should be rejected"
        );
    }
}

#[cfg(test)]
mod config_validation_tests {
    use super::*;

    #[test]
    fn test_redis_url_required() {
        use crate::reactions::platform::PlatformReactionConfig;

        let config_with_empty_url = ReactionConfig {
            id: "test".to_string(),
            queries: vec!["test-query".to_string()],
            auto_start: true,
            config: crate::config::ReactionSpecificConfig::Platform(PlatformReactionConfig {
                redis_url: String::new(), // Empty URL
                pubsub_name: None,
                source_name: None,
                max_stream_length: None,
                emit_control_events: false,
                batch_enabled: false,
                batch_max_size: 100,
                batch_max_wait_ms: 100,
            }),
            priority_queue_capacity: None,
        };

        let (event_tx, _) = tokio::sync::mpsc::channel(100);
        let result = PlatformReaction::new(config_with_empty_url, event_tx);
        assert!(result.is_err(), "Empty redis_url should be rejected");
    }

    #[test]
    fn test_default_config_values() {
        use crate::reactions::platform::PlatformReactionConfig;

        let config = ReactionConfig {
            id: "defaults-test".to_string(),
            queries: vec!["test-query".to_string()],
            auto_start: true,
            config: crate::config::ReactionSpecificConfig::Platform(PlatformReactionConfig {
                redis_url: "redis://localhost:6379".to_string(),
                pubsub_name: None, // Should default to "drasi-pubsub"
                source_name: None, // Should default to "drasi-core"
                max_stream_length: None,
                emit_control_events: true,
                batch_enabled: false,
                batch_max_size: 100,
                batch_max_wait_ms: 100,
            }),
            priority_queue_capacity: None,
        };

        let (event_tx, _) = tokio::sync::mpsc::channel(100);
        let reaction = PlatformReaction::new(config, event_tx).unwrap();

        // Verify defaults are applied
        assert_eq!(reaction.cloud_event_config.pubsub_name, "drasi-pubsub");
        assert_eq!(reaction.cloud_event_config.source, "drasi-core");
        assert_eq!(reaction.emit_control_events, true);
    }

    #[test]
    fn test_custom_config_values() {
        use crate::reactions::platform::PlatformReactionConfig;

        let config = ReactionConfig {
            id: "custom-test".to_string(),
            queries: vec!["test-query".to_string()],
            auto_start: true,
            config: crate::config::ReactionSpecificConfig::Platform(PlatformReactionConfig {
                redis_url: "redis://localhost:6379".to_string(),
                pubsub_name: Some("custom-pubsub".to_string()),
                source_name: Some("custom-source".to_string()),
                max_stream_length: Some(5000),
                emit_control_events: false,
                batch_enabled: true,
                batch_max_size: 50,
                batch_max_wait_ms: 25,
            }),
            priority_queue_capacity: None,
        };

        let (event_tx, _) = tokio::sync::mpsc::channel(100);
        let reaction = PlatformReaction::new(config, event_tx).unwrap();

        // Verify custom values are used
        assert_eq!(reaction.cloud_event_config.pubsub_name, "custom-pubsub");
        assert_eq!(reaction.cloud_event_config.source, "custom-source");
        assert_eq!(reaction.emit_control_events, false);
        assert_eq!(reaction.batch_enabled, true);
        assert_eq!(reaction.batch_max_size, 50);
        assert_eq!(reaction.batch_max_wait_ms, 25);
    }
}

#[cfg(test)]
mod sequence_numbering_tests {
    use super::*;

    #[tokio::test]
    async fn test_sequence_starts_at_zero() {
        let config = create_test_config();
        let (event_tx, _) = tokio::sync::mpsc::channel(100);
        let reaction = PlatformReaction::new(config, event_tx).unwrap();

        let counter = reaction.sequence_counter.read().await;
        assert_eq!(*counter, 0, "Sequence counter should start at 0");
    }

    #[tokio::test]
    async fn test_sequence_increments() {
        let config = create_test_config();
        let (event_tx, _) = tokio::sync::mpsc::channel(100);
        let reaction = PlatformReaction::new(config, event_tx).unwrap();

        // Simulate incrementing sequence counter
        for expected in 1..=10 {
            let sequence = {
                let mut counter = reaction.sequence_counter.write().await;
                *counter += 1;
                *counter
            };
            assert_eq!(
                sequence, expected,
                "Sequence should increment monotonically"
            );
        }
    }
}

#[cfg(test)]
mod profiling_metadata_tests {
    use super::*;
    use crate::profiling::ProfilingMetadata;

    #[test]
    fn test_profiling_metadata_included_in_output() {
        let profiling = ProfilingMetadata {
            source_ns: Some(1744055144490466971),
            reactivator_start_ns: Some(1744055140000000000),
            reactivator_end_ns: Some(1744055142000000000),
            source_receive_ns: Some(1744055159124143047),
            source_send_ns: Some(1744055173551481387),
            query_receive_ns: Some(1744055178510629042),
            query_core_call_ns: Some(1744055178510650750),
            query_core_return_ns: Some(1744055178510848750),
            query_send_ns: Some(1744055178510900000),
            reaction_receive_ns: Some(1744055178510950000),
            reaction_complete_ns: None,
        };

        let query_result = QueryResult {
            query_id: "profiling-test".to_string(),
            timestamp: chrono::Utc::now(),
            results: vec![json!({"type": "add", "data": {"id": "1"}})],
            metadata: HashMap::new(),
            profiling: Some(profiling),
        };

        let result_event = transformer::transform_query_result(query_result, 1, 1).unwrap();

        match result_event {
            ResultEvent::Change(change) => {
                assert!(
                    change.metadata.is_some(),
                    "Metadata should be present when profiling is available"
                );
                let metadata = change.metadata.unwrap();
                assert!(
                    metadata.contains_key("tracking"),
                    "Tracking metadata should be present"
                );

                let tracking = metadata.get("tracking").unwrap().as_object().unwrap();
                assert!(
                    tracking.contains_key("source"),
                    "Source tracking should be present"
                );
                assert!(
                    tracking.contains_key("query"),
                    "Query tracking should be present"
                );
            }
            _ => panic!("Expected Change event"),
        }
    }

    #[test]
    fn test_no_profiling_metadata_when_not_available() {
        let query_result = QueryResult {
            query_id: "no-profiling-test".to_string(),
            timestamp: chrono::Utc::now(),
            results: vec![json!({"type": "add", "data": {"id": "1"}})],
            metadata: HashMap::new(),
            profiling: None, // No profiling data
        };

        let result_event = transformer::transform_query_result(query_result, 1, 1).unwrap();

        match result_event {
            ResultEvent::Change(change) => {
                // Metadata should be None when there's no profiling data and no other metadata
                assert!(
                    change.metadata.is_none(),
                    "Metadata should be None when no profiling data"
                );
            }
            _ => panic!("Expected Change event"),
        }
    }
}

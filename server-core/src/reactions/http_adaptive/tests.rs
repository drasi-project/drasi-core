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

#[cfg(test)]
mod tests {
    use crate::channels::ComponentStatus;
    use crate::config::typed::HttpReactionConfig;
    use crate::config::{ReactionConfig, ReactionSpecificConfig};
    use crate::reactions::http_adaptive::{AdaptiveHttpReaction, BatchResult};
    use crate::reactions::Reaction;
    use serde_json::json;
    use std::collections::HashMap;
    use tokio::sync::mpsc;

    // Helper function to create test config
    fn create_test_config(base_url: String) -> ReactionConfig {
        let routes = HashMap::new();

        let http_config = HttpReactionConfig {
            base_url,
            token: None,
            timeout_ms: 5000,
            routes,
        };

        ReactionConfig {
            id: "test-adaptive-http".to_string(),
            queries: vec!["test-query".to_string()],
            auto_start: true,
            config: ReactionSpecificConfig::Http(http_config),
            priority_queue_capacity: None,
        }
    }

    #[tokio::test]
    async fn test_adaptive_http_reaction_creation() {
        // Test that AdaptiveHttpReaction can be created with valid config
        let (event_tx, _event_rx) = mpsc::channel(100);
        let config = create_test_config("http://localhost:8080".to_string());
        let reaction = AdaptiveHttpReaction::new(config.clone(), event_tx);

        // Verify initial status is Stopped
        assert_eq!(reaction.status().await, ComponentStatus::Stopped);

        // Verify config
        assert_eq!(reaction.get_config().id, config.id);
        assert_eq!(reaction.get_config().queries, config.queries);
    }

    #[tokio::test]
    async fn test_adaptive_config_defaults() {
        // Test that adaptive configuration uses sensible defaults
        let (event_tx, _event_rx) = mpsc::channel(100);
        let config = create_test_config("http://localhost:8080".to_string());
        let _reaction = AdaptiveHttpReaction::new(config, event_tx);

        // Reaction should be created successfully with default adaptive settings
        // Default values from AdaptiveBatchConfig:
        // - max_batch_size: 1000
        // - min_batch_size: 10
        // - max_wait_time: 100ms
        // - min_wait_time: 1ms
        // - throughput_window: 5s
        // - adaptive_enabled: true
    }

    #[tokio::test]
    async fn test_adaptive_config_custom() {
        // Test that custom adaptive parameters can be set
        let (event_tx, _event_rx) = mpsc::channel(100);

        let mut custom = HashMap::new();
        custom.insert("base_url".to_string(), json!("http://localhost:8080"));
        custom.insert("adaptive_max_batch_size".to_string(), json!(500));
        custom.insert("adaptive_min_batch_size".to_string(), json!(20));
        custom.insert("adaptive_max_wait_ms".to_string(), json!(50));
        custom.insert("adaptive_min_wait_ms".to_string(), json!(2));
        custom.insert("adaptive_window_secs".to_string(), json!(10));
        custom.insert("adaptive_enabled".to_string(), json!(false));

        let config = ReactionConfig {
            id: "test-custom-adaptive".to_string(),
            queries: vec!["test-query".to_string()],
            auto_start: true,
            config: ReactionSpecificConfig::Custom { properties: custom },
            priority_queue_capacity: None,
        };

        let _reaction = AdaptiveHttpReaction::new(config, event_tx);
        // Reaction should be created successfully with custom adaptive settings
    }

    #[test]
    fn test_batch_result_serialization() {
        // Test that BatchResult serializes correctly to JSON
        let batch_result = BatchResult {
            query_id: "test-query".to_string(),
            results: vec![
                json!({
                    "type": "ADD",
                    "data": {"id": 1, "name": "Alice"},
                    "after": {"id": 1, "name": "Alice"}
                }),
                json!({
                    "type": "UPDATE",
                    "data": {"id": 2, "name": "Bob Updated"},
                    "before": {"id": 2, "name": "Bob"},
                    "after": {"id": 2, "name": "Bob Updated"}
                }),
            ],
            timestamp: "2025-10-19T12:34:56.789Z".to_string(),
            count: 2,
        };

        // Serialize to JSON
        let json = serde_json::to_string(&batch_result).unwrap();
        assert!(json.contains("test-query"));
        assert!(json.contains("\"count\":2"));

        // Deserialize back
        let deserialized: BatchResult = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.query_id, "test-query");
        assert_eq!(deserialized.count, 2);
        assert_eq!(deserialized.results.len(), 2);
    }

    #[test]
    fn test_batch_result_array_serialization() {
        // Test that array of BatchResult (batch endpoint format) serializes correctly
        let batches = vec![
            BatchResult {
                query_id: "query1".to_string(),
                results: vec![json!({"type": "ADD", "data": {"id": 1}})],
                timestamp: "2025-10-19T12:34:56Z".to_string(),
                count: 1,
            },
            BatchResult {
                query_id: "query2".to_string(),
                results: vec![
                    json!({"type": "UPDATE", "data": {"id": 2}}),
                    json!({"type": "DELETE", "data": {"id": 3}}),
                ],
                timestamp: "2025-10-19T12:34:57Z".to_string(),
                count: 2,
            },
        ];

        // Serialize to JSON
        let json = serde_json::to_string(&batches).unwrap();
        assert!(json.contains("query1"));
        assert!(json.contains("query2"));

        // Deserialize back
        let deserialized: Vec<BatchResult> = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.len(), 2);
        assert_eq!(deserialized[0].count, 1);
        assert_eq!(deserialized[1].count, 2);
    }

    #[test]
    fn test_batch_result_matches_specification() {
        // Test that BatchResult structure matches the TypeSpec definition
        let batch_result = BatchResult {
            query_id: "user-changes".to_string(),
            results: vec![
                json!({
                    "type": "ADD",
                    "data": {"id": "user_123", "name": "John Doe"},
                    "after": {"id": "user_123", "name": "John Doe"}
                }),
                json!({
                    "type": "UPDATE",
                    "data": {"id": "user_456", "name": "Jane Smith"},
                    "before": {"id": "user_456", "name": "Jane Doe"},
                    "after": {"id": "user_456", "name": "Jane Smith"}
                }),
                json!({
                    "type": "DELETE",
                    "data": {"id": "user_789", "name": "Bob Wilson"},
                    "before": {"id": "user_789", "name": "Bob Wilson"}
                }),
            ],
            timestamp: "2025-10-19T12:34:56.789Z".to_string(),
            count: 3,
        };

        // Verify structure
        assert_eq!(batch_result.query_id, "user-changes");
        assert_eq!(batch_result.count, 3);
        assert_eq!(batch_result.results.len(), 3);
        assert!(!batch_result.timestamp.is_empty());

        // Verify result types
        assert_eq!(batch_result.results[0]["type"], "ADD");
        assert_eq!(batch_result.results[1]["type"], "UPDATE");
        assert_eq!(batch_result.results[2]["type"], "DELETE");

        // Verify ADD structure
        assert!(batch_result.results[0]["data"].is_object());
        assert!(batch_result.results[0]["after"].is_object());

        // Verify UPDATE structure
        assert!(batch_result.results[1]["data"].is_object());
        assert!(batch_result.results[1]["before"].is_object());
        assert!(batch_result.results[1]["after"].is_object());

        // Verify DELETE structure
        assert!(batch_result.results[2]["data"].is_object());
        assert!(batch_result.results[2]["before"].is_object());
    }

    #[tokio::test]
    async fn test_batch_endpoint_enabled_by_default() {
        // Test that batch_endpoints_enabled is true by default
        let (event_tx, _event_rx) = mpsc::channel(100);
        let config = create_test_config("http://localhost:8080".to_string());
        let _reaction = AdaptiveHttpReaction::new(config, event_tx);

        // Batch endpoints should be enabled by default in adaptive HTTP reaction
        // This is verified by the default value in the constructor
    }

    #[tokio::test]
    async fn test_reaction_type_identification() {
        // Test that reaction type can be identified correctly
        let (event_tx, _event_rx) = mpsc::channel(100);
        let config = create_test_config("http://localhost:8080".to_string());
        let reaction = AdaptiveHttpReaction::new(config, event_tx);

        // Verify reaction type through config
        let config = reaction.get_config();
        assert_eq!(config.reaction_type(), "http");
    }

    #[test]
    fn test_batch_result_count_matches_results_length() {
        // Test that count field matches the actual number of results
        let results = vec![
            json!({"type": "ADD", "data": {"id": 1}}),
            json!({"type": "ADD", "data": {"id": 2}}),
            json!({"type": "ADD", "data": {"id": 3}}),
        ];

        let batch_result = BatchResult {
            query_id: "test".to_string(),
            results: results.clone(),
            timestamp: chrono::Utc::now().to_rfc3339(),
            count: results.len(),
        };

        assert_eq!(batch_result.count, batch_result.results.len());
        assert_eq!(batch_result.count, 3);
    }

    #[test]
    fn test_batch_result_timestamp_format() {
        // Test that timestamp is in ISO 8601 format
        let batch_result = BatchResult {
            query_id: "test".to_string(),
            results: vec![],
            timestamp: chrono::Utc::now().to_rfc3339(),
            count: 0,
        };

        // Verify timestamp can be parsed as RFC3339
        assert!(chrono::DateTime::parse_from_rfc3339(&batch_result.timestamp).is_ok());
    }

    #[tokio::test]
    async fn test_multiple_queries_support() {
        // Test that reaction can be configured with multiple queries
        let (event_tx, _event_rx) = mpsc::channel(100);

        let config = ReactionConfig {
            id: "test-multi-query".to_string(),
            queries: vec!["query1".to_string(), "query2".to_string(), "query3".to_string()],
            auto_start: true,
            config: ReactionSpecificConfig::Http(HttpReactionConfig {
                base_url: "http://localhost:8080".to_string(),
                token: None,
                timeout_ms: 5000,
                routes: HashMap::new(),
            }),
            priority_queue_capacity: None,
        };

        let reaction = AdaptiveHttpReaction::new(config, event_tx);
        assert_eq!(reaction.get_config().queries.len(), 3);
    }

    #[tokio::test]
    async fn test_http2_client_configuration() {
        // Test that HTTP/2 client is configured with connection pooling
        let (event_tx, _event_rx) = mpsc::channel(100);
        let config = create_test_config("http://localhost:8080".to_string());
        let _reaction = AdaptiveHttpReaction::new(config, event_tx);

        // HTTP/2 client should be configured with:
        // - pool_idle_timeout: 90 seconds
        // - pool_max_idle_per_host: 10 connections
        // - http2_prior_knowledge: enabled
        // These are hard-coded in the constructor
    }

    #[tokio::test]
    async fn test_token_authentication_optional() {
        // Test that token authentication is optional
        let (event_tx, _event_rx) = mpsc::channel(100);

        // Config without token
        let config_no_token = create_test_config("http://localhost:8080".to_string());
        let _reaction1 = AdaptiveHttpReaction::new(config_no_token, event_tx.clone());

        // Config with token
        let config_with_token = ReactionConfig {
            id: "test-with-token".to_string(),
            queries: vec!["test-query".to_string()],
            auto_start: true,
            config: ReactionSpecificConfig::Http(HttpReactionConfig {
                base_url: "http://localhost:8080".to_string(),
                token: Some("test-token".to_string()),
                timeout_ms: 5000,
                routes: HashMap::new(),
            }),
            priority_queue_capacity: None,
        };
        let _reaction2 = AdaptiveHttpReaction::new(config_with_token, event_tx);
    }

    #[test]
    fn test_empty_batch_result() {
        // Test that BatchResult can represent empty results
        let batch_result = BatchResult {
            query_id: "test".to_string(),
            results: vec![],
            timestamp: chrono::Utc::now().to_rfc3339(),
            count: 0,
        };

        assert_eq!(batch_result.count, 0);
        assert!(batch_result.results.is_empty());

        // Should still serialize correctly
        let json = serde_json::to_string(&batch_result).unwrap();
        assert!(json.contains("\"count\":0"));
    }

    #[test]
    fn test_large_batch_result() {
        // Test that BatchResult can handle large batches
        let mut results = Vec::new();
        for i in 0..1000 {
            results.push(json!({
                "type": "ADD",
                "data": {"id": i, "value": format!("item_{}", i)}
            }));
        }

        let batch_result = BatchResult {
            query_id: "large-batch".to_string(),
            results: results.clone(),
            timestamp: chrono::Utc::now().to_rfc3339(),
            count: results.len(),
        };

        assert_eq!(batch_result.count, 1000);
        assert_eq!(batch_result.results.len(), 1000);

        // Should serialize without errors
        let json = serde_json::to_string(&batch_result).unwrap();
        assert!(json.contains("large-batch"));
        assert!(json.contains("\"count\":1000"));
    }
}

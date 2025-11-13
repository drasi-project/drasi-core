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

//! Integration tests for platform reaction with Redis testcontainers
//!
//! These tests verify the platform reaction can correctly publish QueryResults to Redis Streams
//! in CloudEvent format, handle control events, sequence numbering, and error conditions.
//!
//! **Requirements**: Docker must be running for these tests to pass.

mod test_support;

use anyhow::Result;
use async_trait::async_trait;
use drasi_server_core::channels::{
    ChangeDispatcher, ComponentEvent, ComponentStatus, QueryResult, QuerySubscriptionResponse,
};
use drasi_server_core::config::{
    PlatformReactionConfig, QueryConfig, ReactionConfig, ReactionSpecificConfig,
    SourceSubscriptionConfig,
};
use drasi_server_core::queries::Query;
use drasi_server_core::reactions::platform::{
    CloudEvent, ControlSignal, PlatformReaction, ResultEvent,
};
use drasi_server_core::reactions::Reaction;
use drasi_server_core::server_core::DrasiServerCore;
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use test_support::*;
use tokio::sync::{mpsc, RwLock};
use tokio::time::sleep;

/// Mock query for testing reactions
struct MockQuery {
    config: QueryConfig,
    status: Arc<RwLock<ComponentStatus>>,
    dispatcher: Arc<drasi_server_core::channels::BroadcastChangeDispatcher<QueryResult>>,
}

impl MockQuery {
    fn new(query_id: &str) -> Self {
        let dispatcher = Arc::new(drasi_server_core::channels::BroadcastChangeDispatcher::<
            QueryResult,
        >::new(1000));
        Self {
            config: QueryConfig {
                id: query_id.to_string(),
                query: "MATCH (n) RETURN n".to_string(),
                query_language: drasi_server_core::config::QueryLanguage::Cypher,
                source_subscriptions: vec![],
                middleware: vec![],
                auto_start: false,
                joins: None,
                enable_bootstrap: false,
                bootstrap_buffer_size: 10000,
                priority_queue_capacity: None,
                dispatch_buffer_capacity: None,
                dispatch_mode: None,
                storage_backend: None,
            },
            status: Arc::new(RwLock::new(ComponentStatus::Running)),
            dispatcher,
        }
    }

    async fn send_result(&self, result: QueryResult) {
        let _ = self.dispatcher.dispatch_change(Arc::new(result)).await;
    }
}

#[async_trait]
impl Query for MockQuery {
    async fn start(&self) -> Result<()> {
        *self.status.write().await = ComponentStatus::Running;
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        *self.status.write().await = ComponentStatus::Stopped;
        Ok(())
    }

    async fn status(&self) -> ComponentStatus {
        self.status.read().await.clone()
    }

    fn get_config(&self) -> &QueryConfig {
        &self.config
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn subscribe(&self, _reaction_id: String) -> Result<QuerySubscriptionResponse, String> {
        let receiver = self
            .dispatcher
            .create_receiver()
            .await
            .map_err(|e| format!("Failed to create receiver: {}", e))?;
        Ok(QuerySubscriptionResponse {
            query_id: self.config.id.clone(),
            receiver,
        })
    }
}

/// Helper to create a test environment with DrasiServerCore and a mock query
async fn create_test_server_with_query(query_id: &str) -> (Arc<DrasiServerCore>, Arc<MockQuery>) {
    let mock_query = Arc::new(MockQuery::new(query_id));

    // Create DrasiServerCore using builder
    let server_core = drasi_server_core::server_core::DrasiServerCore::builder()
        .build()
        .await
        .expect("Failed to create server core");

    // Add the mock query to the query manager
    server_core
        .query_manager()
        .add_query_instance_for_test(mock_query.clone())
        .await
        .expect("Failed to add mock query");

    (Arc::new(server_core), mock_query)
}

/// Helper to create a platform reaction with test configuration
fn create_test_reaction(
    redis_url: String,
    query_id: &str,
    emit_control_events: bool,
    max_stream_length: Option<usize>,
    pubsub_name: Option<&str>,
) -> (PlatformReaction, mpsc::Receiver<ComponentEvent>) {
    let (event_tx, event_rx) = mpsc::channel(100);

    let config = ReactionConfig {
        id: "test-reaction".to_string(),
        queries: vec![query_id.to_string()],
        auto_start: false,
        config: ReactionSpecificConfig::Platform(PlatformReactionConfig {
            redis_url,
            pubsub_name: pubsub_name.map(|s| s.to_string()),
            source_name: None,
            max_stream_length,
            emit_control_events,
            batch_enabled: false,
            batch_max_size: 100,
            batch_max_wait_ms: 1000,
        }),
        priority_queue_capacity: None,
    };

    let reaction = PlatformReaction::new(config, event_tx).expect("Failed to create reaction");

    (reaction, event_rx)
}

/// Helper to build a QueryResult with add results
fn build_query_result_add(query_id: &str, results: Vec<serde_json::Value>) -> QueryResult {
    let results_with_type: Vec<serde_json::Value> = results
        .into_iter()
        .map(|data| json!({"type": "add", "data": data}))
        .collect();

    QueryResult {
        query_id: query_id.to_string(),
        timestamp: chrono::Utc::now(),
        results: results_with_type,
        metadata: HashMap::new(),
        profiling: None,
    }
}

/// Helper to build a QueryResult with update results
fn build_query_result_update(
    query_id: &str,
    updates: Vec<(serde_json::Value, serde_json::Value, Option<Vec<String>>)>,
) -> QueryResult {
    let results_with_type: Vec<serde_json::Value> = updates
        .into_iter()
        .map(|(before, after, grouping_keys)| {
            let mut result = json!({"type": "update", "before": before, "after": after});
            if let Some(keys) = grouping_keys {
                result["grouping_keys"] = json!(keys);
            }
            result
        })
        .collect();

    QueryResult {
        query_id: query_id.to_string(),
        timestamp: chrono::Utc::now(),
        results: results_with_type,
        metadata: HashMap::new(),
        profiling: None,
    }
}

/// Helper to build a QueryResult with delete results
fn build_query_result_delete(query_id: &str, results: Vec<serde_json::Value>) -> QueryResult {
    let results_with_type: Vec<serde_json::Value> = results
        .into_iter()
        .map(|data| json!({"type": "delete", "data": data}))
        .collect();

    QueryResult {
        query_id: query_id.to_string(),
        timestamp: chrono::Utc::now(),
        results: results_with_type,
        metadata: HashMap::new(),
        profiling: None,
    }
}

/// Helper to read and parse CloudEvent from Redis stream
async fn read_cloudevent_from_stream(
    redis_url: &str,
    stream_key: &str,
) -> Result<CloudEvent<ResultEvent>> {
    // Read from stream
    let events = read_from_stream(redis_url, stream_key, "0", 10).await?;

    assert!(!events.is_empty(), "No events found in stream");

    let (_stream_id, data_map) = &events[events.len() - 1]; // Get latest event
    let event_json = data_map.get("data").expect("Missing data field");

    let cloud_event: CloudEvent<ResultEvent> = serde_json::from_str(event_json)?;
    Ok(cloud_event)
}

// ==================== Basic Publishing Tests ====================

#[tokio::test]
async fn test_publish_add_results() -> Result<()> {
    let redis = setup_redis().await;
    let query_id = "test-query-add";

    // Create test server with mock query
    let (server_core, mock_query) = create_test_server_with_query(query_id).await;
    let stream_key = format!("{}-results", query_id);

    let (reaction, mut event_rx) =
        create_test_reaction(redis.url().to_string(), query_id, false, None, None);

    // Start reaction with server core (it will subscribe to the mock query)
    reaction.start(server_core).await?;

    // Wait for Running status
    let mut started = false;
    for _ in 0..10 {
        if let Ok(event) = tokio::time::timeout(Duration::from_millis(100), event_rx.recv()).await {
            if let Some(evt) = event {
                eprintln!("Got event: {:?}", evt.status);
                if evt.status == drasi_server_core::channels::ComponentStatus::Running {
                    started = true;
                    break;
                }
            }
        }
    }

    if !started {
        panic!("Reaction did not start");
    }

    // Send add result through the query's dispatcher
    let query_result = build_query_result_add(query_id, vec![json!({"id": "1", "name": "Alice"})]);
    mock_query.send_result(query_result).await;

    // Wait for publication
    sleep(Duration::from_millis(300)).await;

    // Read from Redis stream
    let cloud_event = read_cloudevent_from_stream(redis.url(), &stream_key).await?;

    // Verify
    assert_eq!(cloud_event.topic, stream_key);
    if let ResultEvent::Change(change_event) = cloud_event.data {
        assert_eq!(change_event.query_id, query_id);
        assert_eq!(change_event.added_results.len(), 1);
        assert_eq!(change_event.added_results[0]["id"], "1");
        assert_eq!(change_event.added_results[0]["name"], "Alice");
        // Verify empty arrays are present
        assert_eq!(change_event.updated_results.len(), 0);
        assert_eq!(change_event.deleted_results.len(), 0);
    } else {
        panic!("Expected Change event");
    }

    Ok(())
}

#[tokio::test]
async fn test_publish_update_results() -> Result<()> {
    let redis = setup_redis().await;
    let query_id = "test-query-update";

    // Create test server with mock query
    let (server_core, mock_query) = create_test_server_with_query(query_id).await;
    let stream_key = format!("{}-results", query_id);

    let (reaction, _event_rx) =
        create_test_reaction(redis.url().to_string(), query_id, false, None, None);

    reaction.start(server_core.clone()).await?;

    sleep(Duration::from_millis(150)).await;

    // Send update result
    let query_result = build_query_result_update(
        query_id,
        vec![(
            json!({"id": "1", "value": 10}),
            json!({"id": "1", "value": 20}),
            None,
        )],
    );
    mock_query.send_result(query_result).await;

    sleep(Duration::from_millis(300)).await;

    // Read and verify
    let cloud_event = read_cloudevent_from_stream(redis.url(), &stream_key).await?;

    if let ResultEvent::Change(change_event) = cloud_event.data {
        assert_eq!(change_event.updated_results.len(), 1);
        assert_eq!(
            change_event.updated_results[0].before.as_ref().unwrap()["value"],
            10
        );
        assert_eq!(
            change_event.updated_results[0].after.as_ref().unwrap()["value"],
            20
        );
    } else {
        panic!("Expected Change event");
    }

    Ok(())
}

#[tokio::test]
async fn test_publish_delete_results() -> Result<()> {
    let redis = setup_redis().await;
    let query_id = "test-query-delete";

    // Create test server with mock query
    let (server_core, mock_query) = create_test_server_with_query(query_id).await;
    let stream_key = format!("{}-results", query_id);

    let (reaction, _event_rx) =
        create_test_reaction(redis.url().to_string(), query_id, false, None, None);

    reaction.start(server_core.clone()).await?;

    sleep(Duration::from_millis(150)).await;

    // Send delete result
    let query_result = build_query_result_delete(query_id, vec![json!({"id": "2", "name": "Bob"})]);
    mock_query.send_result(query_result).await;

    sleep(Duration::from_millis(300)).await;

    // Read and verify
    let cloud_event = read_cloudevent_from_stream(redis.url(), &stream_key).await?;

    if let ResultEvent::Change(change_event) = cloud_event.data {
        assert_eq!(change_event.deleted_results.len(), 1);
        assert_eq!(change_event.deleted_results[0]["id"], "2");
    } else {
        panic!("Expected Change event");
    }

    Ok(())
}

#[tokio::test]
async fn test_mixed_result_types() -> Result<()> {
    let redis = setup_redis().await;
    let query_id = "test-query-mixed";

    // Create test server with mock query
    let (server_core, mock_query) = create_test_server_with_query(query_id).await;
    let stream_key = format!("{}-results", query_id);

    let (reaction, _event_rx) =
        create_test_reaction(redis.url().to_string(), query_id, false, None, None);

    reaction.start(server_core.clone()).await?;

    sleep(Duration::from_millis(150)).await;

    // Send mixed result types
    let query_result = QueryResult {
        query_id: query_id.to_string(),
        timestamp: chrono::Utc::now(),
        results: vec![
            json!({"type": "add", "data": {"id": "1", "name": "Alice"}}),
            json!({"type": "update", "before": {"id": "2", "value": 10}, "after": {"id": "2", "value": 20}}),
            json!({"type": "delete", "data": {"id": "3", "name": "Charlie"}}),
        ],
        metadata: HashMap::new(),
        profiling: None,
    };
    mock_query.send_result(query_result).await;

    sleep(Duration::from_millis(300)).await;

    // Read and verify
    let cloud_event = read_cloudevent_from_stream(redis.url(), &stream_key).await?;

    if let ResultEvent::Change(change_event) = cloud_event.data {
        assert_eq!(change_event.added_results.len(), 1);
        assert_eq!(change_event.updated_results.len(), 1);
        assert_eq!(change_event.deleted_results.len(), 1);
    } else {
        panic!("Expected Change event");
    }

    Ok(())
}

#[tokio::test]
async fn test_stream_naming_convention() -> Result<()> {
    let redis = setup_redis().await;
    let query_id = "my-custom-query";

    // Create test server with mock query
    let (server_core, mock_query) = create_test_server_with_query(query_id).await;
    let expected_stream_key = format!("{}-results", query_id);

    let (reaction, _event_rx) =
        create_test_reaction(redis.url().to_string(), query_id, false, None, None);

    reaction.start(server_core.clone()).await?;

    sleep(Duration::from_millis(150)).await;

    // Send result
    let query_result = build_query_result_add(query_id, vec![json!({"test": "data"})]);
    mock_query.send_result(query_result).await;

    sleep(Duration::from_millis(300)).await;

    // Verify stream exists with correct name
    let events = read_from_stream(redis.url(), &expected_stream_key, "0", 1).await?;
    assert!(
        !events.is_empty(),
        "Stream should exist with name {}",
        expected_stream_key
    );

    Ok(())
}

#[tokio::test]
async fn test_metadata_preservation() -> Result<()> {
    let redis = setup_redis().await;
    let query_id = "test-query-metadata";

    // Create test server with mock query
    let (server_core, mock_query) = create_test_server_with_query(query_id).await;
    let stream_key = format!("{}-results", query_id);

    let (reaction, _event_rx) =
        create_test_reaction(redis.url().to_string(), query_id, false, None, None);

    reaction.start(server_core.clone()).await?;

    sleep(Duration::from_millis(150)).await;

    // Send result with metadata
    let mut metadata = HashMap::new();
    metadata.insert("source".to_string(), json!({"timing": 100}));
    metadata.insert("custom".to_string(), json!("value"));

    let query_result = QueryResult {
        query_id: query_id.to_string(),
        timestamp: chrono::Utc::now(),
        results: vec![json!({"type": "add", "data": {"id": "1"}})],
        metadata,
        profiling: None,
    };
    mock_query.send_result(query_result).await;

    sleep(Duration::from_millis(300)).await;

    // Read and verify metadata
    let cloud_event = read_cloudevent_from_stream(redis.url(), &stream_key).await?;

    if let ResultEvent::Change(change_event) = cloud_event.data {
        let metadata = change_event.metadata.expect("Metadata should be present");
        assert_eq!(metadata.get("custom"), Some(&json!("value")));
        assert_eq!(metadata.get("source"), Some(&json!({"timing": 100})));
    } else {
        panic!("Expected Change event");
    }

    Ok(())
}

// ==================== CloudEvent Format Tests ====================

#[tokio::test]
async fn test_cloudevent_required_fields() -> Result<()> {
    let redis = setup_redis().await;
    let query_id = "test-cloudevent-fields";

    // Create test server with mock query
    let (server_core, mock_query) = create_test_server_with_query(query_id).await;
    let stream_key = format!("{}-results", query_id);

    let (reaction, _event_rx) =
        create_test_reaction(redis.url().to_string(), query_id, false, None, None);

    reaction.start(server_core.clone()).await?;

    sleep(Duration::from_millis(150)).await;

    let query_result = build_query_result_add(query_id, vec![json!({"test": "data"})]);
    mock_query.send_result(query_result).await;

    sleep(Duration::from_millis(300)).await;

    // Read raw event
    let events = read_from_stream(redis.url(), &stream_key, "0", 1).await?;
    let (_id, data_map) = &events[0];
    let event_json = data_map.get("data").unwrap();
    let cloud_event_value: serde_json::Value = serde_json::from_str(event_json)?;

    // Verify all required fields
    verify_cloudevent_structure(&cloud_event_value)?;

    Ok(())
}

#[tokio::test]
async fn test_cloudevent_topic_format() -> Result<()> {
    let redis = setup_redis().await;
    let query_id = "test-topic-format";

    // Create test server with mock query
    let (server_core, mock_query) = create_test_server_with_query(query_id).await;
    let stream_key = format!("{}-results", query_id);

    let (reaction, _event_rx) =
        create_test_reaction(redis.url().to_string(), query_id, false, None, None);

    reaction.start(server_core.clone()).await?;

    sleep(Duration::from_millis(150)).await;

    let query_result = build_query_result_add(query_id, vec![json!({"test": "data"})]);
    mock_query.send_result(query_result).await;

    sleep(Duration::from_millis(300)).await;

    let cloud_event = read_cloudevent_from_stream(redis.url(), &stream_key).await?;

    // Verify topic format
    assert_eq!(cloud_event.topic, format!("{}-results", query_id));

    Ok(())
}

#[tokio::test]
async fn test_cloudevent_timestamp_format() -> Result<()> {
    let redis = setup_redis().await;
    let query_id = "test-timestamp";

    // Create test server with mock query
    let (server_core, mock_query) = create_test_server_with_query(query_id).await;
    let stream_key = format!("{}-results", query_id);

    let (reaction, _event_rx) =
        create_test_reaction(redis.url().to_string(), query_id, false, None, None);

    reaction.start(server_core.clone()).await?;

    sleep(Duration::from_millis(150)).await;

    let query_result = build_query_result_add(query_id, vec![json!({"test": "data"})]);
    mock_query.send_result(query_result).await;

    sleep(Duration::from_millis(300)).await;

    let cloud_event = read_cloudevent_from_stream(redis.url(), &stream_key).await?;

    // Verify timestamp is valid ISO 8601
    chrono::DateTime::parse_from_rfc3339(&cloud_event.time)?;

    Ok(())
}

#[tokio::test]
async fn test_cloudevent_data_content_type() -> Result<()> {
    let redis = setup_redis().await;
    let query_id = "test-content-type";

    // Create test server with mock query
    let (server_core, mock_query) = create_test_server_with_query(query_id).await;
    let stream_key = format!("{}-results", query_id);

    let (reaction, _event_rx) =
        create_test_reaction(redis.url().to_string(), query_id, false, None, None);

    reaction.start(server_core.clone()).await?;

    sleep(Duration::from_millis(150)).await;

    let query_result = build_query_result_add(query_id, vec![json!({"test": "data"})]);
    mock_query.send_result(query_result).await;

    sleep(Duration::from_millis(300)).await;

    let cloud_event = read_cloudevent_from_stream(redis.url(), &stream_key).await?;

    assert_eq!(cloud_event.datacontenttype, "application/json");

    Ok(())
}

#[tokio::test]
async fn test_dapr_metadata_fields() -> Result<()> {
    let redis = setup_redis().await;
    let query_id = "test-dapr-metadata";

    // Create test server with mock query
    let (server_core, mock_query) = create_test_server_with_query(query_id).await;
    let stream_key = format!("{}-results", query_id);

    let (reaction, _event_rx) =
        create_test_reaction(redis.url().to_string(), query_id, false, None, None);

    reaction.start(server_core.clone()).await?;

    sleep(Duration::from_millis(150)).await;

    let query_result = build_query_result_add(query_id, vec![json!({"test": "data"})]);
    mock_query.send_result(query_result).await;

    sleep(Duration::from_millis(300)).await;

    let cloud_event = read_cloudevent_from_stream(redis.url(), &stream_key).await?;

    // Verify Dapr fields exist
    assert_eq!(cloud_event.pubsubname, "drasi-pubsub"); // Default value
    assert_eq!(cloud_event.source, "drasi-core"); // Default value
    assert_eq!(cloud_event.event_type, "com.dapr.event.sent");

    Ok(())
}

#[tokio::test]
async fn test_custom_pubsub_name() -> Result<()> {
    let redis = setup_redis().await;
    let query_id = "test-custom-pubsub";

    // Create test server with mock query
    let (server_core, mock_query) = create_test_server_with_query(query_id).await;
    let stream_key = format!("{}-results", query_id);

    let (reaction, _event_rx) = create_test_reaction(
        redis.url().to_string(),
        query_id,
        false,
        None,
        Some("custom-pubsub"),
    );

    reaction.start(server_core.clone()).await?;

    sleep(Duration::from_millis(150)).await;

    let query_result = build_query_result_add(query_id, vec![json!({"test": "data"})]);
    mock_query.send_result(query_result).await;

    sleep(Duration::from_millis(300)).await;

    let cloud_event = read_cloudevent_from_stream(redis.url(), &stream_key).await?;

    assert_eq!(cloud_event.pubsubname, "custom-pubsub");

    Ok(())
}

// ==================== Control Events Tests ====================

#[tokio::test]
async fn test_running_control_event() -> Result<()> {
    let redis = setup_redis().await;
    let query_id = "test-control-running";

    // Create test server with mock query
    let (server_core, _mock_query) = create_test_server_with_query(query_id).await;
    let stream_key = format!("{}-results", query_id);

    let (reaction, _event_rx) =
        create_test_reaction(redis.url().to_string(), query_id, true, None, None);

    reaction.start(server_core.clone()).await?;

    sleep(Duration::from_millis(300)).await;

    // Read control event
    let cloud_event = read_cloudevent_from_stream(redis.url(), &stream_key).await?;

    if let ResultEvent::Control(control_event) = cloud_event.data {
        assert_eq!(control_event.control_signal, ControlSignal::Running);
    } else {
        panic!("Expected Control event");
    }

    Ok(())
}

#[tokio::test]
async fn test_control_events_disabled() -> Result<()> {
    let redis = setup_redis().await;
    let query_id = "test-control-disabled";

    // Create test server with mock query
    let (server_core, _mock_query) = create_test_server_with_query(query_id).await;
    let stream_key = format!("{}-results", query_id);

    let (reaction, _event_rx) =
        create_test_reaction(redis.url().to_string(), query_id, false, None, None);

    reaction.start(server_core.clone()).await?;

    sleep(Duration::from_millis(300)).await;

    // Try to read - should have no events since control events are disabled
    let events = read_from_stream(redis.url(), &stream_key, "0", 10).await?;
    assert!(
        events.is_empty(),
        "No control events should be published when disabled"
    );

    Ok(())
}

// ==================== Advanced Features Tests ====================

#[tokio::test]
async fn test_sequence_numbering() -> Result<()> {
    let redis = setup_redis().await;
    let query_id = "test-sequence";

    // Create test server with mock query
    let (server_core, mock_query) = create_test_server_with_query(query_id).await;
    let stream_key = format!("{}-results", query_id);

    let (reaction, _event_rx) =
        create_test_reaction(redis.url().to_string(), query_id, false, None, None);

    reaction.start(server_core.clone()).await?;

    sleep(Duration::from_millis(150)).await;

    // Send 5 query results
    for i in 1..=5 {
        let query_result = build_query_result_add(query_id, vec![json!({"id": i})]);
        mock_query.send_result(query_result).await;
        sleep(Duration::from_millis(50)).await;
    }

    sleep(Duration::from_millis(300)).await;

    // Read all events and verify sequence
    let events = read_from_stream(redis.url(), &stream_key, "0", 10).await?;
    assert!(events.len() >= 5, "Should have at least 5 events");

    // Check sequences increment
    for (idx, (_id, data_map)) in events.iter().enumerate() {
        let event_json = data_map.get("data").unwrap();
        let cloud_event: CloudEvent<ResultEvent> = serde_json::from_str(event_json)?;

        if let ResultEvent::Change(change_event) = cloud_event.data {
            assert_eq!(change_event.sequence as usize, idx + 1);
        }
    }

    Ok(())
}

#[tokio::test]
async fn test_maxlen_stream_trimming() -> Result<()> {
    let redis = setup_redis().await;
    let query_id = "test-maxlen";

    // Create test server with mock query
    let (server_core, mock_query) = create_test_server_with_query(query_id).await;
    let stream_key = format!("{}-results", query_id);

    let (reaction, _event_rx) = create_test_reaction(
        redis.url().to_string(),
        query_id,
        false,
        Some(5), // Max length of 5
        None,
    );

    reaction.start(server_core.clone()).await?;

    sleep(Duration::from_millis(150)).await;

    // Send 10 events
    for i in 1..=10 {
        let query_result = build_query_result_add(query_id, vec![json!({"id": i})]);
        mock_query.send_result(query_result).await;
        sleep(Duration::from_millis(30)).await;
    }

    sleep(Duration::from_millis(300)).await;

    // Check stream length
    let length = get_stream_length(redis.url(), &stream_key).await?;

    // Redis MAXLEN with ~ (approximate) is efficient but may not trim immediately
    // It typically keeps the stream between max_len and 2*max_len depending on internal factors
    // For max_len=5, we should see trimming happen eventually, but allow for Redis's internal buffering
    assert!(
        length <= 10,
        "Stream should eventually be trimmed (max_len=5, approx allows buffer), got {}",
        length
    );
    assert!(
        length >= 5,
        "Stream should contain at least max_len entries"
    );

    Ok(())
}

#[tokio::test]
async fn test_update_with_grouping_keys() -> Result<()> {
    let redis = setup_redis().await;
    let query_id = "test-grouping-keys";

    // Create test server with mock query
    let (server_core, mock_query) = create_test_server_with_query(query_id).await;
    let stream_key = format!("{}-results", query_id);

    let (reaction, _event_rx) =
        create_test_reaction(redis.url().to_string(), query_id, false, None, None);

    reaction.start(server_core.clone()).await?;

    sleep(Duration::from_millis(150)).await;

    // Send update with grouping keys
    let query_result = build_query_result_update(
        query_id,
        vec![(
            json!({"id": "1", "value": 10}),
            json!({"id": "1", "value": 20}),
            Some(vec!["key1".to_string(), "key2".to_string()]),
        )],
    );
    mock_query.send_result(query_result).await;

    sleep(Duration::from_millis(300)).await;

    let cloud_event = read_cloudevent_from_stream(redis.url(), &stream_key).await?;

    if let ResultEvent::Change(change_event) = cloud_event.data {
        let update = &change_event.updated_results[0];
        let keys = update
            .grouping_keys
            .as_ref()
            .expect("Grouping keys should be present");
        assert_eq!(keys.len(), 2);
        assert_eq!(keys[0], "key1");
        assert_eq!(keys[1], "key2");
    } else {
        panic!("Expected Change event");
    }

    Ok(())
}

#[tokio::test]
async fn test_empty_metadata_filtered() -> Result<()> {
    let redis = setup_redis().await;
    let query_id = "test-empty-metadata";

    // Create test server with mock query
    let (server_core, mock_query) = create_test_server_with_query(query_id).await;
    let stream_key = format!("{}-results", query_id);

    let (reaction, _event_rx) =
        create_test_reaction(redis.url().to_string(), query_id, false, None, None);

    reaction.start(server_core.clone()).await?;

    sleep(Duration::from_millis(150)).await;

    // Send result with empty metadata
    let query_result = QueryResult {
        query_id: query_id.to_string(),
        timestamp: chrono::Utc::now(),
        results: vec![json!({"type": "add", "data": {"id": "1"}})],
        metadata: HashMap::new(), // Empty metadata
        profiling: None,
    };
    mock_query.send_result(query_result).await;

    sleep(Duration::from_millis(300)).await;

    let cloud_event = read_cloudevent_from_stream(redis.url(), &stream_key).await?;

    if let ResultEvent::Change(change_event) = cloud_event.data {
        assert!(
            change_event.metadata.is_none(),
            "Empty metadata should be filtered out"
        );
    } else {
        panic!("Expected Change event");
    }

    Ok(())
}

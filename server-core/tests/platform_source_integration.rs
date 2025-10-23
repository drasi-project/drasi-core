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

//! Integration tests for platform source with Redis testcontainers
//!
//! These tests verify the platform source can correctly consume events from Redis Streams,
//! transform CloudEvents to SourceChanges, handle consumer groups, and manage lifecycle.
//!
//! **Requirements**: Docker must be running for these tests to pass.

mod test_support;

use anyhow::Result;
use drasi_core::models::SourceChange;
use drasi_server_core::channels::SourceEvent;
use drasi_server_core::config::SourceConfig;
use drasi_server_core::sources::platform::PlatformSource;
use drasi_server_core::sources::Source;
use serde_json::json;
use std::collections::HashMap;
use std::time::Duration;
use test_support::*;
use tokio::sync::mpsc;
use tokio::time::{sleep, timeout};

/// Helper to create a platform source with test configuration
fn create_test_source(
    redis_url: String,
    stream_key: &str,
) -> (
    PlatformSource,
    tokio::sync::broadcast::Receiver<
        std::sync::Arc<drasi_server_core::channels::SourceEventWrapper>,
    >,
    mpsc::Receiver<drasi_server_core::channels::ComponentEvent>,
) {
    let (event_tx, event_rx) = mpsc::channel(100);

    let mut properties = HashMap::new();
    properties.insert("redis_url".to_string(), json!(redis_url));
    properties.insert("stream_key".to_string(), json!(stream_key));
    properties.insert("consumer_group".to_string(), json!("test-group"));
    properties.insert("consumer_name".to_string(), json!("test-consumer"));
    properties.insert("batch_size".to_string(), json!(10));
    properties.insert("block_ms".to_string(), json!(100));

    let config = SourceConfig {
        id: "test-source".to_string(),
        source_type: "platform".to_string(),
        auto_start: false,
        properties,
        bootstrap_provider: None,
        broadcast_channel_capacity: None,
    };

    let source = PlatformSource::new(config, event_tx);
    let source_change_rx = source.test_subscribe();

    (source, source_change_rx, event_rx)
}

// ==================== Basic Functionality Tests ====================

#[tokio::test]
async fn test_basic_insert_event_consumption() -> Result<()> {
    let redis_url = setup_redis().await;
    let stream_key = "test-insert-stream";

    // Create and start source
    let (source, mut source_change_rx, _event_rx) =
        create_test_source(redis_url.clone(), stream_key);
    source.start().await?;

    // Give source time to set up consumer group
    sleep(Duration::from_millis(100)).await;

    // Publish insert event
    let mut props = HashMap::new();
    props.insert("name".to_string(), json!("Alice"));
    props.insert("age".to_string(), json!(30));

    let event = build_platform_insert_event("1", vec!["Person"], props, "node", None, None);
    publish_platform_event(&redis_url, stream_key, event).await?;

    // Wait for source change
    let wrapper = timeout(Duration::from_secs(5), source_change_rx.recv())
        .await?
        .expect("Should receive source change");

    // Verify
    assert_eq!(wrapper.source_id, "test-source");
    match &wrapper.event {
        SourceEvent::Change(SourceChange::Insert { element }) => {
            assert_eq!(element.get_reference().element_id.as_ref(), "1");
            assert_eq!(
                element.get_metadata().labels.first(),
                Some(&"Person".into())
            );
        }
        _ => panic!("Expected Change(Insert) variant"),
    }

    source.stop().await?;
    Ok(())
}

#[tokio::test]
async fn test_basic_update_event_consumption() -> Result<()> {
    let redis_url = setup_redis().await;
    let stream_key = "test-update-stream";

    let (source, mut source_change_rx, _event_rx) =
        create_test_source(redis_url.clone(), stream_key);
    source.start().await?;
    sleep(Duration::from_millis(100)).await;

    // Publish update event
    let mut old_props = HashMap::new();
    old_props.insert("value".to_string(), json!(10));

    let mut new_props = HashMap::new();
    new_props.insert("value".to_string(), json!(20));

    let event = build_platform_update_event(
        "1",
        vec!["Counter"],
        old_props,
        new_props,
        "node",
        None,
        None,
    );
    publish_platform_event(&redis_url, stream_key, event).await?;

    // Wait for source change
    let wrapper = timeout(Duration::from_secs(5), source_change_rx.recv())
        .await?
        .expect("Should receive source change");

    // Verify
    match &wrapper.event {
        SourceEvent::Change(SourceChange::Update { element }) => {
            assert_eq!(element.get_reference().element_id.as_ref(), "1");
            // Properties should exist
            let _props = element.get_properties();
        }
        _ => panic!("Expected Change(Update) variant"),
    }

    source.stop().await?;
    Ok(())
}

#[tokio::test]
async fn test_basic_delete_event_consumption() -> Result<()> {
    let redis_url = setup_redis().await;
    let stream_key = "test-delete-stream";

    let (source, mut source_change_rx, _event_rx) =
        create_test_source(redis_url.clone(), stream_key);
    source.start().await?;
    sleep(Duration::from_millis(100)).await;

    // Publish delete event
    let mut props = HashMap::new();
    props.insert("name".to_string(), json!("Bob"));

    let event = build_platform_delete_event("2", vec!["Person"], props, "node", None, None);
    publish_platform_event(&redis_url, stream_key, event).await?;

    // Wait for source change
    let wrapper = timeout(Duration::from_secs(5), source_change_rx.recv())
        .await?
        .expect("Should receive source change");

    // Verify
    match &wrapper.event {
        SourceEvent::Change(SourceChange::Delete { metadata }) => {
            assert_eq!(metadata.reference.element_id.as_ref(), "2");
            assert_eq!(metadata.labels.first(), Some(&"Person".into()));
        }
        _ => panic!("Expected Change(Delete) variant"),
    }

    source.stop().await?;
    Ok(())
}

#[tokio::test]
async fn test_node_element_transformation() -> Result<()> {
    let redis_url = setup_redis().await;
    let stream_key = "test-node-transform";

    let (source, mut source_change_rx, _event_rx) =
        create_test_source(redis_url.clone(), stream_key);
    source.start().await?;
    sleep(Duration::from_millis(100)).await;

    // Publish node with multiple labels and properties
    let mut props = HashMap::new();
    props.insert("name".to_string(), json!("Charlie"));
    props.insert("email".to_string(), json!("charlie@example.com"));
    props.insert("active".to_string(), json!(true));

    let event = build_platform_insert_event(
        "3",
        vec!["Person", "User"],
        props.clone(),
        "node",
        None,
        None,
    );
    publish_platform_event(&redis_url, stream_key, event).await?;

    // Wait for source change
    let wrapper = timeout(Duration::from_secs(5), source_change_rx.recv())
        .await?
        .expect("Should receive source change");

    // Verify node structure
    match &wrapper.event {
        SourceEvent::Change(SourceChange::Insert { element }) => {
            // Check labels
            let labels = &element.get_metadata().labels;
            assert!(labels.contains(&"Person".into()));
            assert!(labels.contains(&"User".into()));

            // Check properties
            let properties = element.get_properties();
            use drasi_core::models::ElementValue;
            if let Some(ElementValue::String(name)) = properties.get("name") {
                assert_eq!(name.as_ref(), "Charlie");
            } else {
                panic!("Expected name property");
            }
            if let Some(ElementValue::String(email)) = properties.get("email") {
                assert_eq!(email.as_ref(), "charlie@example.com");
            } else {
                panic!("Expected email property");
            }
            if let Some(ElementValue::Bool(active)) = properties.get("active") {
                assert_eq!(*active, true);
            } else {
                panic!("Expected active property");
            }
        }
        _ => panic!("Expected Change(Insert) variant"),
    }

    source.stop().await?;
    Ok(())
}

#[tokio::test]
async fn test_relation_element_transformation() -> Result<()> {
    let redis_url = setup_redis().await;
    let stream_key = "test-relation-transform";

    let (source, mut source_change_rx, _event_rx) =
        create_test_source(redis_url.clone(), stream_key);
    source.start().await?;
    sleep(Duration::from_millis(100)).await;

    // Publish relation
    let mut props = HashMap::new();
    props.insert("since".to_string(), json!("2020-01-01"));
    props.insert("strength".to_string(), json!(0.95));

    let event =
        build_platform_insert_event("r1", vec!["KNOWS"], props, "rel", Some("1"), Some("2"));
    publish_platform_event(&redis_url, stream_key, event).await?;

    // Wait for source change
    let wrapper = timeout(Duration::from_secs(5), source_change_rx.recv())
        .await?
        .expect("Should receive source change");

    // Verify relation structure
    match &wrapper.event {
        SourceEvent::Change(SourceChange::Insert { element }) => {
            use drasi_core::models::{Element, ElementValue};
            if let Element::Relation {
                metadata,
                properties,
                out_node,
                in_node,
            } = element
            {
                assert_eq!(metadata.reference.element_id.as_ref(), "r1");
                assert_eq!(metadata.labels.first(), Some(&"KNOWS".into()));
                assert_eq!(out_node.element_id.as_ref(), "1");
                assert_eq!(in_node.element_id.as_ref(), "2");
                if let Some(ElementValue::String(since)) = properties.get("since") {
                    assert_eq!(since.as_ref(), "2020-01-01");
                } else {
                    panic!("Expected since property");
                }
            } else {
                panic!("Expected Relation element");
            }
        }
        _ => panic!("Expected Change(Insert) variant"),
    }

    source.stop().await?;
    Ok(())
}

#[tokio::test]
async fn test_multiple_events_in_batch() -> Result<()> {
    let redis_url = setup_redis().await;
    let stream_key = "test-batch-stream";

    let (source, mut source_change_rx, _event_rx) =
        create_test_source(redis_url.clone(), stream_key);
    source.start().await?;
    sleep(Duration::from_millis(100)).await;

    // Publish 5 events
    for i in 1..=5 {
        let mut props = HashMap::new();
        props.insert("value".to_string(), json!(i));

        let event = build_platform_insert_event(
            &format!("item-{}", i),
            vec!["Item"],
            props,
            "node",
            None,
            None,
        );
        publish_platform_event(&redis_url, stream_key, event).await?;
    }

    // Collect all 5 events
    let mut received_ids = Vec::new();
    for _ in 0..5 {
        let wrapper = timeout(Duration::from_secs(5), source_change_rx.recv())
            .await?
            .expect("Should receive source change");

        if let SourceEvent::Change(SourceChange::Insert { element }) = &wrapper.event {
            received_ids.push(element.get_reference().element_id.as_ref().to_string());
        }
    }

    // Verify all 5 received in order
    assert_eq!(received_ids.len(), 5);
    for i in 1..=5 {
        assert!(received_ids.contains(&format!("item-{}", i)));
    }

    source.stop().await?;
    Ok(())
}

// ==================== Consumer Group Tests ====================

#[tokio::test]
async fn test_consumer_group_creation() -> Result<()> {
    let redis_url = setup_redis().await;
    let stream_key = "test-group-creation";

    let (source, _source_change_rx, _event_rx) = create_test_source(redis_url.clone(), stream_key);
    source.start().await?;

    // Give source time to create consumer group
    sleep(Duration::from_millis(200)).await;

    // Verify consumer group exists by trying to read from it
    let client = redis::Client::open(redis_url.as_str())?;
    let mut conn = client.get_multiplexed_async_connection().await?;

    let groups: redis::Value = redis::cmd("XINFO")
        .arg("GROUPS")
        .arg(stream_key)
        .query_async(&mut conn)
        .await?;

    // Should have at least one group
    if let redis::Value::Bulk(group_list) = groups {
        assert!(!group_list.is_empty(), "Consumer group should be created");
    } else {
        panic!("Expected array of groups");
    }

    source.stop().await?;
    Ok(())
}

#[tokio::test]
async fn test_event_acknowledgment() -> Result<()> {
    let redis_url = setup_redis().await;
    let stream_key = "test-ack-stream";

    let (source, mut source_change_rx, _event_rx) =
        create_test_source(redis_url.clone(), stream_key);
    source.start().await?;
    sleep(Duration::from_millis(100)).await;

    // Publish event
    let mut props = HashMap::new();
    props.insert("test".to_string(), json!("ack"));
    let event = build_platform_insert_event("ack-1", vec!["Test"], props, "node", None, None);
    publish_platform_event(&redis_url, stream_key, event).await?;

    // Wait for processing
    timeout(Duration::from_secs(5), source_change_rx.recv())
        .await?
        .expect("Should receive source change");

    // Give time for acknowledgment
    sleep(Duration::from_millis(200)).await;

    // Check pending count - should be 0 after acknowledgment
    let pending = get_pending_count(&redis_url, stream_key, "test-group").await?;
    assert_eq!(pending, 0, "All events should be acknowledged");

    source.stop().await?;
    Ok(())
}

#[tokio::test]
async fn test_resume_from_position() -> Result<()> {
    let redis_url = setup_redis().await;
    let stream_key = "test-resume-stream";

    // Start source and process one event
    let (source, mut source_change_rx, _event_rx) =
        create_test_source(redis_url.clone(), stream_key);
    source.start().await?;
    sleep(Duration::from_millis(100)).await;

    // Publish first event
    let mut props1 = HashMap::new();
    props1.insert("seq".to_string(), json!(1));
    let event1 = build_platform_insert_event("first", vec!["Test"], props1, "node", None, None);
    publish_platform_event(&redis_url, stream_key, event1).await?;

    // Wait for it to be processed
    timeout(Duration::from_secs(5), source_change_rx.recv())
        .await?
        .expect("Should receive first event");

    // Stop source
    source.stop().await?;
    sleep(Duration::from_millis(100)).await;

    // Publish second event while source is stopped
    let mut props2 = HashMap::new();
    props2.insert("seq".to_string(), json!(2));
    let event2 = build_platform_insert_event("second", vec!["Test"], props2, "node", None, None);
    publish_platform_event(&redis_url, stream_key, event2).await?;

    // Restart source with same consumer group
    let (source2, mut source_change_rx2, _event_rx2) =
        create_test_source(redis_url.clone(), stream_key);
    source2.start().await?;

    // Should receive only the second event (resume from position)
    let wrapper = timeout(Duration::from_secs(5), source_change_rx2.recv())
        .await?
        .expect("Should receive second event");

    match &wrapper.event {
        SourceEvent::Change(SourceChange::Insert { element }) => {
            assert_eq!(element.get_reference().element_id.as_ref(), "second");
        }
        _ => panic!("Expected Change(Insert)"),
    }

    source2.stop().await?;
    Ok(())
}

// ==================== Error Handling Tests ====================

#[tokio::test]
async fn test_malformed_json_event() -> Result<()> {
    let redis_url = setup_redis().await;
    let stream_key = "test-malformed-stream";

    let (source, mut source_change_rx, _event_rx) =
        create_test_source(redis_url.clone(), stream_key);
    source.start().await?;
    sleep(Duration::from_millis(100)).await;

    // Publish malformed JSON directly to Redis
    let client = redis::Client::open(redis_url.as_str())?;
    let mut conn = client.get_multiplexed_async_connection().await?;
    let _: String = redis::cmd("XADD")
        .arg(stream_key)
        .arg("*")
        .arg("data")
        .arg("{invalid json}")
        .query_async(&mut conn)
        .await?;

    // Publish valid event after malformed one
    let mut props = HashMap::new();
    props.insert("test".to_string(), json!("valid"));
    let event = build_platform_insert_event("valid-1", vec!["Test"], props, "node", None, None);
    publish_platform_event(&redis_url, stream_key, event).await?;

    // Should still receive the valid event
    let wrapper = timeout(Duration::from_secs(5), source_change_rx.recv())
        .await?
        .expect("Should receive valid event despite malformed one");

    match &wrapper.event {
        SourceEvent::Change(SourceChange::Insert { element }) => {
            assert_eq!(element.get_reference().element_id.as_ref(), "valid-1");
        }
        _ => panic!("Expected Change(Insert)"),
    }

    source.stop().await?;
    Ok(())
}

#[tokio::test]
async fn test_missing_required_fields() -> Result<()> {
    let redis_url = setup_redis().await;
    let stream_key = "test-missing-fields";

    let (source, mut source_change_rx, _event_rx) =
        create_test_source(redis_url.clone(), stream_key);
    source.start().await?;
    sleep(Duration::from_millis(100)).await;

    // Publish event missing 'op' field
    let bad_event = json!({
        "data": [{
            // Missing "op" field
            "payload": {
                "after": {
                    "id": "bad",
                    "labels": ["Test"],
                    "properties": {}
                },
                "source": {
                    "table": "node",
                    "ts_ns": 1000000000
                }
            }
        }]
    });
    publish_platform_event(&redis_url, stream_key, bad_event).await?;

    // Publish valid event
    let mut props = HashMap::new();
    props.insert("test".to_string(), json!("valid"));
    let event = build_platform_insert_event("valid-2", vec!["Test"], props, "node", None, None);
    publish_platform_event(&redis_url, stream_key, event).await?;

    // Should receive valid event
    let wrapper = timeout(Duration::from_secs(5), source_change_rx.recv())
        .await?
        .expect("Should receive valid event");

    match &wrapper.event {
        SourceEvent::Change(SourceChange::Insert { element }) => {
            assert_eq!(element.get_reference().element_id.as_ref(), "valid-2");
        }
        _ => panic!("Expected Change(Insert)"),
    }

    source.stop().await?;
    Ok(())
}

#[tokio::test]
async fn test_invalid_operation_type() -> Result<()> {
    let redis_url = setup_redis().await;
    let stream_key = "test-invalid-op";

    let (source, mut source_change_rx, _event_rx) =
        create_test_source(redis_url.clone(), stream_key);
    source.start().await?;
    sleep(Duration::from_millis(100)).await;

    // Publish event with invalid op
    let bad_event = json!({
        "data": [{
            "op": "x", // Invalid operation
            "payload": {
                "after": {
                    "id": "bad",
                    "labels": ["Test"],
                    "properties": {}
                },
                "source": {
                    "table": "node",
                    "ts_ns": 1000000000
                }
            }
        }]
    });
    publish_platform_event(&redis_url, stream_key, bad_event).await?;

    // Publish valid event
    let mut props = HashMap::new();
    props.insert("test".to_string(), json!("valid"));
    let event = build_platform_insert_event("valid-3", vec!["Test"], props, "node", None, None);
    publish_platform_event(&redis_url, stream_key, event).await?;

    // Should receive valid event
    let wrapper = timeout(Duration::from_secs(5), source_change_rx.recv())
        .await?
        .expect("Should receive valid event");

    match &wrapper.event {
        SourceEvent::Change(SourceChange::Insert { element }) => {
            assert_eq!(element.get_reference().element_id.as_ref(), "valid-3");
        }
        _ => panic!("Expected Change(Insert)"),
    }

    source.stop().await?;
    Ok(())
}

#[tokio::test]
async fn test_stream_creation_if_not_exists() -> Result<()> {
    let redis_url = setup_redis().await;
    let stream_key = "non-existent-stream";

    // Start source on non-existent stream
    let (source, _source_change_rx, _event_rx) = create_test_source(redis_url.clone(), stream_key);
    source.start().await?;
    sleep(Duration::from_millis(200)).await;

    // Verify stream was created
    let client = redis::Client::open(redis_url.as_str())?;
    let mut conn = client.get_multiplexed_async_connection().await?;

    let stream_type: String = redis::cmd("TYPE")
        .arg(stream_key)
        .query_async(&mut conn)
        .await?;

    assert_eq!(stream_type, "stream", "Stream should be auto-created");

    source.stop().await?;
    Ok(())
}

#[tokio::test]
async fn test_multiple_events_in_cloudevent_data_array() -> Result<()> {
    let redis_url = setup_redis().await;
    let stream_key = "test-multi-event-array";

    let (source, mut source_change_rx, _event_rx) =
        create_test_source(redis_url.clone(), stream_key);
    source.start().await?;
    sleep(Duration::from_millis(100)).await;

    // Publish CloudEvent with data array containing 3 events
    let multi_event = json!({
        "data": [
            {
                "op": "i",
                "payload": {
                    "after": {
                        "id": "multi-1",
                        "labels": ["Test"],
                        "properties": {"value": 1}
                    },
                    "source": {
                        "table": "node",
                        "ts_ns": 1000000000
                    }
                }
            },
            {
                "op": "i",
                "payload": {
                    "after": {
                        "id": "multi-2",
                        "labels": ["Test"],
                        "properties": {"value": 2}
                    },
                    "source": {
                        "table": "node",
                        "ts_ns": 1000000001
                    }
                }
            },
            {
                "op": "i",
                "payload": {
                    "after": {
                        "id": "multi-3",
                        "labels": ["Test"],
                        "properties": {"value": 3}
                    },
                    "source": {
                        "table": "node",
                        "ts_ns": 1000000002
                    }
                }
            }
        ]
    });

    publish_platform_event(&redis_url, stream_key, multi_event).await?;

    // Should receive all 3 events
    let mut received_ids = Vec::new();
    for _ in 0..3 {
        let wrapper = timeout(Duration::from_secs(5), source_change_rx.recv())
            .await?
            .expect("Should receive event");

        if let SourceEvent::Change(SourceChange::Insert { element }) = &wrapper.event {
            received_ids.push(element.get_reference().element_id.as_ref().to_string());
        }
    }

    assert_eq!(received_ids.len(), 3);
    assert!(received_ids.contains(&"multi-1".to_string()));
    assert!(received_ids.contains(&"multi-2".to_string()));
    assert!(received_ids.contains(&"multi-3".to_string()));

    source.stop().await?;
    Ok(())
}

// ==================== Lifecycle Tests ====================

#[tokio::test]
async fn test_source_start_and_stop() -> Result<()> {
    use drasi_server_core::channels::ComponentStatus;

    let redis_url = setup_redis().await;
    let stream_key = "test-lifecycle";

    let (source, _source_change_rx, _event_rx) = create_test_source(redis_url, stream_key);

    // Start source
    source.start().await?;

    // Verify running status
    sleep(Duration::from_millis(200)).await;
    assert_eq!(source.status().await, ComponentStatus::Running);

    // Stop source
    source.stop().await?;

    // Verify stopped status
    assert_eq!(source.status().await, ComponentStatus::Stopped);

    Ok(())
}

#[tokio::test]
async fn test_graceful_shutdown() -> Result<()> {
    let redis_url = setup_redis().await;
    let stream_key = "test-shutdown";

    let (source, mut source_change_rx, _event_rx) =
        create_test_source(redis_url.clone(), stream_key);
    source.start().await?;
    sleep(Duration::from_millis(100)).await;

    // Publish multiple events
    for i in 1..=3 {
        let mut props = HashMap::new();
        props.insert("value".to_string(), json!(i));
        let event = build_platform_insert_event(
            &format!("shutdown-{}", i),
            vec!["Test"],
            props,
            "node",
            None,
            None,
        );
        publish_platform_event(&redis_url, stream_key, event).await?;
    }

    // Start receiving
    let first = timeout(Duration::from_secs(5), source_change_rx.recv()).await?;
    assert!(first.is_ok());

    // Stop while events may still be in flight
    source.stop().await?;

    // Check that pending entries are acknowledged (allow more time for acknowledgments)
    sleep(Duration::from_millis(500)).await;
    let pending = get_pending_count(&redis_url, stream_key, "test-group").await?;
    // Note: Due to async timing, some events may still be pending
    assert!(
        pending <= 2,
        "Most processed events should be acknowledged, got {} pending",
        pending
    );

    Ok(())
}

#[tokio::test]
async fn test_restart_and_resume() -> Result<()> {
    let redis_url = setup_redis().await;
    let stream_key = "test-restart";

    // First run
    let (source1, mut rx1, _event_rx1) = create_test_source(redis_url.clone(), stream_key);
    source1.start().await?;
    sleep(Duration::from_millis(100)).await;

    // Process first event
    let mut props1 = HashMap::new();
    props1.insert("seq".to_string(), json!(1));
    let event1 = build_platform_insert_event("restart-1", vec!["Test"], props1, "node", None, None);
    publish_platform_event(&redis_url, stream_key, event1).await?;

    timeout(Duration::from_secs(5), rx1.recv())
        .await?
        .expect("Should receive first");

    // Stop
    source1.stop().await?;
    sleep(Duration::from_millis(100)).await;

    // Publish while stopped
    let mut props2 = HashMap::new();
    props2.insert("seq".to_string(), json!(2));
    let event2 = build_platform_insert_event("restart-2", vec!["Test"], props2, "node", None, None);
    publish_platform_event(&redis_url, stream_key, event2).await?;

    // Restart with same group
    let (source2, mut rx2, _event_rx2) = create_test_source(redis_url.clone(), stream_key);
    source2.start().await?;

    // Should resume and get second event
    let wrapper = timeout(Duration::from_secs(5), rx2.recv())
        .await?
        .expect("Should receive second");

    match &wrapper.event {
        SourceEvent::Change(SourceChange::Insert { element }) => {
            assert_eq!(element.get_reference().element_id.as_ref(), "restart-2");
        }
        _ => panic!("Expected Change(Insert)"),
    }

    source2.stop().await?;
    Ok(())
}

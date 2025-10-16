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

//! Test utilities for Redis-based platform integration tests
//!
//! This module provides helper functions for testing platform sources and reactions
//! using testcontainers to provide a real Redis server environment.

use anyhow::Result;
use serde_json::{json, Value};
use std::collections::HashMap;
use testcontainers_modules::redis::Redis;

/// Setup a Redis testcontainer and return the connection URL
///
/// Returns the connection URL for the Redis container
pub async fn setup_redis() -> String {
    use testcontainers::runners::AsyncRunner;

    // Start Redis container
    let container = Redis::default().start().await.unwrap();
    let redis_port = container.get_host_port_ipv4(6379).await.unwrap();
    let redis_url = format!("redis://127.0.0.1:{}", redis_port);

    // Keep container alive by leaking it (test framework will clean up)
    std::mem::forget(container);

    redis_url
}

/// Publish a platform CloudEvent to a Redis stream
///
/// # Arguments
/// * `redis_url` - Redis connection URL
/// * `stream_key` - Stream key to publish to
/// * `cloud_event` - CloudEvent JSON value to publish
///
/// # Returns
/// The stream ID of the published event
#[allow(dead_code)]
pub async fn publish_platform_event(
    redis_url: &str,
    stream_key: &str,
    cloud_event: Value,
) -> Result<String> {
    let client = redis::Client::open(redis_url)?;
    let mut conn = client.get_multiplexed_async_connection().await?;

    // Serialize CloudEvent to JSON string
    let event_json = serde_json::to_string(&cloud_event)?;

    // Publish to Redis stream with "data" field
    let stream_id: String = redis::cmd("XADD")
        .arg(stream_key)
        .arg("*") // Auto-generate stream ID
        .arg("data")
        .arg(event_json)
        .query_async(&mut conn)
        .await?;

    Ok(stream_id)
}

/// Read events from a Redis stream
///
/// # Arguments
/// * `redis_url` - Redis connection URL
/// * `stream_key` - Stream key to read from
/// * `start_id` - Stream ID to start reading from ("0" for all, ">" for new)
/// * `count` - Maximum number of events to read
///
/// # Returns
/// Vec of (stream_id, event_data) tuples
#[allow(dead_code)]
pub async fn read_from_stream(
    redis_url: &str,
    stream_key: &str,
    start_id: &str,
    count: usize,
) -> Result<Vec<(String, HashMap<String, String>)>> {
    use redis::streams::StreamReadReply;

    let client = redis::Client::open(redis_url)?;
    let mut conn = client.get_multiplexed_async_connection().await?;

    // Use XREAD command directly
    let results: StreamReadReply = redis::cmd("XREAD")
        .arg("COUNT")
        .arg(count)
        .arg("STREAMS")
        .arg(stream_key)
        .arg(start_id)
        .query_async(&mut conn)
        .await?;

    let mut events = Vec::new();
    for stream_key in results.keys {
        for stream_id in stream_key.ids {
            let mut data = HashMap::new();
            for (key, value) in stream_id.map {
                // Handle redis::Value::Data (Vec<u8>)
                if let redis::Value::Data(bytes) = value {
                    let value_str = String::from_utf8(bytes)?;
                    data.insert(key, value_str);
                }
            }
            events.push((stream_id.id, data));
        }
    }

    Ok(events)
}

/// Create a consumer group on a Redis stream
///
/// # Arguments
/// * `redis_url` - Redis connection URL
/// * `stream_key` - Stream key
/// * `group_name` - Consumer group name
///
/// # Returns
/// Ok if successful, Err if group already exists or other error
#[allow(dead_code)]
pub async fn create_consumer_group(
    redis_url: &str,
    stream_key: &str,
    group_name: &str,
) -> Result<()> {
    let client = redis::Client::open(redis_url)?;
    let mut conn = client.get_multiplexed_async_connection().await?;

    // Create consumer group with MKSTREAM flag
    let _: Result<String, redis::RedisError> = redis::cmd("XGROUP")
        .arg("CREATE")
        .arg(stream_key)
        .arg(group_name)
        .arg("0") // Start from beginning
        .arg("MKSTREAM") // Create stream if it doesn't exist
        .query_async(&mut conn)
        .await;

    // Ignore "BUSYGROUP" error if group already exists
    Ok(())
}

/// Build a platform insert event in CloudEvent format
///
/// # Arguments
/// * `element_id` - Element ID
/// * `labels` - Element labels
/// * `properties` - Element properties
/// * `element_type` - "node" or "rel"
/// * `start_id` - Start node ID (for relations only)
/// * `end_id` - End node ID (for relations only)
pub fn build_platform_insert_event(
    element_id: &str,
    labels: Vec<&str>,
    properties: HashMap<String, Value>,
    element_type: &str,
    start_id: Option<&str>,
    end_id: Option<&str>,
) -> Value {
    let ts_ns = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0) as u64;

    let mut after = json!({
        "id": element_id,
        "labels": labels,
        "properties": properties,
    });

    // Add relation-specific fields
    if element_type == "rel" || element_type == "relation" {
        after["startId"] = json!(start_id.unwrap_or(""));
        after["endId"] = json!(end_id.unwrap_or(""));
    }

    json!({
        "data": [{
            "op": "i",
            "payload": {
                "after": after,
                "source": {
                    "table": element_type,
                    "ts_ns": ts_ns,
                }
            }
        }]
    })
}

/// Build a platform update event in CloudEvent format
pub fn build_platform_update_event(
    element_id: &str,
    labels: Vec<&str>,
    old_properties: HashMap<String, Value>,
    new_properties: HashMap<String, Value>,
    element_type: &str,
    start_id: Option<&str>,
    end_id: Option<&str>,
) -> Value {
    let ts_ns = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0) as u64;

    let mut after = json!({
        "id": element_id,
        "labels": labels,
        "properties": new_properties,
    });

    let mut before = json!({
        "id": element_id,
        "labels": labels,
        "properties": old_properties,
    });

    // Add relation-specific fields
    if element_type == "rel" || element_type == "relation" {
        after["startId"] = json!(start_id.unwrap_or(""));
        after["endId"] = json!(end_id.unwrap_or(""));
        before["startId"] = json!(start_id.unwrap_or(""));
        before["endId"] = json!(end_id.unwrap_or(""));
    }

    json!({
        "data": [{
            "op": "u",
            "payload": {
                "after": after,
                "before": before,
                "source": {
                    "table": element_type,
                    "ts_ns": ts_ns,
                }
            }
        }]
    })
}

/// Build a platform delete event in CloudEvent format
pub fn build_platform_delete_event(
    element_id: &str,
    labels: Vec<&str>,
    properties: HashMap<String, Value>,
    element_type: &str,
    start_id: Option<&str>,
    end_id: Option<&str>,
) -> Value {
    let ts_ns = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0) as u64;

    let mut before = json!({
        "id": element_id,
        "labels": labels,
        "properties": properties,
    });

    // Add relation-specific fields
    if element_type == "rel" || element_type == "relation" {
        before["startId"] = json!(start_id.unwrap_or(""));
        before["endId"] = json!(end_id.unwrap_or(""));
    }

    json!({
        "data": [{
            "op": "d",
            "payload": {
                "before": before,
                "source": {
                    "table": element_type,
                    "ts_ns": ts_ns,
                }
            }
        }]
    })
}

/// Verify CloudEvent structure has all required fields
///
/// # Arguments
/// * `cloud_event` - CloudEvent JSON value to verify
///
/// # Returns
/// Ok if valid, Err with description if invalid
pub fn verify_cloudevent_structure(cloud_event: &Value) -> Result<()> {
    // Required fields
    let required_fields = vec![
        "id", "source", "specversion", "type",
        "datacontenttype", "topic", "time", "data"
    ];

    for field in required_fields {
        if cloud_event.get(field).is_none() {
            return Err(anyhow::anyhow!("Missing required field: {}", field));
        }
    }

    // Verify values
    if cloud_event["specversion"] != "1.0" {
        return Err(anyhow::anyhow!("Invalid specversion, expected '1.0'"));
    }

    if cloud_event["datacontenttype"] != "application/json" {
        return Err(anyhow::anyhow!("Invalid datacontenttype, expected 'application/json'"));
    }

    if cloud_event["type"] != "com.dapr.event.sent" {
        return Err(anyhow::anyhow!("Invalid type, expected 'com.dapr.event.sent'"));
    }

    // Verify ID is a valid UUID v4
    let id = cloud_event["id"].as_str()
        .ok_or_else(|| anyhow::anyhow!("id is not a string"))?;
    uuid::Uuid::parse_str(id)
        .map_err(|_| anyhow::anyhow!("id is not a valid UUID"))?;

    // Verify timestamp is ISO 8601
    let time = cloud_event["time"].as_str()
        .ok_or_else(|| anyhow::anyhow!("time is not a string"))?;
    chrono::DateTime::parse_from_rfc3339(time)
        .map_err(|_| anyhow::anyhow!("time is not valid ISO 8601 format"))?;

    Ok(())
}

/// Get stream length using XLEN command
#[allow(dead_code)]
pub async fn get_stream_length(redis_url: &str, stream_key: &str) -> Result<usize> {
    let client = redis::Client::open(redis_url)?;
    let mut conn = client.get_multiplexed_async_connection().await?;

    let length: usize = redis::cmd("XLEN")
        .arg(stream_key)
        .query_async(&mut conn)
        .await?;

    Ok(length)
}

/// Get pending entries count for a consumer group
#[allow(dead_code)]
pub async fn get_pending_count(
    redis_url: &str,
    stream_key: &str,
    group_name: &str,
) -> Result<usize> {
    let client = redis::Client::open(redis_url)?;
    let mut conn = client.get_multiplexed_async_connection().await?;

    let result: Vec<redis::Value> = redis::cmd("XPENDING")
        .arg(stream_key)
        .arg(group_name)
        .query_async(&mut conn)
        .await?;

    // XPENDING returns [count, min_id, max_id, consumers]
    // We want the count (first element)
    if let Some(redis::Value::Int(count)) = result.first() {
        Ok(*count as usize)
    } else {
        Ok(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_setup_redis() {
        let redis_url = setup_redis().await;
        assert!(redis_url.starts_with("redis://127.0.0.1:"));

        // Verify we can connect
        let client = redis::Client::open(redis_url.as_str()).unwrap();
        let mut conn = client.get_multiplexed_async_connection().await.unwrap();
        let _: String = redis::cmd("PING").query_async(&mut conn).await.unwrap();
    }

    #[test]
    fn test_build_platform_insert_event_node() {
        let mut props = HashMap::new();
        props.insert("name".to_string(), json!("Alice"));
        props.insert("age".to_string(), json!(30));

        let event = build_platform_insert_event(
            "1",
            vec!["Person"],
            props,
            "node",
            None,
            None,
        );

        assert_eq!(event["data"][0]["op"], "i");
        assert_eq!(event["data"][0]["payload"]["after"]["id"], "1");
        assert_eq!(event["data"][0]["payload"]["after"]["labels"][0], "Person");
        assert_eq!(event["data"][0]["payload"]["after"]["properties"]["name"], "Alice");
        assert_eq!(event["data"][0]["payload"]["source"]["table"], "node");
    }

    #[test]
    fn test_build_platform_insert_event_relation() {
        let mut props = HashMap::new();
        props.insert("since".to_string(), json!("2020"));

        let event = build_platform_insert_event(
            "r1",
            vec!["KNOWS"],
            props,
            "rel",
            Some("1"),
            Some("2"),
        );

        assert_eq!(event["data"][0]["op"], "i");
        assert_eq!(event["data"][0]["payload"]["after"]["startId"], "1");
        assert_eq!(event["data"][0]["payload"]["after"]["endId"], "2");
    }

    #[test]
    fn test_build_platform_update_event() {
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

        assert_eq!(event["data"][0]["op"], "u");
        assert_eq!(event["data"][0]["payload"]["before"]["properties"]["value"], 10);
        assert_eq!(event["data"][0]["payload"]["after"]["properties"]["value"], 20);
    }

    #[test]
    fn test_build_platform_delete_event() {
        let mut props = HashMap::new();
        props.insert("name".to_string(), json!("Bob"));

        let event = build_platform_delete_event(
            "2",
            vec!["Person"],
            props,
            "node",
            None,
            None,
        );

        assert_eq!(event["data"][0]["op"], "d");
        assert_eq!(event["data"][0]["payload"]["before"]["id"], "2");
        assert!(event["data"][0]["payload"]["after"].is_null());
    }

    #[test]
    fn test_verify_cloudevent_structure_valid() {
        let cloud_event = json!({
            "id": "550e8400-e29b-41d4-a716-446655440000",
            "source": "drasi-core",
            "specversion": "1.0",
            "type": "com.dapr.event.sent",
            "datacontenttype": "application/json",
            "topic": "test-results",
            "time": "2025-01-01T00:00:00.000Z",
            "data": {},
            "pubsubname": "drasi-pubsub",
        });

        assert!(verify_cloudevent_structure(&cloud_event).is_ok());
    }

    #[test]
    fn test_verify_cloudevent_structure_missing_field() {
        let cloud_event = json!({
            "id": "550e8400-e29b-41d4-a716-446655440000",
            "source": "drasi-core",
            // Missing specversion
            "type": "com.dapr.event.sent",
            "datacontenttype": "application/json",
            "topic": "test-results",
            "time": "2025-01-01T00:00:00.000Z",
            "data": {},
        });

        assert!(verify_cloudevent_structure(&cloud_event).is_err());
    }

    #[test]
    fn test_verify_cloudevent_structure_invalid_uuid() {
        let cloud_event = json!({
            "id": "not-a-uuid",
            "source": "drasi-core",
            "specversion": "1.0",
            "type": "com.dapr.event.sent",
            "datacontenttype": "application/json",
            "topic": "test-results",
            "time": "2025-01-01T00:00:00.000Z",
            "data": {},
        });

        assert!(verify_cloudevent_structure(&cloud_event).is_err());
    }
}

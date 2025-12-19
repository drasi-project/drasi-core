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

//! Integration tests for SSE reaction with full DrasiLib setup.
//!
//! These tests verify the SSE reaction works correctly when integrated with
//! a full Drasi stack including sources, queries, and the actual SSE endpoint.

use drasi_lib::{DrasiLib, Query};
use drasi_reaction_sse::{QueryConfig, SseReaction, TemplateSpec};
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::timeout;

/// Mock application source that can emit test data
mod mock_source {
    use async_trait::async_trait;
    use drasi_lib::plugin_core::{QuerySubscriber, Source};
    use drasi_lib::channels::{ComponentEventSender, ComponentStatus};
    use serde_json::Value;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    pub struct MockSource {
        id: String,
        subscribers: Arc<Mutex<Vec<Arc<dyn QuerySubscriber>>>>,
        status: Arc<tokio::sync::RwLock<ComponentStatus>>,
    }

    impl MockSource {
        pub fn new(id: &str) -> Self {
            Self {
                id: id.to_string(),
                subscribers: Arc::new(Mutex::new(Vec::new())),
                status: Arc::new(tokio::sync::RwLock::new(ComponentStatus::NotStarted)),
            }
        }

        pub async fn emit_event(&self, operation: &str, element: Value) -> anyhow::Result<()> {
            let event = serde_json::json!({
                "operation": operation,
                "element": element,
            });

            let subscribers = self.subscribers.lock().await;
            for subscriber in subscribers.iter() {
                subscriber.on_source_change(&self.id, event.clone()).await?;
            }
            Ok(())
        }
    }

    #[async_trait]
    impl Source for MockSource {
        fn id(&self) -> &str {
            &self.id
        }

        fn type_name(&self) -> &str {
            "mock"
        }

        async fn subscribe(&self, subscriber: Arc<dyn QuerySubscriber>) -> anyhow::Result<()> {
            self.subscribers.lock().await.push(subscriber);
            Ok(())
        }

        async fn unsubscribe(&self, _subscriber_id: &str) -> anyhow::Result<()> {
            Ok(())
        }

        async fn start(&self, _event_tx: ComponentEventSender) -> anyhow::Result<()> {
            *self.status.write().await = ComponentStatus::Running;
            Ok(())
        }

        async fn stop(&self) -> anyhow::Result<()> {
            *self.status.write().await = ComponentStatus::Stopped;
            Ok(())
        }

        fn is_realtime(&self) -> bool {
            true
        }
    }
}

/// Helper to connect to SSE endpoint and read events
async fn read_sse_events(
    url: &str,
    count: usize,
    timeout_secs: u64,
) -> anyhow::Result<Vec<String>> {
    let client = reqwest::Client::new();
    let mut events = Vec::new();

    let response = timeout(
        Duration::from_secs(timeout_secs),
        client.get(url).send()
    ).await??;

    if !response.status().is_success() {
        anyhow::bail!("Failed to connect to SSE endpoint: {}", response.status());
    }

    let mut stream = response.bytes_stream();
    use futures::StreamExt;

    while events.len() < count {
        match timeout(Duration::from_secs(5), stream.next()).await {
            Ok(Some(Ok(chunk))) => {
                let text = String::from_utf8_lossy(&chunk);
                for line in text.lines() {
                    if line.starts_with("data: ") {
                        let data = line.trim_start_matches("data: ");
                        events.push(data.to_string());
                    }
                }
            }
            Ok(Some(Err(e))) => {
                anyhow::bail!("Error reading SSE stream: {}", e);
            }
            Ok(None) => {
                break;
            }
            Err(_) => {
                // Timeout waiting for next event
                break;
            }
        }
    }

    Ok(events)
}

#[tokio::test]
async fn test_sse_with_basic_query() -> anyhow::Result<()> {
    // Create a mock source
    let source = Arc::new(mock_source::MockSource::new("test-source"));

    // Define a simple query
    let query = Query::cypher("test-query")
        .query(r#"
            MATCH (n:Person)
            RETURN n.name AS name, n.age AS age
        "#)
        .from_source("test-source")
        .auto_start(true)
        .build();

    // Create SSE reaction on a unique port
    let sse_port = 18080;
    let sse_reaction = SseReaction::builder("test-sse")
        .with_query("test-query")
        .with_port(sse_port)
        .with_sse_path("/events")
        .build()?;

    // Build DrasiLib
    let drasi = Arc::new(
        DrasiLib::builder()
            .with_id("integration-test")
            .with_source(source.clone())
            .with_query(query)
            .with_reaction(sse_reaction)
            .build()
            .await?
    );

    // Start the system
    drasi.start().await?;

    // Give the system time to start
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Connect to SSE endpoint in background task
    let sse_url = format!("http://localhost:{}/events", sse_port);
    let events_handle = tokio::spawn(async move {
        read_sse_events(&sse_url, 2, 10).await
    });

    // Give client time to connect
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Emit test events through the mock source
    source.emit_event("insert", json!({
        "type": "node",
        "id": "person1",
        "labels": ["Person"],
        "properties": {
            "name": "Alice",
            "age": 30
        }
    })).await?;

    source.emit_event("insert", json!({
        "type": "node",
        "id": "person2",
        "labels": ["Person"],
        "properties": {
            "name": "Bob",
            "age": 25
        }
    })).await?;

    // Wait for events to be received
    let events = events_handle.await??;

    // Verify we got events
    assert!(!events.is_empty(), "Should have received SSE events");

    // Parse and verify event structure
    for event_str in &events {
        if event_str.contains("heartbeat") {
            continue; // Skip heartbeats
        }
        
        let event: serde_json::Value = serde_json::from_str(event_str)?;
        assert!(event.get("queryId").is_some() || event.get("results").is_some(),
                "Event should have queryId or results field");
    }

    // Stop the system
    drasi.stop().await?;

    Ok(())
}

#[tokio::test]
async fn test_sse_with_custom_templates() -> anyhow::Result<()> {
    // Create a mock source
    let source = Arc::new(mock_source::MockSource::new("test-source"));

    // Define a simple query
    let query = Query::cypher("sensor-query")
        .query(r#"
            MATCH (s:Sensor)
            RETURN s.id AS id, s.temperature AS temperature
        "#)
        .from_source("test-source")
        .auto_start(true)
        .build();

    // Create SSE reaction with custom template
    let sse_port = 18081;
    let sensor_config = QueryConfig {
        added: Some(TemplateSpec {
            path: None,
            template: r#"{"event":"sensor_added","sensor":"{{after.id}}","temp":{{after.temperature}}}"#.to_string(),
        }),
        updated: Some(TemplateSpec {
            path: None,
            template: r#"{"event":"sensor_updated","sensor":"{{after.id}}","temp":{{after.temperature}}}"#.to_string(),
        }),
        deleted: None,
    };

    let sse_reaction = SseReaction::builder("test-sse")
        .with_query("sensor-query")
        .with_route("sensor-query", sensor_config)
        .with_port(sse_port)
        .with_sse_path("/sensors")
        .build()?;

    // Build DrasiLib
    let drasi = Arc::new(
        DrasiLib::builder()
            .with_id("integration-test-templates")
            .with_source(source.clone())
            .with_query(query)
            .with_reaction(sse_reaction)
            .build()
            .await?
    );

    // Start the system
    drasi.start().await?;

    // Give the system time to start
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Connect to SSE endpoint in background task
    let sse_url = format!("http://localhost:{}/sensors", sse_port);
    let events_handle = tokio::spawn(async move {
        read_sse_events(&sse_url, 1, 10).await
    });

    // Give client time to connect
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Emit test event
    source.emit_event("insert", json!({
        "type": "node",
        "id": "sensor1",
        "labels": ["Sensor"],
        "properties": {
            "id": "temp-01",
            "temperature": 25.5
        }
    })).await?;

    // Wait for events to be received
    let events = events_handle.await??;

    // Verify we got at least one non-heartbeat event
    let data_events: Vec<_> = events.iter()
        .filter(|e| !e.contains("heartbeat"))
        .collect();
    
    assert!(!data_events.is_empty(), "Should have received at least one data event");

    // Verify custom template was applied
    for event_str in data_events {
        let event: serde_json::Value = serde_json::from_str(event_str)?;
        // Custom template should have "event" field
        assert!(event.get("event").is_some(), "Custom template should have 'event' field");
        assert!(event.get("sensor").is_some() || event.get("temp").is_some(),
                "Custom template should have sensor data fields");
    }

    // Stop the system
    drasi.stop().await?;

    Ok(())
}

#[tokio::test]
async fn test_sse_multi_path() -> anyhow::Result<()> {
    // Create a mock source
    let source = Arc::new(mock_source::MockSource::new("test-source"));

    // Define two queries
    let query1 = Query::cypher("sensors")
        .query(r#"MATCH (s:Sensor) RETURN s.id AS id"#)
        .from_source("test-source")
        .auto_start(true)
        .build();

    let query2 = Query::cypher("alerts")
        .query(r#"MATCH (a:Alert) RETURN a.message AS message"#)
        .from_source("test-source")
        .auto_start(true)
        .build();

    // Create SSE reaction with different paths per query
    let sse_port = 18082;
    let sensor_config = QueryConfig {
        added: Some(TemplateSpec {
            path: Some("/sensors".to_string()),
            template: r#"{"type":"sensor","data":{{json after}}}"#.to_string(),
        }),
        updated: None,
        deleted: None,
    };

    let alert_config = QueryConfig {
        added: Some(TemplateSpec {
            path: Some("/alerts".to_string()),
            template: r#"{"type":"alert","data":{{json after}}}"#.to_string(),
        }),
        updated: None,
        deleted: None,
    };

    let sse_reaction = SseReaction::builder("test-sse")
        .with_query("sensors")
        .with_query("alerts")
        .with_route("sensors", sensor_config)
        .with_route("alerts", alert_config)
        .with_port(sse_port)
        .build()?;

    // Build DrasiLib
    let drasi = Arc::new(
        DrasiLib::builder()
            .with_id("integration-test-multipath")
            .with_source(source.clone())
            .with_query(query1)
            .with_query(query2)
            .with_reaction(sse_reaction)
            .build()
            .await?
    );

    // Start the system
    drasi.start().await?;
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Connect to both SSE endpoints
    let sensor_url = format!("http://localhost:{}/sensors", sse_port);
    let alert_url = format!("http://localhost:{}/alerts", sse_port);

    let sensor_handle = tokio::spawn(async move {
        read_sse_events(&sensor_url, 1, 10).await
    });

    let alert_handle = tokio::spawn(async move {
        read_sse_events(&alert_url, 1, 10).await
    });

    tokio::time::sleep(Duration::from_millis(200)).await;

    // Emit events for both queries
    source.emit_event("insert", json!({
        "type": "node",
        "id": "sensor1",
        "labels": ["Sensor"],
        "properties": {"id": "s1"}
    })).await?;

    source.emit_event("insert", json!({
        "type": "node",
        "id": "alert1",
        "labels": ["Alert"],
        "properties": {"message": "High temperature"}
    })).await?;

    // Verify events on different paths
    let sensor_events = sensor_handle.await??;
    let alert_events = alert_handle.await??;

    // Both should have received events
    let sensor_data: Vec<_> = sensor_events.iter()
        .filter(|e| !e.contains("heartbeat"))
        .collect();
    let alert_data: Vec<_> = alert_events.iter()
        .filter(|e| !e.contains("heartbeat"))
        .collect();

    assert!(!sensor_data.is_empty(), "Sensor path should receive sensor events");
    assert!(!alert_data.is_empty(), "Alert path should receive alert events");

    // Verify events have correct type
    for event_str in sensor_data {
        let event: serde_json::Value = serde_json::from_str(event_str)?;
        if let Some(event_type) = event.get("type").and_then(|v| v.as_str()) {
            assert_eq!(event_type, "sensor", "Sensor path should only get sensor events");
        }
    }

    for event_str in alert_data {
        let event: serde_json::Value = serde_json::from_str(event_str)?;
        if let Some(event_type) = event.get("type").and_then(|v| v.as_str()) {
            assert_eq!(event_type, "alert", "Alert path should only get alert events");
        }
    }

    drasi.stop().await?;
    Ok(())
}

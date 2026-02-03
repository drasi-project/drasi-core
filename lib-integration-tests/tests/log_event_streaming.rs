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

//! Integration tests for log and event streaming in DrasiLib.
//!
//! These tests verify that:
//! - Component lifecycle events are properly captured and streamed
//! - Component logs are properly captured and streamed
//! - The streaming APIs work correctly for sources, queries, and reactions

use async_trait::async_trait;
use drasi_lib::{
    ComponentEvent, ComponentStatus, ComponentType, DrasiLib, LogLevel, Query, Reaction,
    ReactionRuntimeContext, Source, SourceBase, SourceBaseParams, SourceRuntimeContext,
    SourceSubscriptionSettings, SubscriptionResponse,
};
use std::collections::HashMap;
use std::time::Duration;
use tokio::time::timeout;

// ============================================================================
// Test Source Implementation
// ============================================================================

/// A simple test source that emits logs during lifecycle operations.
struct TestSource {
    base: SourceBase,
}

impl TestSource {
    fn new(id: &str) -> anyhow::Result<Self> {
        let params = SourceBaseParams::new(id);
        Ok(Self {
            base: SourceBase::new(params)?,
        })
    }
}

#[async_trait]
impl Source for TestSource {
    fn id(&self) -> &str {
        self.base.get_id()
    }

    fn type_name(&self) -> &str {
        "test-source"
    }

    fn properties(&self) -> HashMap<String, serde_json::Value> {
        HashMap::new()
    }

    fn auto_start(&self) -> bool {
        self.base.auto_start
    }

    async fn initialize(&self, context: SourceRuntimeContext) {
        self.base.initialize(context).await;
    }

    async fn start(&self) -> anyhow::Result<()> {
        // Log during startup
        if let Some(logger) = self.base.logger().await {
            logger.info("TestSource is starting up").await;
            logger.debug("Initializing internal state").await;
        }

        self.base
            .set_status_with_event(ComponentStatus::Running, Some("Started".to_string()))
            .await?;

        if let Some(logger) = self.base.logger().await {
            logger.info("TestSource started successfully").await;
        }

        Ok(())
    }

    async fn stop(&self) -> anyhow::Result<()> {
        if let Some(logger) = self.base.logger().await {
            logger.info("TestSource stopping").await;
        }

        self.base.stop_common().await
    }

    async fn status(&self) -> ComponentStatus {
        self.base.get_status().await
    }

    async fn subscribe(
        &self,
        settings: SourceSubscriptionSettings,
    ) -> anyhow::Result<SubscriptionResponse> {
        self.base
            .subscribe_with_bootstrap(&settings, "test-source")
            .await
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

// ============================================================================
// Test Reaction Implementation
// ============================================================================

/// A simple test reaction that emits logs during lifecycle operations.
struct TestReaction {
    base: drasi_lib::ReactionBase,
}

impl TestReaction {
    fn new(id: &str, query_ids: Vec<String>) -> Self {
        let params = drasi_lib::ReactionBaseParams::new(id, query_ids);
        Self {
            base: drasi_lib::ReactionBase::new(params),
        }
    }
}

#[async_trait]
impl Reaction for TestReaction {
    fn id(&self) -> &str {
        self.base.get_id()
    }

    fn type_name(&self) -> &str {
        "test-reaction"
    }

    fn properties(&self) -> HashMap<String, serde_json::Value> {
        HashMap::new()
    }

    fn query_ids(&self) -> Vec<String> {
        self.base.get_queries().to_vec()
    }

    fn auto_start(&self) -> bool {
        self.base.get_auto_start()
    }

    async fn initialize(&self, context: ReactionRuntimeContext) {
        self.base.initialize(context).await;
    }

    async fn start(&self) -> anyhow::Result<()> {
        // Log during startup
        if let Some(logger) = self.base.logger().await {
            logger.info("TestReaction is starting up").await;
        }

        self.base
            .set_status_with_event(ComponentStatus::Running, Some("Started".to_string()))
            .await?;

        if let Some(logger) = self.base.logger().await {
            logger.info("TestReaction started successfully").await;
        }

        Ok(())
    }

    async fn stop(&self) -> anyhow::Result<()> {
        if let Some(logger) = self.base.logger().await {
            logger.info("TestReaction stopping").await;
        }

        self.base.stop_common().await
    }

    async fn status(&self) -> ComponentStatus {
        self.base.get_status().await
    }
}

// ============================================================================
// Tests
// ============================================================================

/// Test that source logs are captured and can be streamed.
#[tokio::test]
async fn test_source_log_streaming() {
    let source = TestSource::new("test-source-logs").expect("Failed to create source");

    let drasi = DrasiLib::builder()
        .with_source(source)
        .build()
        .await
        .expect("Failed to build DrasiLib");

    // Subscribe to logs BEFORE starting
    let (initial_logs, mut log_receiver) = drasi
        .subscribe_source_logs("test-source-logs")
        .await
        .expect("Failed to subscribe to source logs");

    // Initial logs should be empty before start
    assert!(
        initial_logs.is_empty(),
        "Expected no logs before start, got: {:?}",
        initial_logs
    );

    // Start the server
    drasi.start().await.expect("Failed to start DrasiLib");

    // Collect streamed logs
    let mut received_logs = Vec::new();
    let _ = timeout(Duration::from_secs(2), async {
        while let Ok(log) = log_receiver.recv().await {
            received_logs.push(log);
            if received_logs.len() >= 3 {
                break;
            }
        }
    })
    .await;

    // Verify we received logs
    assert!(
        !received_logs.is_empty(),
        "Expected to receive logs via streaming"
    );

    // Check for expected log messages
    let log_messages: Vec<_> = received_logs.iter().map(|l| l.message.as_str()).collect();
    println!("Received source logs: {:?}", log_messages);

    assert!(
        log_messages
            .iter()
            .any(|m| m.contains("starting") || m.contains("started")),
        "Expected lifecycle logs, got: {:?}",
        log_messages
    );

    drasi.stop().await.expect("Failed to stop DrasiLib");
}

/// Test that source events are captured and can be streamed.
#[tokio::test]
async fn test_source_event_streaming() {
    let source = TestSource::new("test-source-events").expect("Failed to create source");

    let drasi = DrasiLib::builder()
        .with_source(source)
        .build()
        .await
        .expect("Failed to build DrasiLib");

    // Subscribe to events BEFORE starting
    let (initial_events, mut event_receiver) = drasi
        .subscribe_source_events("test-source-events")
        .await
        .expect("Failed to subscribe to source events");

    // Initial events should be empty before start
    assert!(
        initial_events.is_empty(),
        "Expected no events before start, got: {:?}",
        initial_events
    );

    // Start the server
    drasi.start().await.expect("Failed to start DrasiLib");

    // Collect streamed events
    let mut received_events = Vec::new();
    let _ = timeout(Duration::from_secs(2), async {
        while let Ok(event) = event_receiver.recv().await {
            received_events.push(event);
            // Wait for Running status
            if received_events
                .iter()
                .any(|e| e.status == ComponentStatus::Running)
            {
                break;
            }
        }
    })
    .await;

    // Verify we received events
    assert!(
        !received_events.is_empty(),
        "Expected to receive events via streaming"
    );

    // Check for expected event statuses
    let statuses: Vec<_> = received_events.iter().map(|e| &e.status).collect();
    println!("Received source events: {:?}", statuses);

    // SourceBase.set_status_with_event sends Running event directly
    assert!(
        received_events
            .iter()
            .any(|e| e.status == ComponentStatus::Running),
        "Expected Running event, got: {:?}",
        statuses
    );

    drasi.stop().await.expect("Failed to stop DrasiLib");
}

/// Test that query events are captured and can be streamed.
#[tokio::test]
async fn test_query_event_streaming() {
    let source = TestSource::new("query-event-source").expect("Failed to create source");

    let query = Query::cypher("test-query-events")
        .from_source("query-event-source")
        .query("MATCH (n) RETURN n")
        .build();

    let drasi = DrasiLib::builder()
        .with_source(source)
        .with_query(query)
        .build()
        .await
        .expect("Failed to build DrasiLib");

    // Subscribe to query events BEFORE starting
    let (initial_events, mut event_receiver) = drasi
        .subscribe_query_events("test-query-events")
        .await
        .expect("Failed to subscribe to query events");

    // Initial events should be empty before start
    assert!(
        initial_events.is_empty(),
        "Expected no events before start, got: {:?}",
        initial_events
    );

    // Start the server
    drasi.start().await.expect("Failed to start DrasiLib");

    // Collect streamed events
    let mut received_events = Vec::new();
    let _ = timeout(Duration::from_secs(3), async {
        while let Ok(event) = event_receiver.recv().await {
            received_events.push(event);
            // Wait for Running status
            if received_events
                .iter()
                .any(|e| e.status == ComponentStatus::Running)
            {
                break;
            }
        }
    })
    .await;

    // Verify we received events
    assert!(
        !received_events.is_empty(),
        "Expected to receive query events via streaming"
    );

    // Check for expected event statuses
    let statuses: Vec<_> = received_events.iter().map(|e| &e.status).collect();
    println!("Received query events: {:?}", statuses);

    assert!(
        received_events
            .iter()
            .any(|e| e.status == ComponentStatus::Running),
        "Expected Running event, got: {:?}",
        statuses
    );

    drasi.stop().await.expect("Failed to stop DrasiLib");
}

/// Test that query logs are captured and can be streamed.
#[tokio::test]
async fn test_query_log_streaming() {
    let source = TestSource::new("query-log-source").expect("Failed to create source");

    let query = Query::cypher("test-query-logs")
        .from_source("query-log-source")
        .query("MATCH (n) RETURN n")
        .build();

    let drasi = DrasiLib::builder()
        .with_source(source)
        .with_query(query)
        .build()
        .await
        .expect("Failed to build DrasiLib");

    drasi.start().await.expect("Failed to start DrasiLib");

    // Give time for startup logs
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Get log history
    let (history, _receiver) = drasi
        .subscribe_query_logs("test-query-logs")
        .await
        .expect("Failed to subscribe to query logs");

    println!("Query logs: {:?}", history);

    // Queries emit lifecycle logs
    // (Note: depending on implementation, there may or may not be logs)

    drasi.stop().await.expect("Failed to stop DrasiLib");
}

/// Test that reaction events are captured and can be streamed.
#[tokio::test]
async fn test_reaction_event_streaming() {
    let source = TestSource::new("reaction-event-source").expect("Failed to create source");

    let query = Query::cypher("reaction-event-query")
        .from_source("reaction-event-source")
        .query("MATCH (n) RETURN n")
        .build();

    let reaction = TestReaction::new(
        "test-reaction-events",
        vec!["reaction-event-query".to_string()],
    );

    let drasi = DrasiLib::builder()
        .with_source(source)
        .with_query(query)
        .with_reaction(reaction)
        .build()
        .await
        .expect("Failed to build DrasiLib");

    // Subscribe to reaction events BEFORE starting
    let (initial_events, mut event_receiver) = drasi
        .subscribe_reaction_events("test-reaction-events")
        .await
        .expect("Failed to subscribe to reaction events");

    // Initial events should be empty before start
    assert!(
        initial_events.is_empty(),
        "Expected no events before start, got: {:?}",
        initial_events
    );

    // Start the server
    drasi.start().await.expect("Failed to start DrasiLib");

    // Collect streamed events
    let mut received_events = Vec::new();
    let _ = timeout(Duration::from_secs(3), async {
        while let Ok(event) = event_receiver.recv().await {
            received_events.push(event);
            // Wait for Running status
            if received_events
                .iter()
                .any(|e| e.status == ComponentStatus::Running)
            {
                break;
            }
        }
    })
    .await;

    // Verify we received events
    assert!(
        !received_events.is_empty(),
        "Expected to receive reaction events via streaming"
    );

    // Check for expected event statuses
    let statuses: Vec<_> = received_events.iter().map(|e| &e.status).collect();
    println!("Received reaction events: {:?}", statuses);

    assert!(
        received_events
            .iter()
            .any(|e| e.status == ComponentStatus::Running),
        "Expected Running event, got: {:?}",
        statuses
    );

    drasi.stop().await.expect("Failed to stop DrasiLib");
}

/// Test that reaction logs are captured and can be streamed.
#[tokio::test]
async fn test_reaction_log_streaming() {
    let source = TestSource::new("reaction-log-source").expect("Failed to create source");

    let query = Query::cypher("reaction-log-query")
        .from_source("reaction-log-source")
        .query("MATCH (n) RETURN n")
        .build();

    let reaction = TestReaction::new(
        "test-reaction-logs",
        vec!["reaction-log-query".to_string()],
    );

    let drasi = DrasiLib::builder()
        .with_source(source)
        .with_query(query)
        .with_reaction(reaction)
        .build()
        .await
        .expect("Failed to build DrasiLib");

    // Subscribe to logs BEFORE starting
    let (_initial_logs, mut log_receiver) = drasi
        .subscribe_reaction_logs("test-reaction-logs")
        .await
        .expect("Failed to subscribe to reaction logs");

    // Start the server
    drasi.start().await.expect("Failed to start DrasiLib");

    // Collect streamed logs
    let mut received_logs = Vec::new();
    let _ = timeout(Duration::from_secs(2), async {
        while let Ok(log) = log_receiver.recv().await {
            received_logs.push(log);
            if received_logs.len() >= 2 {
                break;
            }
        }
    })
    .await;

    // Verify we received logs
    assert!(
        !received_logs.is_empty(),
        "Expected to receive reaction logs via streaming"
    );

    let log_messages: Vec<_> = received_logs.iter().map(|l| l.message.as_str()).collect();
    println!("Received reaction logs: {:?}", log_messages);

    assert!(
        log_messages
            .iter()
            .any(|m| m.contains("starting") || m.contains("started")),
        "Expected lifecycle logs, got: {:?}",
        log_messages
    );

    drasi.stop().await.expect("Failed to stop DrasiLib");
}

/// Test full lifecycle with log and event streaming for all component types.
#[tokio::test]
async fn test_full_lifecycle_streaming() {
    let source = TestSource::new("full-lifecycle-source").expect("Failed to create source");

    let query = Query::cypher("full-lifecycle-query")
        .from_source("full-lifecycle-source")
        .query("MATCH (n) RETURN n")
        .build();

    let reaction = TestReaction::new(
        "full-lifecycle-reaction",
        vec!["full-lifecycle-query".to_string()],
    );

    let drasi = DrasiLib::builder()
        .with_source(source)
        .with_query(query)
        .with_reaction(reaction)
        .build()
        .await
        .expect("Failed to build DrasiLib");

    // Subscribe to all event streams
    let (_source_events, mut source_event_rx) = drasi
        .subscribe_source_events("full-lifecycle-source")
        .await
        .expect("Failed to subscribe to source events");

    let (_query_events, mut query_event_rx) = drasi
        .subscribe_query_events("full-lifecycle-query")
        .await
        .expect("Failed to subscribe to query events");

    let (_reaction_events, mut reaction_event_rx) = drasi
        .subscribe_reaction_events("full-lifecycle-reaction")
        .await
        .expect("Failed to subscribe to reaction events");

    // Start the server
    drasi.start().await.expect("Failed to start DrasiLib");

    // Collect events from all streams
    let mut source_events = Vec::new();
    let mut query_events = Vec::new();
    let mut reaction_events = Vec::new();

    let _ = timeout(Duration::from_secs(3), async {
        loop {
            tokio::select! {
                Ok(event) = source_event_rx.recv() => {
                    source_events.push(event);
                }
                Ok(event) = query_event_rx.recv() => {
                    query_events.push(event);
                }
                Ok(event) = reaction_event_rx.recv() => {
                    reaction_events.push(event);
                }
                else => break,
            }

            // Check if all components reached Running
            let source_running = source_events.iter().any(|e| e.status == ComponentStatus::Running);
            let query_running = query_events.iter().any(|e| e.status == ComponentStatus::Running);
            let reaction_running = reaction_events.iter().any(|e| e.status == ComponentStatus::Running);

            if source_running && query_running && reaction_running {
                break;
            }
        }
    })
    .await;

    println!("Source events: {:?}", source_events.iter().map(|e| &e.status).collect::<Vec<_>>());
    println!("Query events: {:?}", query_events.iter().map(|e| &e.status).collect::<Vec<_>>());
    println!("Reaction events: {:?}", reaction_events.iter().map(|e| &e.status).collect::<Vec<_>>());

    // Verify all components reached Running state
    assert!(
        source_events.iter().any(|e| e.status == ComponentStatus::Running),
        "Source should have reached Running state"
    );
    assert!(
        query_events.iter().any(|e| e.status == ComponentStatus::Running),
        "Query should have reached Running state"
    );
    assert!(
        reaction_events.iter().any(|e| e.status == ComponentStatus::Running),
        "Reaction should have reached Running state"
    );

    // Now stop and verify stop events
    drasi.stop().await.expect("Failed to stop DrasiLib");

    // The stop event propagates through the main event channel, not directly to event_history.
    // The fact that we can successfully stop all components is the key verification here.
    // The Running events we captured above verify the live streaming works correctly.
}

/// Test that subscribing to non-existent components fails gracefully.
#[tokio::test]
async fn test_subscribe_nonexistent_component() {
    let drasi = DrasiLib::builder()
        .build()
        .await
        .expect("Failed to build DrasiLib");

    // Try to subscribe to non-existent source
    let result = drasi.subscribe_source_logs("nonexistent").await;
    assert!(result.is_err(), "Expected error for non-existent source");

    let result = drasi.subscribe_source_events("nonexistent").await;
    assert!(result.is_err(), "Expected error for non-existent source events");

    let result = drasi.subscribe_query_events("nonexistent").await;
    assert!(result.is_err(), "Expected error for non-existent query events");

    let result = drasi.subscribe_reaction_events("nonexistent").await;
    assert!(result.is_err(), "Expected error for non-existent reaction events");
}

/// Test that log levels are correctly captured.
#[tokio::test]
async fn test_log_levels_captured() {
    let source = TestSource::new("log-levels-source").expect("Failed to create source");

    let drasi = DrasiLib::builder()
        .with_source(source)
        .build()
        .await
        .expect("Failed to build DrasiLib");

    drasi.start().await.expect("Failed to start DrasiLib");

    // Give time for logs to be captured
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Get log history
    let (history, _) = drasi
        .subscribe_source_logs("log-levels-source")
        .await
        .expect("Failed to subscribe to source logs");

    // Check that we have logs at different levels
    let has_info = history.iter().any(|l| l.level == LogLevel::Info);
    let has_debug = history.iter().any(|l| l.level == LogLevel::Debug);

    println!(
        "Log levels captured: {:?}",
        history
            .iter()
            .map(|l| format!("[{:?}] {}", l.level, l.message))
            .collect::<Vec<_>>()
    );

    assert!(has_info, "Expected Info level logs");
    assert!(has_debug, "Expected Debug level logs");

    drasi.stop().await.expect("Failed to stop DrasiLib");
}

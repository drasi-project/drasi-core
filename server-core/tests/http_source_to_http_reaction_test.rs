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

//! # HttpSource to HttpReaction Integration Test
//!
//! This test validates that data flows correctly from HttpSource through
//! a simple Cypher query to HttpReaction.
//!
//! ## Test Flow
//! 1. Start mock webhook server to receive HTTP calls
//! 2. Create DrasiServerCore with HttpSource, Query, HttpReaction
//! 3. POST 3 CloudEvents to HttpSource
//! 4. Verify webhook server received 3 HTTP POST requests
//! 5. Validate webhook payloads contain correct data

use anyhow::Result;
use axum::{extract::State, routing::post, Json, Router};
use drasi_server_core::{DrasiServerCore, Properties, Query, Reaction, Source};
use reqwest::Client;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use tokio::time::{sleep, Duration};

/// Shared state for capturing HTTP webhooks
#[derive(Clone)]
struct MockWebhookState {
    events: Arc<Mutex<Vec<Value>>>,
}

impl MockWebhookState {
    fn new() -> Self {
        Self {
            events: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn add_event(&self, body: Value) {
        self.events.lock().unwrap().push(body);
    }

    fn get_events(&self) -> Vec<Value> {
        self.events.lock().unwrap().clone()
    }
}

/// Mock webhook handler
async fn handle_webhook(
    State(state): State<MockWebhookState>,
    req: axum::http::Request<axum::body::Body>,
) -> Json<Value> {
    let (_parts, body) = req.into_parts();
    let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
    let body_value: Value = serde_json::from_slice(&bytes).unwrap_or(json!({}));

    state.add_event(body_value);

    Json(json!({"success": true}))
}

/// Start mock webhook server
async fn start_mock_webhook_server(state: MockWebhookState, port: u16) -> Result<()> {
    let app = Router::new()
        .route("/webhook", post(handle_webhook))
        .with_state(state);

    let addr = format!("127.0.0.1:{}", port);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    sleep(Duration::from_millis(200)).await;
    Ok(())
}

#[tokio::test]
async fn test_http_source_to_http_reaction() -> Result<()> {
    // ============================================================================
    // SETUP PHASE: Start mock webhook server
    // ============================================================================

    const SOURCE_PORT: u16 = 8200;
    const WEBHOOK_PORT: u16 = 9200;

    let webhook_state = MockWebhookState::new();
    start_mock_webhook_server(webhook_state.clone(), WEBHOOK_PORT).await?;

    // ============================================================================
    // SETUP PHASE: Create DrasiServerCore with source, query, reaction
    // ============================================================================

    let core = DrasiServerCore::builder()
        .with_id("test-http-to-http")
        .add_source(
            Source::http("http-source")
                .with_properties(
                    Properties::new()
                        .with_string("host", "127.0.0.1")
                        .with_int("port", SOURCE_PORT as i64)
                        .with_int("timeout_ms", 30000),
                )
                .auto_start(true)
                .build(),
        )
        .add_query(
            Query::cypher("event-query")
                .query("MATCH (n:Event) RETURN n.id as id, n.type as type")
                .from_source("http-source")
                .auto_start(true)
                .build()
        )
        .add_reaction(
            Reaction::http("http-reaction")
                .subscribe_to("event-query")
                .auto_start(true)
                .with_properties(
                    Properties::new()
                        .with_string("base_url", &format!("http://127.0.0.1:{}", WEBHOOK_PORT))
                        .with_int("timeout_ms", 10000)
                        .with_value("routes", json!({
                            "event-query": {
                                "added": {
                                    "url": "/webhook",
                                    "method": "POST",
                                    "body": r#"{"id": "{{after.id}}", "type": "{{after.type}}"}"#
                                }
                            }
                        }))
                )
                .build(),
        )
        .build()
        .await?;

    core.start().await?;

    // Give components time to initialize
    sleep(Duration::from_millis(1000)).await;

    // ============================================================================
    // DATA INJECTION PHASE: Send CloudEvents to HttpSource
    // ============================================================================

    let client = Client::new();
    let source_url = format!(
        "http://127.0.0.1:{}/sources/http-source/events",
        SOURCE_PORT
    );

    // Send event 1
    client
        .post(&source_url)
        .json(&json!({
            "operation": "insert",
            "element": {
                "type": "node",
                "id": "event_1",
                "labels": ["Event"],
                "properties": {
                    "id": "e1",
                    "type": "click"
                }
            },
            "timestamp": 1234567890000i64
        }))
        .send()
        .await?;

    // Send event 2
    client
        .post(&source_url)
        .json(&json!({
            "operation": "insert",
            "element": {
                "type": "node",
                "id": "event_2",
                "labels": ["Event"],
                "properties": {
                    "id": "e2",
                    "type": "view"
                }
            },
            "timestamp": 1234567891000i64
        }))
        .send()
        .await?;

    // Send event 3
    client
        .post(&source_url)
        .json(&json!({
            "operation": "insert",
            "element": {
                "type": "node",
                "id": "event_3",
                "labels": ["Event"],
                "properties": {
                    "id": "e3",
                    "type": "submit"
                }
            },
            "timestamp": 1234567892000i64
        }))
        .send()
        .await?;

    // ============================================================================
    // VERIFICATION PHASE: Wait for webhooks and validate
    // ============================================================================

    // Sleep to allow processing through query and HTTP reaction
    sleep(Duration::from_secs(3)).await;

    let events = webhook_state.get_events();
    assert_eq!(events.len(), 3, "Should receive exactly 3 webhook calls");

    // Validate webhook payloads
    let mut found_ids = std::collections::HashSet::new();
    for event in events.iter() {
        let id = event
            .get("id")
            .and_then(|v| v.as_str())
            .expect("Event should have id field");
        let event_type = event
            .get("type")
            .and_then(|v| v.as_str())
            .expect("Event should have type field");

        found_ids.insert(id.to_string());

        // Validate id-type pairs match what we inserted
        match id {
            "e1" => assert_eq!(event_type, "click"),
            "e2" => assert_eq!(event_type, "view"),
            "e3" => assert_eq!(event_type, "submit"),
            _ => panic!("Unexpected id: {}", id),
        }
    }

    assert_eq!(found_ids.len(), 3, "Should have 3 unique event IDs");

    // ============================================================================
    // CLEANUP PHASE
    // ============================================================================

    core.stop().await?;
    Ok(())
}

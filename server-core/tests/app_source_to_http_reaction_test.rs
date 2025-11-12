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

//! # ApplicationSource to HttpReaction Integration Test
//!
//! This test validates that data flows correctly from ApplicationSource through
//! a simple Cypher query to HttpReaction.
//!
//! ## Test Flow
//! 1. Start mock webhook server to receive HTTP calls
//! 2. Create DrasiServerCore with ApplicationSource, Query, HttpReaction
//! 3. Inject 3 Product nodes via ApplicationSource
//! 4. Verify webhook server received 3 HTTP POST requests
//! 5. Validate webhook payloads contain correct data

use anyhow::Result;
use axum::{
    extract::State,
    routing::post,
    Json, Router,
};
use drasi_core::models::ElementPropertyMap;
use drasi_server_core::{DrasiServerCore, Properties, PropertyMapBuilder, Query, Reaction, Source};
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

/// Helper function to create property map for Product nodes
fn create_product_props(id: &str, price: i64) -> ElementPropertyMap {
    PropertyMapBuilder::new()
        .with_string("id", id)
        .with_integer("price", price)
        .build()
}

#[tokio::test]
async fn test_application_source_to_http_reaction() -> Result<()> {
    // ============================================================================
    // SETUP PHASE: Start mock webhook server
    // ============================================================================

    const WEBHOOK_PORT: u16 = 9100;
    let webhook_state = MockWebhookState::new();
    start_mock_webhook_server(webhook_state.clone(), WEBHOOK_PORT).await?;

    // ============================================================================
    // SETUP PHASE: Create DrasiServerCore with source, query, reaction
    // ============================================================================

    let core = DrasiServerCore::builder()
        .with_id("test-app-to-http")
        .add_source(Source::application("product-source").auto_start(true).build())
        .add_query(
            Query::cypher("product-query")
                .query("MATCH (n:Product) RETURN n.id as id, n.price as price")
                .from_source("product-source")
                .auto_start(true)
                .build()
        )
        .add_reaction(
            Reaction::http("product-reaction")
                .subscribe_to("product-query")
                .auto_start(true)
                .with_properties(
                    Properties::new()
                        .with_string("base_url", &format!("http://127.0.0.1:{}", WEBHOOK_PORT))
                        .with_int("timeout_ms", 10000)
                        .with_value("routes", json!({
                            "product-query": {
                                "added": {
                                    "url": "/webhook",
                                    "method": "POST",
                                    "body": r#"{"id": "{{after.id}}", "price": {{after.price}}}"#
                                }
                            }
                        }))
                )
                .build(),
        )
        .build()
        .await?;

    core.start().await?;
    let core = Arc::new(core);

    // Give reactions time to start
    sleep(Duration::from_millis(200)).await;

    // ============================================================================
    // DATA INJECTION PHASE: Push test data through source
    // ============================================================================

    let source_handle = core.source_handle("product-source").await?;

    source_handle
        .send_node_insert("product-1", vec!["Product"], create_product_props("p1", 100))
        .await?;

    source_handle
        .send_node_insert("product-2", vec!["Product"], create_product_props("p2", 200))
        .await?;

    source_handle
        .send_node_insert("product-3", vec!["Product"], create_product_props("p3", 300))
        .await?;

    // ============================================================================
    // VERIFICATION PHASE: Wait for webhooks and validate
    // ============================================================================

    // Sleep to allow HTTP requests to complete
    sleep(Duration::from_secs(2)).await;

    let events = webhook_state.get_events();
    assert_eq!(events.len(), 3, "Should receive exactly 3 webhook calls");

    // Validate webhook payloads
    let mut found_ids = std::collections::HashSet::new();
    for event in events.iter() {
        let id = event.get("id").and_then(|v| v.as_str()).expect("Event should have id field");
        let price = event.get("price").and_then(|v| v.as_i64()).expect("Event should have price field");

        found_ids.insert(id.to_string());

        // Validate id-price pairs match what we inserted
        match id {
            "p1" => assert_eq!(price, 100),
            "p2" => assert_eq!(price, 200),
            "p3" => assert_eq!(price, 300),
            _ => panic!("Unexpected id: {}", id),
        }
    }

    assert_eq!(found_ids.len(), 3, "Should have 3 unique product IDs");

    // ============================================================================
    // CLEANUP PHASE
    // ============================================================================

    core.stop().await?;
    Ok(())
}

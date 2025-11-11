// Simple test to verify HTTP reaction works with ApplicationSource

use anyhow::Result;
use axum::{extract::State, routing::post, Json, Router};
use drasi_server_core::{DrasiServerCore, PropertyMapBuilder, Query, Reaction, Source, Properties};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use tokio::time::{sleep, Duration};

#[derive(Clone)]
struct WebhookState {
    events: Arc<Mutex<Vec<Value>>>,
}

impl WebhookState {
    fn new() -> Self {
        Self {
            events: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn get_events(&self) -> Vec<Value> {
        self.events.lock().unwrap().clone()
    }

    fn count(&self) -> usize {
        self.events.lock().unwrap().len()
    }
}

async fn handle_webhook(
    State(state): State<WebhookState>,
    Json(body): Json<Value>,
) -> Json<Value> {
    println!("Webhook received: {:?}", body);
    state.events.lock().unwrap().push(body);
    Json(json!({"success": true}))
}

#[tokio::test]
async fn test_application_source_to_http_reaction() -> Result<()> {
    // Start mock webhook server
    let webhook_state = WebhookState::new();
    let app = Router::new()
        .route("/webhook", post(handle_webhook))
        .with_state(webhook_state.clone());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:9100").await?;
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    sleep(Duration::from_millis(200)).await;

    // Create DrasiServerCore with ApplicationSource and HTTP reaction
    let core = DrasiServerCore::builder()
        .with_id("test-app-to-http")
        .add_source(Source::application("app-source").auto_start(true).build())
        .add_query(
            Query::cypher("test-query")
                .query("MATCH (n:TestNode) RETURN n.id AS id, n.value AS value")
                .from_source("app-source")
                .auto_start(true)
                .build(),
        )
        .add_reaction(
            Reaction::http("http-reaction")
                .subscribe_to("test-query")
                .with_properties(
                    Properties::new()
                        .with_string("base_url", "http://127.0.0.1:9100")
                        .with_int("timeout_ms", 10000)
                        .with_value(
                            "routes",
                            json!({
                                "test-query": {
                                    "added": {
                                        "url": "/webhook",
                                        "method": "POST",
                                        "body": r#"{"id": "{{after.id}}", "value": "{{after.value}}"}"#
                                    }
                                }
                            }),
                        ),
                )
                .build(),
        )
        .build()
        .await?;

    core.start().await?;

    // Wrap in Arc for sharing
    let core = Arc::new(core);

    sleep(Duration::from_millis(1000)).await;

    println!("=== System started, sending data ===");

    // Get source handle and send data
    let source_handle = core.source_handle("app-source").await?;
    source_handle
        .send_node_insert(
            "node-1",
            vec!["TestNode"],
            PropertyMapBuilder::new()
                .with_string("id", "test-123")
                .with_string("value", "hello-world")
                .build(),
        )
        .await?;

    println!("=== Data sent, waiting for webhook ===");
    sleep(Duration::from_secs(3)).await;

    // Verify webhook received
    let count = webhook_state.count();
    println!("=== Webhooks received: {} ===", count);

    let events = webhook_state.get_events();
    for (i, event) in events.iter().enumerate() {
        println!("Event {}: {:?}", i, event);
    }

    assert!(
        count > 0,
        "HTTP reaction should have sent webhook. Received: {}",
        count
    );

    core.stop().await?;
    Ok(())
}

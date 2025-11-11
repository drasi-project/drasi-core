use anyhow::Result;
use axum::{
    extract::State,
    routing::{delete, post, put},
    Json, Router,
};
use drasi_server_core::{DrasiServerCore, Properties, Query, Reaction, Source};
use reqwest::Client;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use tokio::time::{sleep, Duration};

/// Shared state for capturing HTTP reaction webhooks
#[derive(Clone)]
struct MockWebhookState {
    events: Arc<Mutex<Vec<ReceivedWebhook>>>,
}

#[derive(Debug, Clone)]
struct ReceivedWebhook {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Option<Value>,
}

impl MockWebhookState {
    fn new() -> Self {
        Self {
            events: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn add_event(&self, method: String, path: String, headers: Vec<(String, String)>, body: Option<Value>) {
        self.events.lock().unwrap().push(ReceivedWebhook {
            method,
            path,
            headers,
            body,
        });
    }

    fn get_events(&self) -> Vec<ReceivedWebhook> {
        self.events.lock().unwrap().clone()
    }

    fn clear(&self) {
        self.events.lock().unwrap().clear();
    }

    fn count(&self) -> usize {
        self.events.lock().unwrap().len()
    }
}

/// Mock HTTP server handler for POST requests
async fn handle_post(
    State(state): State<MockWebhookState>,
    req: axum::http::Request<axum::body::Body>,
) -> Json<Value> {
    let path = req.uri().path().to_string();
    let headers: Vec<(String, String)> = req
        .headers()
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
        .collect();

    let (_parts, body) = req.into_parts();
    let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
    let body_value: Option<Value> = serde_json::from_slice(&bytes).ok();

    state.add_event("POST".to_string(), path, headers, body_value);

    Json(json!({"success": true}))
}

/// Mock HTTP server handler for PUT requests
async fn handle_put(
    State(state): State<MockWebhookState>,
    req: axum::http::Request<axum::body::Body>,
) -> Json<Value> {
    let path = req.uri().path().to_string();
    let headers: Vec<(String, String)> = req
        .headers()
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
        .collect();

    let (_parts, body) = req.into_parts();
    let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
    let body_value: Option<Value> = serde_json::from_slice(&bytes).ok();

    state.add_event("PUT".to_string(), path, headers, body_value);

    Json(json!({"success": true}))
}

/// Mock HTTP server handler for DELETE requests
async fn handle_delete(
    State(state): State<MockWebhookState>,
    req: axum::http::Request<axum::body::Body>,
) -> Json<Value> {
    let path = req.uri().path().to_string();
    let headers: Vec<(String, String)> = req
        .headers()
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
        .collect();

    state.add_event("DELETE".to_string(), path, headers, None);

    Json(json!({"success": true}))
}

/// Start a mock HTTP server to receive reaction webhooks
async fn start_mock_webhook_server(state: MockWebhookState, port: u16) -> Result<()> {
    let app = Router::new()
        .route("/webhook/created", post(handle_post))
        .route("/webhook/updated/:id", put(handle_put))
        .route("/webhook/deleted/:id", delete(handle_delete))
        .with_state(state);

    let addr = format!("127.0.0.1:{}", port);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    // Give the server time to start
    sleep(Duration::from_millis(200)).await;
    Ok(())
}

/// Helper function to create a DrasiServerCore instance for testing
async fn create_test_server_core(source_port: u16, webhook_port: u16, test_id: &str) -> Result<DrasiServerCore> {
    let source_id = format!("http-source-{}", test_id);
    let query_id = format!("user-query-{}", test_id);
    let reaction_id = format!("http-reaction-{}", test_id);
    let core_id = format!("test-http-integration-{}", test_id);

    Ok(DrasiServerCore::builder()
        .with_id(&core_id)
        .add_source(
            Source::http(&source_id)
                .with_properties(
                    Properties::new()
                        .with_string("host", "127.0.0.1")
                        .with_int("port", source_port as i64)
                        .with_int("timeout_ms", 30000),
                )
                .auto_start(true)
                .build(),
        )
        .add_query(
            Query::cypher(&query_id)
                .query("MATCH (n:User) RETURN n.id AS id, n.name AS name, n.email AS email")
                .from_source(&source_id)
                .auto_start(true)
                .build(),
        )
        .add_reaction(
            Reaction::http(&reaction_id)
                .subscribe_to(&query_id)
                .with_properties(
                    Properties::new()
                        .with_string("base_url", &format!("http://127.0.0.1:{}", webhook_port))
                        .with_int("timeout_ms", 10000)
                        .with_value(
                            "routes",
                            json!({
                                query_id: {
                                    "added": {
                                        "url": "/webhook/created",
                                        "method": "POST",
                                        "body": r#"{"id": "{{after.id}}", "name": "{{after.name}}", "email": "{{after.email}}"}"#,
                                        "headers": {
                                            "X-Source": "drasi",
                                            "X-Operation": "add"
                                        }
                                    },
                                    "updated": {
                                        "url": "/webhook/updated/{{after.id}}",
                                        "method": "PUT",
                                        "body": r#"{"id": "{{after.id}}", "name": "{{after.name}}", "email": "{{after.email}}", "previous_name": "{{before.name}}"}"#,
                                        "headers": {
                                            "X-Source": "drasi",
                                            "X-Operation": "update"
                                        }
                                    },
                                    "deleted": {
                                        "url": "/webhook/deleted/{{before.id}}",
                                        "method": "DELETE",
                                        "headers": {
                                            "X-Source": "drasi",
                                            "X-Operation": "delete"
                                        }
                                    }
                                }
                            }),
                        ),
                )
                .auto_start(true)
                .build(),
        )
        .build()
        .await?)
}

#[tokio::test]
async fn test_http_source_single_insert() -> Result<()> {
    const SOURCE_PORT: u16 = 8080;
    const WEBHOOK_PORT: u16 = 9000;

    // Setup mock webhook server
    let webhook_state = MockWebhookState::new();
    start_mock_webhook_server(webhook_state.clone(), WEBHOOK_PORT).await?;

    // Create and start DrasiServerCore
    let core = create_test_server_core(SOURCE_PORT, WEBHOOK_PORT, "single-insert").await?;
    core.start().await?;

    // Give components time to initialize
    sleep(Duration::from_millis(1000)).await;

    // Check component status
    let source_status = core.get_source_status("http-source-single-insert").await?;
    let query_status = core.get_query_status("user-query-single-insert").await?;
    let reaction_status = core.get_reaction_status("http-reaction-single-insert").await?;

    println!("Source status: {:?}", source_status);
    println!("Query status: {:?}", query_status);
    println!("Reaction status: {:?}", reaction_status);

    // Send a single insert event to HTTP source
    let client = Client::new();
    let source_url = format!("http://127.0.0.1:{}/sources/http-source-single-insert/events", SOURCE_PORT);
    let response = client
        .post(&source_url)
        .json(&json!({
            "operation": "insert",
            "element": {
                "type": "node",
                "id": "user_1",
                "labels": ["User"],
                "properties": {
                    "id": "user_1",
                    "name": "Alice",
                    "email": "alice@example.com"
                }
            },
            "timestamp": 1234567890000i64
        }))
        .send()
        .await?;

    let status = response.status();
    let body_text = response.text().await?;

    assert!(
        status.is_success(),
        "HTTP source should accept the event. Status: {}, Body: {}",
        status,
        body_text
    );
    println!("Event sent successfully. Response: {}", body_text);

    // Wait for processing through query and reaction
    println!("Waiting for event processing...");
    sleep(Duration::from_secs(5)).await;
    println!("Wait complete, checking for webhooks...");

    // Verify HTTP reaction received the event
    let events = webhook_state.get_events();
    assert!(
        !events.is_empty(),
        "HTTP reaction should have sent at least one webhook. Events received: {}",
        events.len()
    );

    // Verify the webhook details
    let webhook = &events[0];
    assert_eq!(webhook.method, "POST", "Should use POST method for added");
    assert_eq!(
        webhook.path, "/webhook/created",
        "Should use correct URL path"
    );

    // Verify headers
    assert!(
        webhook.headers.iter().any(|(k, v)| k == "x-source" && v == "drasi"),
        "Should include X-Source header"
    );
    assert!(
        webhook.headers.iter().any(|(k, v)| k == "x-operation" && v == "add"),
        "Should include X-Operation header"
    );

    // Verify body content
    if let Some(body) = &webhook.body {
        assert_eq!(body["id"], "user_1", "Should include user ID");
        assert_eq!(body["name"], "Alice", "Should include user name");
        assert_eq!(
            body["email"], "alice@example.com",
            "Should include user email"
        );
    } else {
        panic!("Webhook should have a body");
    }

    // Cleanup
    core.stop().await?;
    Ok(())
}

#[tokio::test]
async fn test_http_source_update_operation() -> Result<()> {
    const SOURCE_PORT: u16 = 8081;
    const WEBHOOK_PORT: u16 = 9001;

    // Setup mock webhook server
    let webhook_state = MockWebhookState::new();
    start_mock_webhook_server(webhook_state.clone(), WEBHOOK_PORT).await?;

    // Create and start DrasiServerCore
    let core = create_test_server_core(SOURCE_PORT, WEBHOOK_PORT, "update").await?;
    core.start().await?;
    sleep(Duration::from_millis(500)).await;

    let client = Client::new();
    let source_url = format!("http://127.0.0.1:{}/sources/http-source-update/events", SOURCE_PORT);

    // Insert initial user
    client
        .post(&source_url)
        .json(&json!({
            "operation": "insert",
            "element": {
                "type": "node",
                "id": "user_2",
                "labels": ["User"],
                "properties": {
                    "id": "user_2",
                    "name": "Bob",
                    "email": "bob@example.com"
                }
            },
            "timestamp": 1234567890000i64
        }))
        .send()
        .await?;

    sleep(Duration::from_secs(1)).await;
    webhook_state.clear(); // Clear the insert event

    // Update the user
    let response = client
        .post(&source_url)
        .json(&json!({
            "operation": "update",
            "element": {
                "type": "node",
                "id": "user_2",
                "labels": ["User"],
                "properties": {
                    "id": "user_2",
                    "name": "Robert",
                    "email": "robert@example.com"
                }
            },
            "timestamp": 1234567891000i64
        }))
        .send()
        .await?;

    assert!(response.status().is_success());
    sleep(Duration::from_secs(2)).await;

    // Verify update webhook
    let events = webhook_state.get_events();
    assert!(!events.is_empty(), "Should receive update webhook");

    let webhook = &events[0];
    assert_eq!(webhook.method, "PUT", "Should use PUT method for updated");
    assert!(
        webhook.path.contains("/webhook/updated/user_2"),
        "Should include user ID in path"
    );

    if let Some(body) = &webhook.body {
        assert_eq!(body["name"], "Robert", "Should have updated name");
        assert_eq!(body["previous_name"], "Bob", "Should include previous name");
    } else {
        panic!("Update webhook should have a body");
    }

    core.stop().await?;
    Ok(())
}

#[tokio::test]
async fn test_http_source_delete_operation() -> Result<()> {
    const SOURCE_PORT: u16 = 8082;
    const WEBHOOK_PORT: u16 = 9002;

    // Setup mock webhook server
    let webhook_state = MockWebhookState::new();
    start_mock_webhook_server(webhook_state.clone(), WEBHOOK_PORT).await?;

    // Create and start DrasiServerCore
    let core = create_test_server_core(SOURCE_PORT, WEBHOOK_PORT, "delete").await?;
    core.start().await?;
    sleep(Duration::from_millis(500)).await;

    let client = Client::new();
    let source_url = format!("http://127.0.0.1:{}/sources/http-source-delete/events", SOURCE_PORT);

    // Insert a user first
    client
        .post(&source_url)
        .json(&json!({
            "operation": "insert",
            "element": {
                "type": "node",
                "id": "user_3",
                "labels": ["User"],
                "properties": {
                    "id": "user_3",
                    "name": "Charlie",
                    "email": "charlie@example.com"
                }
            },
            "timestamp": 1234567890000i64
        }))
        .send()
        .await?;

    sleep(Duration::from_secs(1)).await;
    webhook_state.clear(); // Clear the insert event

    // Delete the user
    let response = client
        .post(&source_url)
        .json(&json!({
            "operation": "delete",
            "id": "user_3",
            "labels": ["User"],
            "timestamp": 1234567892000i64
        }))
        .send()
        .await?;

    assert!(response.status().is_success());
    sleep(Duration::from_secs(2)).await;

    // Verify delete webhook
    let events = webhook_state.get_events();
    assert!(!events.is_empty(), "Should receive delete webhook");

    let webhook = &events[0];
    assert_eq!(webhook.method, "DELETE", "Should use DELETE method");
    assert!(
        webhook.path.contains("/webhook/deleted/user_3"),
        "Should include user ID in path"
    );

    core.stop().await?;
    Ok(())
}

#[tokio::test]
async fn test_http_source_batch_operations() -> Result<()> {
    const SOURCE_PORT: u16 = 8083;
    const WEBHOOK_PORT: u16 = 9003;

    // Setup mock webhook server
    let webhook_state = MockWebhookState::new();
    start_mock_webhook_server(webhook_state.clone(), WEBHOOK_PORT).await?;

    // Create and start DrasiServerCore
    let core = create_test_server_core(SOURCE_PORT, WEBHOOK_PORT, "batch").await?;
    core.start().await?;
    sleep(Duration::from_millis(500)).await;

    // Send batch of events
    let client = Client::new();
    let source_url = format!("http://127.0.0.1:{}/sources/http-source-batch/events/batch", SOURCE_PORT);
    let response = client
        .post(&source_url)
        .json(&json!({
            "events": [
                {
                    "operation": "insert",
                    "element": {
                        "type": "node",
                        "id": "user_batch_1",
                        "labels": ["User"],
                        "properties": {
                            "id": "user_batch_1",
                            "name": "David",
                            "email": "david@example.com"
                        }
                    },
                    "timestamp": 1234567890000i64
                },
                {
                    "operation": "insert",
                    "element": {
                        "type": "node",
                        "id": "user_batch_2",
                        "labels": ["User"],
                        "properties": {
                            "id": "user_batch_2",
                            "name": "Eve",
                            "email": "eve@example.com"
                        }
                    },
                    "timestamp": 1234567891000i64
                },
                {
                    "operation": "insert",
                    "element": {
                        "type": "node",
                        "id": "user_batch_3",
                        "labels": ["User"],
                        "properties": {
                            "id": "user_batch_3",
                            "name": "Frank",
                            "email": "frank@example.com"
                        }
                    },
                    "timestamp": 1234567892000i64
                }
            ]
        }))
        .send()
        .await?;

    assert!(
        response.status().is_success(),
        "Batch endpoint should accept events"
    );

    // Wait for processing
    sleep(Duration::from_secs(3)).await;

    // Verify we received webhooks for all three users
    let event_count = webhook_state.count();
    assert_eq!(
        event_count, 3,
        "Should receive webhooks for all 3 batch events, got {}",
        event_count
    );

    // Verify all webhooks are for POST (added) operations
    let events = webhook_state.get_events();
    for event in &events {
        assert_eq!(event.method, "POST", "All should be POST for inserts");
    }

    core.stop().await?;
    Ok(())
}

#[tokio::test]
async fn test_http_source_query_filter() -> Result<()> {
    const SOURCE_PORT: u16 = 8084;
    const WEBHOOK_PORT: u16 = 9004;

    // This test verifies that the query properly filters data
    let webhook_state = MockWebhookState::new();
    start_mock_webhook_server(webhook_state.clone(), WEBHOOK_PORT).await?;

    // Create server with a filtered query (only users with specific property)
    let core = DrasiServerCore::builder()
        .with_id("test-filtered-query")
        .add_source(
            Source::http("http-source-filter")
                .with_properties(
                    Properties::new()
                        .with_string("host", "127.0.0.1")
                        .with_int("port", SOURCE_PORT as i64),
                )
                .auto_start(true)
                .build(),
        )
        .add_query(
            Query::cypher("premium-user-query")
                .query("MATCH (n:User) WHERE n.premium = true RETURN n.id AS id, n.name AS name")
                .from_source("http-source-filter")
                .auto_start(true)
                .build(),
        )
        .add_reaction(
            Reaction::http("http-reaction-filter")
                .subscribe_to("premium-user-query")
                .with_properties(
                    Properties::new()
                        .with_string("base_url", &format!("http://127.0.0.1:{}", WEBHOOK_PORT))
                        .with_value(
                            "routes",
                            json!({
                                "premium-user-query": {
                                    "added": {
                                        "url": "/webhook/created",
                                        "method": "POST",
                                        "body": r#"{"id": "{{after.id}}", "name": "{{after.name}}"}"#
                                    }
                                }
                            }),
                        ),
                )
                .auto_start(true)
                .build(),
        )
        .build()
        .await?;

    core.start().await?;
    sleep(Duration::from_millis(500)).await;

    let client = Client::new();
    let source_url = format!("http://127.0.0.1:{}/sources/http-source-filter/events", SOURCE_PORT);

    // Insert non-premium user (should NOT trigger webhook)
    client
        .post(&source_url)
        .json(&json!({
            "operation": "insert",
            "element": {
                "type": "node",
                "id": "user_regular",
                "labels": ["User"],
                "properties": {
                    "id": "user_regular",
                    "name": "Regular User",
                    "premium": false
                }
            }
        }))
        .send()
        .await?;

    sleep(Duration::from_secs(1)).await;

    // Insert premium user (SHOULD trigger webhook)
    client
        .post(&source_url)
        .json(&json!({
            "operation": "insert",
            "element": {
                "type": "node",
                "id": "user_premium",
                "labels": ["User"],
                "properties": {
                    "id": "user_premium",
                    "name": "Premium User",
                    "premium": true
                }
            }
        }))
        .send()
        .await?;

    sleep(Duration::from_secs(2)).await;

    // Should only have received 1 webhook (for premium user)
    let events = webhook_state.get_events();
    assert_eq!(
        events.len(),
        1,
        "Should only receive webhook for premium user"
    );

    if let Some(body) = &events[0].body {
        assert_eq!(body["id"], "user_premium", "Should be premium user");
        assert_eq!(body["name"], "Premium User", "Should have correct name");
    }

    core.stop().await?;
    Ok(())
}

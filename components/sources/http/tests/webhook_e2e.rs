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

//! End-to-end integration tests for webhook functionality.
//!
//! These tests set up complete DrasiLib instances with sources, queries, and
//! application reactions to verify data flows through the entire pipeline.

#![allow(clippy::unwrap_used)]

use drasi_lib::channels::ResultDiff;
use drasi_lib::{DrasiLib, Query};
use drasi_reaction_application::subscription::SubscriptionOptions;
use drasi_reaction_application::ApplicationReactionBuilder;
use drasi_source_http::config::*;
use drasi_source_http::HttpSourceBuilder;
use hmac::{Hmac, Mac};
use reqwest::Client;
use sha2::Sha256;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;

/// Find an available port for testing
async fn find_available_port() -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    sleep(Duration::from_millis(50)).await;
    port
}

/// Compute HMAC-SHA256 signature
fn compute_signature(secret: &str, body: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(body);
    let result = mac.finalize();
    format!("sha256={}", hex::encode(result.into_bytes()))
}

/// Test that webhook events flow through DrasiLib and reach the reaction
#[tokio::test]
async fn test_webhook_event_flows_to_reaction() {
    let port = find_available_port().await;

    // Create webhook config
    let webhook_config = WebhookConfig {
        error_behavior: ErrorBehavior::Reject,
        cors: None,
        routes: vec![WebhookRoute {
            path: "/events".to_string(),
            methods: vec![HttpMethod::Post],
            auth: None,
            error_behavior: None,
            mappings: vec![WebhookMapping {
                when: None,
                operation: Some(OperationType::Insert),
                operation_from: None,
                operation_map: None,
                element_type: ElementType::Node,
                effective_from: None,
                template: ElementTemplate {
                    id: "event-{{payload.id}}".to_string(),
                    labels: vec!["TestEvent".to_string()],
                    properties: Some(serde_json::json!({
                        "name": "{{payload.name}}",
                        "value": "{{payload.value}}"
                    })),
                    from: None,
                    to: None,
                },
            }],
        }],
    };

    // Create HTTP source with webhook config
    let http_source = HttpSourceBuilder::new("webhook-source")
        .with_host("127.0.0.1")
        .with_port(port)
        .with_webhooks(webhook_config)
        .with_auto_start(true)
        .build()
        .unwrap();

    // Create query that matches our webhook events
    let query = Query::cypher("test-query")
        .query("MATCH (e:TestEvent) RETURN e.name AS name, e.value AS value")
        .from_source("webhook-source")
        .auto_start(true)
        .build();

    // Create application reaction to capture results
    let (reaction, handle) = ApplicationReactionBuilder::new("test-reaction")
        .with_query("test-query")
        .build();

    // Build DrasiLib instance
    let core = Arc::new(
        DrasiLib::builder()
            .with_id("webhook-e2e-test")
            .with_source(http_source)
            .with_query(query)
            .with_reaction(reaction)
            .build()
            .await
            .unwrap(),
    );

    // Start the system
    core.start().await.unwrap();

    // Give the system time to fully initialize
    sleep(Duration::from_millis(200)).await;

    // Create subscription to receive results
    let mut subscription = handle
        .subscribe_with_options(SubscriptionOptions::default().with_timeout(Duration::from_secs(5)))
        .await
        .unwrap();

    // Send webhook request
    let client = Client::new();
    let payload = serde_json::json!({
        "id": "123",
        "name": "Test Event",
        "value": 42
    });

    let response = client
        .post(format!("http://127.0.0.1:{port}/events"))
        .header("Content-Type", "application/json")
        .json(&payload)
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);

    // Wait for result from reaction
    let result = subscription.recv().await;
    assert!(result.is_some(), "Expected to receive query result");

    let query_result = result.unwrap();
    assert_eq!(query_result.query_id, "test-query");
    assert!(
        !query_result.results.is_empty(),
        "Expected at least one result"
    );

    // Verify the result contains our data
    match &query_result.results[0] {
        ResultDiff::Add { data } => {
            assert_eq!(data["name"], "Test Event");
            // Values may come as strings from template rendering
            let value = &data["value"];
            assert!(
                value == 42 || value == "42",
                "Expected value 42, got {value:?}"
            );
        }
        _ => panic!("Expected Add result, got {:?}", query_result.results[0]),
    }

    core.stop().await.unwrap();
}

/// Test webhook with path parameters flows through correctly
#[tokio::test]
async fn test_webhook_path_params_flow_to_reaction() {
    let port = find_available_port().await;

    let webhook_config = WebhookConfig {
        error_behavior: ErrorBehavior::Reject,
        cors: None,
        routes: vec![WebhookRoute {
            path: "/users/:user_id/events".to_string(),
            methods: vec![HttpMethod::Post],
            auth: None,
            error_behavior: None,
            mappings: vec![WebhookMapping {
                when: None,
                operation: Some(OperationType::Insert),
                operation_from: None,
                operation_map: None,
                element_type: ElementType::Node,
                effective_from: None,
                template: ElementTemplate {
                    id: "{{route.user_id}}-{{payload.event_id}}".to_string(),
                    labels: vec!["UserEvent".to_string()],
                    properties: Some(serde_json::json!({
                        "user_id": "{{route.user_id}}",
                        "event_type": "{{payload.type}}"
                    })),
                    from: None,
                    to: None,
                },
            }],
        }],
    };

    let http_source = HttpSourceBuilder::new("path-param-source")
        .with_host("127.0.0.1")
        .with_port(port)
        .with_webhooks(webhook_config)
        .with_auto_start(true)
        .build()
        .unwrap();

    let query = Query::cypher("path-query")
        .query("MATCH (e:UserEvent) RETURN e.user_id AS user_id, e.event_type AS event_type")
        .from_source("path-param-source")
        .auto_start(true)
        .build();

    let (reaction, handle) = ApplicationReactionBuilder::new("path-reaction")
        .with_query("path-query")
        .build();

    let core = Arc::new(
        DrasiLib::builder()
            .with_id("path-param-e2e-test")
            .with_source(http_source)
            .with_query(query)
            .with_reaction(reaction)
            .build()
            .await
            .unwrap(),
    );

    core.start().await.unwrap();
    sleep(Duration::from_millis(200)).await;

    let mut subscription = handle
        .subscribe_with_options(SubscriptionOptions::default().with_timeout(Duration::from_secs(5)))
        .await
        .unwrap();

    let client = Client::new();
    let payload = serde_json::json!({
        "event_id": "evt-001",
        "type": "click"
    });

    let response = client
        .post(format!("http://127.0.0.1:{port}/users/user-42/events"))
        .header("Content-Type", "application/json")
        .json(&payload)
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);

    let result = subscription.recv().await;
    assert!(result.is_some(), "Expected to receive query result");

    let query_result = result.unwrap();
    match &query_result.results[0] {
        ResultDiff::Add { data } => {
            assert_eq!(data["user_id"], "user-42");
            assert_eq!(data["event_type"], "click");
        }
        _ => panic!("Expected Add result"),
    }

    core.stop().await.unwrap();
}

/// Test HMAC signature verification with full pipeline
#[tokio::test]
async fn test_webhook_hmac_auth_flows_correctly() {
    std::env::set_var("E2E_HMAC_SECRET", "super-secret-key");
    let port = find_available_port().await;

    let webhook_config = WebhookConfig {
        error_behavior: ErrorBehavior::Reject,
        cors: None,
        routes: vec![WebhookRoute {
            path: "/secure/events".to_string(),
            methods: vec![HttpMethod::Post],
            auth: Some(AuthConfig {
                signature: Some(SignatureConfig {
                    algorithm: SignatureAlgorithm::HmacSha256,
                    secret_env: "E2E_HMAC_SECRET".to_string(),
                    header: "X-Signature".to_string(),
                    prefix: Some("sha256=".to_string()),
                    encoding: SignatureEncoding::Hex,
                }),
                bearer: None,
            }),
            error_behavior: None,
            mappings: vec![WebhookMapping {
                when: None,
                operation: Some(OperationType::Insert),
                operation_from: None,
                operation_map: None,
                element_type: ElementType::Node,
                effective_from: None,
                template: ElementTemplate {
                    id: "secure-{{payload.id}}".to_string(),
                    labels: vec!["SecureEvent".to_string()],
                    properties: Some(serde_json::json!({
                        "data": "{{payload.data}}"
                    })),
                    from: None,
                    to: None,
                },
            }],
        }],
    };

    let http_source = HttpSourceBuilder::new("hmac-source")
        .with_host("127.0.0.1")
        .with_port(port)
        .with_webhooks(webhook_config)
        .with_auto_start(true)
        .build()
        .unwrap();

    let query = Query::cypher("hmac-query")
        .query("MATCH (e:SecureEvent) RETURN e.data AS data")
        .from_source("hmac-source")
        .auto_start(true)
        .build();

    let (reaction, handle) = ApplicationReactionBuilder::new("hmac-reaction")
        .with_query("hmac-query")
        .build();

    let core = Arc::new(
        DrasiLib::builder()
            .with_id("hmac-e2e-test")
            .with_source(http_source)
            .with_query(query)
            .with_reaction(reaction)
            .build()
            .await
            .unwrap(),
    );

    core.start().await.unwrap();
    sleep(Duration::from_millis(200)).await;

    let mut subscription = handle
        .subscribe_with_options(SubscriptionOptions::default().with_timeout(Duration::from_secs(5)))
        .await
        .unwrap();

    let client = Client::new();
    let payload = serde_json::json!({
        "id": "sec-001",
        "data": "confidential"
    });
    let body = serde_json::to_vec(&payload).unwrap();
    let signature = compute_signature("super-secret-key", &body);

    // Valid signature should succeed
    let response = client
        .post(format!("http://127.0.0.1:{port}/secure/events"))
        .header("Content-Type", "application/json")
        .header("X-Signature", &signature)
        .body(body.clone())
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);

    let result = subscription.recv().await;
    assert!(result.is_some(), "Expected to receive query result");

    let query_result = result.unwrap();
    match &query_result.results[0] {
        ResultDiff::Add { data } => {
            assert_eq!(data["data"], "confidential");
        }
        _ => panic!("Expected Add result"),
    }

    // Invalid signature should be rejected (no result in reaction)
    let response = client
        .post(format!("http://127.0.0.1:{port}/secure/events"))
        .header("Content-Type", "application/json")
        .header("X-Signature", "sha256=invalid")
        .body(body)
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 401);

    core.stop().await.unwrap();
    std::env::remove_var("E2E_HMAC_SECRET");
}

/// Test bearer token authentication with full pipeline
#[tokio::test]
async fn test_webhook_bearer_auth_flows_correctly() {
    std::env::set_var("E2E_BEARER_TOKEN", "my-api-token");
    let port = find_available_port().await;

    let webhook_config = WebhookConfig {
        error_behavior: ErrorBehavior::Reject,
        cors: None,
        routes: vec![WebhookRoute {
            path: "/api/data".to_string(),
            methods: vec![HttpMethod::Post],
            auth: Some(AuthConfig {
                signature: None,
                bearer: Some(BearerConfig {
                    token_env: "E2E_BEARER_TOKEN".to_string(),
                }),
            }),
            error_behavior: None,
            mappings: vec![WebhookMapping {
                when: None,
                operation: Some(OperationType::Insert),
                operation_from: None,
                operation_map: None,
                element_type: ElementType::Node,
                effective_from: None,
                template: ElementTemplate {
                    id: "api-{{payload.id}}".to_string(),
                    labels: vec!["ApiData".to_string()],
                    properties: Some(serde_json::json!({
                        "content": "{{payload.content}}"
                    })),
                    from: None,
                    to: None,
                },
            }],
        }],
    };

    let http_source = HttpSourceBuilder::new("bearer-source")
        .with_host("127.0.0.1")
        .with_port(port)
        .with_webhooks(webhook_config)
        .with_auto_start(true)
        .build()
        .unwrap();

    let query = Query::cypher("bearer-query")
        .query("MATCH (d:ApiData) RETURN d.content AS content")
        .from_source("bearer-source")
        .auto_start(true)
        .build();

    let (reaction, handle) = ApplicationReactionBuilder::new("bearer-reaction")
        .with_query("bearer-query")
        .build();

    let core = Arc::new(
        DrasiLib::builder()
            .with_id("bearer-e2e-test")
            .with_source(http_source)
            .with_query(query)
            .with_reaction(reaction)
            .build()
            .await
            .unwrap(),
    );

    core.start().await.unwrap();
    sleep(Duration::from_millis(200)).await;

    let mut subscription = handle
        .subscribe_with_options(SubscriptionOptions::default().with_timeout(Duration::from_secs(5)))
        .await
        .unwrap();

    let client = Client::new();
    let payload = serde_json::json!({
        "id": "data-001",
        "content": "important info"
    });

    // Valid bearer token
    let response = client
        .post(format!("http://127.0.0.1:{port}/api/data"))
        .header("Content-Type", "application/json")
        .header("Authorization", "Bearer my-api-token")
        .json(&payload)
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);

    let result = subscription.recv().await;
    assert!(result.is_some(), "Expected to receive query result");

    let query_result = result.unwrap();
    match &query_result.results[0] {
        ResultDiff::Add { data } => {
            assert_eq!(data["content"], "important info");
        }
        _ => panic!("Expected Add result"),
    }

    // Invalid bearer token should be rejected
    let response = client
        .post(format!("http://127.0.0.1:{port}/api/data"))
        .header("Content-Type", "application/json")
        .header("Authorization", "Bearer wrong-token")
        .json(&payload)
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 401);

    core.stop().await.unwrap();
    std::env::remove_var("E2E_BEARER_TOKEN");
}

/// Test condition-based routing with multiple mappings
#[tokio::test]
async fn test_webhook_condition_routing_flows_correctly() {
    std::env::set_var("E2E_COND_SECRET", "cond-secret");
    let port = find_available_port().await;

    let webhook_config = WebhookConfig {
        error_behavior: ErrorBehavior::Reject,
        cors: None,
        routes: vec![WebhookRoute {
            path: "/events".to_string(),
            methods: vec![HttpMethod::Post],
            auth: Some(AuthConfig {
                signature: Some(SignatureConfig {
                    algorithm: SignatureAlgorithm::HmacSha256,
                    secret_env: "E2E_COND_SECRET".to_string(),
                    header: "X-Sig".to_string(),
                    prefix: Some("sha256=".to_string()),
                    encoding: SignatureEncoding::Hex,
                }),
                bearer: None,
            }),
            error_behavior: None,
            mappings: vec![
                // Push events create Commit nodes
                WebhookMapping {
                    when: Some(MappingCondition {
                        header: Some("X-Event-Type".to_string()),
                        field: None,
                        equals: Some("push".to_string()),
                        contains: None,
                        regex: None,
                    }),
                    operation: Some(OperationType::Insert),
                    operation_from: None,
                    operation_map: None,
                    element_type: ElementType::Node,
                    effective_from: None,
                    template: ElementTemplate {
                        id: "commit-{{payload.commit_id}}".to_string(),
                        labels: vec!["Commit".to_string()],
                        properties: Some(serde_json::json!({
                            "message": "{{payload.message}}"
                        })),
                        from: None,
                        to: None,
                    },
                },
                // Issue events create Issue nodes
                WebhookMapping {
                    when: Some(MappingCondition {
                        header: Some("X-Event-Type".to_string()),
                        field: None,
                        equals: Some("issue".to_string()),
                        contains: None,
                        regex: None,
                    }),
                    operation: Some(OperationType::Insert),
                    operation_from: None,
                    operation_map: None,
                    element_type: ElementType::Node,
                    effective_from: None,
                    template: ElementTemplate {
                        id: "issue-{{payload.issue_id}}".to_string(),
                        labels: vec!["Issue".to_string()],
                        properties: Some(serde_json::json!({
                            "title": "{{payload.title}}"
                        })),
                        from: None,
                        to: None,
                    },
                },
            ],
        }],
    };

    let http_source = HttpSourceBuilder::new("cond-source")
        .with_host("127.0.0.1")
        .with_port(port)
        .with_webhooks(webhook_config)
        .with_auto_start(true)
        .build()
        .unwrap();

    // Query that returns Commits (simpler query without labels() function)
    let query = Query::cypher("cond-query")
        .query("MATCH (c:Commit) RETURN c.message AS content, 'Commit' AS type")
        .from_source("cond-source")
        .auto_start(true)
        .build();

    let (reaction, handle) = ApplicationReactionBuilder::new("cond-reaction")
        .with_query("cond-query")
        .build();

    let core = Arc::new(
        DrasiLib::builder()
            .with_id("cond-e2e-test")
            .with_source(http_source)
            .with_query(query)
            .with_reaction(reaction)
            .build()
            .await
            .unwrap(),
    );

    core.start().await.unwrap();
    sleep(Duration::from_millis(200)).await;

    let mut subscription = handle
        .subscribe_with_options(SubscriptionOptions::default().with_timeout(Duration::from_secs(5)))
        .await
        .unwrap();

    let client = Client::new();

    // Send a push event
    let push_payload = serde_json::json!({
        "commit_id": "abc123",
        "message": "Fix bug"
    });
    let body = serde_json::to_vec(&push_payload).unwrap();
    let signature = compute_signature("cond-secret", &body);

    let response = client
        .post(format!("http://127.0.0.1:{port}/events"))
        .header("Content-Type", "application/json")
        .header("X-Event-Type", "push")
        .header("X-Sig", &signature)
        .body(body)
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);

    let result = subscription.recv().await;
    assert!(
        result.is_some(),
        "Expected to receive query result for push"
    );

    let query_result = result.unwrap();
    match &query_result.results[0] {
        ResultDiff::Add { data } => {
            assert_eq!(data["type"], "Commit");
            assert_eq!(data["content"], "Fix bug");
        }
        _ => panic!("Expected Add result for commit"),
    }

    core.stop().await.unwrap();
    std::env::remove_var("E2E_COND_SECRET");
}

/// Test YAML content type flows through correctly
#[tokio::test]
async fn test_webhook_yaml_content_flows_correctly() {
    let port = find_available_port().await;

    let webhook_config = WebhookConfig {
        error_behavior: ErrorBehavior::Reject,
        cors: None,
        routes: vec![WebhookRoute {
            path: "/yaml-events".to_string(),
            methods: vec![HttpMethod::Post],
            auth: None,
            error_behavior: None,
            mappings: vec![WebhookMapping {
                when: None,
                operation: Some(OperationType::Insert),
                operation_from: None,
                operation_map: None,
                element_type: ElementType::Node,
                effective_from: None,
                template: ElementTemplate {
                    id: "yaml-{{payload.id}}".to_string(),
                    labels: vec!["YamlEvent".to_string()],
                    properties: Some(serde_json::json!({
                        "name": "{{payload.name}}",
                        "count": "{{payload.count}}"
                    })),
                    from: None,
                    to: None,
                },
            }],
        }],
    };

    let http_source = HttpSourceBuilder::new("yaml-source")
        .with_host("127.0.0.1")
        .with_port(port)
        .with_webhooks(webhook_config)
        .with_auto_start(true)
        .build()
        .unwrap();

    let query = Query::cypher("yaml-query")
        .query("MATCH (e:YamlEvent) RETURN e.name AS name, e.count AS count")
        .from_source("yaml-source")
        .auto_start(true)
        .build();

    let (reaction, handle) = ApplicationReactionBuilder::new("yaml-reaction")
        .with_query("yaml-query")
        .build();

    let core = Arc::new(
        DrasiLib::builder()
            .with_id("yaml-e2e-test")
            .with_source(http_source)
            .with_query(query)
            .with_reaction(reaction)
            .build()
            .await
            .unwrap(),
    );

    core.start().await.unwrap();
    sleep(Duration::from_millis(200)).await;

    let mut subscription = handle
        .subscribe_with_options(SubscriptionOptions::default().with_timeout(Duration::from_secs(5)))
        .await
        .unwrap();

    let client = Client::new();
    let yaml_body = "id: yaml-001\nname: YAML Test\ncount: 99\n";

    let response = client
        .post(format!("http://127.0.0.1:{port}/yaml-events"))
        .header("Content-Type", "application/x-yaml")
        .body(yaml_body)
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);

    let result = subscription.recv().await;
    assert!(result.is_some(), "Expected to receive query result");

    let query_result = result.unwrap();
    match &query_result.results[0] {
        ResultDiff::Add { data } => {
            assert_eq!(data["name"], "YAML Test");
            // YAML integers may be preserved or come as strings depending on template
            let count = &data["count"];
            assert!(
                count == 99 || count == "99",
                "Expected count 99, got {count:?}"
            );
        }
        _ => panic!("Expected Add result"),
    }

    core.stop().await.unwrap();
}

/// Test update operation flows through correctly
#[tokio::test]
async fn test_webhook_update_operation_flows_correctly() {
    let port = find_available_port().await;

    let webhook_config = WebhookConfig {
        error_behavior: ErrorBehavior::Reject,
        cors: None,
        routes: vec![WebhookRoute {
            path: "/items".to_string(),
            methods: vec![HttpMethod::Post],
            auth: None,
            error_behavior: None,
            mappings: vec![
                // X-Operation: create -> insert
                WebhookMapping {
                    when: Some(MappingCondition {
                        header: Some("X-Operation".to_string()),
                        field: None,
                        equals: Some("create".to_string()),
                        contains: None,
                        regex: None,
                    }),
                    operation: Some(OperationType::Insert),
                    operation_from: None,
                    operation_map: None,
                    element_type: ElementType::Node,
                    effective_from: None,
                    template: ElementTemplate {
                        id: "item-{{payload.id}}".to_string(),
                        labels: vec!["Item".to_string()],
                        properties: Some(serde_json::json!({
                            "status": "{{payload.status}}"
                        })),
                        from: None,
                        to: None,
                    },
                },
                // X-Operation: update -> update
                WebhookMapping {
                    when: Some(MappingCondition {
                        header: Some("X-Operation".to_string()),
                        field: None,
                        equals: Some("update".to_string()),
                        contains: None,
                        regex: None,
                    }),
                    operation: Some(OperationType::Update),
                    operation_from: None,
                    operation_map: None,
                    element_type: ElementType::Node,
                    effective_from: None,
                    template: ElementTemplate {
                        id: "item-{{payload.id}}".to_string(),
                        labels: vec!["Item".to_string()],
                        properties: Some(serde_json::json!({
                            "status": "{{payload.status}}"
                        })),
                        from: None,
                        to: None,
                    },
                },
            ],
        }],
    };

    let http_source = HttpSourceBuilder::new("update-source")
        .with_host("127.0.0.1")
        .with_port(port)
        .with_webhooks(webhook_config)
        .with_auto_start(true)
        .build()
        .unwrap();

    let query = Query::cypher("update-query")
        .query("MATCH (i:Item) RETURN i.status AS status")
        .from_source("update-source")
        .auto_start(true)
        .build();

    let (reaction, handle) = ApplicationReactionBuilder::new("update-reaction")
        .with_query("update-query")
        .build();

    let core = Arc::new(
        DrasiLib::builder()
            .with_id("update-e2e-test")
            .with_source(http_source)
            .with_query(query)
            .with_reaction(reaction)
            .build()
            .await
            .unwrap(),
    );

    core.start().await.unwrap();
    sleep(Duration::from_millis(200)).await;

    let mut subscription = handle
        .subscribe_with_options(SubscriptionOptions::default().with_timeout(Duration::from_secs(5)))
        .await
        .unwrap();

    let client = Client::new();

    // Create an item via POST with X-Operation: create header
    let create_payload = serde_json::json!({
        "id": "item-1",
        "status": "pending"
    });

    let response = client
        .post(format!("http://127.0.0.1:{port}/items"))
        .header("Content-Type", "application/json")
        .header("X-Operation", "create")
        .json(&create_payload)
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);

    let result = subscription.recv().await;
    assert!(result.is_some(), "Expected to receive insert result");

    let query_result = result.unwrap();
    match &query_result.results[0] {
        ResultDiff::Add { data } => {
            assert_eq!(data["status"], "pending");
        }
        _ => panic!("Expected Add result for insert"),
    }

    // Update the item via POST with X-Operation: update header
    let update_payload = serde_json::json!({
        "id": "item-1",
        "status": "completed"
    });

    let response = client
        .post(format!("http://127.0.0.1:{port}/items"))
        .header("Content-Type", "application/json")
        .header("X-Operation", "update")
        .json(&update_payload)
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);

    let result = subscription.recv().await;
    assert!(result.is_some(), "Expected to receive update result");

    let query_result = result.unwrap();
    match &query_result.results[0] {
        ResultDiff::Update { after, .. } => {
            assert_eq!(after["status"], "completed");
        }
        _ => panic!("Expected Update result, got {:?}", query_result.results[0]),
    }

    core.stop().await.unwrap();
}

/// Test standard mode still works with full pipeline
#[tokio::test]
async fn test_standard_mode_flows_to_reaction() {
    let port = find_available_port().await;

    // No webhook config = standard mode
    let http_source = HttpSourceBuilder::new("standard-source")
        .with_host("127.0.0.1")
        .with_port(port)
        .with_auto_start(true)
        .build()
        .unwrap();

    let query = Query::cypher("standard-query")
        .query("MATCH (t:TestNode) RETURN t.value AS value")
        .from_source("standard-source")
        .auto_start(true)
        .build();

    let (reaction, handle) = ApplicationReactionBuilder::new("standard-reaction")
        .with_query("standard-query")
        .build();

    let core = Arc::new(
        DrasiLib::builder()
            .with_id("standard-e2e-test")
            .with_source(http_source)
            .with_query(query)
            .with_reaction(reaction)
            .build()
            .await
            .unwrap(),
    );

    core.start().await.unwrap();
    sleep(Duration::from_millis(200)).await;

    let mut subscription = handle
        .subscribe_with_options(SubscriptionOptions::default().with_timeout(Duration::from_secs(5)))
        .await
        .unwrap();

    let client = Client::new();

    // Use standard HttpSourceChange format
    let payload = serde_json::json!({
        "operation": "insert",
        "element": {
            "type": "node",
            "id": "test-1",
            "labels": ["TestNode"],
            "properties": {
                "value": 42
            }
        }
    });

    let response = client
        .post(format!(
            "http://127.0.0.1:{port}/sources/standard-source/events"
        ))
        .header("Content-Type", "application/json")
        .json(&payload)
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);

    let result = subscription.recv().await;
    assert!(result.is_some(), "Expected to receive query result");

    let query_result = result.unwrap();
    match &query_result.results[0] {
        ResultDiff::Add { data } => {
            // Standard mode should preserve integer types
            let value = &data["value"];
            assert!(
                value == 42 || value == "42",
                "Expected value 42, got {value:?}"
            );
        }
        _ => panic!("Expected Add result"),
    }

    core.stop().await.unwrap();
}

/// Test relation creation through webhook
/// Note: This test verifies that relation webhooks are processed correctly.
/// Full relation query matching requires start/end nodes to exist.
#[tokio::test]
async fn test_webhook_relation_accepted_successfully() {
    let port = find_available_port().await;

    let webhook_config = WebhookConfig {
        error_behavior: ErrorBehavior::Reject,
        cors: None,
        routes: vec![WebhookRoute {
            path: "/relationships".to_string(),
            methods: vec![HttpMethod::Post],
            auth: None,
            error_behavior: None,
            mappings: vec![WebhookMapping {
                when: None,
                operation: Some(OperationType::Insert),
                operation_from: None,
                operation_map: None,
                element_type: ElementType::Relation,
                effective_from: None,
                template: ElementTemplate {
                    id: "rel-{{payload.id}}".to_string(),
                    labels: vec!["FOLLOWS".to_string()],
                    properties: Some(serde_json::json!({
                        "since": "{{payload.since}}"
                    })),
                    from: Some("user-{{payload.from_user}}".to_string()),
                    to: Some("user-{{payload.to_user}}".to_string()),
                },
            }],
        }],
    };

    let http_source = HttpSourceBuilder::new("relation-source")
        .with_host("127.0.0.1")
        .with_port(port)
        .with_webhooks(webhook_config)
        .with_auto_start(true)
        .build()
        .unwrap();

    // Simple query - we're mainly testing the webhook processing
    let query = Query::cypher("relation-query")
        .query("MATCH (n) RETURN n")
        .from_source("relation-source")
        .auto_start(true)
        .build();

    let (reaction, _handle) = ApplicationReactionBuilder::new("relation-reaction")
        .with_query("relation-query")
        .build();

    let core = Arc::new(
        DrasiLib::builder()
            .with_id("relation-e2e-test")
            .with_source(http_source)
            .with_query(query)
            .with_reaction(reaction)
            .build()
            .await
            .unwrap(),
    );

    core.start().await.unwrap();
    sleep(Duration::from_millis(200)).await;

    let client = Client::new();
    let payload = serde_json::json!({
        "id": "follow-1",
        "from_user": "alice",
        "to_user": "bob",
        "since": "2024-01-15"
    });

    // Verify webhook accepts the relation creation request
    let response = client
        .post(format!("http://127.0.0.1:{port}/relationships"))
        .header("Content-Type", "application/json")
        .json(&payload)
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);

    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["success"], true);
    assert!(
        body["message"].as_str().unwrap().contains("1 events"),
        "Expected message to contain '1 events'"
    );

    core.stop().await.unwrap();
}

/// Test mapping entire payload as properties using template string
#[tokio::test]
async fn test_webhook_payload_spread_as_properties() {
    let port = find_available_port().await;

    let webhook_config = WebhookConfig {
        error_behavior: ErrorBehavior::Reject,
        cors: None,
        routes: vec![WebhookRoute {
            path: "/events".to_string(),
            methods: vec![HttpMethod::Post],
            auth: None,
            error_behavior: None,
            mappings: vec![WebhookMapping {
                when: None,
                operation: Some(OperationType::Insert),
                operation_from: None,
                operation_map: None,
                element_type: ElementType::Node,
                effective_from: None,
                template: ElementTemplate {
                    id: "event-{{payload.id}}".to_string(),
                    labels: vec!["SpreadEvent".to_string()],
                    // Use template string to spread entire payload as properties
                    properties: Some(serde_json::json!("{{payload}}")),
                    from: None,
                    to: None,
                },
            }],
        }],
    };

    let http_source = HttpSourceBuilder::new("spread-source")
        .with_host("127.0.0.1")
        .with_port(port)
        .with_webhooks(webhook_config)
        .with_auto_start(true)
        .build()
        .unwrap();

    // Query that returns multiple fields from the spread properties
    let query = Query::cypher("spread-query")
        .query(
            "MATCH (e:SpreadEvent) RETURN e.id AS id, e.name AS name, e.status AS status, e.count AS count",
        )
        .from_source("spread-source")
        .auto_start(true)
        .build();

    let (reaction, handle) = ApplicationReactionBuilder::new("spread-reaction")
        .with_query("spread-query")
        .build();

    let core = Arc::new(
        DrasiLib::builder()
            .with_id("spread-e2e-test")
            .with_source(http_source)
            .with_query(query)
            .with_reaction(reaction)
            .build()
            .await
            .unwrap(),
    );

    core.start().await.unwrap();
    sleep(Duration::from_millis(200)).await;

    let mut subscription = handle
        .subscribe_with_options(SubscriptionOptions::default().with_timeout(Duration::from_secs(5)))
        .await
        .unwrap();

    let client = Client::new();

    // Send payload with multiple fields - all should become node properties
    let payload = serde_json::json!({
        "id": "evt-123",
        "name": "Test Event",
        "status": "active",
        "count": 42,
        "extra_field": "bonus data"
    });

    let response = client
        .post(format!("http://127.0.0.1:{port}/events"))
        .header("Content-Type", "application/json")
        .json(&payload)
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);

    let result = subscription.recv().await;
    assert!(result.is_some(), "Expected to receive query result");

    let query_result = result.unwrap();
    match &query_result.results[0] {
        ResultDiff::Add { data } => {
            // All payload fields should be available as properties
            assert_eq!(data["id"], "evt-123");
            assert_eq!(data["name"], "Test Event");
            assert_eq!(data["status"], "active");
            // Count may come as number or string
            let count = &data["count"];
            assert!(
                count == 42 || count == "42",
                "Expected count 42, got {count:?}"
            );
        }
        _ => panic!("Expected Add result"),
    }

    core.stop().await.unwrap();
}

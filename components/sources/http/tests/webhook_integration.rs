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

//! Integration tests for webhook functionality.
//!
//! These tests spin up actual HTTP servers and make real HTTP requests
//! to verify end-to-end webhook processing.

#![allow(clippy::unwrap_used)]

use drasi_lib::Source;
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
    // Give OS time to release the port
    sleep(Duration::from_millis(50)).await;
    port
}

/// Create a webhook config for GitHub-style webhooks
fn create_github_webhook_config() -> WebhookConfig {
    WebhookConfig {
        error_behavior: ErrorBehavior::Reject,
        cors: None,
        routes: vec![WebhookRoute {
            path: "/github/events".to_string(),
            methods: vec![HttpMethod::Post],
            auth: Some(AuthConfig {
                signature: Some(SignatureConfig {
                    algorithm: SignatureAlgorithm::HmacSha256,
                    secret_env: "TEST_GITHUB_SECRET".to_string(),
                    header: "X-Hub-Signature-256".to_string(),
                    prefix: Some("sha256=".to_string()),
                    encoding: SignatureEncoding::Hex,
                }),
                bearer: None,
            }),
            error_behavior: Some(ErrorBehavior::Reject),
            mappings: vec![
                WebhookMapping {
                    when: Some(MappingCondition {
                        header: Some("X-GitHub-Event".to_string()),
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
                        id: "commit-{{payload.head_commit.id}}".to_string(),
                        labels: vec!["Commit".to_string()],
                        properties: Some(serde_json::json!({
                            "message": "{{payload.head_commit.message}}",
                            "author": "{{payload.head_commit.author.name}}"
                        })),
                        from: None,
                        to: None,
                    },
                },
                WebhookMapping {
                    when: Some(MappingCondition {
                        header: Some("X-GitHub-Event".to_string()),
                        field: None,
                        equals: Some("issues".to_string()),
                        contains: None,
                        regex: None,
                    }),
                    operation: None,
                    operation_from: Some("payload.action".to_string()),
                    operation_map: Some({
                        let mut map = HashMap::new();
                        map.insert("opened".to_string(), OperationType::Insert);
                        map.insert("edited".to_string(), OperationType::Update);
                        map.insert("closed".to_string(), OperationType::Delete);
                        map
                    }),
                    element_type: ElementType::Node,
                    effective_from: None,
                    template: ElementTemplate {
                        id: "issue-{{payload.issue.number}}".to_string(),
                        labels: vec!["Issue".to_string()],
                        properties: Some(serde_json::json!({
                            "title": "{{payload.issue.title}}",
                            "state": "{{payload.issue.state}}"
                        })),
                        from: None,
                        to: None,
                    },
                },
            ],
        }],
    }
}

/// Compute HMAC-SHA256 signature
fn compute_signature(secret: &str, body: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(body);
    let result = mac.finalize();
    format!("sha256={}", hex::encode(result.into_bytes()))
}

#[tokio::test]
async fn test_webhook_mode_health_check() {
    let port = find_available_port().await;

    let webhook_config = WebhookConfig {
        error_behavior: ErrorBehavior::AcceptAndLog,
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
                    id: "{{payload.id}}".to_string(),
                    labels: vec!["Event".to_string()],
                    properties: None,
                    from: None,
                    to: None,
                },
            }],
        }],
    };

    let source = HttpSourceBuilder::new("test-webhook")
        .with_host("127.0.0.1")
        .with_port(port)
        .with_webhooks(webhook_config)
        .with_auto_start(false)
        .build()
        .unwrap();

    let source = Arc::new(source);

    // Start the source
    source.start().await.unwrap();

    // Give server time to start
    sleep(Duration::from_millis(100)).await;

    // Test health endpoint
    let client = Client::new();
    let response = client
        .get(format!("http://127.0.0.1:{port}/health"))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);

    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["status"], "healthy");

    // Stop the source
    source.stop().await.unwrap();
}

#[tokio::test]
async fn test_webhook_mode_disables_standard_endpoints() {
    let port = find_available_port().await;

    let webhook_config = WebhookConfig {
        error_behavior: ErrorBehavior::Reject,
        cors: None,
        routes: vec![WebhookRoute {
            path: "/custom/events".to_string(),
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
                    id: "{{payload.id}}".to_string(),
                    labels: vec!["Event".to_string()],
                    properties: None,
                    from: None,
                    to: None,
                },
            }],
        }],
    };

    let source = HttpSourceBuilder::new("test-source")
        .with_host("127.0.0.1")
        .with_port(port)
        .with_webhooks(webhook_config)
        .with_auto_start(false)
        .build()
        .unwrap();

    let source = Arc::new(source);
    source.start().await.unwrap();
    sleep(Duration::from_millis(100)).await;

    let client = Client::new();

    // Standard endpoint should return 404 in webhook mode
    let response = client
        .post(format!("http://127.0.0.1:{port}/sources/test-source/events"))
        .header("Content-Type", "application/json")
        .body(r#"{"operation":"insert","element":{"type":"node","id":"1","labels":["Test"],"properties":{}}}"#)
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 404);

    source.stop().await.unwrap();
}

#[tokio::test]
async fn test_webhook_simple_payload_processing() {
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
                    labels: vec!["Event".to_string()],
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

    let source = HttpSourceBuilder::new("test-source")
        .with_host("127.0.0.1")
        .with_port(port)
        .with_webhooks(webhook_config)
        .with_auto_start(false)
        .build()
        .unwrap();

    let source = Arc::new(source);
    source.start().await.unwrap();
    sleep(Duration::from_millis(100)).await;

    let client = Client::new();

    // Send a webhook payload
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

    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["success"], true);
    assert!(body["message"].as_str().unwrap().contains("1 events"));

    source.stop().await.unwrap();
}

#[tokio::test]
async fn test_webhook_with_path_parameters() {
    let port = find_available_port().await;

    let webhook_config = WebhookConfig {
        error_behavior: ErrorBehavior::Reject,
        cors: None,
        routes: vec![WebhookRoute {
            path: "/users/:user_id/events/:event_type".to_string(),
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
                    id: "{{route.user_id}}-{{route.event_type}}-{{payload.id}}".to_string(),
                    labels: vec!["{{route.event_type}}".to_string()],
                    properties: Some(serde_json::json!({
                        "user_id": "{{route.user_id}}",
                        "data": "{{payload.data}}"
                    })),
                    from: None,
                    to: None,
                },
            }],
        }],
    };

    let source = HttpSourceBuilder::new("test-source")
        .with_host("127.0.0.1")
        .with_port(port)
        .with_webhooks(webhook_config)
        .with_auto_start(false)
        .build()
        .unwrap();

    let source = Arc::new(source);
    source.start().await.unwrap();
    sleep(Duration::from_millis(100)).await;

    let client = Client::new();

    let payload = serde_json::json!({
        "id": "event-1",
        "data": "test data"
    });

    let response = client
        .post(format!(
            "http://127.0.0.1:{port}/users/user123/events/click"
        ))
        .header("Content-Type", "application/json")
        .json(&payload)
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);

    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["success"], true);

    source.stop().await.unwrap();
}

#[tokio::test]
async fn test_webhook_with_hmac_signature() {
    // Set up environment variable for the test
    std::env::set_var("TEST_GITHUB_SECRET", "my-webhook-secret");

    let port = find_available_port().await;
    let webhook_config = create_github_webhook_config();

    let source = HttpSourceBuilder::new("github-source")
        .with_host("127.0.0.1")
        .with_port(port)
        .with_webhooks(webhook_config)
        .with_auto_start(false)
        .build()
        .unwrap();

    let source = Arc::new(source);
    source.start().await.unwrap();
    sleep(Duration::from_millis(100)).await;

    let client = Client::new();

    // Create a push event payload
    let payload = serde_json::json!({
        "head_commit": {
            "id": "abc123",
            "message": "Fix bug",
            "author": {
                "name": "John Doe"
            }
        }
    });

    let body = serde_json::to_vec(&payload).unwrap();
    let signature = compute_signature("my-webhook-secret", &body);

    // Test with valid signature
    let response = client
        .post(format!("http://127.0.0.1:{port}/github/events"))
        .header("Content-Type", "application/json")
        .header("X-GitHub-Event", "push")
        .header("X-Hub-Signature-256", &signature)
        .body(body.clone())
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);

    // Test with invalid signature
    let response = client
        .post(format!("http://127.0.0.1:{port}/github/events"))
        .header("Content-Type", "application/json")
        .header("X-GitHub-Event", "push")
        .header("X-Hub-Signature-256", "sha256=invalid")
        .body(body)
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 401);

    source.stop().await.unwrap();
    std::env::remove_var("TEST_GITHUB_SECRET");
}

#[tokio::test]
async fn test_webhook_with_bearer_token() {
    std::env::set_var("TEST_BEARER_TOKEN", "secret-token-123");

    let port = find_available_port().await;

    let webhook_config = WebhookConfig {
        error_behavior: ErrorBehavior::Reject,
        cors: None,
        routes: vec![WebhookRoute {
            path: "/api/events".to_string(),
            methods: vec![HttpMethod::Post],
            auth: Some(AuthConfig {
                signature: None,
                bearer: Some(BearerConfig {
                    token_env: "TEST_BEARER_TOKEN".to_string(),
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
                    id: "{{payload.id}}".to_string(),
                    labels: vec!["Event".to_string()],
                    properties: None,
                    from: None,
                    to: None,
                },
            }],
        }],
    };

    let source = HttpSourceBuilder::new("api-source")
        .with_host("127.0.0.1")
        .with_port(port)
        .with_webhooks(webhook_config)
        .with_auto_start(false)
        .build()
        .unwrap();

    let source = Arc::new(source);
    source.start().await.unwrap();
    sleep(Duration::from_millis(100)).await;

    let client = Client::new();

    let payload = serde_json::json!({"id": "test-1"});

    // Test with valid bearer token
    let response = client
        .post(format!("http://127.0.0.1:{port}/api/events"))
        .header("Content-Type", "application/json")
        .header("Authorization", "Bearer secret-token-123")
        .json(&payload)
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);

    // Test with invalid bearer token
    let response = client
        .post(format!("http://127.0.0.1:{port}/api/events"))
        .header("Content-Type", "application/json")
        .header("Authorization", "Bearer wrong-token")
        .json(&payload)
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 401);

    // Test without bearer token
    let response = client
        .post(format!("http://127.0.0.1:{port}/api/events"))
        .header("Content-Type", "application/json")
        .json(&payload)
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 401);

    source.stop().await.unwrap();
    std::env::remove_var("TEST_BEARER_TOKEN");
}

#[tokio::test]
async fn test_webhook_condition_matching() {
    std::env::set_var("TEST_GITHUB_SECRET_CONDITION", "test-secret-cond");

    let port = find_available_port().await;

    // Create config inline with the correct env var name
    let webhook_config = WebhookConfig {
        error_behavior: ErrorBehavior::Reject,
        cors: None,
        routes: vec![WebhookRoute {
            path: "/github/events".to_string(),
            methods: vec![HttpMethod::Post],
            auth: Some(AuthConfig {
                signature: Some(SignatureConfig {
                    algorithm: SignatureAlgorithm::HmacSha256,
                    secret_env: "TEST_GITHUB_SECRET_CONDITION".to_string(),
                    header: "X-Hub-Signature-256".to_string(),
                    prefix: Some("sha256=".to_string()),
                    encoding: SignatureEncoding::Hex,
                }),
                bearer: None,
            }),
            error_behavior: Some(ErrorBehavior::Reject),
            mappings: vec![
                WebhookMapping {
                    when: Some(MappingCondition {
                        header: Some("X-GitHub-Event".to_string()),
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
                        id: "commit-{{payload.head_commit.id}}".to_string(),
                        labels: vec!["Commit".to_string()],
                        properties: Some(serde_json::json!({
                            "message": "{{payload.head_commit.message}}"
                        })),
                        from: None,
                        to: None,
                    },
                },
                WebhookMapping {
                    when: Some(MappingCondition {
                        header: Some("X-GitHub-Event".to_string()),
                        field: None,
                        equals: Some("issues".to_string()),
                        contains: None,
                        regex: None,
                    }),
                    operation: None,
                    operation_from: Some("payload.action".to_string()),
                    operation_map: Some({
                        let mut map = HashMap::new();
                        map.insert("opened".to_string(), OperationType::Insert);
                        map
                    }),
                    element_type: ElementType::Node,
                    effective_from: None,
                    template: ElementTemplate {
                        id: "issue-{{payload.issue.number}}".to_string(),
                        labels: vec!["Issue".to_string()],
                        properties: None,
                        from: None,
                        to: None,
                    },
                },
            ],
        }],
    };

    let source = HttpSourceBuilder::new("github-source-cond")
        .with_host("127.0.0.1")
        .with_port(port)
        .with_webhooks(webhook_config)
        .with_auto_start(false)
        .build()
        .unwrap();

    let source = Arc::new(source);
    source.start().await.unwrap();
    sleep(Duration::from_millis(100)).await;

    let client = Client::new();

    // Test push event (should match first mapping)
    let push_payload = serde_json::json!({
        "head_commit": {
            "id": "commit-1",
            "message": "Initial commit",
            "author": {"name": "Alice"}
        }
    });
    let body = serde_json::to_vec(&push_payload).unwrap();
    let signature = compute_signature("test-secret-cond", &body);

    let response = client
        .post(format!("http://127.0.0.1:{port}/github/events"))
        .header("Content-Type", "application/json")
        .header("X-GitHub-Event", "push")
        .header("X-Hub-Signature-256", &signature)
        .body(body)
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);

    // Test issues event with opened action
    let issue_payload = serde_json::json!({
        "action": "opened",
        "issue": {
            "number": 42,
            "title": "Bug report",
            "state": "open"
        }
    });
    let body = serde_json::to_vec(&issue_payload).unwrap();
    let signature = compute_signature("test-secret-cond", &body);

    let response = client
        .post(format!("http://127.0.0.1:{port}/github/events"))
        .header("Content-Type", "application/json")
        .header("X-GitHub-Event", "issues")
        .header("X-Hub-Signature-256", &signature)
        .body(body)
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);

    // Test unmatched event type
    let other_payload = serde_json::json!({"data": "test"});
    let body = serde_json::to_vec(&other_payload).unwrap();
    let signature = compute_signature("test-secret-cond", &body);

    let response = client
        .post(format!("http://127.0.0.1:{port}/github/events"))
        .header("Content-Type", "application/json")
        .header("X-GitHub-Event", "unknown_event")
        .header("X-Hub-Signature-256", &signature)
        .body(body)
        .send()
        .await
        .unwrap();

    // Should fail because no mapping matches
    assert_eq!(response.status(), 400);

    source.stop().await.unwrap();
    std::env::remove_var("TEST_GITHUB_SECRET_CONDITION");
}

#[tokio::test]
async fn test_webhook_unmatched_route_returns_404() {
    let port = find_available_port().await;

    let webhook_config = WebhookConfig {
        error_behavior: ErrorBehavior::Reject,
        cors: None,
        routes: vec![WebhookRoute {
            path: "/specific/route".to_string(),
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
                    id: "{{payload.id}}".to_string(),
                    labels: vec!["Event".to_string()],
                    properties: None,
                    from: None,
                    to: None,
                },
            }],
        }],
    };

    let source = HttpSourceBuilder::new("test-source")
        .with_host("127.0.0.1")
        .with_port(port)
        .with_webhooks(webhook_config)
        .with_auto_start(false)
        .build()
        .unwrap();

    let source = Arc::new(source);
    source.start().await.unwrap();
    sleep(Duration::from_millis(100)).await;

    let client = Client::new();

    // Request to non-existent route
    let response = client
        .post(format!("http://127.0.0.1:{port}/wrong/route"))
        .header("Content-Type", "application/json")
        .body(r#"{"id": "1"}"#)
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 404);

    source.stop().await.unwrap();
}

#[tokio::test]
async fn test_standard_mode_works_without_webhooks() {
    let port = find_available_port().await;

    // Create source without webhook config (standard mode)
    let source = HttpSourceBuilder::new("standard-source")
        .with_host("127.0.0.1")
        .with_port(port)
        .with_auto_start(false)
        .build()
        .unwrap();

    let source = Arc::new(source);
    source.start().await.unwrap();
    sleep(Duration::from_millis(100)).await;

    let client = Client::new();

    // Standard endpoint should work
    let response = client
        .post(format!(
            "http://127.0.0.1:{port}/sources/standard-source/events"
        ))
        .header("Content-Type", "application/json")
        .body(
            r#"{"operation":"insert","element":{"type":"node","id":"1","labels":["Test"],"properties":{}}}"#,
        )
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);

    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["success"], true);

    source.stop().await.unwrap();
}

#[tokio::test]
async fn test_webhook_yaml_content_type() {
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
                    id: "{{payload.id}}".to_string(),
                    labels: vec!["Event".to_string()],
                    properties: Some(serde_json::json!({
                        "name": "{{payload.name}}"
                    })),
                    from: None,
                    to: None,
                },
            }],
        }],
    };

    let source = HttpSourceBuilder::new("yaml-source")
        .with_host("127.0.0.1")
        .with_port(port)
        .with_webhooks(webhook_config)
        .with_auto_start(false)
        .build()
        .unwrap();

    let source = Arc::new(source);
    source.start().await.unwrap();
    sleep(Duration::from_millis(100)).await;

    let client = Client::new();

    // Send YAML payload
    let yaml_body = "id: yaml-event-1\nname: YAML Test Event\n";

    let response = client
        .post(format!("http://127.0.0.1:{port}/events"))
        .header("Content-Type", "application/x-yaml")
        .body(yaml_body)
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);

    let body: serde_json::Value = response.json().await.unwrap();
    assert_eq!(body["success"], true);

    source.stop().await.unwrap();
}

#[tokio::test]
async fn test_webhook_method_filtering() {
    let port = find_available_port().await;

    let webhook_config = WebhookConfig {
        error_behavior: ErrorBehavior::Reject,
        cors: None,
        routes: vec![WebhookRoute {
            path: "/events".to_string(),
            methods: vec![HttpMethod::Post, HttpMethod::Put],
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
                    id: "{{payload.id}}".to_string(),
                    labels: vec!["Event".to_string()],
                    properties: None,
                    from: None,
                    to: None,
                },
            }],
        }],
    };

    let source = HttpSourceBuilder::new("method-source")
        .with_host("127.0.0.1")
        .with_port(port)
        .with_webhooks(webhook_config)
        .with_auto_start(false)
        .build()
        .unwrap();

    let source = Arc::new(source);
    source.start().await.unwrap();
    sleep(Duration::from_millis(100)).await;

    let client = Client::new();

    let payload = serde_json::json!({"id": "test"});

    // POST should work
    let response = client
        .post(format!("http://127.0.0.1:{port}/events"))
        .header("Content-Type", "application/json")
        .json(&payload)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);

    // PUT should work
    let response = client
        .put(format!("http://127.0.0.1:{port}/events"))
        .header("Content-Type", "application/json")
        .json(&payload)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);

    // DELETE should not match (returns 404)
    let response = client
        .delete(format!("http://127.0.0.1:{port}/events"))
        .header("Content-Type", "application/json")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 404);

    source.stop().await.unwrap();
}

#[tokio::test]
async fn test_webhook_cors_preflight() {
    let port = find_available_port().await;

    let webhook_config = WebhookConfig {
        error_behavior: ErrorBehavior::Reject,
        cors: Some(CorsConfig {
            enabled: true,
            allow_origins: vec!["https://example.com".to_string()],
            allow_methods: vec!["GET".to_string(), "POST".to_string(), "OPTIONS".to_string()],
            allow_headers: vec!["Content-Type".to_string(), "Authorization".to_string()],
            expose_headers: vec![],
            allow_credentials: false,
            max_age: 3600,
        }),
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
                    id: "{{payload.id}}".to_string(),
                    labels: vec!["Event".to_string()],
                    properties: None,
                    from: None,
                    to: None,
                },
            }],
        }],
    };

    let source = HttpSourceBuilder::new("cors-source")
        .with_host("127.0.0.1")
        .with_port(port)
        .with_webhooks(webhook_config)
        .with_auto_start(false)
        .build()
        .unwrap();

    let source = Arc::new(source);
    source.start().await.unwrap();
    sleep(Duration::from_millis(100)).await;

    let client = Client::new();

    // Test CORS preflight request (OPTIONS)
    let response = client
        .request(
            reqwest::Method::OPTIONS,
            format!("http://127.0.0.1:{port}/events"),
        )
        .header("Origin", "https://example.com")
        .header("Access-Control-Request-Method", "POST")
        .header("Access-Control-Request-Headers", "Content-Type")
        .send()
        .await
        .unwrap();

    // Should return 200 with CORS headers
    assert_eq!(response.status(), 200);
    assert!(response
        .headers()
        .contains_key("access-control-allow-origin"));
    assert!(response
        .headers()
        .contains_key("access-control-allow-methods"));

    // Test actual POST request with CORS origin
    let response = client
        .post(format!("http://127.0.0.1:{port}/events"))
        .header("Origin", "https://example.com")
        .header("Content-Type", "application/json")
        .body(r#"{"id": "test-1"}"#)
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    // CORS header should be present in response
    let cors_origin = response
        .headers()
        .get("access-control-allow-origin")
        .and_then(|v| v.to_str().ok());
    assert_eq!(cors_origin, Some("https://example.com"));

    source.stop().await.unwrap();
}

#[tokio::test]
async fn test_webhook_cors_wildcard_origin() {
    let port = find_available_port().await;

    let webhook_config = WebhookConfig {
        error_behavior: ErrorBehavior::Reject,
        cors: Some(CorsConfig::default()), // Default uses "*" for origins
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
                    id: "{{payload.id}}".to_string(),
                    labels: vec!["Event".to_string()],
                    properties: None,
                    from: None,
                    to: None,
                },
            }],
        }],
    };

    let source = HttpSourceBuilder::new("cors-wildcard-source")
        .with_host("127.0.0.1")
        .with_port(port)
        .with_webhooks(webhook_config)
        .with_auto_start(false)
        .build()
        .unwrap();

    let source = Arc::new(source);
    source.start().await.unwrap();
    sleep(Duration::from_millis(100)).await;

    let client = Client::new();

    // Test request from any origin
    let response = client
        .post(format!("http://127.0.0.1:{port}/events"))
        .header("Origin", "https://any-domain.com")
        .header("Content-Type", "application/json")
        .body(r#"{"id": "test-1"}"#)
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    // Wildcard origin should allow any
    let cors_origin = response
        .headers()
        .get("access-control-allow-origin")
        .and_then(|v| v.to_str().ok());
    assert_eq!(cors_origin, Some("*"));

    source.stop().await.unwrap();
}

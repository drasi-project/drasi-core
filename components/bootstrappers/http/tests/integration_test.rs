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

//! End-to-end integration tests for the HTTP bootstrap provider.
//!
//! These tests spin up real HTTP servers using axum and verify
//! the full bootstrap flow: HTTP request → parse → map → emit events.

use axum::{
    extract::{Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    routing::post,
    Json, Router,
};
use drasi_bootstrap_http::config::*;
use drasi_bootstrap_http::HttpBootstrapProvider;
use drasi_core::models::{Element, SourceChange};
use drasi_lib::bootstrap::{
    BootstrapContext, BootstrapProvider, BootstrapRequest, BootstrapResult,
};
use drasi_lib::channels::BootstrapEvent;
use serde_json::{json, Value as JsonValue};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::mpsc;

// ── Test helpers ────────────────────────────────────────────────────────────

#[allow(clippy::unwrap_used)]
/// Start a test server and return its base URL.
async fn start_server(app: Router) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://127.0.0.1:{}", addr.port()) // DevSkim: ignore DS137138
}

/// Create a bootstrap context for testing.
fn test_context(source_id: &str) -> BootstrapContext {
    BootstrapContext::new_minimal("test-server".to_string(), source_id.to_string())
}

/// Create a bootstrap request.
fn test_request(node_labels: Vec<String>, relation_labels: Vec<String>) -> BootstrapRequest {
    BootstrapRequest {
        query_id: "test-query".to_string(),
        node_labels,
        relation_labels,
        request_id: "test-request".to_string(),
    }
}

/// Collect all bootstrap events from the channel.
async fn collect_events(mut rx: mpsc::Receiver<BootstrapEvent>) -> Vec<BootstrapEvent> {
    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event);
    }
    events
}

// ── Test: Simple JSON endpoint (no pagination) ──────────────────────────────

#[tokio::test]
async fn test_simple_json_endpoint() {
    let app = Router::new().route(
        "/users",
        get(|| async {
            Json(json!([
                {"id": "1", "name": "Alice", "email": "alice@example.com"},
                {"id": "2", "name": "Bob", "email": "bob@example.com"},
                {"id": "3", "name": "Charlie", "email": "charlie@example.com"}
            ]))
        }),
    );

    let base_url = start_server(app).await;

    let config = HttpBootstrapConfig {
        endpoints: vec![EndpointConfig {
            url: format!("{base_url}/users"),
            method: HttpMethod::Get,
            headers: HashMap::new(),
            body: None,
            auth: None,
            pagination: None,
            response: ResponseConfig {
                items_path: "$".to_string(),
                content_type: None,
                mappings: vec![ElementMappingConfig {
                    element_type: ElementType::Node,
                    template: ElementTemplate {
                        id: "{{item.id}}".to_string(),
                        labels: vec!["User".to_string()],
                        properties: Some(json!({
                            "name": "{{item.name}}",
                            "email": "{{item.email}}"
                        })),
                        from: None,
                        to: None,
                    },
                }],
            },
        }],
        timeout_seconds: 10,
        max_retries: 0,
        retry_delay_ms: 100,
    };

    let provider = HttpBootstrapProvider::new(config).unwrap();
    let context = test_context("test-source");
    let request = test_request(vec!["User".to_string()], vec![]);

    let (tx, mut rx) = mpsc::channel(100);
    let result = provider
        .bootstrap(request, &context, tx, None)
        .await
        .unwrap();

    assert_eq!(result.event_count, 3);

    let events = collect_events(rx).await;
    assert_eq!(events.len(), 3);

    // Verify first event
    match &events[0].change {
        SourceChange::Insert { element } => match element {
            Element::Node {
                metadata,
                properties,
            } => {
                assert_eq!(&*metadata.reference.element_id, "1");
                assert_eq!(&*metadata.labels[0], "User");
            }
            _ => panic!("Expected Node"),
        },
        _ => panic!("Expected Insert"),
    }
}

// ── Test: Offset/Limit pagination ───────────────────────────────────────────

#[tokio::test]
async fn test_offset_limit_pagination() {
    // Generate 25 items, serve in pages of 10
    let all_items: Vec<JsonValue> = (1..=25)
        .map(|i| json!({"id": format!("{i}"), "name": format!("User {i}")}))
        .collect();

    let items = Arc::new(all_items);

    let app = Router::new().route(
        "/users",
        get(move |Query(params): Query<HashMap<String, String>>| {
            let items = items.clone();
            async move {
                let offset: usize = params
                    .get("offset")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0);
                let limit: usize = params
                    .get("limit")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(10);

                let page: Vec<&JsonValue> = items.iter().skip(offset).take(limit).collect();
                let response = json!({
                    "data": page,
                    "total": items.len()
                });
                Json(response)
            }
        }),
    );

    let base_url = start_server(app).await;

    let config = HttpBootstrapConfig {
        endpoints: vec![EndpointConfig {
            url: format!("{base_url}/users"),
            method: HttpMethod::Get,
            headers: HashMap::new(),
            body: None,
            auth: None,
            pagination: Some(PaginationConfig::OffsetLimit {
                offset_param: "offset".to_string(),
                limit_param: "limit".to_string(),
                page_size: 10,
                total_path: Some("$.total".to_string()),
            }),
            response: ResponseConfig {
                items_path: "$.data".to_string(),
                content_type: None,
                mappings: vec![ElementMappingConfig {
                    element_type: ElementType::Node,
                    template: ElementTemplate {
                        id: "{{item.id}}".to_string(),
                        labels: vec!["User".to_string()],
                        properties: Some(json!({"name": "{{item.name}}"})),
                        from: None,
                        to: None,
                    },
                }],
            },
        }],
        timeout_seconds: 10,
        max_retries: 0,
        retry_delay_ms: 100,
    };

    let provider = HttpBootstrapProvider::new(config).unwrap();
    let context = test_context("test-source");
    let request = test_request(vec!["User".to_string()], vec![]);

    let (tx, mut rx) = mpsc::channel(100);
    let result = provider
        .bootstrap(request, &context, tx, None)
        .await
        .unwrap();

    assert_eq!(result.event_count, 25);
    let events = collect_events(rx).await;
    assert_eq!(events.len(), 25);
}

// ── Test: Cursor-based pagination (Stripe-style) ────────────────────────────

#[tokio::test]
async fn test_cursor_pagination_stripe_style() {
    // Simulate Stripe-style pagination: starting_after + has_more
    let all_items: Vec<JsonValue> = (1..=15)
        .map(|i| json!({"id": format!("cus_{i}"), "name": format!("Customer {i}")}))
        .collect();

    let items = Arc::new(all_items);

    let app = Router::new().route(
        "/v1/customers",
        get(move |Query(params): Query<HashMap<String, String>>| {
            let items = items.clone();
            async move {
                let limit: usize = params
                    .get("limit")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(5);
                let starting_after = params.get("starting_after").cloned();

                let start_idx = match starting_after {
                    Some(ref cursor) => items
                        .iter()
                        .position(|item| item["id"] == *cursor)
                        .map(|pos| pos + 1)
                        .unwrap_or(0),
                    None => 0,
                };

                let page: Vec<&JsonValue> = items.iter().skip(start_idx).take(limit).collect();
                let has_more = start_idx + page.len() < items.len();

                let response = json!({
                    "object": "list",
                    "data": page,
                    "has_more": has_more
                });
                Json(response)
            }
        }),
    );

    let base_url = start_server(app).await;

    let config = HttpBootstrapConfig {
        endpoints: vec![EndpointConfig {
            url: format!("{base_url}/v1/customers"),
            method: HttpMethod::Get,
            headers: HashMap::new(),
            body: None,
            auth: None,
            pagination: Some(PaginationConfig::Cursor {
                cursor_param: "starting_after".to_string(),
                cursor_path: "$.data[-1].id".to_string(),
                has_more_path: Some("$.has_more".to_string()),
                page_size_param: Some("limit".to_string()),
                page_size: Some(5),
            }),
            response: ResponseConfig {
                items_path: "$.data".to_string(),
                content_type: None,
                mappings: vec![ElementMappingConfig {
                    element_type: ElementType::Node,
                    template: ElementTemplate {
                        id: "{{item.id}}".to_string(),
                        labels: vec!["Customer".to_string()],
                        properties: Some(json!({"name": "{{item.name}}"})),
                        from: None,
                        to: None,
                    },
                }],
            },
        }],
        timeout_seconds: 10,
        max_retries: 0,
        retry_delay_ms: 100,
    };

    let provider = HttpBootstrapProvider::new(config).unwrap();
    let context = test_context("test-source");
    let request = test_request(vec!["Customer".to_string()], vec![]);

    let (tx, mut rx) = mpsc::channel(100);
    let result = provider
        .bootstrap(request, &context, tx, None)
        .await
        .unwrap();

    assert_eq!(result.event_count, 15);
    let events = collect_events(rx).await;
    assert_eq!(events.len(), 15);
}

// ── Test: Link header pagination (GitHub-style) ─────────────────────────────

#[tokio::test]
async fn test_link_header_pagination_github_style() {
    // Simulate GitHub-style pagination with Link headers
    let all_items: Vec<JsonValue> = (1..=12)
        .map(|i| json!({"id": i, "full_name": format!("org/repo-{i}")}))
        .collect();

    let items = Arc::new(all_items);

    let app = Router::new().route(
        "/repos",
        get(
            move |Query(params): Query<HashMap<String, String>>, req_headers: HeaderMap| {
                let items = items.clone();
                async move {
                    let page: usize = params.get("page").and_then(|v| v.parse().ok()).unwrap_or(1);
                    let per_page: usize = params
                        .get("per_page")
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(5);

                    let start = (page - 1) * per_page;
                    let page_items: Vec<&JsonValue> =
                        items.iter().skip(start).take(per_page).collect();
                    let total_pages = items.len().div_ceil(per_page);

                    let host = req_headers
                        .get("host")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("localhost");

                    let body = serde_json::to_string(&page_items).unwrap();

                    let mut response = Response::builder().status(StatusCode::OK);

                    // Add Link header if there's a next page
                    if page < total_pages {
                        let next_url =
                            format!("http://{host}/repos?page={}&per_page={per_page}", page + 1); // DevSkim: ignore DS137138
                        let link_value = format!("<{next_url}>; rel=\"next\"");
                        response = response.header("Link", link_value);
                    }

                    response
                        .header(header::CONTENT_TYPE, "application/json")
                        .body(body)
                        .unwrap()
                }
            },
        ),
    );

    let base_url = start_server(app).await;

    let config = HttpBootstrapConfig {
        endpoints: vec![EndpointConfig {
            url: format!("{base_url}/repos"),
            method: HttpMethod::Get,
            headers: HashMap::new(),
            body: None,
            auth: None,
            pagination: Some(PaginationConfig::LinkHeader {
                page_size_param: Some("per_page".to_string()),
                page_size: Some(5),
            }),
            response: ResponseConfig {
                items_path: "$".to_string(),
                content_type: None,
                mappings: vec![ElementMappingConfig {
                    element_type: ElementType::Node,
                    template: ElementTemplate {
                        id: "{{item.id}}".to_string(),
                        labels: vec!["Repository".to_string()],
                        properties: Some(json!({"full_name": "{{item.full_name}}"})),
                        from: None,
                        to: None,
                    },
                }],
            },
        }],
        timeout_seconds: 10,
        max_retries: 0,
        retry_delay_ms: 100,
    };

    let provider = HttpBootstrapProvider::new(config).unwrap();
    let context = test_context("test-source");
    let request = test_request(vec!["Repository".to_string()], vec![]);

    let (tx, mut rx) = mpsc::channel(100);
    let result = provider
        .bootstrap(request, &context, tx, None)
        .await
        .unwrap();

    assert_eq!(result.event_count, 12);
    let events = collect_events(rx).await;
    assert_eq!(events.len(), 12);
}

// ── Test: Next-URL pagination (Salesforce-style) ────────────────────────────

#[tokio::test]
async fn test_next_url_pagination_salesforce_style() {
    // Simulate Salesforce-style pagination with nextRecordsUrl
    let all_items: Vec<JsonValue> = (1..=18)
        .map(|i| json!({"Id": format!("001{i:04}"), "Name": format!("Account {i}")}))
        .collect();

    let items = Arc::new(all_items);

    let app = Router::new()
        .route(
            "/services/data/v56.0/query",
            get({
                let items = items.clone();
                move |Query(params): Query<HashMap<String, String>>| {
                    let items = items.clone();
                    async move {
                        let page_size = 7;
                        let page_items: Vec<&JsonValue> = items.iter().take(page_size).collect();
                        let has_more = items.len() > page_size;

                        let mut response = json!({
                            "totalSize": items.len(),
                            "done": !has_more,
                            "records": page_items
                        });

                        if has_more {
                            response["nextRecordsUrl"] = json!("/services/data/v56.0/query/next-1");
                        }

                        Json(response)
                    }
                }
            }),
        )
        .route(
            "/services/data/v56.0/query/next-1",
            get({
                let items = items.clone();
                move || {
                    let items = items.clone();
                    async move {
                        let page_size = 7;
                        let page_items: Vec<&JsonValue> =
                            items.iter().skip(page_size).take(page_size).collect();
                        let remaining = items.len() - page_size - page_items.len();

                        let mut response = json!({
                            "totalSize": items.len(),
                            "done": remaining == 0,
                            "records": page_items
                        });

                        if remaining > 0 {
                            response["nextRecordsUrl"] = json!("/services/data/v56.0/query/next-2");
                        }

                        Json(response)
                    }
                }
            }),
        )
        .route(
            "/services/data/v56.0/query/next-2",
            get({
                let items = items.clone();
                move || {
                    let items = items.clone();
                    async move {
                        let page_size = 7;
                        let page_items: Vec<&JsonValue> =
                            items.iter().skip(page_size * 2).collect();

                        let response = json!({
                            "totalSize": items.len(),
                            "done": true,
                            "records": page_items
                        });

                        Json(response)
                    }
                }
            }),
        );

    let base_url = start_server(app).await;

    let config = HttpBootstrapConfig {
        endpoints: vec![EndpointConfig {
            url: format!("{base_url}/services/data/v56.0/query?q=SELECT+Id,Name+FROM+Account"),
            method: HttpMethod::Get,
            headers: HashMap::new(),
            body: None,
            auth: None,
            pagination: Some(PaginationConfig::NextUrl {
                next_url_path: "$.nextRecordsUrl".to_string(),
                base_url: Some(base_url.clone()),
            }),
            response: ResponseConfig {
                items_path: "$.records".to_string(),
                content_type: None,
                mappings: vec![ElementMappingConfig {
                    element_type: ElementType::Node,
                    template: ElementTemplate {
                        id: "{{item.Id}}".to_string(),
                        labels: vec!["Account".to_string()],
                        properties: Some(json!({"name": "{{item.Name}}"})),
                        from: None,
                        to: None,
                    },
                }],
            },
        }],
        timeout_seconds: 10,
        max_retries: 0,
        retry_delay_ms: 100,
    };

    let provider = HttpBootstrapProvider::new(config).unwrap();
    let context = test_context("test-source");
    let request = test_request(vec!["Account".to_string()], vec![]);

    let (tx, mut rx) = mpsc::channel(100);
    let result = provider
        .bootstrap(request, &context, tx, None)
        .await
        .unwrap();

    assert_eq!(result.event_count, 18);
    let events = collect_events(rx).await;
    assert_eq!(events.len(), 18);
}

// ── Test: Bearer token authentication ───────────────────────────────────────

#[tokio::test]
async fn test_bearer_auth() {
    let app = Router::new().route(
        "/protected",
        get(|headers: HeaderMap| async move {
            let auth_header = headers.get("authorization");
            match auth_header {
                Some(value) if value.to_str().unwrap_or("") == "Bearer test-secret-token" => {
                    Json(json!([{"id": "1", "data": "secret"}]))
                }
                _ => {
                    panic!("Expected valid bearer token");
                }
            }
        }),
    );

    let base_url = start_server(app).await;

    // Set the env var for the test
    unsafe { std::env::set_var("TEST_BEARER_TOKEN", "test-secret-token") };

    let config = HttpBootstrapConfig {
        endpoints: vec![EndpointConfig {
            url: format!("{base_url}/protected"),
            method: HttpMethod::Get,
            headers: HashMap::new(),
            body: None,
            auth: Some(AuthConfig::Bearer {
                token_env: "TEST_BEARER_TOKEN".to_string(),
            }),
            pagination: None,
            response: ResponseConfig {
                items_path: "$".to_string(),
                content_type: None,
                mappings: vec![ElementMappingConfig {
                    element_type: ElementType::Node,
                    template: ElementTemplate {
                        id: "{{item.id}}".to_string(),
                        labels: vec!["Secret".to_string()],
                        properties: Some(json!({"data": "{{item.data}}"})),
                        from: None,
                        to: None,
                    },
                }],
            },
        }],
        timeout_seconds: 10,
        max_retries: 0,
        retry_delay_ms: 100,
    };

    let provider = HttpBootstrapProvider::new(config).unwrap();
    let context = test_context("test-source");
    let request = test_request(vec!["Secret".to_string()], vec![]);

    let (tx, mut rx) = mpsc::channel(100);
    let result = provider
        .bootstrap(request, &context, tx, None)
        .await
        .unwrap();

    assert_eq!(result.event_count, 1);

    unsafe { std::env::remove_var("TEST_BEARER_TOKEN") };
}

// ── Test: API key authentication (header) ───────────────────────────────────

#[tokio::test]
async fn test_api_key_header_auth() {
    let app = Router::new().route(
        "/products",
        get(|headers: HeaderMap| async move {
            let api_key = headers.get("X-Shopify-Access-Token");
            match api_key {
                Some(value) if value.to_str().unwrap_or("") == "shop-token-123" => {
                    Json(json!({"products": [
                        {"id": "p1", "title": "Widget"},
                        {"id": "p2", "title": "Gadget"}
                    ]}))
                }
                _ => {
                    panic!("Expected valid API key header");
                }
            }
        }),
    );

    let base_url = start_server(app).await;

    unsafe { std::env::set_var("TEST_SHOPIFY_TOKEN", "shop-token-123") };

    let config = HttpBootstrapConfig {
        endpoints: vec![EndpointConfig {
            url: format!("{base_url}/products"),
            method: HttpMethod::Get,
            headers: HashMap::new(),
            body: None,
            auth: Some(AuthConfig::ApiKey {
                location: ApiKeyLocation::Header,
                name: "X-Shopify-Access-Token".to_string(),
                value_env: "TEST_SHOPIFY_TOKEN".to_string(),
            }),
            pagination: None,
            response: ResponseConfig {
                items_path: "$.products".to_string(),
                content_type: None,
                mappings: vec![ElementMappingConfig {
                    element_type: ElementType::Node,
                    template: ElementTemplate {
                        id: "{{item.id}}".to_string(),
                        labels: vec!["Product".to_string()],
                        properties: Some(json!({"title": "{{item.title}}"})),
                        from: None,
                        to: None,
                    },
                }],
            },
        }],
        timeout_seconds: 10,
        max_retries: 0,
        retry_delay_ms: 100,
    };

    let provider = HttpBootstrapProvider::new(config).unwrap();
    let context = test_context("test-source");
    let request = test_request(vec!["Product".to_string()], vec![]);

    let (tx, mut rx) = mpsc::channel(100);
    let result = provider
        .bootstrap(request, &context, tx, None)
        .await
        .unwrap();

    assert_eq!(result.event_count, 2);

    unsafe { std::env::remove_var("TEST_SHOPIFY_TOKEN") };
}

// ── Test: Basic authentication ──────────────────────────────────────────────

#[tokio::test]
async fn test_basic_auth() {
    use base64::Engine;

    let app = Router::new().route(
        "/v1/calls",
        get(|headers: HeaderMap| async move {
            let auth_header = headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");

            // Expected: Basic base64("user123:pass456")
            let expected = format!(
                "Basic {}",
                base64::engine::general_purpose::STANDARD.encode("user123:pass456")
            );

            if auth_header == expected {
                Json(json!([{"sid": "CA001", "from": "+1234"}]))
            } else {
                panic!("Expected valid basic auth, got: {auth_header}");
            }
        }),
    );

    let base_url = start_server(app).await;

    unsafe {
        std::env::set_var("TEST_BASIC_USER", "user123");
        std::env::set_var("TEST_BASIC_PASS", "pass456");
    }

    let config = HttpBootstrapConfig {
        endpoints: vec![EndpointConfig {
            url: format!("{base_url}/v1/calls"),
            method: HttpMethod::Get,
            headers: HashMap::new(),
            body: None,
            auth: Some(AuthConfig::Basic {
                username_env: "TEST_BASIC_USER".to_string(),
                password_env: Some("TEST_BASIC_PASS".to_string()),
            }),
            pagination: None,
            response: ResponseConfig {
                items_path: "$".to_string(),
                content_type: None,
                mappings: vec![ElementMappingConfig {
                    element_type: ElementType::Node,
                    template: ElementTemplate {
                        id: "{{item.sid}}".to_string(),
                        labels: vec!["Call".to_string()],
                        properties: Some(json!({"from": "{{item.from}}"})),
                        from: None,
                        to: None,
                    },
                }],
            },
        }],
        timeout_seconds: 10,
        max_retries: 0,
        retry_delay_ms: 100,
    };

    let provider = HttpBootstrapProvider::new(config).unwrap();
    let context = test_context("test-source");
    let request = test_request(vec!["Call".to_string()], vec![]);

    let (tx, mut rx) = mpsc::channel(100);
    let result = provider
        .bootstrap(request, &context, tx, None)
        .await
        .unwrap();

    assert_eq!(result.event_count, 1);

    unsafe {
        std::env::remove_var("TEST_BASIC_USER");
        std::env::remove_var("TEST_BASIC_PASS");
    }
}

// ── Test: OAuth2 Client Credentials ─────────────────────────────────────────

#[tokio::test]
async fn test_oauth2_client_credentials() {
    // Token server
    let token_app = Router::new().route(
        "/oauth/token",
        post(|body: String| async move {
            // Verify the token request contains expected fields
            assert!(body.contains("grant_type=client_credentials"));
            assert!(body.contains("client_id=test-client-id"));
            assert!(body.contains("client_secret=test-client-secret"));

            Json(json!({
                "access_token": "oauth-access-token-xyz",
                "token_type": "Bearer",
                "expires_in": 3600
            }))
        }),
    );

    let token_url = start_server(token_app).await;

    // Resource server
    let resource_app = Router::new().route(
        "/api/data",
        get(|headers: HeaderMap| async move {
            let auth = headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");

            if auth == "Bearer oauth-access-token-xyz" {
                Json(json!([{"id": "d1", "value": "data1"}]))
            } else {
                panic!("Expected OAuth2 bearer token, got: {auth}");
            }
        }),
    );

    let resource_url = start_server(resource_app).await;

    unsafe {
        std::env::set_var("TEST_OAUTH_CLIENT_ID", "test-client-id");
        std::env::set_var("TEST_OAUTH_CLIENT_SECRET", "test-client-secret");
    }

    let config = HttpBootstrapConfig {
        endpoints: vec![EndpointConfig {
            url: format!("{resource_url}/api/data"),
            method: HttpMethod::Get,
            headers: HashMap::new(),
            body: None,
            auth: Some(AuthConfig::OAuth2ClientCredentials {
                token_url: format!("{token_url}/oauth/token"),
                client_id_env: "TEST_OAUTH_CLIENT_ID".to_string(),
                client_secret_env: "TEST_OAUTH_CLIENT_SECRET".to_string(),
                scopes: vec![],
            }),
            pagination: None,
            response: ResponseConfig {
                items_path: "$".to_string(),
                content_type: None,
                mappings: vec![ElementMappingConfig {
                    element_type: ElementType::Node,
                    template: ElementTemplate {
                        id: "{{item.id}}".to_string(),
                        labels: vec!["Data".to_string()],
                        properties: Some(json!({"value": "{{item.value}}"})),
                        from: None,
                        to: None,
                    },
                }],
            },
        }],
        timeout_seconds: 10,
        max_retries: 0,
        retry_delay_ms: 100,
    };

    let provider = HttpBootstrapProvider::new(config).unwrap();
    let context = test_context("test-source");
    let request = test_request(vec!["Data".to_string()], vec![]);

    let (tx, mut rx) = mpsc::channel(100);
    let result = provider
        .bootstrap(request, &context, tx, None)
        .await
        .unwrap();

    assert_eq!(result.event_count, 1);

    unsafe {
        std::env::remove_var("TEST_OAUTH_CLIENT_ID");
        std::env::remove_var("TEST_OAUTH_CLIENT_SECRET");
    }
}

// ── Test: Multi-endpoint bootstrap ──────────────────────────────────────────

#[tokio::test]
async fn test_multi_endpoint_bootstrap() {
    // Users endpoint
    let users_app = Router::new().route(
        "/users",
        get(|| async {
            Json(json!([
                {"id": "u1", "name": "Alice"},
                {"id": "u2", "name": "Bob"}
            ]))
        }),
    );

    // Orders endpoint
    let orders_app = Router::new().route(
        "/orders",
        get(|| async {
            Json(json!([
                {"id": "o1", "user_id": "u1", "amount": 99.99},
                {"id": "o2", "user_id": "u2", "amount": 49.50},
                {"id": "o3", "user_id": "u1", "amount": 25.00}
            ]))
        }),
    );

    let users_url = start_server(users_app).await;
    let orders_url = start_server(orders_app).await;

    let config = HttpBootstrapConfig {
        endpoints: vec![
            EndpointConfig {
                url: format!("{users_url}/users"),
                method: HttpMethod::Get,
                headers: HashMap::new(),
                body: None,
                auth: None,
                pagination: None,
                response: ResponseConfig {
                    items_path: "$".to_string(),
                    content_type: None,
                    mappings: vec![ElementMappingConfig {
                        element_type: ElementType::Node,
                        template: ElementTemplate {
                            id: "{{item.id}}".to_string(),
                            labels: vec!["User".to_string()],
                            properties: Some(json!({"name": "{{item.name}}"})),
                            from: None,
                            to: None,
                        },
                    }],
                },
            },
            EndpointConfig {
                url: format!("{orders_url}/orders"),
                method: HttpMethod::Get,
                headers: HashMap::new(),
                body: None,
                auth: None,
                pagination: None,
                response: ResponseConfig {
                    items_path: "$".to_string(),
                    content_type: None,
                    mappings: vec![ElementMappingConfig {
                        element_type: ElementType::Node,
                        template: ElementTemplate {
                            id: "{{item.id}}".to_string(),
                            labels: vec!["Order".to_string()],
                            properties: Some(json!({"amount": "{{item.amount}}"})),
                            from: None,
                            to: None,
                        },
                    }],
                },
            },
        ],
        timeout_seconds: 10,
        max_retries: 0,
        retry_delay_ms: 100,
    };

    let provider = HttpBootstrapProvider::new(config).unwrap();
    let context = test_context("test-source");
    let request = test_request(vec!["User".to_string(), "Order".to_string()], vec![]);

    let (tx, mut rx) = mpsc::channel(100);
    let result = provider
        .bootstrap(request, &context, tx, None)
        .await
        .unwrap();

    assert_eq!(result.event_count, 5); // 2 users + 3 orders
    let events = collect_events(rx).await;
    assert_eq!(events.len(), 5);
}

// ── Test: Retry on failure ──────────────────────────────────────────────────

#[tokio::test]
async fn test_retry_on_failure() {
    let request_count = Arc::new(AtomicU64::new(0));

    let app = Router::new().route(
        "/flaky",
        get({
            let request_count = request_count.clone();
            move || {
                let request_count = request_count.clone();
                async move {
                    let count = request_count.fetch_add(1, Ordering::SeqCst);
                    if count < 2 {
                        // First 2 requests fail
                        (StatusCode::INTERNAL_SERVER_ERROR, "Server Error").into_response()
                    } else {
                        // Third request succeeds
                        Json(json!([{"id": "1", "name": "Success"}])).into_response()
                    }
                }
            }
        }),
    );

    let base_url = start_server(app).await;

    let config = HttpBootstrapConfig {
        endpoints: vec![EndpointConfig {
            url: format!("{base_url}/flaky"),
            method: HttpMethod::Get,
            headers: HashMap::new(),
            body: None,
            auth: None,
            pagination: None,
            response: ResponseConfig {
                items_path: "$".to_string(),
                content_type: None,
                mappings: vec![ElementMappingConfig {
                    element_type: ElementType::Node,
                    template: ElementTemplate {
                        id: "{{item.id}}".to_string(),
                        labels: vec!["Item".to_string()],
                        properties: Some(json!({"name": "{{item.name}}"})),
                        from: None,
                        to: None,
                    },
                }],
            },
        }],
        timeout_seconds: 10,
        max_retries: 3,
        retry_delay_ms: 50, // Short delay for tests
    };

    let provider = HttpBootstrapProvider::new(config).unwrap();
    let context = test_context("test-source");
    let request = test_request(vec!["Item".to_string()], vec![]);

    let (tx, mut rx) = mpsc::channel(100);
    let result = provider
        .bootstrap(request, &context, tx, None)
        .await
        .unwrap();

    assert_eq!(result.event_count, 1);
    // Should have made 3 requests total (2 failures + 1 success)
    assert_eq!(request_count.load(Ordering::SeqCst), 3);
}

// ── Test: Relations mapping ─────────────────────────────────────────────────

#[tokio::test]
async fn test_relations_mapping() {
    let app = Router::new().route(
        "/follows",
        get(|| async {
            Json(json!([
                {"id": "f1", "follower": "u1", "following": "u2"},
                {"id": "f2", "follower": "u2", "following": "u3"}
            ]))
        }),
    );

    let base_url = start_server(app).await;

    let config = HttpBootstrapConfig {
        endpoints: vec![EndpointConfig {
            url: format!("{base_url}/follows"),
            method: HttpMethod::Get,
            headers: HashMap::new(),
            body: None,
            auth: None,
            pagination: None,
            response: ResponseConfig {
                items_path: "$".to_string(),
                content_type: None,
                mappings: vec![ElementMappingConfig {
                    element_type: ElementType::Relation,
                    template: ElementTemplate {
                        id: "{{item.id}}".to_string(),
                        labels: vec!["FOLLOWS".to_string()],
                        properties: None,
                        from: Some("{{item.follower}}".to_string()),
                        to: Some("{{item.following}}".to_string()),
                    },
                }],
            },
        }],
        timeout_seconds: 10,
        max_retries: 0,
        retry_delay_ms: 100,
    };

    let provider = HttpBootstrapProvider::new(config).unwrap();
    let context = test_context("test-source");
    let request = test_request(vec![], vec!["FOLLOWS".to_string()]);

    let (tx, mut rx) = mpsc::channel(100);
    let result = provider
        .bootstrap(request, &context, tx, None)
        .await
        .unwrap();

    assert_eq!(result.event_count, 2);

    let events = collect_events(rx).await;
    match &events[0].change {
        SourceChange::Insert { element } => match element {
            Element::Relation {
                metadata,
                in_node,
                out_node,
                ..
            } => {
                assert_eq!(&*metadata.reference.element_id, "f1");
                assert_eq!(&*metadata.labels[0], "FOLLOWS");
                assert_eq!(&*in_node.element_id, "u1");
                assert_eq!(&*out_node.element_id, "u2");
            }
            _ => panic!("Expected Relation"),
        },
        _ => panic!("Expected Insert"),
    }
}

// ── Test: Page number pagination ────────────────────────────────────────────

#[tokio::test]
async fn test_page_number_pagination() {
    let all_items: Vec<JsonValue> = (1..=22)
        .map(|i| json!({"id": format!("{i}"), "title": format!("Item {i}")}))
        .collect();

    let items = Arc::new(all_items);

    let app = Router::new().route(
        "/items",
        get(move |Query(params): Query<HashMap<String, String>>| {
            let items = items.clone();
            async move {
                let page: usize = params.get("page").and_then(|v| v.parse().ok()).unwrap_or(1);
                let per_page: usize = params
                    .get("per_page")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(10);

                let start = (page - 1) * per_page;
                let page_items: Vec<&JsonValue> = items.iter().skip(start).take(per_page).collect();
                let total_pages = items.len().div_ceil(per_page);

                Json(json!({
                    "items": page_items,
                    "meta": {
                        "total_pages": total_pages,
                        "current_page": page
                    }
                }))
            }
        }),
    );

    let base_url = start_server(app).await;

    let config = HttpBootstrapConfig {
        endpoints: vec![EndpointConfig {
            url: format!("{base_url}/items"),
            method: HttpMethod::Get,
            headers: HashMap::new(),
            body: None,
            auth: None,
            pagination: Some(PaginationConfig::PageNumber {
                page_param: "page".to_string(),
                page_size_param: "per_page".to_string(),
                page_size: 10,
                total_pages_path: Some("$.meta.total_pages".to_string()),
            }),
            response: ResponseConfig {
                items_path: "$.items".to_string(),
                content_type: None,
                mappings: vec![ElementMappingConfig {
                    element_type: ElementType::Node,
                    template: ElementTemplate {
                        id: "{{item.id}}".to_string(),
                        labels: vec!["Item".to_string()],
                        properties: Some(json!({"title": "{{item.title}}"})),
                        from: None,
                        to: None,
                    },
                }],
            },
        }],
        timeout_seconds: 10,
        max_retries: 0,
        retry_delay_ms: 100,
    };

    let provider = HttpBootstrapProvider::new(config).unwrap();
    let context = test_context("test-source");
    let request = test_request(vec!["Item".to_string()], vec![]);

    let (tx, mut rx) = mpsc::channel(100);
    let result = provider
        .bootstrap(request, &context, tx, None)
        .await
        .unwrap();

    assert_eq!(result.event_count, 22);
    let events = collect_events(rx).await;
    assert_eq!(events.len(), 22);
}

// ── Test: Label filtering ───────────────────────────────────────────────────

#[tokio::test]
async fn test_label_filtering() {
    let app = Router::new().route(
        "/mixed",
        get(|| async {
            Json(json!([
                {"id": "1", "name": "Alice"},
                {"id": "2", "name": "Bob"},
                {"id": "3", "name": "Charlie"}
            ]))
        }),
    );

    let base_url = start_server(app).await;

    let config = HttpBootstrapConfig {
        endpoints: vec![EndpointConfig {
            url: format!("{base_url}/mixed"),
            method: HttpMethod::Get,
            headers: HashMap::new(),
            body: None,
            auth: None,
            pagination: None,
            response: ResponseConfig {
                items_path: "$".to_string(),
                content_type: None,
                mappings: vec![ElementMappingConfig {
                    element_type: ElementType::Node,
                    template: ElementTemplate {
                        id: "{{item.id}}".to_string(),
                        labels: vec!["Person".to_string()],
                        properties: Some(json!({"name": "{{item.name}}"})),
                        from: None,
                        to: None,
                    },
                }],
            },
        }],
        timeout_seconds: 10,
        max_retries: 0,
        retry_delay_ms: 100,
    };

    let provider = HttpBootstrapProvider::new(config).unwrap();
    let context = test_context("test-source");

    // Request only "Animal" labels - should filter out all "Person" nodes
    let request = test_request(vec!["Animal".to_string()], vec![]);

    let (tx, mut rx) = mpsc::channel(100);
    let result = provider
        .bootstrap(request, &context, tx, None)
        .await
        .unwrap();

    assert_eq!(result.event_count, 0);
}

// ── Test: Descriptor/DTO layer (config goes through full deserialization pipeline) ──

#[tokio::test]
async fn test_descriptor_creates_provider_from_json_config() {
    use drasi_bootstrap_http::descriptor::HttpBootstrapDescriptor;
    use drasi_plugin_sdk::BootstrapPluginDescriptor;

    let app = Router::new().route(
        "/users",
        get(|| async {
            Json(json!([
                {"id": "d1", "name": "DtoAlice"},
                {"id": "d2", "name": "DtoBob"}
            ]))
        }),
    );

    let base_url = start_server(app).await;

    // Build config as raw JSON (as the host framework would pass it)
    let config_json = json!({
        "endpoints": [{
            "url": format!("{base_url}/users"),
            "method": "GET",
            "response": {
                "itemsPath": "$",
                "mappings": [{
                    "elementType": "node",
                    "template": {
                        "id": "{{item.id}}",
                        "labels": ["User"],
                        "properties": {"name": "{{item.name}}"}
                    }
                }]
            }
        }],
        "timeoutSeconds": 10,
        "maxRetries": 0,
        "retryDelayMs": 100
    });

    let descriptor = HttpBootstrapDescriptor;
    let source_config = json!({});
    let provider = descriptor
        .create_bootstrap_provider(&config_json, &source_config)
        .await
        .expect("Descriptor should create provider from JSON config");

    let context = test_context("dto-source");
    let request = test_request(vec!["User".to_string()], vec![]);
    let (tx, mut rx) = mpsc::channel(100);
    let result = provider
        .bootstrap(request, &context, tx, None)
        .await
        .unwrap();

    assert_eq!(result.event_count, 2);
    let events = collect_events(rx).await;
    assert_eq!(events.len(), 2);
}

#[tokio::test]
async fn test_descriptor_rejects_unknown_fields() {
    use drasi_bootstrap_http::descriptor::HttpBootstrapDescriptor;
    use drasi_plugin_sdk::BootstrapPluginDescriptor;

    let config_json = json!({
        "endpoints": [{
            "url": "https://example.com/api",
            "response": {
                "itemsPath": "$",
                "mappings": [{
                    "elementType": "node",
                    "template": {
                        "id": "{{item.id}}",
                        "labels": ["Test"]
                    }
                }]
            }
        }],
        "bogusField": true
    });

    let descriptor = HttpBootstrapDescriptor;
    let source_config = json!({});
    let result = descriptor
        .create_bootstrap_provider(&config_json, &source_config)
        .await;

    assert!(result.is_err(), "Should reject unknown fields");
    let err = result.err().unwrap().to_string();
    assert!(
        err.contains("bogusField") || err.contains("unknown field"),
        "Error should mention the unknown field, got: {err}"
    );
}

#[tokio::test]
async fn test_descriptor_validation_catches_empty_url() {
    use drasi_bootstrap_http::descriptor::HttpBootstrapDescriptor;
    use drasi_plugin_sdk::BootstrapPluginDescriptor;

    let config_json = json!({
        "endpoints": [{
            "url": "",
            "response": {
                "itemsPath": "$",
                "mappings": [{
                    "elementType": "node",
                    "template": {
                        "id": "{{item.id}}",
                        "labels": ["Test"]
                    }
                }]
            }
        }]
    });

    let descriptor = HttpBootstrapDescriptor;
    let source_config = json!({});
    let result = descriptor
        .create_bootstrap_provider(&config_json, &source_config)
        .await;

    assert!(result.is_err(), "Should reject empty URL");
    let err = result.err().unwrap().to_string();
    assert!(
        err.contains("url cannot be empty"),
        "Error should mention empty URL, got: {err}"
    );
}

// ── Test: XML response parsing ──────────────────────────────────────────────
// Note: quick-xml's serde deserializer to serde_json::Value does not preserve
// repeated sibling elements as arrays (last element wins). XML responses that
// contain arrays should wrap items in unique keys or use a flat structure.

#[tokio::test]
async fn test_xml_response_parsing() {
    let app = Router::new().route(
        "/data.xml",
        get(|| async {
            // Use a single-item XML with flat text nodes
            Response::builder()
                .header(header::CONTENT_TYPE, "application/xml")
                .body(
                    r#"<user><id>x1</id><name>XmlAlice</name><email>alice@xml.com</email></user>"#
                        .to_string(),
                )
                .unwrap()
        }),
    );

    let base_url = start_server(app).await;

    let config = HttpBootstrapConfig {
        endpoints: vec![EndpointConfig {
            url: format!("{base_url}/data.xml"),
            method: HttpMethod::Get,
            headers: HashMap::new(),
            body: None,
            auth: None,
            pagination: None,
            response: ResponseConfig {
                // The root element is stripped by quick-xml, so the parsed JSON
                // is {"id": ..., "name": ..., "email": ...} — a single object.
                // extract_items wraps a single object in a vec.
                items_path: "$".to_string(),
                content_type: Some(ContentTypeOverride::Xml),
                mappings: vec![ElementMappingConfig {
                    element_type: ElementType::Node,
                    template: ElementTemplate {
                        id: "{{item.id}}".to_string(),
                        labels: vec!["XmlNode".to_string()],
                        properties: Some(
                            json!({"name": "{{item.name}}", "email": "{{item.email}}"}),
                        ),
                        from: None,
                        to: None,
                    },
                }],
            },
        }],
        timeout_seconds: 10,
        max_retries: 0,
        retry_delay_ms: 100,
    };

    let provider = HttpBootstrapProvider::new(config).unwrap();
    let context = test_context("xml-source");
    let request = test_request(vec!["XmlNode".to_string()], vec![]);

    let (tx, mut rx) = mpsc::channel(100);
    let result = provider
        .bootstrap(request, &context, tx, None)
        .await
        .unwrap();

    assert_eq!(result.event_count, 1);
    let events = collect_events(rx).await;
    assert_eq!(events.len(), 1);

    match &events[0].change {
        SourceChange::Insert { element } => match element {
            Element::Node { metadata, .. } => {
                assert_eq!(&*metadata.labels[0], "XmlNode");
            }
            _ => panic!("Expected Node"),
        },
        _ => panic!("Expected Insert"),
    }
}

// ── Test: YAML response parsing ─────────────────────────────────────────────

#[tokio::test]
async fn test_yaml_response_parsing() {
    let app = Router::new().route(
        "/data.yaml",
        get(|| async {
            Response::builder()
                .header(header::CONTENT_TYPE, "application/x-yaml")
                .body(
                    r#"- id: "y1"
  name: YamlAlice
- id: "y2"
  name: YamlBob
- id: "y3"
  name: YamlCharlie
"#
                    .to_string(),
                )
                .unwrap()
        }),
    );

    let base_url = start_server(app).await;

    let config = HttpBootstrapConfig {
        endpoints: vec![EndpointConfig {
            url: format!("{base_url}/data.yaml"),
            method: HttpMethod::Get,
            headers: HashMap::new(),
            body: None,
            auth: None,
            pagination: None,
            response: ResponseConfig {
                items_path: "$".to_string(),
                content_type: Some(ContentTypeOverride::Yaml),
                mappings: vec![ElementMappingConfig {
                    element_type: ElementType::Node,
                    template: ElementTemplate {
                        id: "{{item.id}}".to_string(),
                        labels: vec!["YamlNode".to_string()],
                        properties: Some(json!({"name": "{{item.name}}"})),
                        from: None,
                        to: None,
                    },
                }],
            },
        }],
        timeout_seconds: 10,
        max_retries: 0,
        retry_delay_ms: 100,
    };

    let provider = HttpBootstrapProvider::new(config).unwrap();
    let context = test_context("yaml-source");
    let request = test_request(vec!["YamlNode".to_string()], vec![]);

    let (tx, mut rx) = mpsc::channel(100);
    let result = provider
        .bootstrap(request, &context, tx, None)
        .await
        .unwrap();

    assert_eq!(result.event_count, 3);
    let events = collect_events(rx).await;
    assert_eq!(events.len(), 3);

    match &events[0].change {
        SourceChange::Insert { element } => match element {
            Element::Node {
                metadata,
                properties,
            } => {
                assert_eq!(&*metadata.labels[0], "YamlNode");
                let name = properties.get("name").expect("name property missing");
                match name {
                    drasi_core::models::ElementValue::String(s) => assert_eq!(&**s, "YamlAlice"),
                    other => panic!("Expected String, got {other:?}"),
                }
            }
            _ => panic!("Expected Node"),
        },
        _ => panic!("Expected Insert"),
    }
}

// ── Test: Type preservation through template engine ─────────────────────────

#[tokio::test]
async fn test_type_preservation_in_properties() {
    let app = Router::new().route(
        "/typed",
        get(|| async {
            Json(json!([
                {"id": "t1", "count": 42, "rate": 3.15, "active": true, "zip": "00123"}
            ]))
        }),
    );

    let base_url = start_server(app).await;

    let config = HttpBootstrapConfig {
        endpoints: vec![EndpointConfig {
            url: format!("{base_url}/typed"),
            method: HttpMethod::Get,
            headers: HashMap::new(),
            body: None,
            auth: None,
            pagination: None,
            response: ResponseConfig {
                items_path: "$".to_string(),
                content_type: None,
                mappings: vec![ElementMappingConfig {
                    element_type: ElementType::Node,
                    template: ElementTemplate {
                        id: "{{item.id}}".to_string(),
                        labels: vec!["TypedNode".to_string()],
                        properties: Some(json!({
                            "count": "{{item.count}}",
                            "rate": "{{item.rate}}",
                            "active": "{{item.active}}",
                            "zip": "{{item.zip}}"
                        })),
                        from: None,
                        to: None,
                    },
                }],
            },
        }],
        timeout_seconds: 10,
        max_retries: 0,
        retry_delay_ms: 100,
    };

    let provider = HttpBootstrapProvider::new(config).unwrap();
    let context = test_context("typed-source");
    let request = test_request(vec!["TypedNode".to_string()], vec![]);

    let (tx, mut rx) = mpsc::channel(100);
    let result = provider
        .bootstrap(request, &context, tx, None)
        .await
        .unwrap();
    assert_eq!(result.event_count, 1);

    let events = collect_events(rx).await;
    match &events[0].change {
        SourceChange::Insert { element } => match element {
            Element::Node { properties, .. } => {
                use drasi_core::models::ElementValue;
                use ordered_float::OrderedFloat;

                // Integer stays integer
                assert_eq!(properties.get("count"), Some(&ElementValue::Integer(42)));
                // Float stays float
                assert_eq!(
                    properties.get("rate"),
                    Some(&ElementValue::Float(OrderedFloat(3.15)))
                );
                // Bool stays bool
                assert_eq!(properties.get("active"), Some(&ElementValue::Bool(true)));
                // String "00123" stays string (not coerced to int 123)
                let zip = properties.get("zip").expect("zip property missing");
                match zip {
                    ElementValue::String(s) => assert_eq!(&**s, "00123"),
                    other => panic!("Expected zip to be String, got {other:?}"),
                }
            }
            _ => panic!("Expected Node"),
        },
        _ => panic!("Expected Insert"),
    }
}

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

//! End-to-end integration test: HTTP bootstrap → DrasiLib query engine → Cypher results.
//!
//! Verifies that data fetched by HttpBootstrapProvider actually flows through
//! the query engine and is queryable via `get_query_results()`.

use async_trait::async_trait;
use axum::{routing::get, Json, Router};
use drasi_bootstrap_http::config::*;
use drasi_bootstrap_http::HttpBootstrapProvider;
use drasi_lib::{
    wait_for_status, ComponentStatus, DispatchMode, DrasiLib, Query, Source, SourceBase,
    SourceBaseParams, SourceRuntimeContext, SourceSubscriptionSettings, SubscriptionResponse,
};
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::time::timeout;

// ============================================================================
// Test Source — minimal source that delegates bootstrap to its provider
// ============================================================================

struct BootstrapTestSource {
    base: Arc<SourceBase>,
}

impl BootstrapTestSource {
    fn new(id: &str, provider: HttpBootstrapProvider) -> anyhow::Result<Self> {
        let params = SourceBaseParams::new(id).with_bootstrap_provider(provider);
        let base = Arc::new(SourceBase::new(params)?);
        Ok(Self { base })
    }
}

#[async_trait]
impl Source for BootstrapTestSource {
    fn id(&self) -> &str {
        self.base.get_id()
    }

    fn type_name(&self) -> &str {
        "bootstrap-test"
    }

    fn properties(&self) -> HashMap<String, serde_json::Value> {
        HashMap::new()
    }

    fn auto_start(&self) -> bool {
        self.base.get_auto_start()
    }

    async fn start(&self) -> anyhow::Result<()> {
        self.base
            .set_status(ComponentStatus::Running, Some("Started".to_string()))
            .await;
        Ok(())
    }

    async fn stop(&self) -> anyhow::Result<()> {
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
            .subscribe_with_bootstrap(&settings, "bootstrap-test")
            .await
    }

    async fn deprovision(&self) -> anyhow::Result<()> {
        Ok(())
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn initialize(&self, context: SourceRuntimeContext) {
        self.base.initialize(context).await;
    }

    fn dispatch_mode(&self) -> DispatchMode {
        DispatchMode::Channel
    }
}

// ============================================================================
// Test helpers
// ============================================================================

#[allow(clippy::unwrap_used)]
async fn start_server(app: Router) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap(); // DevSkim: ignore DS137138
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://127.0.0.1:{}", addr.port()) // DevSkim: ignore DS137138
}

// ============================================================================
// E2E Tests
// ============================================================================

/// Verifies that nodes bootstrapped from an HTTP API are queryable via Cypher.
///
/// Flow: HTTP server → HttpBootstrapProvider → SourceBase → Query engine → get_query_results()
#[tokio::test]
async fn test_bootstrapped_nodes_queryable_via_cypher() {
    let _ = env_logger::try_init();

    // Start a test HTTP server returning 3 User nodes
    let app = Router::new().route(
        "/users",
        get(|| async {
            Json(json!([
                {"id": "u1", "name": "Alice", "age": 30},
                {"id": "u2", "name": "Bob", "age": 25},
                {"id": "u3", "name": "Charlie", "age": 35}
            ]))
        }),
    );
    let base_url = start_server(app).await;

    // Create HttpBootstrapProvider pointing at our test server
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
                            "age": "{{item.age}}"
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
    let source = BootstrapTestSource::new("http-src", provider).unwrap();

    // Build DrasiLib with source + query
    let drasi = DrasiLib::builder()
        .with_source(source)
        .with_query(
            Query::cypher("user-query")
                .query("MATCH (u:User) RETURN u.name AS name, u.age AS age")
                .from_source("http-src")
                .enable_bootstrap(true)
                .build(),
        )
        .build()
        .await
        .unwrap();

    drasi.start().await.unwrap();

    // Wait for components to be running
    wait_for_status(
        &drasi.component_graph(),
        "http-src",
        &[ComponentStatus::Running],
        Duration::from_secs(5),
    )
    .await
    .unwrap();
    wait_for_status(
        &drasi.component_graph(),
        "user-query",
        &[ComponentStatus::Running],
        Duration::from_secs(5),
    )
    .await
    .unwrap();

    // Query results should contain the 3 bootstrapped users
    let results = timeout(Duration::from_secs(10), async {
        loop {
            match drasi.get_query_results("user-query").await {
                Ok(results) if results.len() == 3 => return results,
                _ => tokio::time::sleep(Duration::from_millis(100)).await,
            }
        }
    })
    .await
    .expect("Timed out waiting for 3 bootstrapped query results");

    // Collect names and sort for deterministic assertion
    let mut names: Vec<String> = results
        .iter()
        .filter_map(|r| r.get("name").and_then(|v| v.as_str()).map(String::from))
        .collect();
    names.sort();

    assert_eq!(names, vec!["Alice", "Bob", "Charlie"]);

    let _ = drasi.stop().await;
}

/// Verifies that paginated HTTP bootstrap data flows through to query results.
///
/// Uses offset/limit pagination across 3 pages to bootstrap 15 items total.
#[tokio::test]
async fn test_paginated_bootstrap_queryable_via_cypher() {
    let _ = env_logger::try_init();

    // Generate 15 items, serve in pages of 5
    let all_items: Vec<serde_json::Value> = (1..=15)
        .map(|i| json!({"id": format!("p{i}"), "title": format!("Product {i}"), "price": i * 10}))
        .collect();

    let items = Arc::new(all_items);

    let app = Router::new().route(
        "/products",
        get({
            let items = items.clone();
            move |query: axum::extract::Query<HashMap<String, String>>| {
                let items = items.clone();
                async move {
                    let offset: usize = query
                        .get("offset")
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(0);
                    let limit: usize = query.get("limit").and_then(|v| v.parse().ok()).unwrap_or(5);
                    let page: Vec<_> = items.iter().skip(offset).take(limit).cloned().collect();
                    Json(json!({
                        "items": page,
                        "total": items.len()
                    }))
                }
            }
        }),
    );
    let base_url = start_server(app).await;

    let config = HttpBootstrapConfig {
        endpoints: vec![EndpointConfig {
            url: format!("{base_url}/products"),
            method: HttpMethod::Get,
            headers: HashMap::new(),
            body: None,
            auth: None,
            pagination: Some(PaginationConfig::OffsetLimit {
                offset_param: "offset".to_string(),
                limit_param: "limit".to_string(),
                page_size: 5,
                total_path: Some("$.total".to_string()),
            }),
            response: ResponseConfig {
                items_path: "$.items".to_string(),
                content_type: None,
                mappings: vec![ElementMappingConfig {
                    element_type: ElementType::Node,
                    template: ElementTemplate {
                        id: "{{item.id}}".to_string(),
                        labels: vec!["Product".to_string()],
                        properties: Some(json!({
                            "title": "{{item.title}}",
                            "price": "{{item.price}}"
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
    let source = BootstrapTestSource::new("product-src", provider).unwrap();

    let drasi = DrasiLib::builder()
        .with_source(source)
        .with_query(
            Query::cypher("product-query")
                .query("MATCH (p:Product) RETURN p.title AS title")
                .from_source("product-src")
                .enable_bootstrap(true)
                .build(),
        )
        .build()
        .await
        .unwrap();

    drasi.start().await.unwrap();

    wait_for_status(
        &drasi.component_graph(),
        "product-src",
        &[ComponentStatus::Running],
        Duration::from_secs(5),
    )
    .await
    .unwrap();
    wait_for_status(
        &drasi.component_graph(),
        "product-query",
        &[ComponentStatus::Running],
        Duration::from_secs(5),
    )
    .await
    .unwrap();

    // All 15 products should be queryable
    let results = timeout(Duration::from_secs(10), async {
        loop {
            match drasi.get_query_results("product-query").await {
                Ok(results) if results.len() == 15 => return results,
                _ => tokio::time::sleep(Duration::from_millis(100)).await,
            }
        }
    })
    .await
    .expect("Timed out waiting for 15 paginated query results");

    assert_eq!(results.len(), 15);

    // Verify some specific titles exist
    let titles: Vec<String> = results
        .iter()
        .filter_map(|r| r.get("title").and_then(|v| v.as_str()).map(String::from))
        .collect();
    assert!(titles.contains(&"Product 1".to_string()));
    assert!(titles.contains(&"Product 15".to_string()));

    let _ = drasi.stop().await;
}

/// Verifies that relationships bootstrapped from HTTP are queryable via Cypher traversal.
///
/// Bootstraps User nodes and KNOWS relationships, then queries the graph pattern.
#[tokio::test]
async fn test_bootstrapped_relationships_queryable_via_cypher() {
    let _ = env_logger::try_init();

    let app = Router::new()
        .route(
            "/users",
            get(|| async {
                Json(json!([
                    {"id": "u1", "name": "Alice"},
                    {"id": "u2", "name": "Bob"}
                ]))
            }),
        )
        .route(
            "/friendships",
            get(|| async {
                Json(json!([
                    {"id": "f1", "from_user": "u1", "to_user": "u2"}
                ]))
            }),
        );
    let base_url = start_server(app).await;

    let config = HttpBootstrapConfig {
        endpoints: vec![
            // Endpoint 1: User nodes
            EndpointConfig {
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
                                "name": "{{item.name}}"
                            })),
                            from: None,
                            to: None,
                        },
                    }],
                },
            },
            // Endpoint 2: KNOWS relationships
            EndpointConfig {
                url: format!("{base_url}/friendships"),
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
                            labels: vec!["KNOWS".to_string()],
                            properties: None,
                            from: Some("{{item.from_user}}".to_string()),
                            to: Some("{{item.to_user}}".to_string()),
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
    let source = BootstrapTestSource::new("social-src", provider).unwrap();

    let drasi = DrasiLib::builder()
        .with_source(source)
        .with_query(
            Query::cypher("friends-query")
                .query(
                    "MATCH (a:User)-[:KNOWS]->(b:User) \
                     RETURN a.name AS person, b.name AS friend",
                )
                .from_source("social-src")
                .enable_bootstrap(true)
                .build(),
        )
        .build()
        .await
        .unwrap();

    drasi.start().await.unwrap();

    wait_for_status(
        &drasi.component_graph(),
        "social-src",
        &[ComponentStatus::Running],
        Duration::from_secs(5),
    )
    .await
    .unwrap();
    wait_for_status(
        &drasi.component_graph(),
        "friends-query",
        &[ComponentStatus::Running],
        Duration::from_secs(5),
    )
    .await
    .unwrap();

    // The traversal query should find Alice -> Bob
    let results = timeout(Duration::from_secs(10), async {
        loop {
            match drasi.get_query_results("friends-query").await {
                Ok(results) if !results.is_empty() => return results,
                _ => tokio::time::sleep(Duration::from_millis(100)).await,
            }
        }
    })
    .await
    .expect("Timed out waiting for relationship query results");

    assert_eq!(results.len(), 1);
    assert_eq!(
        results[0].get("person").and_then(|v| v.as_str()),
        Some("Alice")
    );
    assert_eq!(
        results[0].get("friend").and_then(|v| v.as_str()),
        Some("Bob")
    );

    let _ = drasi.stop().await;
}

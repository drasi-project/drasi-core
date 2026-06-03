// Copyright 2026 The Drasi Authors.
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

//! End-to-end tests for the unified HTTP reaction against a wiremock
//! server. Covers both standard per-result delivery and adaptive
//! coalesced batching, including the render-error fallback path.

mod mock_server;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use drasi_lib::channels::{QueryResult, ResultDiff};
use drasi_lib::Reaction;
use drasi_reaction_http::{
    AdaptiveBatchConfig, HttpCallExt, HttpQueryConfig, HttpReaction, TemplateSpec,
};
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

fn make_query_result(query_id: &str, diffs: Vec<ResultDiff>) -> QueryResult {
    QueryResult::new(query_id.to_string(), 0, Utc::now(), diffs, HashMap::new())
}

async fn enqueue_add(r: &HttpReaction, query_id: &str, data: serde_json::Value) {
    let qr = make_query_result(
        query_id,
        vec![ResultDiff::Add {
            data,
            row_signature: 0,
        }],
    );
    r.enqueue_query_result(qr).await.expect("enqueue");
}

async fn enqueue_update(
    r: &HttpReaction,
    query_id: &str,
    before: serde_json::Value,
    after: serde_json::Value,
) {
    let qr = make_query_result(
        query_id,
        vec![ResultDiff::Update {
            data: json!({}),
            before,
            after,
            grouping_keys: None,
            row_signature: 0,
        }],
    );
    r.enqueue_query_result(qr).await.expect("enqueue");
}

async fn enqueue_delete(r: &HttpReaction, query_id: &str, data: serde_json::Value) {
    let qr = make_query_result(
        query_id,
        vec![ResultDiff::Delete {
            data,
            row_signature: 0,
        }],
    );
    r.enqueue_query_result(qr).await.expect("enqueue");
}

/// Poll until `expected` requests have been observed, failing the test (with
/// the observed count) if the deadline elapses first. This gives a clear
/// synchronization point instead of silently returning on timeout.
async fn wait_for_requests(server: &wiremock::MockServer, expected: usize, max_ms: u64) {
    let deadline = std::time::Instant::now() + Duration::from_millis(max_ms);
    loop {
        let count = server.received_requests().await.unwrap_or_default().len();
        if count >= expected {
            return;
        }
        if std::time::Instant::now() >= deadline {
            panic!(
                "timed out after {max_ms}ms waiting for {expected} request(s); observed {count}"
            );
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

// ---------------------------------------------------------------------------
// Standard mode
// ---------------------------------------------------------------------------

#[tokio::test]
async fn standard_default_fallback_posts_to_changes_query() {
    let server = mock_server::start().await;
    Mock::given(method("POST"))
        .and(path("/changes/q1"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let r = Arc::new(
        HttpReaction::builder("standard-default")
            .with_base_url(server.uri())
            .with_query("q1")
            .build()
            .unwrap(),
    );
    r.start().await.unwrap();
    enqueue_add(&r, "q1", json!({"id": 1})).await;
    wait_for_requests(&server, 1, 2000).await;
    r.stop().await.unwrap();

    let reqs = server.received_requests().await.unwrap();
    assert!(!reqs.is_empty(), "expected at least one request");
    assert_eq!(reqs[0].url.path(), "/changes/q1");
}

#[tokio::test]
async fn standard_uses_per_query_template() {
    let server = mock_server::start().await;
    Mock::given(method("PUT"))
        .and(path("/items/42"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let mut headers = HashMap::new();
    headers.insert("X-Trace".to_string(), "trace-{{after.id}}".to_string());
    let spec = TemplateSpec {
        template: r#"{"id":{{after.id}}}"#.to_string(),
        extension: HttpCallExt {
            url: "/items/{{after.id}}".to_string(),
            method: "PUT".to_string(),
            headers,
        },
    };
    let r = Arc::new(
        HttpReaction::builder("standard-template")
            .with_base_url(server.uri())
            .with_query("q1")
            .with_query_template(
                "q1",
                HttpQueryConfig {
                    added: Some(spec),
                    ..Default::default()
                },
            )
            .build()
            .unwrap(),
    );
    r.start().await.unwrap();
    enqueue_add(&r, "q1", json!({"id": 42})).await;
    wait_for_requests(&server, 1, 2000).await;
    r.stop().await.unwrap();

    let reqs = server.received_requests().await.unwrap();
    assert_eq!(reqs[0].url.path(), "/items/42");
    assert_eq!(reqs[0].method.as_str(), "PUT");
    assert_eq!(
        reqs[0].headers.get("x-trace").map(|v| v.to_str().unwrap()),
        Some("trace-42")
    );
    let body = std::str::from_utf8(&reqs[0].body).unwrap();
    assert_eq!(body, r#"{"id":42}"#);
}

#[tokio::test]
async fn standard_render_error_falls_back_to_changes_query() {
    let server = mock_server::start().await;
    Mock::given(method("POST"))
        .and(path("/changes/q1"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    // `{{unknownhelper}}` is not registered → handlebars returns an error.
    let bad_spec = TemplateSpec {
        template: "{{unknownhelper after.id}}".to_string(),
        extension: HttpCallExt {
            url: "/items".to_string(),
            method: "POST".to_string(),
            headers: HashMap::new(),
        },
    };
    let r = Arc::new(
        HttpReaction::builder("standard-fallback")
            .with_base_url(server.uri())
            .with_query("q1")
            .with_query_template(
                "q1",
                HttpQueryConfig {
                    added: Some(bad_spec),
                    ..Default::default()
                },
            )
            .build()
            .unwrap(),
    );
    r.start().await.unwrap();
    enqueue_add(&r, "q1", json!({"id": 1})).await;
    wait_for_requests(&server, 1, 2000).await;
    r.stop().await.unwrap();

    let reqs = server.received_requests().await.unwrap();
    assert!(!reqs.is_empty(), "fallback should have produced a request");
    assert_eq!(reqs[0].url.path(), "/changes/q1");
}

#[tokio::test]
async fn standard_authorization_header_sent_when_token_set() {
    let server = mock_server::start().await;
    Mock::given(method("POST"))
        .and(path("/changes/q1"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let r = Arc::new(
        HttpReaction::builder("standard-auth")
            .with_base_url(server.uri())
            .with_token("xyz")
            .with_query("q1")
            .build()
            .unwrap(),
    );
    r.start().await.unwrap();
    enqueue_add(&r, "q1", json!({"id": 1})).await;
    wait_for_requests(&server, 1, 2000).await;
    r.stop().await.unwrap();

    let reqs = server.received_requests().await.unwrap();
    assert_eq!(
        reqs[0]
            .headers
            .get("authorization")
            .map(|v| v.to_str().unwrap()),
        Some("Bearer xyz")
    );
}

// ---------------------------------------------------------------------------
// UPDATE / DELETE context building
// ---------------------------------------------------------------------------

#[tokio::test]
async fn standard_update_template_receives_before_and_after() {
    let server = mock_server::start().await;
    Mock::given(method("PUT"))
        .and(path("/items/1"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let spec = TemplateSpec {
        template: r#"{"old":"{{before.v}}","new":"{{after.v}}"}"#.to_string(),
        extension: HttpCallExt {
            url: "/items/{{after.id}}".to_string(),
            method: "PUT".to_string(),
            headers: HashMap::new(),
        },
    };
    let r = Arc::new(
        HttpReaction::builder("standard-update")
            .with_base_url(server.uri())
            .with_query("q1")
            .with_query_template(
                "q1",
                HttpQueryConfig {
                    updated: Some(spec),
                    ..Default::default()
                },
            )
            .build()
            .unwrap(),
    );
    r.start().await.unwrap();
    enqueue_update(
        &r,
        "q1",
        json!({"id":1,"v":"old"}),
        json!({"id":1,"v":"new"}),
    )
    .await;
    wait_for_requests(&server, 1, 2000).await;
    r.stop().await.unwrap();

    let reqs = server.received_requests().await.unwrap();
    assert_eq!(reqs[0].url.path(), "/items/1");
    assert_eq!(reqs[0].method.as_str(), "PUT");
    let body = std::str::from_utf8(&reqs[0].body).unwrap();
    assert_eq!(body, r#"{"old":"old","new":"new"}"#);
}

#[tokio::test]
async fn standard_delete_template_receives_before() {
    let server = mock_server::start().await;
    Mock::given(method("DELETE"))
        .and(path("/items/7"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let spec = TemplateSpec {
        template: r#"{"gone":{{before.id}}}"#.to_string(),
        extension: HttpCallExt {
            url: "/items/{{before.id}}".to_string(),
            method: "DELETE".to_string(),
            headers: HashMap::new(),
        },
    };
    let r = Arc::new(
        HttpReaction::builder("standard-delete")
            .with_base_url(server.uri())
            .with_query("q1")
            .with_query_template(
                "q1",
                HttpQueryConfig {
                    deleted: Some(spec),
                    ..Default::default()
                },
            )
            .build()
            .unwrap(),
    );
    r.start().await.unwrap();
    enqueue_delete(&r, "q1", json!({"id":7})).await;
    wait_for_requests(&server, 1, 2000).await;
    r.stop().await.unwrap();

    let reqs = server.received_requests().await.unwrap();
    assert_eq!(reqs[0].url.path(), "/items/7");
    assert_eq!(reqs[0].method.as_str(), "DELETE");
    let body = std::str::from_utf8(&reqs[0].body).unwrap();
    assert_eq!(body, r#"{"gone":7}"#);
}

// ---------------------------------------------------------------------------
// Server error handling
// ---------------------------------------------------------------------------

#[tokio::test]
async fn standard_continues_after_server_error_response() {
    let server = mock_server::start().await;
    Mock::given(method("POST"))
        .and(path("/changes/q1"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;

    let r = Arc::new(
        HttpReaction::builder("standard-5xx")
            .with_base_url(server.uri())
            .with_query("q1")
            .build()
            .unwrap(),
    );
    r.start().await.unwrap();

    // First result gets a 500; the reaction should log and keep running.
    enqueue_add(&r, "q1", json!({"id": 1})).await;
    wait_for_requests(&server, 1, 2000).await;

    // A second result must still be delivered (the loop did not exit).
    enqueue_add(&r, "q1", json!({"id": 2})).await;
    wait_for_requests(&server, 2, 2000).await;
    r.stop().await.unwrap();

    let reqs = server.received_requests().await.unwrap();
    assert!(
        reqs.len() >= 2,
        "reaction should keep processing after a 5xx response, got {} requests",
        reqs.len()
    );
}

// ---------------------------------------------------------------------------
// Adaptive mode
// ---------------------------------------------------------------------------

#[tokio::test]
async fn adaptive_without_batch_endpoint_uses_per_route_delivery() {
    let server = mock_server::start().await;
    Mock::given(method("POST"))
        .and(path("/changes/q1"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let r = Arc::new(
        HttpReaction::builder("adaptive-no-batch")
            .with_base_url(server.uri())
            .with_query("q1")
            .with_adaptive(AdaptiveBatchConfig {
                adaptive_min_batch_size: 1,
                adaptive_max_batch_size: 16,
                adaptive_window_size: 10,
                adaptive_batch_timeout_ms: 50,
            })
            .build()
            .unwrap(),
    );
    r.start().await.unwrap();
    for i in 0..3 {
        enqueue_add(&r, "q1", json!({"id": i})).await;
    }
    wait_for_requests(&server, 3, 3000).await;
    r.stop().await.unwrap();

    let reqs = server.received_requests().await.unwrap();
    assert!(reqs.len() >= 3, "expected at least 3 per-route requests");
}

#[tokio::test]
async fn adaptive_with_batch_endpoint_posts_to_batch_when_multi() {
    let server = mock_server::start().await;
    Mock::given(method("POST"))
        .and(path("/batch"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let r = Arc::new(
        HttpReaction::builder("adaptive-batch")
            .with_base_url(server.uri())
            .with_query("q1")
            .with_adaptive(AdaptiveBatchConfig {
                adaptive_min_batch_size: 3,
                adaptive_max_batch_size: 16,
                adaptive_window_size: 10,
                adaptive_batch_timeout_ms: 100,
            })
            .with_batch_endpoint("/batch")
            .build()
            .unwrap(),
    );
    r.start().await.unwrap();

    // Send a burst quickly so the batcher coalesces into a multi-item batch.
    let qr = make_query_result(
        "q1",
        (0..5)
            .map(|i| ResultDiff::Add {
                data: json!({"id": i}),
                row_signature: 0,
            })
            .collect(),
    );
    r.enqueue_query_result(qr).await.expect("enqueue");

    wait_for_requests(&server, 1, 3000).await;
    r.stop().await.unwrap();

    let reqs = server.received_requests().await.unwrap();
    assert!(!reqs.is_empty(), "expected a batch POST");
    let batch_request = reqs
        .iter()
        .find(|r| r.url.path() == "/batch")
        .expect("expected a /batch POST");
    let body: serde_json::Value = serde_json::from_slice(&batch_request.body).unwrap();
    assert!(body.is_array(), "batch body should be an array");
    let arr = body.as_array().unwrap();
    assert!(!arr.is_empty());
    let first = &arr[0];
    assert_eq!(first.get("query_id"), Some(&json!("q1")));
    assert!(first.get("count").and_then(|c| c.as_u64()).unwrap_or(0) >= 1);
}

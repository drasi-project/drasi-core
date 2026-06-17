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

//! End-to-end tests for the HTTP reaction against a wiremock
//! server. Covers both standard per-result delivery and adaptive
//! coalesced batching, including the render-error fallback path.

mod mock_server;
mod mock_source;

use anyhow::Result;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use drasi_lib::channels::{QueryResult, ResultDiff};
use drasi_lib::Reaction;
use drasi_lib::{DrasiLib, Query};
use drasi_reaction_http::{
    AdaptiveBatchConfig, HttpCallExt, HttpQueryConfig, HttpReaction, TemplateSpec,
};
use mock_source::{MockSource, PropertyMapBuilder};
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

#[tokio::test]
async fn end_to_end_through_drasilib_delivers_to_http_server() -> Result<()> {
    let server = mock_server::start().await;
    Mock::given(method("POST"))
        .and(path("/changes/people-query"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let (mock_source, handle) = MockSource::new("test-source")?;
    let query = Query::cypher("people-query")
        .query(
            r#"
            MATCH (p:Person)
            RETURN p.name AS name, p.age AS age
            "#,
        )
        .from_source("test-source")
        .auto_start(true)
        .build();
    let reaction = HttpReaction::builder("http-e2e")
        .with_base_url(server.uri())
        .with_query("people-query")
        .build()?;

    let core = Arc::new(
        DrasiLib::builder()
            .with_id("http-e2e-core")
            .with_source(mock_source)
            .with_query(query)
            .with_reaction(reaction)
            .build()
            .await?,
    );

    core.start().await?;
    tokio::time::sleep(Duration::from_millis(500)).await;

    let props = PropertyMapBuilder::new()
        .with_string("name", "Ada")
        .with_integer("age", 36)
        .build();
    handle
        .send_node_insert("person-1", vec!["Person"], props)
        .await?;

    wait_for_requests(&server, 1, 5000).await;
    core.stop().await?;

    let reqs = server.received_requests().await.unwrap();
    assert_eq!(reqs[0].url.path(), "/changes/people-query");
    let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
    assert_eq!(body.get("operation"), Some(&json!("ADD")));
    assert_eq!(body.get("queryId"), Some(&json!("people-query")));
    assert_eq!(
        body.get("after").and_then(|a| a.get("name")),
        Some(&json!("Ada"))
    );

    Ok(())
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
    assert_eq!(reqs[0].method.as_str(), "POST");
    assert_eq!(
        reqs[0]
            .headers
            .get("content-type")
            .map(|v| v.to_str().unwrap()),
        Some("application/json")
    );
    // With no template, the body is the DefaultChangeNotification envelope.
    let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
    assert_eq!(body.get("operation"), Some(&json!("ADD")));
    assert_eq!(body.get("queryId"), Some(&json!("q1")));
    assert_eq!(body.get("after"), Some(&json!({"id": 1})));
    assert!(body.get("before").is_none(), "ADD must omit 'before'");
    let ts = body
        .get("timestamp")
        .and_then(|t| t.as_str())
        .expect("timestamp must be present and a string");
    chrono::DateTime::parse_from_rfc3339(ts).expect("timestamp must be RFC 3339");
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
async fn standard_body_render_error_uses_default_envelope_on_configured_route() {
    let server = mock_server::start().await;
    Mock::given(method("POST"))
        .and(path("/items"))
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
    assert_eq!(
        reqs[0].url.path(),
        "/items",
        "body render failure keeps the configured URL"
    );
    let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
    assert_eq!(body.get("operation"), Some(&json!("ADD")));
    assert_eq!(body.get("queryId"), Some(&json!("q1")));
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
    wait_for_requests(&server, 4, 3000).await;
    r.stop().await.unwrap();

    let reqs = server.received_requests().await.unwrap();
    assert!(
        reqs.iter().any(|req| {
            serde_json::from_slice::<serde_json::Value>(&req.body)
                .ok()
                .and_then(|body| body.get("after").and_then(|after| after.get("id")).cloned())
                == Some(json!(2))
        }),
        "reaction should process the second event after a 5xx response, got {} requests",
        reqs.len()
    );
}

// ---------------------------------------------------------------------------
// Adaptive mode
// ---------------------------------------------------------------------------

#[tokio::test]
async fn adaptive_without_batch_endpoint_is_rejected() {
    let err = HttpReaction::builder("adaptive-no-batch")
        .with_base_url("http://localhost")
        .with_query("q1")
        .with_adaptive(AdaptiveBatchConfig {
            adaptive_min_batch_size: 1,
            adaptive_max_batch_size: 16,
            adaptive_window_size: 10,
            adaptive_batch_timeout_ms: 50,
        })
        .build()
        .err()
        .expect("adaptive mode should require batchEndpoint");
    assert!(
        err.to_string().contains("batchEndpoint"),
        "error should mention batchEndpoint: {err}"
    );
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
                adaptive_min_batch_size: 5,
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
    assert_eq!(reqs.len(), 1, "expected exactly one batch POST");
    let batch_request = &reqs[0];
    assert_eq!(batch_request.url.path(), "/batch");
    assert_eq!(batch_request.method.as_str(), "POST");
    assert_eq!(
        batch_request
            .headers
            .get("content-type")
            .map(|v| v.to_str().unwrap()),
        Some("application/json")
    );

    let body: serde_json::Value = serde_json::from_slice(&batch_request.body).unwrap();
    let batch = body
        .get("batch")
        .and_then(|b| b.as_array())
        .expect("batch body should be a { \"batch\": [...] } container (Pattern C)");
    // One QueryResult with 5 diffs → 5 Pattern A items inside the single
    // batch container.
    assert_eq!(batch.len(), 5, "expected 5 coalesced items");
    assert!(
        body.get("queryId").is_none() && body.get("results").is_none(),
        "the Pattern C container has only the 'batch' key"
    );

    for (i, item) in batch.iter().enumerate() {
        assert_eq!(
            item.get("operation"),
            Some(&json!("ADD")),
            "item {i} should be ADD"
        );
        assert_eq!(
            item.get("queryId"),
            Some(&json!("q1")),
            "item {i} should carry queryId"
        );
        assert_eq!(
            item.get("sequenceId"),
            Some(&json!(0)),
            "item {i} should carry sequenceId"
        );
        assert_eq!(
            item.get("after").and_then(|d| d.get("id")),
            Some(&json!(i as i64)),
            "item {i} should carry after.id={i}"
        );
        assert!(
            item.get("before").is_none(),
            "item {i} (ADD) should omit 'before'"
        );
        assert!(
            item.get("query_id").is_none(),
            "snake_case keys must be gone"
        );
    }
}

// ---------------------------------------------------------------------------
// Adaptive partial-batch flush on timeout
// ---------------------------------------------------------------------------

/// When the inbound rate is below `min_batch_size`, the batcher must still
/// flush a partial batch once `max_wait_time` (the `adaptive_batch_timeout_ms`
/// from config) elapses. Deterministic: a single diff is sent and we wait
/// for one HTTP request to land.
#[tokio::test]
async fn adaptive_flushes_partial_batch_on_timeout_below_min_size() {
    let server = mock_server::start().await;
    Mock::given(method("POST"))
        .and(path("/batch"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let r = Arc::new(
        HttpReaction::builder("adaptive-partial-flush")
            .with_base_url(server.uri())
            .with_query("q1")
            .with_adaptive(AdaptiveBatchConfig {
                // min is intentionally larger than what we will send, so the
                // batch can only be emitted via the timeout path.
                adaptive_min_batch_size: 10,
                adaptive_max_batch_size: 100,
                adaptive_window_size: 10,
                adaptive_batch_timeout_ms: 100,
            })
            .with_batch_endpoint("/batch")
            .build()
            .unwrap(),
    );
    r.start().await.unwrap();

    enqueue_add(&r, "q1", json!({"id": 1})).await;

    // 2 s is generous relative to the 100 ms timeout, eliminating flakiness
    // from CI scheduler jitter while still detecting a real regression.
    wait_for_requests(&server, 1, 2000).await;
    r.stop().await.unwrap();

    let reqs = server.received_requests().await.unwrap();
    assert_eq!(
        reqs.len(),
        1,
        "the partial batch should be flushed exactly once"
    );
    assert_eq!(reqs[0].url.path(), "/batch");
    let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
    let batch = body
        .get("batch")
        .and_then(|b| b.as_array())
        .expect("partial flush should still use BatchEnvelope");
    assert_eq!(batch.len(), 1);
    assert_eq!(batch[0].get("operation"), Some(&json!("ADD")));
    assert_eq!(batch[0].get("queryId"), Some(&json!("q1")));
    assert_eq!(batch[0].get("after"), Some(&json!({"id": 1})));
}

// ---------------------------------------------------------------------------
// SSRF guard
// ---------------------------------------------------------------------------

/// `process_result` rejects a rendered absolute URL whose host does not match
/// the configured `base_url` host (SSRF guard in process.rs). The reaction
/// must log and continue without producing an outbound request.
///
/// The test pairs the SSRF case with a safe relative-URL case in the *same*
/// `QueryResult`. Because the standard loop iterates result diffs in order,
/// the successful safe request acts as a synchronization barrier proving the
/// SSRF case has already been processed and rejected.
#[tokio::test]
async fn ssrf_guard_blocks_absolute_url_with_mismatched_host() {
    let server = mock_server::start().await;
    Mock::given(method("POST"))
        .and(path("/safe"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    // ADD template renders an absolute URL pointing at a different host —
    // must be blocked by the SSRF guard.
    let ssrf_add = TemplateSpec {
        template: r#"{}"#.to_string(),
        extension: HttpCallExt {
            url: "http://evil.example.com/x".to_string(),
            method: "POST".to_string(),
            headers: HashMap::new(),
        },
    };
    // DELETE template renders a safe relative URL — must be delivered, and
    // serves as the synchronization barrier for the test.
    let safe_delete = TemplateSpec {
        template: r#"{}"#.to_string(),
        extension: HttpCallExt {
            url: "/safe".to_string(),
            method: "POST".to_string(),
            headers: HashMap::new(),
        },
    };

    let r = Arc::new(
        HttpReaction::builder("ssrf-guard")
            .with_base_url(server.uri())
            .with_query("q1")
            .with_query_template(
                "q1",
                HttpQueryConfig {
                    added: Some(ssrf_add),
                    deleted: Some(safe_delete),
                    ..Default::default()
                },
            )
            .build()
            .unwrap(),
    );
    r.start().await.unwrap();

    // Single QueryResult with two diffs guarantees in-order processing by the
    // standard loop: SSRF first (rejected), then the safe Delete (delivered).
    let qr = make_query_result(
        "q1",
        vec![
            ResultDiff::Add {
                data: json!({"id": 1}),
                row_signature: 0,
            },
            ResultDiff::Delete {
                data: json!({"id": 1}),
                row_signature: 0,
            },
        ],
    );
    r.enqueue_query_result(qr).await.expect("enqueue");

    wait_for_requests(&server, 1, 2000).await;
    r.stop().await.unwrap();

    let reqs = server.received_requests().await.unwrap();
    assert_eq!(
        reqs.len(),
        1,
        "SSRF request must be blocked; only the safe request should land"
    );
    assert_eq!(reqs[0].url.path(), "/safe");
}

// ---------------------------------------------------------------------------
// Default-envelope coverage: DELETE / UPDATE / Aggregation / Noop
// ---------------------------------------------------------------------------
//
// These tests lock down the shape of the DefaultChangeNotification envelope
// (schema/output.schema.json) emitted by the default fallback path for every
// ResultDiff variant. They are the wire-format counterpart to the unit tests
// in src/output.rs and guard against silent regressions in standard_loop or
// adaptive_loop.

#[tokio::test]
async fn default_envelope_delete_carries_before_only() {
    let server = mock_server::start().await;
    Mock::given(method("POST"))
        .and(path("/changes/q1"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let r = Arc::new(
        HttpReaction::builder("default-delete")
            .with_base_url(server.uri())
            .with_query("q1")
            .build()
            .unwrap(),
    );
    r.start().await.unwrap();
    enqueue_delete(&r, "q1", json!({"id": 7})).await;
    wait_for_requests(&server, 1, 2000).await;
    r.stop().await.unwrap();

    let reqs = server.received_requests().await.unwrap();
    assert_eq!(reqs.len(), 1);
    let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
    assert_eq!(body.get("operation"), Some(&json!("DELETE")));
    assert_eq!(body.get("queryId"), Some(&json!("q1")));
    assert_eq!(body.get("before"), Some(&json!({"id": 7})));
    assert!(body.get("after").is_none(), "DELETE must omit 'after'");
}

#[tokio::test]
async fn default_envelope_update_carries_before_and_after() {
    let server = mock_server::start().await;
    Mock::given(method("POST"))
        .and(path("/changes/q1"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let r = Arc::new(
        HttpReaction::builder("default-update")
            .with_base_url(server.uri())
            .with_query("q1")
            .build()
            .unwrap(),
    );
    r.start().await.unwrap();
    enqueue_update(
        &r,
        "q1",
        json!({"id": 1, "v": "old"}),
        json!({"id": 1, "v": "new"}),
    )
    .await;
    wait_for_requests(&server, 1, 2000).await;
    r.stop().await.unwrap();

    let reqs = server.received_requests().await.unwrap();
    assert_eq!(reqs.len(), 1);
    let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
    assert_eq!(body.get("operation"), Some(&json!("UPDATE")));
    assert_eq!(body.get("queryId"), Some(&json!("q1")));
    assert_eq!(body.get("before"), Some(&json!({"id": 1, "v": "old"})));
    assert_eq!(body.get("after"), Some(&json!({"id": 1, "v": "new"})));
}

#[tokio::test]
async fn default_envelope_aggregation_first_emission_maps_to_update_without_before() {
    let server = mock_server::start().await;
    Mock::given(method("POST"))
        .and(path("/changes/q1"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let r = Arc::new(
        HttpReaction::builder("default-agg")
            .with_base_url(server.uri())
            .with_query("q1")
            .build()
            .unwrap(),
    );
    r.start().await.unwrap();

    // First emission of a grouping key (no prior value).
    let qr = make_query_result(
        "q1",
        vec![ResultDiff::Aggregation {
            before: None,
            after: json!({"region": "north", "count": 1}),
            row_signature: 0,
        }],
    );
    r.enqueue_query_result(qr).await.expect("enqueue");
    wait_for_requests(&server, 1, 2000).await;
    r.stop().await.unwrap();

    let reqs = server.received_requests().await.unwrap();
    assert_eq!(reqs.len(), 1);
    let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
    // Aggregation is delivered as UPDATE (matches gRPC, Azure Storage, RabbitMQ).
    assert_eq!(body.get("operation"), Some(&json!("UPDATE")));
    assert_eq!(body.get("queryId"), Some(&json!("q1")));
    assert_eq!(
        body.get("after"),
        Some(&json!({"region": "north", "count": 1}))
    );
    assert!(
        body.get("before").is_none(),
        "first aggregation emission must omit 'before'"
    );
}

#[tokio::test]
async fn default_envelope_aggregation_change_carries_prior_aggregate_as_before() {
    let server = mock_server::start().await;
    Mock::given(method("POST"))
        .and(path("/changes/q1"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let r = Arc::new(
        HttpReaction::builder("default-agg-change")
            .with_base_url(server.uri())
            .with_query("q1")
            .build()
            .unwrap(),
    );
    r.start().await.unwrap();

    let qr = make_query_result(
        "q1",
        vec![ResultDiff::Aggregation {
            before: Some(json!({"region": "north", "count": 10})),
            after: json!({"region": "north", "count": 11}),
            row_signature: 0,
        }],
    );
    r.enqueue_query_result(qr).await.expect("enqueue");
    wait_for_requests(&server, 1, 2000).await;
    r.stop().await.unwrap();

    let reqs = server.received_requests().await.unwrap();
    assert_eq!(reqs.len(), 1);
    let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
    assert_eq!(body.get("operation"), Some(&json!("UPDATE")));
    assert_eq!(
        body.get("before"),
        Some(&json!({"region": "north", "count": 10}))
    );
    assert_eq!(
        body.get("after"),
        Some(&json!({"region": "north", "count": 11}))
    );
}

#[tokio::test]
async fn noop_results_are_silently_dropped_without_emitting_requests() {
    let server = mock_server::start().await;
    // Only the safe ADD path is mounted. If Noops produced a request, it would
    // 404 against this server but still appear in received_requests().
    Mock::given(method("POST"))
        .and(path("/changes/q1"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let r = Arc::new(
        HttpReaction::builder("noop-skip")
            .with_base_url(server.uri())
            .with_query("q1")
            .build()
            .unwrap(),
    );
    r.start().await.unwrap();

    // Single QueryResult with a Noop followed by an Add. The standard loop
    // iterates diffs in order; the trailing Add's request acts as the
    // synchronization barrier proving the Noop has been processed
    // (and skipped, since only one request lands).
    let qr = make_query_result(
        "q1",
        vec![
            ResultDiff::Noop,
            ResultDiff::Add {
                data: json!({"id": 1}),
                row_signature: 0,
            },
        ],
    );
    r.enqueue_query_result(qr).await.expect("enqueue");
    wait_for_requests(&server, 1, 2000).await;
    r.stop().await.unwrap();

    let reqs = server.received_requests().await.unwrap();
    assert_eq!(
        reqs.len(),
        1,
        "Noop must produce no HTTP request; only the ADD should land"
    );
    let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
    assert_eq!(body.get("operation"), Some(&json!("ADD")));
}

// ---------------------------------------------------------------------------
// Default-envelope: sequenceId + metadata threading
// ---------------------------------------------------------------------------

#[tokio::test]
async fn default_envelope_carries_sequence_id_and_metadata() {
    let server = mock_server::start().await;
    Mock::given(method("POST"))
        .and(path("/changes/q1"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let r = Arc::new(
        HttpReaction::builder("default-seq-meta")
            .with_base_url(server.uri())
            .with_query("q1")
            .build()
            .unwrap(),
    );
    r.start().await.unwrap();

    let mut metadata = HashMap::new();
    metadata.insert("source".to_string(), json!("sensors"));
    let qr = QueryResult::new(
        "q1".to_string(),
        99,
        Utc::now(),
        vec![ResultDiff::Add {
            data: json!({"id": 1}),
            row_signature: 0,
        }],
        metadata,
    );
    r.enqueue_query_result(qr).await.expect("enqueue");
    wait_for_requests(&server, 1, 2000).await;
    r.stop().await.unwrap();

    let reqs = server.received_requests().await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
    assert_eq!(body.get("sequenceId"), Some(&json!(99)));
    assert_eq!(body.get("metadata"), Some(&json!({"source": "sensors"})));
}

// ---------------------------------------------------------------------------
// Templating: required context keys are available end-to-end
// ---------------------------------------------------------------------------

#[tokio::test]
async fn template_context_exposes_query_id_metadata_and_operation() {
    let server = mock_server::start().await;
    Mock::given(method("POST"))
        .and(path("/ingest"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let spec = TemplateSpec {
        template: r#"{"q":"{{query_id}}","src":"{{metadata.source}}","op":"{{operation}}"}"#
            .to_string(),
        extension: HttpCallExt {
            url: "/ingest".to_string(),
            method: "POST".to_string(),
            headers: HashMap::new(),
        },
    };
    let r = Arc::new(
        HttpReaction::builder("ctx")
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

    let mut metadata = HashMap::new();
    metadata.insert("source".to_string(), json!("sensors"));
    let qr = QueryResult::new(
        "q1".to_string(),
        1,
        Utc::now(),
        vec![ResultDiff::Add {
            data: json!({"id": 1}),
            row_signature: 0,
        }],
        metadata,
    );
    r.enqueue_query_result(qr).await.expect("enqueue");
    wait_for_requests(&server, 1, 2000).await;
    r.stop().await.unwrap();

    let reqs = server.received_requests().await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).unwrap();
    assert_eq!(body.get("q"), Some(&json!("q1")));
    assert_eq!(body.get("src"), Some(&json!("sensors")));
    assert_eq!(body.get("op"), Some(&json!("ADD")));
}

// ---------------------------------------------------------------------------
// Templating: per-query route resolves via the last dotted segment
// ---------------------------------------------------------------------------

#[tokio::test]
async fn route_resolves_via_last_dotted_segment() {
    let server = mock_server::start().await;
    Mock::given(method("POST"))
        .and(path("/seg"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    // Route keyed by the bare segment "orders"; the wire query id is dotted.
    let spec = TemplateSpec {
        template: r#"{"ok":true}"#.to_string(),
        extension: HttpCallExt {
            url: "/seg".to_string(),
            method: "POST".to_string(),
            headers: HashMap::new(),
        },
    };
    let r = Arc::new(
        HttpReaction::builder("seg")
            .with_base_url(server.uri())
            .with_query("source.orders")
            .with_query_template(
                "orders",
                HttpQueryConfig {
                    added: Some(spec),
                    ..Default::default()
                },
            )
            .build()
            .unwrap(),
    );
    r.start().await.unwrap();
    enqueue_add(&r, "source.orders", json!({"id": 1})).await;
    wait_for_requests(&server, 1, 2000).await;
    r.stop().await.unwrap();

    let reqs = server.received_requests().await.unwrap();
    assert_eq!(reqs.len(), 1);
    assert_eq!(
        reqs[0].url.path(),
        "/seg",
        "the segment-keyed route should have matched the dotted query id"
    );
}

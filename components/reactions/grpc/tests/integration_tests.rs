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

//! End-to-end integration tests for the gRPC reaction.
//!
//! Each test:
//! 1. Spins up an in-process mock `ReactionService` server (see
//!    `mock_server.rs`) bound to an ephemeral loopback port.
//! 2. Builds a real `GrpcReaction` via the public builder API, starts
//!    it, enqueues `QueryResult` events through the public `Reaction`
//!    trait, then stops it.
//! 3. Asserts the **exact** wire shape of the items the server
//!    received: `item_type` enum value, `row_signature`, presence /
//!    absence and contents of `before` / `after` / `payload`, plus
//!    metadata propagated as gRPC headers.
//!
//! Compared to `src/tests.rs::integration` (which exercises the
//! send/runner internals via `pub(crate)` APIs), this file pins the
//! public contract a downstream embedder sees end-to-end.

mod mock_server;

use std::collections::HashMap;
use std::time::Duration;

use chrono::Utc;
use drasi_lib::channels::{QueryResult, ResultDiff};
use drasi_lib::reactions::common::{AdaptiveBatchConfig, QueryConfig, TemplateSpec};
use drasi_lib::Reaction;
use drasi_reaction_grpc::{GrpcReaction, OutputTemplates};
use serde_json::json;

use crate::mock_server::struct_to_json;

// Mirrors of the prost-generated QueryResultItemType numeric tags so
// integration assertions don't need to import generated proto code.
const ITEM_TYPE_ADD: i32 = 1;
const ITEM_TYPE_UPDATE: i32 = 2;
const ITEM_TYPE_DELETE: i32 = 3;
const ITEM_TYPE_AGGREGATION: i32 = 4;
const ITEM_TYPE_NOOP: i32 = 5;

fn add(data: serde_json::Value, row_signature: u64) -> ResultDiff {
    ResultDiff::Add {
        data,
        row_signature,
    }
}

fn delete(data: serde_json::Value, row_signature: u64) -> ResultDiff {
    ResultDiff::Delete {
        data,
        row_signature,
    }
}

fn update(before: serde_json::Value, after: serde_json::Value, row_signature: u64) -> ResultDiff {
    ResultDiff::Update {
        data: json!({}),
        before,
        after,
        grouping_keys: None,
        row_signature,
    }
}

fn make_query_result(query_id: &str, diffs: Vec<ResultDiff>) -> QueryResult {
    QueryResult::new(query_id.to_string(), 0, Utc::now(), diffs, HashMap::new())
}

fn template_for_add(template: &str) -> QueryConfig {
    QueryConfig {
        added: Some(TemplateSpec::new(template)),
        updated: None,
        deleted: None,
    }
}

fn template_full(template_add: &str, template_upd: &str, template_del: &str) -> QueryConfig {
    QueryConfig {
        added: Some(TemplateSpec::new(template_add)),
        updated: Some(TemplateSpec::new(template_upd)),
        deleted: Some(TemplateSpec::new(template_del)),
    }
}

/// Drain shutdown helper — stops the reaction, then awaits a small
/// grace period to let the bounded-shutdown timeout fire if it must.
async fn shutdown(reaction: &GrpcReaction) {
    let _ = reaction.stop().await;
}

// ---------------------------------------------------------------------------
// Fixed batching
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fixed_no_template_emits_one_rpc_with_correct_item_type_and_no_payload() {
    let server = mock_server::start().await;
    let reaction = GrpcReaction::builder("test-fixed-add")
        .with_endpoint(server.endpoint.clone())
        .with_queries(vec!["q1".into()])
        .with_fixed_batching(1, 10_000)
        .build()
        .expect("builder");
    reaction.start().await.expect("start");

    reaction
        .enqueue_query_result(make_query_result(
            "q1",
            vec![add(json!({"id": 7, "name": "alice"}), 42)],
        ))
        .await
        .expect("enqueue");

    let total = server
        .recorder
        .wait_for_items(1, Duration::from_secs(5))
        .await;
    assert_eq!(total, 1, "expected exactly one item delivered");

    let batches = server.recorder.batches().await;
    assert_eq!(batches.len(), 1, "expected exactly one ProcessResults RPC");
    let batch = &batches[0];
    assert_eq!(batch.query_id, "q1");
    assert!(
        batch.had_rfc3339_timestamp,
        "QueryResult envelope must carry a valid timestamp"
    );

    let item = &batch.items[0];
    assert_eq!(item.item_type, ITEM_TYPE_ADD);
    assert_eq!(item.row_signature, 42, "row_signature must propagate");
    assert!(item.before.is_none(), "ADD must not populate `before`");
    assert_eq!(
        item.after.as_ref().map(struct_to_json),
        Some(json!({"id": 7, "name": "alice"})),
        "ADD must populate `after` with the raw row state"
    );
    assert!(
        item.payload.is_none(),
        "no template configured ⇒ payload must be absent"
    );

    shutdown(&reaction).await;
    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fixed_with_template_populates_payload_and_keeps_before_after() {
    let server = mock_server::start().await;
    let templates = OutputTemplates {
        default_template: Some(template_for_add(
            r#"{"event":"created","id":"{{after.id}}"}"#,
        )),
        routes: HashMap::new(),
    };
    let reaction = GrpcReaction::builder("test-fixed-tmpl")
        .with_endpoint(server.endpoint.clone())
        .with_queries(vec!["q1".into()])
        .with_fixed_batching(1, 10_000)
        .with_output_templates(templates)
        .build()
        .expect("builder");
    reaction.start().await.expect("start");

    reaction
        .enqueue_query_result(make_query_result(
            "q1",
            vec![add(json!({"id": "abc", "name": "alice"}), 0)],
        ))
        .await
        .expect("enqueue");

    let total = server
        .recorder
        .wait_for_items(1, Duration::from_secs(5))
        .await;
    assert_eq!(total, 1);

    let batches = server.recorder.batches().await;
    let item = &batches[0].items[0];
    assert_eq!(item.item_type, ITEM_TYPE_ADD);
    assert_eq!(
        item.payload.as_ref().map(struct_to_json),
        Some(json!({"event": "created", "id": "abc"})),
        "payload must be the rendered template output"
    );
    assert_eq!(
        item.after.as_ref().map(struct_to_json),
        Some(json!({"id": "abc", "name": "alice"})),
        "after must still carry the raw row state alongside payload"
    );

    shutdown(&reaction).await;
    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fixed_template_render_failure_omits_payload_but_keeps_before_after() {
    let server = mock_server::start().await;
    // Non-JSON template output triggers the parse-failure path in
    // TemplateEngine::render_payload, which logs a warning and returns
    // None — `payload` must be omitted, NOT replaced with raw data, and
    // the event must still be delivered.
    let templates = OutputTemplates {
        default_template: Some(template_for_add("plain text {{after.id}}")),
        routes: HashMap::new(),
    };
    let reaction = GrpcReaction::builder("test-fixed-render-fail")
        .with_endpoint(server.endpoint.clone())
        .with_queries(vec!["q1".into()])
        .with_fixed_batching(1, 10_000)
        .with_output_templates(templates)
        .build()
        .expect("builder");
    reaction.start().await.expect("start");

    reaction
        .enqueue_query_result(make_query_result("q1", vec![add(json!({"id": 99}), 0)]))
        .await
        .expect("enqueue");

    let total = server
        .recorder
        .wait_for_items(1, Duration::from_secs(5))
        .await;
    assert_eq!(
        total, 1,
        "event must still be delivered after render failure"
    );

    let batches = server.recorder.batches().await;
    let item = &batches[0].items[0];
    assert!(
        item.payload.is_none(),
        "render failure must omit payload, NOT substitute raw data"
    );
    assert_eq!(
        item.after.as_ref().map(struct_to_json),
        Some(json!({"id": 99})),
        "after must still carry the raw row state so receivers can recover"
    );

    shutdown(&reaction).await;
    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fixed_update_template_context_has_before_and_after() {
    let server = mock_server::start().await;
    let templates = OutputTemplates {
        default_template: Some(template_full(
            "{}",
            r#"{"v":"{{after.v}}","prev":"{{before.v}}"}"#,
            "{}",
        )),
        routes: HashMap::new(),
    };
    let reaction = GrpcReaction::builder("test-update")
        .with_endpoint(server.endpoint.clone())
        .with_queries(vec!["q1".into()])
        .with_fixed_batching(1, 10_000)
        .with_output_templates(templates)
        .build()
        .expect("builder");
    reaction.start().await.expect("start");

    reaction
        .enqueue_query_result(make_query_result(
            "q1",
            vec![update(json!({"v": "old"}), json!({"v": "new"}), 7)],
        ))
        .await
        .expect("enqueue");

    server
        .recorder
        .wait_for_items(1, Duration::from_secs(5))
        .await;
    let batches = server.recorder.batches().await;
    let item = &batches[0].items[0];
    assert_eq!(item.item_type, ITEM_TYPE_UPDATE);
    assert_eq!(item.row_signature, 7);
    assert_eq!(
        item.before.as_ref().map(struct_to_json),
        Some(json!({"v": "old"}))
    );
    assert_eq!(
        item.after.as_ref().map(struct_to_json),
        Some(json!({"v": "new"}))
    );
    assert_eq!(
        item.payload.as_ref().map(struct_to_json),
        Some(json!({"v": "new", "prev": "old"}))
    );

    shutdown(&reaction).await;
    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fixed_delete_template_context_has_before_only() {
    let server = mock_server::start().await;
    let templates = OutputTemplates {
        default_template: Some(template_full("{}", "{}", r#"{"deleted":"{{before.id}}"}"#)),
        routes: HashMap::new(),
    };
    let reaction = GrpcReaction::builder("test-delete")
        .with_endpoint(server.endpoint.clone())
        .with_queries(vec!["q1".into()])
        .with_fixed_batching(1, 10_000)
        .with_output_templates(templates)
        .build()
        .expect("builder");
    reaction.start().await.expect("start");

    reaction
        .enqueue_query_result(make_query_result(
            "q1",
            vec![delete(json!({"id": "x42"}), 13)],
        ))
        .await
        .expect("enqueue");

    server
        .recorder
        .wait_for_items(1, Duration::from_secs(5))
        .await;
    let batches = server.recorder.batches().await;
    let item = &batches[0].items[0];
    assert_eq!(item.item_type, ITEM_TYPE_DELETE);
    assert_eq!(item.row_signature, 13);
    assert_eq!(
        item.before.as_ref().map(struct_to_json),
        Some(json!({"id": "x42"}))
    );
    assert!(item.after.is_none(), "DELETE must not populate `after`");
    assert_eq!(
        item.payload.as_ref().map(struct_to_json),
        Some(json!({"deleted": "x42"}))
    );

    shutdown(&reaction).await;
    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn metadata_is_propagated_as_grpc_headers_end_to_end() {
    let server = mock_server::start().await;
    let reaction = GrpcReaction::builder("test-meta")
        .with_endpoint(server.endpoint.clone())
        .with_queries(vec!["q1".into()])
        .with_fixed_batching(1, 10_000)
        .with_metadata("authorization", "Bearer test-token")
        .with_metadata("x-tenant", "tenant-42")
        .build()
        .expect("builder");
    reaction.start().await.expect("start");

    reaction
        .enqueue_query_result(make_query_result("q1", vec![add(json!({"id": 1}), 0)]))
        .await
        .expect("enqueue");

    server
        .recorder
        .wait_for_items(1, Duration::from_secs(5))
        .await;
    let batches = server.recorder.batches().await;
    let observed = &batches[0].metadata_headers;
    assert_eq!(
        observed.get("authorization").map(String::as_str),
        Some("Bearer test-token"),
        "authorization must arrive as an actual gRPC header end-to-end"
    );
    assert_eq!(
        observed.get("x-tenant").map(String::as_str),
        Some("tenant-42")
    );

    shutdown(&reaction).await;
    server.shutdown().await;
}

// ---------------------------------------------------------------------------
// Adaptive batching
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn adaptive_delivers_steady_load() {
    // The gRPC adaptive runner buffers per-query in its main loop and
    // only forwards to the AdaptiveBatcher when either (a) the
    // query_id changes or (b) ≥100 items have accumulated. To exercise
    // a real adaptive flush via the batcher we therefore alternate
    // two query IDs — every other event forces the runner to release
    // the previously-buffered batch downstream.
    let server = mock_server::start().await;
    let reaction = GrpcReaction::builder("test-adaptive")
        .with_endpoint(server.endpoint.clone())
        .with_queries(vec!["q1".into(), "q2".into()])
        // Uses the new per-field setter (gap p09): implicitly enables
        // adaptive mode with the rest of the adaptive defaults.
        .with_min_batch_size(1)
        .with_max_batch_size(50)
        .with_batch_timeout_ms(100)
        .build()
        .expect("builder");
    reaction.start().await.expect("start");

    for i in 0..10 {
        let q = if i % 2 == 0 { "q1" } else { "q2" };
        reaction
            .enqueue_query_result(make_query_result(q, vec![add(json!({"id": i}), i as u64)]))
            .await
            .expect("enqueue");
    }

    // The final item for whichever query is "last" remains buffered
    // until stop()/drain. Wait for 9 of 10 to confirm steady-state
    // delivery, then stop to flush the trailing item.
    let observed = server
        .recorder
        .wait_for_items(9, Duration::from_secs(5))
        .await;
    assert!(
        observed >= 9,
        "adaptive must deliver alternating-query events promptly via batcher coalescing; \
         observed {observed} of 10 before shutdown"
    );

    reaction.stop().await.expect("stop");
    // After shutdown the runner's drain flushes the trailing buffered
    // item; assert the total settled at 10.
    let total = server
        .recorder
        .wait_for_items(10, Duration::from_secs(5))
        .await;
    assert_eq!(
        total, 10,
        "adaptive must deliver every enqueued item exactly once (including \
         the trailing item flushed on shutdown drain)"
    );

    let batches = server.recorder.batches().await;
    let mut observed_ids: Vec<u64> = batches
        .iter()
        .flat_map(|b| b.items.iter().map(|i| i.row_signature))
        .collect();
    observed_ids.sort_unstable();
    assert_eq!(
        observed_ids,
        (0..10u64).collect::<Vec<_>>(),
        "every row_signature must arrive exactly once across all batches"
    );

    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn adaptive_flushes_buffered_batch_on_shutdown_drain() {
    // The gRPC adaptive runner accumulates per-query in its main loop
    // and relies on the drain path to flush any in-flight batch on
    // shutdown. With min_batch_size=10 and 1 item enqueued, the
    // batcher would never fire on size alone — the drain must release
    // the item, exercising the bounded `tokio::time::timeout(5s,
    // batcher_handle)` we added (gap p02) without hitting the timeout.
    let server = mock_server::start().await;
    let reaction = GrpcReaction::builder("test-adaptive-drain")
        .with_endpoint(server.endpoint.clone())
        .with_queries(vec!["q1".into()])
        .with_adaptive_batching(AdaptiveBatchConfig {
            adaptive_min_batch_size: 10,
            adaptive_max_batch_size: 100,
            adaptive_window_size: 10,
            adaptive_batch_timeout_ms: 100,
        })
        .build()
        .expect("builder");
    reaction.start().await.expect("start");

    reaction
        .enqueue_query_result(make_query_result("q1", vec![add(json!({"id": 1}), 0)]))
        .await
        .expect("enqueue");

    // Give the runner a moment to receive and buffer; nothing should be
    // sent yet (min=10, only 1 item, same query_id, < 100 threshold).
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        server.recorder.total_items().await,
        0,
        "single buffered item must not be flushed before drain"
    );

    let stop_start = std::time::Instant::now();
    reaction.stop().await.expect("stop");
    let stop_elapsed = stop_start.elapsed();
    assert!(
        stop_elapsed < Duration::from_secs(3),
        "drain must complete promptly (< 3 s); took {stop_elapsed:?}"
    );

    let total = server
        .recorder
        .wait_for_items(1, Duration::from_secs(2))
        .await;
    assert_eq!(total, 1, "drain must flush the buffered batch");

    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn adaptive_shutdown_completes_within_bounded_window() {
    // The runner_adaptive shutdown drain is bounded to 5 s via
    // tokio::time::timeout (gap p02). With a healthy server, stop() must
    // complete well within that — assert under 3 s to give headroom and
    // surface any regression to the unbounded behavior.
    let server = mock_server::start().await;
    let reaction = GrpcReaction::builder("test-adaptive-stop")
        .with_endpoint(server.endpoint.clone())
        .with_queries(vec!["q1".into()])
        .with_adaptive_batching(AdaptiveBatchConfig {
            adaptive_min_batch_size: 1,
            adaptive_max_batch_size: 10,
            adaptive_window_size: 10,
            adaptive_batch_timeout_ms: 50,
        })
        .build()
        .expect("builder");
    reaction.start().await.expect("start");

    reaction
        .enqueue_query_result(make_query_result("q1", vec![add(json!({"id": 1}), 0)]))
        .await
        .expect("enqueue");

    // Let one batch land so the drain path is exercised against an
    // actually-streaming runner.
    server
        .recorder
        .wait_for_items(1, Duration::from_secs(5))
        .await;

    let stop_start = std::time::Instant::now();
    reaction.stop().await.expect("stop");
    let stop_elapsed = stop_start.elapsed();
    assert!(
        stop_elapsed < Duration::from_secs(3),
        "stop() should complete promptly (< 3 s) under healthy conditions; \
         took {stop_elapsed:?}"
    );

    server.shutdown().await;
}

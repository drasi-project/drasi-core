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
//!    absence and contents of `before` / `after`, plus metadata
//!    propagated as gRPC headers.

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
// Aggregation surfaces as UPDATE on the wire; Noop is dropped before
// the wire — neither has a wire enum value.
const ITEM_TYPE_ADD: i32 = 1;
const ITEM_TYPE_UPDATE: i32 = 2;
const ITEM_TYPE_DELETE: i32 = 3;

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

fn aggregation(
    before: Option<serde_json::Value>,
    after: serde_json::Value,
    row_signature: u64,
) -> ResultDiff {
    ResultDiff::Aggregation {
        before,
        after,
        row_signature,
    }
}

fn make_query_result(query_id: &str, diffs: Vec<ResultDiff>) -> QueryResult {
    QueryResult::new(query_id.to_string(), 0, Utc::now(), diffs, HashMap::new())
}

fn template_added(template: &str) -> QueryConfig {
    QueryConfig {
        added: Some(TemplateSpec::new(template)),
        updated: None,
        deleted: None,
    }
}

fn template_updated(template: &str) -> QueryConfig {
    QueryConfig {
        added: None,
        updated: Some(TemplateSpec::new(template)),
        deleted: None,
    }
}

fn template_deleted(template: &str) -> QueryConfig {
    QueryConfig {
        added: None,
        updated: None,
        deleted: Some(TemplateSpec::new(template)),
    }
}

async fn shutdown(reaction: &GrpcReaction) {
    let _ = reaction.stop().await;
}

// ---------------------------------------------------------------------------
// Fixed batching — ADD
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fixed_add_no_template_emits_raw_row_in_after_and_no_before() {
    let server = mock_server::start().await;
    let reaction = GrpcReaction::builder("test-fixed-add-raw")
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
    assert_eq!(total, 1);

    let batches = server.recorder.batches().await;
    assert_eq!(batches.len(), 1, "expected exactly one ProcessResults RPC");
    let batch = &batches[0];
    assert_eq!(batch.query_id, "q1");
    assert!(batch.had_rfc3339_timestamp);

    let item = &batch.items[0];
    assert_eq!(item.item_type, ITEM_TYPE_ADD);
    assert_eq!(item.row_signature, 42);
    assert!(item.before.is_none(), "ADD must leave `before` absent");
    assert_eq!(
        item.after.as_ref().map(struct_to_json),
        Some(json!({"id": 7, "name": "alice"})),
        "ADD without a template must carry the raw row in `after`"
    );

    shutdown(&reaction).await;
    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fixed_add_with_template_renders_into_after() {
    let server = mock_server::start().await;
    let templates = OutputTemplates {
        default_template: Some(template_added(r#"{"event":"created","id":"{{row.id}}"}"#)),
        routes: HashMap::new(),
    };
    let reaction = GrpcReaction::builder("test-fixed-add-tmpl")
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
            vec![add(json!({"id": "abc", "extra": "ignored"}), 0)],
        ))
        .await
        .expect("enqueue");

    server
        .recorder
        .wait_for_items(1, Duration::from_secs(5))
        .await;
    let batches = server.recorder.batches().await;
    let item = &batches[0].items[0];
    assert_eq!(item.item_type, ITEM_TYPE_ADD);
    assert_eq!(
        item.after.as_ref().map(struct_to_json),
        Some(json!({"event": "created", "id": "abc"})),
        "ADD's `after` must carry the rendered template output, not the raw row"
    );
    assert!(item.before.is_none(), "ADD never populates `before`");

    shutdown(&reaction).await;
    server.shutdown().await;
}

// ---------------------------------------------------------------------------
// Fixed batching — DELETE
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fixed_delete_no_template_emits_raw_row_in_before_and_no_after() {
    let server = mock_server::start().await;
    let reaction = GrpcReaction::builder("test-fixed-del-raw")
        .with_endpoint(server.endpoint.clone())
        .with_queries(vec!["q1".into()])
        .with_fixed_batching(1, 10_000)
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
    assert!(item.after.is_none(), "DELETE must leave `after` absent");

    shutdown(&reaction).await;
    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fixed_delete_with_template_renders_into_before() {
    let server = mock_server::start().await;
    let templates = OutputTemplates {
        default_template: Some(template_deleted(r#"{"removed":"{{row.id}}"}"#)),
        routes: HashMap::new(),
    };
    let reaction = GrpcReaction::builder("test-fixed-del-tmpl")
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
            vec![delete(json!({"id": "x42"}), 0)],
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
    assert_eq!(
        item.before.as_ref().map(struct_to_json),
        Some(json!({"removed": "x42"})),
        "DELETE's `before` must carry the rendered template output"
    );
    assert!(item.after.is_none());

    shutdown(&reaction).await;
    server.shutdown().await;
}

// ---------------------------------------------------------------------------
// Fixed batching — UPDATE (the key new behavior)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fixed_update_no_template_emits_raw_before_and_after() {
    let server = mock_server::start().await;
    let reaction = GrpcReaction::builder("test-fixed-upd-raw")
        .with_endpoint(server.endpoint.clone())
        .with_queries(vec!["q1".into()])
        .with_fixed_batching(1, 10_000)
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

    shutdown(&reaction).await;
    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fixed_update_template_renders_both_before_and_after_independently() {
    // The single `updated` template is applied independently to the
    // before-row and the after-row. Each rendered output lands in its
    // corresponding proto field.
    let server = mock_server::start().await;
    let templates = OutputTemplates {
        default_template: Some(template_updated(r#"{"value":"{{row.v}}"}"#)),
        routes: HashMap::new(),
    };
    let reaction = GrpcReaction::builder("test-fixed-upd-tmpl")
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
            vec![update(json!({"v": "old"}), json!({"v": "new"}), 99)],
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
    assert_eq!(item.row_signature, 99);
    assert_eq!(
        item.before.as_ref().map(struct_to_json),
        Some(json!({"value": "old"})),
        "`before` must be rendered from the before-row"
    );
    assert_eq!(
        item.after.as_ref().map(struct_to_json),
        Some(json!({"value": "new"})),
        "`after` must be rendered from the after-row"
    );

    shutdown(&reaction).await;
    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fixed_update_template_sees_side_variable_differently_per_render() {
    let server = mock_server::start().await;
    let templates = OutputTemplates {
        default_template: Some(template_updated(r#"{"v":"{{row.v}}","s":"{{side}}"}"#)),
        routes: HashMap::new(),
    };
    let reaction = GrpcReaction::builder("test-fixed-upd-side")
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
            vec![update(json!({"v": "old"}), json!({"v": "new"}), 0)],
        ))
        .await
        .expect("enqueue");

    server
        .recorder
        .wait_for_items(1, Duration::from_secs(5))
        .await;
    let batches = server.recorder.batches().await;
    let item = &batches[0].items[0];
    assert_eq!(
        item.before.as_ref().map(struct_to_json),
        Some(json!({"v": "old", "s": "before"}))
    );
    assert_eq!(
        item.after.as_ref().map(struct_to_json),
        Some(json!({"v": "new", "s": "after"}))
    );

    shutdown(&reaction).await;
    server.shutdown().await;
}

// ---------------------------------------------------------------------------
// Render-failure fall-back
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fixed_template_render_failure_falls_back_to_raw_row_state() {
    let server = mock_server::start().await;
    let templates = OutputTemplates {
        default_template: Some(template_added("plain text {{row.id}}")),
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
        .enqueue_query_result(make_query_result(
            "q1",
            vec![add(json!({"id": 99}), 0)],
        ))
        .await
        .expect("enqueue");

    let total = server
        .recorder
        .wait_for_items(1, Duration::from_secs(5))
        .await;
    assert_eq!(total, 1, "event must still be delivered after render failure");

    let batches = server.recorder.batches().await;
    let item = &batches[0].items[0];
    assert_eq!(
        item.after.as_ref().map(struct_to_json),
        Some(json!({"id": 99})),
        "render failure must fall back to the raw row state in `after`"
    );

    shutdown(&reaction).await;
    server.shutdown().await;
}

// ---------------------------------------------------------------------------
// Aggregation → UPDATE on the wire
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn aggregation_emits_as_update_on_the_wire_with_both_sides_templated() {
    let server = mock_server::start().await;
    let templates = OutputTemplates {
        default_template: Some(template_updated(r#"{"sum":{{row.sum}}}"#)),
        routes: HashMap::new(),
    };
    let reaction = GrpcReaction::builder("test-agg")
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
            vec![aggregation(Some(json!({"sum": 1})), json!({"sum": 5}), 0)],
        ))
        .await
        .expect("enqueue");

    server
        .recorder
        .wait_for_items(1, Duration::from_secs(5))
        .await;
    let batches = server.recorder.batches().await;
    let item = &batches[0].items[0];
    assert_eq!(
        item.item_type, ITEM_TYPE_UPDATE,
        "Aggregation must surface as UPDATE on the wire"
    );
    assert_eq!(
        item.before.as_ref().map(struct_to_json),
        Some(json!({"sum": 1}))
    );
    assert_eq!(
        item.after.as_ref().map(struct_to_json),
        Some(json!({"sum": 5}))
    );

    shutdown(&reaction).await;
    server.shutdown().await;
}

// ---------------------------------------------------------------------------
// Noop dropped at the runner — never reaches the wire
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn noop_results_are_not_emitted_on_the_wire() {
    let server = mock_server::start().await;
    let reaction = GrpcReaction::builder("test-noop-drop")
        .with_endpoint(server.endpoint.clone())
        .with_queries(vec!["q1".into()])
        .with_fixed_batching(1, 10_000)
        .build()
        .expect("builder");
    reaction.start().await.expect("start");

    reaction
        .enqueue_query_result(make_query_result(
            "q1",
            vec![
                ResultDiff::Noop,
                add(json!({"id": 1}), 0),
                ResultDiff::Noop,
            ],
        ))
        .await
        .expect("enqueue");

    let total = server
        .recorder
        .wait_for_items(1, Duration::from_secs(5))
        .await;
    assert_eq!(
        total, 1,
        "exactly one item must arrive — Noop entries are dropped at the runner"
    );

    let batches = server.recorder.batches().await;
    let total_items: usize = batches.iter().map(|b| b.items.len()).sum();
    assert_eq!(total_items, 1, "no Noop items should leak through");
    assert_eq!(batches[0].items[0].item_type, ITEM_TYPE_ADD);

    shutdown(&reaction).await;
    server.shutdown().await;
}

// ---------------------------------------------------------------------------
// Metadata propagation
// ---------------------------------------------------------------------------

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
        .enqueue_query_result(make_query_result(
            "q1",
            vec![add(json!({"id": 1}), 0)],
        ))
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
        Some("Bearer test-token")
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
    let server = mock_server::start().await;
    let reaction = GrpcReaction::builder("test-adaptive")
        .with_endpoint(server.endpoint.clone())
        .with_queries(vec!["q1".into(), "q2".into()])
        .with_min_batch_size(1)
        .with_max_batch_size(50)
        .with_batch_timeout_ms(100)
        .build()
        .expect("builder");
    reaction.start().await.expect("start");

    for i in 0..10 {
        let q = if i % 2 == 0 { "q1" } else { "q2" };
        reaction
            .enqueue_query_result(make_query_result(
                q,
                vec![add(json!({"id": i}), i as u64)],
            ))
            .await
            .expect("enqueue");
    }

    let observed = server
        .recorder
        .wait_for_items(9, Duration::from_secs(5))
        .await;
    assert!(observed >= 9, "observed {observed} of 10 pre-stop");

    reaction.stop().await.expect("stop");
    let total = server
        .recorder
        .wait_for_items(10, Duration::from_secs(5))
        .await;
    assert_eq!(total, 10);

    let batches = server.recorder.batches().await;
    let mut observed_ids: Vec<u64> = batches
        .iter()
        .flat_map(|b| b.items.iter().map(|i| i.row_signature))
        .collect();
    observed_ids.sort_unstable();
    assert_eq!(observed_ids, (0..10u64).collect::<Vec<_>>());

    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn adaptive_flushes_buffered_batch_on_shutdown_drain() {
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
        .enqueue_query_result(make_query_result(
            "q1",
            vec![add(json!({"id": 1}), 0)],
        ))
        .await
        .expect("enqueue");

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
        .enqueue_query_result(make_query_result(
            "q1",
            vec![add(json!({"id": 1}), 0)],
        ))
        .await
        .expect("enqueue");

    server
        .recorder
        .wait_for_items(1, Duration::from_secs(5))
        .await;

    let stop_start = std::time::Instant::now();
    reaction.stop().await.expect("stop");
    let stop_elapsed = stop_start.elapsed();
    assert!(
        stop_elapsed < Duration::from_secs(3),
        "stop() must complete promptly; took {stop_elapsed:?}"
    );

    server.shutdown().await;
}

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

//! End-to-end recovery / at-least-once tests for the HTTP reaction.
//!
//! Each test wires a **real** pipeline through the public `DrasiLib` API —
//! mock source → Cypher query (with an outbox) → the HTTP reaction → a wiremock
//! server — backed by a persistent state store, and asserts on **what the
//! downstream server actually received**. Recovery is driven with the public
//! `stop_reaction` / `start_reaction` lifecycle calls.
//!
//! ComputationGraph is always used; no runtime selector or opt-in feature is needed.
//!
//! Scenarios:
//! * `at_least_once_replays_unacked_events_after_restart` — events produced while
//!   the reaction is stopped are replayed from the query outbox on restart, and
//!   already-delivered events are not re-sent.
//! * `clean_restart_does_not_redeliver_acked_events` — a restart with nothing
//!   missed delivers no duplicates.
//! * `config_hash_preserved_across_repeated_restarts` — the reaction survives
//!   repeated restarts without a false `config_hash` reset (regression guard).
//! * `strict_fail_stops_on_sustained_failure_then_recovers_on_restart` — under the
//!   default Strict policy a sustained 5xx outage stops the reaction without
//!   losing the un-acked event, which is replayed once the downstream recovers.
//! * `auto_skip_gap_keeps_running_and_skips_failed_batch` — under AutoSkipGap a
//!   sustained outage is skipped and the reaction keeps delivering later events.
//! * `adaptive_permanent_4xx_is_dropped_and_reaction_keeps_running` — in adaptive
//!   (batched) mode a permanent 4xx is a poison batch: it is dropped, the reaction
//!   stays Running, and later batches are delivered.

mod mock_server;
mod mock_source;
#[path = "../../tests/recovery_support.rs"]
mod recovery_support;

use std::sync::Arc;
use std::time::Duration;

use drasi_lib::channels::{ComponentStatus, ResultDiff};
use drasi_lib::recovery::ReactionRecoveryPolicy;
use drasi_lib::state_store::StateStoreProvider;
use drasi_lib::{DrasiLib, MemoryStateStoreProvider, Query};
use drasi_reaction_http::{AdaptiveBatchConfig, HttpReaction};
use serde_json::Value;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use recovery_support::{outbox, require_current_runtime, wait_for_checkpoint, wait_for_outbox};

const SOURCE: &str = "e2e-source";
const QUERY: &str = "e2e-query";
const REACTION: &str = "e2e-http";

/// Build a `source → query(outbox) → http reaction` pipeline that persists
/// checkpoints to `store` and posts to `base_url` with the given recovery
/// policy. Standard (per-result) delivery unless `adaptive` is set.
async fn build_core(
    base_url: String,
    store: Arc<dyn StateStoreProvider>,
    policy: ReactionRecoveryPolicy,
    adaptive: bool,
) -> (Arc<DrasiLib>, mock_source::MockSourceHandle) {
    let (mock_source, handle) = mock_source::MockSource::new(SOURCE).expect("mock source");

    let query = Query::cypher(QUERY)
        .query("MATCH (p:Person) RETURN p.name AS name, p.age AS age")
        .from_source(SOURCE)
        .with_outbox_capacity(100)
        .auto_start(true)
        .build();

    let mut builder = HttpReaction::builder(REACTION)
        .with_base_url(base_url)
        .from_query(QUERY)
        .with_recovery_policy(policy);
    if adaptive {
        builder = builder
            .with_adaptive(AdaptiveBatchConfig {
                adaptive_min_batch_size: 1,
                adaptive_max_batch_size: 16,
                adaptive_window_size: 10,
                adaptive_batch_timeout_ms: 50,
            })
            .with_batch_endpoint("/batch");
    }
    let reaction = builder.build().expect("reaction builder");

    require_current_runtime();
    let core = Arc::new(
        DrasiLib::builder()
            .with_id("e2e-core")
            .with_source(mock_source)
            .with_query(query)
            .with_reaction(reaction)
            .with_state_store_provider(store)
            .build()
            .await
            .expect("build core"),
    );
    core.computation_control()
        .expect("ComputationGraph controller");

    (core, handle)
}

async fn insert_person(handle: &mock_source::MockSourceHandle, id: &str, name: &str) {
    let props = mock_source::PropertyMapBuilder::new()
        .with_string("name", name)
        .with_integer("age", 30)
        .build();
    handle
        .send_node_insert(id, vec!["Person"], props)
        .await
        .expect("send node insert");
}

/// Mount the only responder for the given path, replacing any existing mocks
/// **and** clearing the recorded request log (so post-recovery assertions only
/// see post-recovery requests).
async fn respond_with(server: &MockServer, request_path: &str, status: u16) {
    server.reset().await;
    Mock::given(method("POST"))
        .and(path(request_path.to_string()))
        .respond_with(ResponseTemplate::new(status))
        .mount(server)
        .await;
}

fn extract_names(body: &Value, out: &mut Vec<String>) {
    if let Some(items) = body.get("batch").and_then(|b| b.as_array()) {
        for item in items {
            extract_names(item, out);
        }
    } else if let Some(name) = body
        .get("after")
        .and_then(|a| a.get("name"))
        .and_then(|n| n.as_str())
    {
        out.push(name.to_string());
    }
}

/// Every person name the downstream has received so far (one per delivered row),
/// in arrival order. Duplicates show up as repeats.
async fn names_received(server: &MockServer) -> Vec<String> {
    let reqs = server.received_requests().await.expect("recorded requests");
    let mut names = Vec::new();
    for req in reqs {
        let body = serde_json::from_slice::<Value>(&req.body).expect("HTTP result JSON");
        extract_names(&body, &mut names);
    }
    names
}

async fn assert_received(core: &DrasiLib, server: &MockServer, expected: &[(u64, &str)]) {
    let results = outbox(core, QUERY).await;
    let mut changes = Vec::new();
    for request in server.received_requests().await.expect("recorded requests") {
        let body: Value = serde_json::from_slice(&request.body).expect("HTTP result JSON");
        if let Some(batch) = body.get("batch") {
            changes.extend(batch.as_array().expect("batch array").iter().cloned());
        } else {
            changes.push(body);
        }
    }
    assert_eq!(changes.len(), expected.len());
    for (change, (sequence, name)) in changes.iter().zip(expected) {
        let result = results
            .results
            .iter()
            .find(|result| result.sequence == *sequence)
            .expect("delivered sequence is retained in the outbox");
        let [ResultDiff::Add { data, .. }] = result.results.as_slice() else {
            panic!("one added person per query result: {result:?}");
        };
        assert_eq!(data, &serde_json::json!({"name": name, "age": 30}));
        assert_eq!(change["queryId"], QUERY);
        assert_eq!(change["sequenceId"], *sequence);
        assert_eq!(change["operation"], "ADD");
        assert_eq!(change["after"], *data);
        assert!(change.get("before").is_none());
        assert_eq!(change["timestamp"], result.timestamp.to_rfc3339());
    }
}

fn sorted(mut v: Vec<String>) -> Vec<String> {
    v.sort();
    v
}

async fn wait_for_name_count(server: &MockServer, target: usize, timeout: Duration) -> usize {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let n = names_received(server).await.len();
        if n >= target || tokio::time::Instant::now() >= deadline {
            return n;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Assert that the delivered-name count never exceeds `expected` for the whole
/// `settle` window, polling continuously and failing fast on the first excess.
///
/// This is the no-duplicate guard: `stop_reaction_and_wait` only returns once the
/// reaction reaches `Stopped`, by which point its final checkpoint is persisted,
/// so the post-restart forwarder dedups every already-acked sequence and a
/// duplicate cannot occur in a correct implementation. Polling fail-fast (rather
/// than sleeping then checking once) catches a stray duplicate whenever it lands
/// in the window instead of only at the end.
async fn assert_no_extra_deliveries(server: &MockServer, expected: usize, settle: Duration) {
    let deadline = tokio::time::Instant::now() + settle;
    loop {
        let n = names_received(server).await.len();
        assert!(
            n <= expected,
            "unexpected extra delivery: saw {n}, expected at most {expected} — {:?}",
            names_received(server).await
        );
        if tokio::time::Instant::now() >= deadline {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn wait_for_reaction_status(
    core: &DrasiLib,
    status: ComponentStatus,
    timeout: Duration,
) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if core
            .get_reaction_status(REACTION)
            .await
            .map(|s| s == status)
            .unwrap_or(false)
        {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn stop_reaction_and_wait(core: &DrasiLib) {
    core.stop_reaction(REACTION).await.expect("stop reaction");
    assert!(
        wait_for_reaction_status(core, ComponentStatus::Stopped, Duration::from_secs(5)).await,
        "reaction did not reach Stopped"
    );
}

// ---------------------------------------------------------------------------

/// At-least-once: events produced while the reaction is stopped are replayed
/// from the outbox on restart, and already-delivered events are not re-sent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn at_least_once_replays_unacked_events_after_restart() {
    let server = mock_server::start().await;
    respond_with(&server, "/changes/e2e-query", 200).await;
    let store = Arc::new(MemoryStateStoreProvider::new());
    let (core, handle) = build_core(
        server.uri(),
        store.clone(),
        ReactionRecoveryPolicy::Strict,
        false,
    )
    .await;
    core.start().await.expect("start core");
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Deliver two events while running (they get checkpointed).
    insert_person(&handle, "p1", "Alice").await;
    insert_person(&handle, "p2", "Bob").await;
    assert_eq!(
        wait_for_name_count(&server, 2, Duration::from_secs(10)).await,
        2
    );
    wait_for_checkpoint(&core, store.as_ref(), REACTION, QUERY, 2).await;
    assert_received(&core, &server, &[(1, "Alice"), (2, "Bob")]).await;

    // Stop the reaction, then produce two more (they queue in the outbox).
    stop_reaction_and_wait(&core).await;
    insert_person(&handle, "p3", "Carol").await;
    insert_person(&handle, "p4", "Dave").await;
    wait_for_outbox(&core, QUERY, 4).await;
    wait_for_checkpoint(&core, store.as_ref(), REACTION, QUERY, 2).await;

    // Restart — the reaction replays the two missed events from the outbox.
    core.start_reaction(REACTION)
        .await
        .expect("restart reaction");
    assert_eq!(
        wait_for_name_count(&server, 4, Duration::from_secs(10)).await,
        4
    );
    // No fifth (duplicate) delivery may arrive during the settle window.
    assert_no_extra_deliveries(&server, 4, Duration::from_millis(500)).await;

    assert_eq!(
        sorted(names_received(&server).await),
        vec!["Alice", "Bob", "Carol", "Dave"],
        "all four events delivered exactly once across the restart"
    );
    assert_received(
        &core,
        &server,
        &[(1, "Alice"), (2, "Bob"), (3, "Carol"), (4, "Dave")],
    )
    .await;
    wait_for_checkpoint(&core, store.as_ref(), REACTION, QUERY, 4).await;

    core.shutdown().await.expect("shutdown core");
}

/// A restart with nothing missed must not re-deliver already-acked events.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clean_restart_does_not_redeliver_acked_events() {
    let server = mock_server::start().await;
    respond_with(&server, "/changes/e2e-query", 200).await;
    let store = Arc::new(MemoryStateStoreProvider::new());
    let (core, handle) = build_core(
        server.uri(),
        store.clone(),
        ReactionRecoveryPolicy::Strict,
        false,
    )
    .await;
    core.start().await.expect("start core");
    tokio::time::sleep(Duration::from_millis(300)).await;

    insert_person(&handle, "p1", "Alice").await;
    insert_person(&handle, "p2", "Bob").await;
    assert_eq!(
        wait_for_name_count(&server, 2, Duration::from_secs(10)).await,
        2
    );
    let checkpoint = wait_for_checkpoint(&core, store.as_ref(), REACTION, QUERY, 2).await;

    stop_reaction_and_wait(&core).await;
    core.start_reaction(REACTION)
        .await
        .expect("restart reaction");
    assert!(
        wait_for_reaction_status(&core, ComponentStatus::Running, Duration::from_secs(5)).await
    );
    // No duplicate may arrive at any point in the settle window.
    assert_no_extra_deliveries(&server, 2, Duration::from_millis(750)).await;

    assert_eq!(
        sorted(names_received(&server).await),
        vec!["Alice", "Bob"],
        "a clean restart must not re-deliver acked events"
    );
    assert_received(&core, &server, &[(1, "Alice"), (2, "Bob")]).await;
    assert_eq!(
        wait_for_checkpoint(&core, store.as_ref(), REACTION, QUERY, 2).await,
        checkpoint
    );

    core.shutdown().await.expect("shutdown core");
}

/// A permanently-rejected (poison) event is dropped **and its sequence is
/// checkpointed**, so it is not replayed on restart — proving the standard
/// loop's `Dropped` outcome advances the checkpoint.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn permanent_4xx_event_is_dropped_and_not_replayed_after_restart() {
    let server = mock_server::start().await;
    // The downstream permanently rejects every request with a 404 (poison).
    Mock::given(method("POST"))
        .and(path("/changes/e2e-query"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    let store = Arc::new(MemoryStateStoreProvider::new());
    let (core, handle) = build_core(
        server.uri(),
        store.clone(),
        ReactionRecoveryPolicy::Strict,
        false,
    )
    .await;
    core.start().await.expect("start core");
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Alice is attempted, rejected with 404, dropped as poison; the reaction
    // stays up and advances its checkpoint past Alice's sequence.
    insert_person(&handle, "p1", "Alice").await;
    assert_eq!(
        wait_for_name_count(&server, 1, Duration::from_secs(10)).await,
        1,
        "Alice's request reached the server (and was 404'd)"
    );
    wait_for_checkpoint(&core, store.as_ref(), REACTION, QUERY, 1).await;
    assert_received(&core, &server, &[(1, "Alice")]).await;
    assert!(
        wait_for_reaction_status(&core, ComponentStatus::Running, Duration::from_secs(2)).await,
        "a permanent 4xx must not fail-stop the reaction (it is dropped as poison)"
    );

    // Switch the endpoint to healthy and CLEAR the request log, so any Alice
    // request seen from here on can only be a replay.
    respond_with(&server, "/changes/e2e-query", 200).await;

    // Restart: if the dropped event's sequence had NOT been checkpointed, the
    // forwarder would replay Alice from the outbox. It must not.
    stop_reaction_and_wait(&core).await;
    core.start_reaction(REACTION)
        .await
        .expect("restart reaction");
    insert_person(&handle, "p2", "Bob").await;

    assert_eq!(
        wait_for_name_count(&server, 1, Duration::from_secs(10)).await,
        1
    );
    assert_no_extra_deliveries(&server, 1, Duration::from_millis(750)).await;
    assert_eq!(
        names_received(&server).await,
        vec!["Bob"],
        "the dropped event's sequence was checkpointed, so Alice is not replayed"
    );
    assert_received(&core, &server, &[(2, "Bob")]).await;
    wait_for_checkpoint(&core, store.as_ref(), REACTION, QUERY, 2).await;

    core.shutdown().await.expect("shutdown core");
}

/// Repeated restarts must not trigger a false `config_hash` mismatch (which
/// under Strict would fail `start_reaction`). Regression guard for the
/// lazy-`config_hash` fix.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn config_hash_preserved_across_repeated_restarts() {
    let server = mock_server::start().await;
    respond_with(&server, "/changes/e2e-query", 200).await;
    let store = Arc::new(MemoryStateStoreProvider::new());
    let (core, handle) = build_core(
        server.uri(),
        store.clone(),
        ReactionRecoveryPolicy::Strict,
        false,
    )
    .await;
    core.start().await.expect("start core");
    tokio::time::sleep(Duration::from_millis(300)).await;

    insert_person(&handle, "p1", "Alice").await;
    assert_eq!(
        wait_for_name_count(&server, 1, Duration::from_secs(10)).await,
        1
    );
    let first = wait_for_checkpoint(&core, store.as_ref(), REACTION, QUERY, 1).await;

    stop_reaction_and_wait(&core).await;
    core.start_reaction(REACTION)
        .await
        .expect("restart #1 must succeed — no false config_hash mismatch");
    insert_person(&handle, "p2", "Bob").await;
    assert_eq!(
        wait_for_name_count(&server, 2, Duration::from_secs(10)).await,
        2
    );
    let second = wait_for_checkpoint(&core, store.as_ref(), REACTION, QUERY, 2).await;
    assert_eq!(second.config_hash, first.config_hash);

    stop_reaction_and_wait(&core).await;
    core.start_reaction(REACTION)
        .await
        .expect("restart #2 must succeed — no false config_hash mismatch");
    insert_person(&handle, "p3", "Carol").await;
    assert_eq!(
        wait_for_name_count(&server, 3, Duration::from_secs(10)).await,
        3
    );

    assert!(
        wait_for_reaction_status(&core, ComponentStatus::Running, Duration::from_secs(5)).await,
        "reaction must stay Running across repeated restarts"
    );
    assert_eq!(
        sorted(names_received(&server).await),
        vec!["Alice", "Bob", "Carol"],
        "every event delivered exactly once across two restarts"
    );
    assert_received(&core, &server, &[(1, "Alice"), (2, "Bob"), (3, "Carol")]).await;
    let third = wait_for_checkpoint(&core, store.as_ref(), REACTION, QUERY, 3).await;
    assert_eq!(third.config_hash, first.config_hash);

    core.shutdown().await.expect("shutdown core");
}

/// Under the default Strict policy a sustained 5xx outage drives the reaction to
/// `Error` without advancing the checkpoint; once the downstream recovers, a
/// restart replays the un-acked event (no loss).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn strict_fail_stops_on_sustained_failure_then_recovers_on_restart() {
    let server = mock_server::start().await;
    respond_with(&server, "/changes/e2e-query", 503).await; // downstream is down
    let store = Arc::new(MemoryStateStoreProvider::new());
    let (core, handle) = build_core(
        server.uri(),
        store.clone(),
        ReactionRecoveryPolicy::Strict,
        false,
    )
    .await;
    core.start().await.expect("start core");
    tokio::time::sleep(Duration::from_millis(300)).await;

    // The reaction exhausts its retries on this event and fail-stops.
    insert_person(&handle, "p1", "Alice").await;
    assert!(
        wait_for_reaction_status(&core, ComponentStatus::Error, Duration::from_secs(15)).await,
        "Strict reaction must fail-stop on a sustained delivery failure"
    );
    wait_for_outbox(&core, QUERY, 1).await;
    wait_for_checkpoint(&core, store.as_ref(), REACTION, QUERY, 0).await;

    // Recover: downstream comes back (this also clears the failed-attempt log),
    // operator restarts the reaction (valid Error → Starting transition).
    respond_with(&server, "/changes/e2e-query", 200).await;
    core.start_reaction(REACTION)
        .await
        .expect("restart reaction from Error");

    assert_eq!(
        wait_for_name_count(&server, 1, Duration::from_secs(10)).await,
        1
    );
    assert_eq!(
        names_received(&server).await,
        vec!["Alice"],
        "the un-acked event is replayed exactly once after recovery"
    );
    assert_received(&core, &server, &[(1, "Alice")]).await;
    wait_for_checkpoint(&core, store.as_ref(), REACTION, QUERY, 1).await;

    core.shutdown().await.expect("shutdown core");
}

/// Under AutoSkipGap a sustained outage is skipped (favoring uptime) and the
/// reaction keeps running to deliver later events.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auto_skip_gap_keeps_running_and_skips_failed_batch() {
    let server = mock_server::start().await;
    respond_with(&server, "/changes/e2e-query", 503).await;
    let store = Arc::new(MemoryStateStoreProvider::new());
    let (core, handle) = build_core(
        server.uri(),
        store.clone(),
        ReactionRecoveryPolicy::AutoSkipGap,
        false,
    )
    .await;
    core.start().await.expect("start core");
    tokio::time::sleep(Duration::from_millis(300)).await;

    insert_person(&handle, "p1", "Alice").await;
    wait_for_checkpoint(&core, store.as_ref(), REACTION, QUERY, 1).await;
    let failed_attempts = names_received(&server).await;
    assert!(
        !failed_attempts.is_empty(),
        "the failed delivery was attempted"
    );
    assert!(failed_attempts.iter().all(|name| name == "Alice"));
    assert!(
        wait_for_reaction_status(&core, ComponentStatus::Running, Duration::from_secs(2)).await,
        "AutoSkipGap reaction must stay Running after a skipped event"
    );

    // Downstream recovers; later events are delivered.
    respond_with(&server, "/changes/e2e-query", 200).await;
    insert_person(&handle, "p2", "Bob").await;
    assert_eq!(
        wait_for_name_count(&server, 1, Duration::from_secs(10)).await,
        1
    );
    assert_eq!(
        names_received(&server).await,
        vec!["Bob"],
        "the skipped event is dropped; later events are delivered"
    );
    assert_received(&core, &server, &[(2, "Bob")]).await;
    wait_for_checkpoint(&core, store.as_ref(), REACTION, QUERY, 2).await;

    core.shutdown().await.expect("shutdown core");
}

/// In adaptive (batched) mode a permanent 4xx is a poison batch: it is dropped,
/// the reaction stays Running, and later batches are delivered.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn adaptive_permanent_4xx_is_dropped_and_reaction_keeps_running() {
    let server = mock_server::start().await;
    respond_with(&server, "/batch", 400).await; // permanent client error
    let store = Arc::new(MemoryStateStoreProvider::new());
    let (core, handle) = build_core(
        server.uri(),
        store.clone(),
        ReactionRecoveryPolicy::Strict,
        true,
    )
    .await;
    core.start().await.expect("start core");
    tokio::time::sleep(Duration::from_millis(300)).await;

    // The first batch is rejected with a 4xx — dropped as poison, not retried.
    insert_person(&handle, "p1", "Alice").await;
    wait_for_checkpoint(&core, store.as_ref(), REACTION, QUERY, 1).await;
    assert_received(&core, &server, &[(1, "Alice")]).await;
    assert!(
        wait_for_reaction_status(&core, ComponentStatus::Running, Duration::from_secs(2)).await,
        "a permanent 4xx must not fail-stop the reaction"
    );

    // Downstream now accepts; a later batch is delivered.
    respond_with(&server, "/batch", 200).await;
    insert_person(&handle, "p2", "Bob").await;
    assert_eq!(
        wait_for_name_count(&server, 1, Duration::from_secs(10)).await,
        1
    );
    assert_eq!(
        names_received(&server).await,
        vec!["Bob"],
        "the poison batch is dropped; later batches are delivered"
    );
    assert_received(&core, &server, &[(2, "Bob")]).await;
    wait_for_checkpoint(&core, store.as_ref(), REACTION, QUERY, 2).await;
    stop_reaction_and_wait(&core).await;
    core.start_reaction(REACTION)
        .await
        .expect("restart adaptive reaction");
    assert!(
        wait_for_reaction_status(&core, ComponentStatus::Running, Duration::from_secs(5)).await
    );
    assert_no_extra_deliveries(&server, 1, Duration::from_millis(500)).await;
    assert_received(&core, &server, &[(2, "Bob")]).await;

    core.shutdown().await.expect("shutdown core");
}

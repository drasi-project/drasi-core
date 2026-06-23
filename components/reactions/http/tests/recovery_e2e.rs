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

use std::sync::Arc;
use std::time::Duration;

use drasi_lib::channels::ComponentStatus;
use drasi_lib::recovery::ReactionRecoveryPolicy;
use drasi_lib::state_store::StateStoreProvider;
use drasi_lib::{DrasiLib, MemoryStateStoreProvider, Query};
use drasi_reaction_http::{AdaptiveBatchConfig, HttpReaction};
use serde_json::Value;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

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
    let reqs = server.received_requests().await.unwrap_or_default();
    let mut names = Vec::new();
    for req in reqs {
        if let Ok(body) = serde_json::from_slice::<Value>(&req.body) {
            extract_names(&body, &mut names);
        }
    }
    names
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
    let (core, handle) =
        build_core(server.uri(), store, ReactionRecoveryPolicy::Strict, false).await;
    core.start().await.expect("start core");
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Deliver two events while running (they get checkpointed).
    insert_person(&handle, "p1", "Alice").await;
    insert_person(&handle, "p2", "Bob").await;
    assert_eq!(
        wait_for_name_count(&server, 2, Duration::from_secs(10)).await,
        2
    );

    // Stop the reaction, then produce two more (they queue in the outbox).
    stop_reaction_and_wait(&core).await;
    insert_person(&handle, "p3", "Carol").await;
    insert_person(&handle, "p4", "Dave").await;
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Restart — the reaction replays the two missed events from the outbox.
    core.start_reaction(REACTION)
        .await
        .expect("restart reaction");
    assert_eq!(
        wait_for_name_count(&server, 4, Duration::from_secs(10)).await,
        4
    );
    tokio::time::sleep(Duration::from_millis(300)).await;

    assert_eq!(
        sorted(names_received(&server).await),
        vec!["Alice", "Bob", "Carol", "Dave"],
        "all four events delivered exactly once across the restart"
    );

    core.stop().await.expect("stop core");
}

/// A restart with nothing missed must not re-deliver already-acked events.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clean_restart_does_not_redeliver_acked_events() {
    let server = mock_server::start().await;
    respond_with(&server, "/changes/e2e-query", 200).await;
    let store = Arc::new(MemoryStateStoreProvider::new());
    let (core, handle) =
        build_core(server.uri(), store, ReactionRecoveryPolicy::Strict, false).await;
    core.start().await.expect("start core");
    tokio::time::sleep(Duration::from_millis(300)).await;

    insert_person(&handle, "p1", "Alice").await;
    insert_person(&handle, "p2", "Bob").await;
    assert_eq!(
        wait_for_name_count(&server, 2, Duration::from_secs(10)).await,
        2
    );

    stop_reaction_and_wait(&core).await;
    core.start_reaction(REACTION)
        .await
        .expect("restart reaction");
    assert!(
        wait_for_reaction_status(&core, ComponentStatus::Running, Duration::from_secs(5)).await
    );
    tokio::time::sleep(Duration::from_millis(500)).await;

    assert_eq!(
        sorted(names_received(&server).await),
        vec!["Alice", "Bob"],
        "a clean restart must not re-deliver acked events"
    );

    core.stop().await.expect("stop core");
}

/// Repeated restarts must not trigger a false `config_hash` mismatch (which
/// under Strict would fail `start_reaction`). Regression guard for the
/// lazy-`config_hash` fix.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn config_hash_preserved_across_repeated_restarts() {
    let server = mock_server::start().await;
    respond_with(&server, "/changes/e2e-query", 200).await;
    let store = Arc::new(MemoryStateStoreProvider::new());
    let (core, handle) =
        build_core(server.uri(), store, ReactionRecoveryPolicy::Strict, false).await;
    core.start().await.expect("start core");
    tokio::time::sleep(Duration::from_millis(300)).await;

    insert_person(&handle, "p1", "Alice").await;
    assert_eq!(
        wait_for_name_count(&server, 1, Duration::from_secs(10)).await,
        1
    );

    stop_reaction_and_wait(&core).await;
    core.start_reaction(REACTION)
        .await
        .expect("restart #1 must succeed — no false config_hash mismatch");
    insert_person(&handle, "p2", "Bob").await;
    assert_eq!(
        wait_for_name_count(&server, 2, Duration::from_secs(10)).await,
        2
    );

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

    core.stop().await.expect("stop core");
}

/// Under the default Strict policy a sustained 5xx outage drives the reaction to
/// `Error` without advancing the checkpoint; once the downstream recovers, a
/// restart replays the un-acked event (no loss).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn strict_fail_stops_on_sustained_failure_then_recovers_on_restart() {
    let server = mock_server::start().await;
    respond_with(&server, "/changes/e2e-query", 503).await; // downstream is down
    let store = Arc::new(MemoryStateStoreProvider::new());
    let (core, handle) =
        build_core(server.uri(), store, ReactionRecoveryPolicy::Strict, false).await;
    core.start().await.expect("start core");
    tokio::time::sleep(Duration::from_millis(300)).await;

    // The reaction exhausts its retries on this event and fail-stops.
    insert_person(&handle, "p1", "Alice").await;
    assert!(
        wait_for_reaction_status(&core, ComponentStatus::Error, Duration::from_secs(15)).await,
        "Strict reaction must fail-stop on a sustained delivery failure"
    );

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

    core.stop().await.expect("stop core");
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
        store,
        ReactionRecoveryPolicy::AutoSkipGap,
        false,
    )
    .await;
    core.start().await.expect("start core");
    tokio::time::sleep(Duration::from_millis(300)).await;

    insert_person(&handle, "p1", "Alice").await;
    tokio::time::sleep(Duration::from_secs(2)).await; // let retries exhaust + skip
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

    core.stop().await.expect("stop core");
}

/// In adaptive (batched) mode a permanent 4xx is a poison batch: it is dropped,
/// the reaction stays Running, and later batches are delivered.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn adaptive_permanent_4xx_is_dropped_and_reaction_keeps_running() {
    let server = mock_server::start().await;
    respond_with(&server, "/batch", 400).await; // permanent client error
    let store = Arc::new(MemoryStateStoreProvider::new());
    let (core, handle) =
        build_core(server.uri(), store, ReactionRecoveryPolicy::Strict, true).await;
    core.start().await.expect("start core");
    tokio::time::sleep(Duration::from_millis(300)).await;

    // The first batch is rejected with a 4xx — dropped as poison, not retried.
    insert_person(&handle, "p1", "Alice").await;
    tokio::time::sleep(Duration::from_secs(1)).await;
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

    core.stop().await.expect("stop core");
}

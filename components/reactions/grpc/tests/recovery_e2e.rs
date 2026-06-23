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

//! End-to-end recovery / at-least-once tests for the gRPC reaction.
//!
//! Each test wires a **real** pipeline through the public `DrasiLib` API —
//! mock source → Cypher query (with an outbox) → the gRPC reaction → a mock
//! gRPC server — backed by a persistent state store, and asserts on **what the
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
//!   default Strict policy a sustained outage stops the reaction without losing the
//!   un-acked event, which is replayed once the downstream recovers.
//! * `auto_skip_gap_keeps_running_and_skips_failed_batch` — under AutoSkipGap a
//!   sustained outage is skipped and the reaction keeps delivering later events.

mod mock_server;
mod mock_source;

use std::sync::Arc;
use std::time::Duration;

use drasi_lib::channels::ComponentStatus;
use drasi_lib::recovery::ReactionRecoveryPolicy;
use drasi_lib::state_store::StateStoreProvider;
use drasi_lib::{DrasiLib, MemoryStateStoreProvider, Query};
use drasi_reaction_grpc::GrpcReaction;

use mock_server::{struct_to_json, MockServer};
use mock_source::{MockSource, MockSourceHandle, PropertyMapBuilder};

const SOURCE: &str = "e2e-source";
const QUERY: &str = "e2e-query";
const REACTION: &str = "e2e-grpc";

/// Build a `source → query(outbox) → grpc reaction` pipeline that persists
/// checkpoints to `store` and delivers to `endpoint` with the given recovery
/// policy. Fixed batching of 1 means one outbox entry == one delivered batch.
async fn build_core(
    endpoint: String,
    store: Arc<dyn StateStoreProvider>,
    policy: ReactionRecoveryPolicy,
) -> (Arc<DrasiLib>, MockSourceHandle) {
    let (mock_source, handle) = MockSource::new(SOURCE).expect("mock source");

    let query = Query::cypher(QUERY)
        .query("MATCH (p:Person) RETURN p.name AS name, p.age AS age")
        .from_source(SOURCE)
        .with_outbox_capacity(100)
        .auto_start(true)
        .build();

    let reaction = GrpcReaction::builder(REACTION)
        .with_endpoint(endpoint)
        .from_query(QUERY)
        .with_fixed_batching(1, 200)
        .with_recovery_policy(policy)
        .build()
        .expect("reaction builder");

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

async fn insert_person(handle: &MockSourceHandle, id: &str, name: &str) {
    let props = PropertyMapBuilder::new()
        .with_string("name", name)
        .with_integer("age", 30)
        .build();
    handle
        .send_node_insert(id, vec!["Person"], props)
        .await
        .expect("send node insert");
}

/// Every person name the downstream server has received so far, in arrival
/// order (one entry per delivered row). Duplicates would show up as repeats.
async fn names_received(server: &MockServer) -> Vec<String> {
    let mut names = Vec::new();
    for batch in server.recorder.batches().await {
        for item in &batch.items {
            if let Some(after) = item.after.as_ref() {
                if let Some(name) = struct_to_json(after).get("name").and_then(|v| v.as_str()) {
                    names.push(name.to_string());
                }
            }
        }
    }
    names
}

fn sorted(mut v: Vec<String>) -> Vec<String> {
    v.sort();
    v
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
    // Arrange: a healthy downstream and a persistent checkpoint store.
    let server = mock_server::start().await;
    let store = Arc::new(MemoryStateStoreProvider::new());
    let (core, handle) = build_core(
        server.endpoint.clone(),
        store,
        ReactionRecoveryPolicy::Strict,
    )
    .await;
    core.start().await.expect("start core");
    tokio::time::sleep(Duration::from_millis(300)).await; // let subscriptions wire up

    // Act 1: deliver two events while running (they get checkpointed).
    insert_person(&handle, "p1", "Alice").await;
    insert_person(&handle, "p2", "Bob").await;
    assert_eq!(
        server
            .recorder
            .wait_for_items(2, Duration::from_secs(10))
            .await,
        2
    );

    // Act 2: stop the reaction, then produce two more (they queue in the outbox).
    stop_reaction_and_wait(&core).await;
    insert_person(&handle, "p3", "Carol").await;
    insert_person(&handle, "p4", "Dave").await;
    tokio::time::sleep(Duration::from_millis(500)).await; // let the query fill the outbox

    // Act 3: restart — the reaction replays the two missed events from the outbox.
    core.start_reaction(REACTION)
        .await
        .expect("restart reaction");
    assert_eq!(
        server
            .recorder
            .wait_for_items(4, Duration::from_secs(10))
            .await,
        4
    );
    tokio::time::sleep(Duration::from_millis(300)).await; // catch any erroneous extra deliveries

    // Assert: every event delivered exactly once — the missed two replayed, the
    // first two were not re-sent.
    assert_eq!(
        sorted(names_received(&server).await),
        vec!["Alice", "Bob", "Carol", "Dave"],
        "all four events delivered exactly once across the restart"
    );

    core.stop().await.expect("stop core");
    server.shutdown().await;
}

/// A restart with nothing missed must not re-deliver already-acked events.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clean_restart_does_not_redeliver_acked_events() {
    let server = mock_server::start().await;
    let store = Arc::new(MemoryStateStoreProvider::new());
    let (core, handle) = build_core(
        server.endpoint.clone(),
        store,
        ReactionRecoveryPolicy::Strict,
    )
    .await;
    core.start().await.expect("start core");
    tokio::time::sleep(Duration::from_millis(300)).await;

    insert_person(&handle, "p1", "Alice").await;
    insert_person(&handle, "p2", "Bob").await;
    assert_eq!(
        server
            .recorder
            .wait_for_items(2, Duration::from_secs(10))
            .await,
        2
    );

    // Restart with no new events in between.
    stop_reaction_and_wait(&core).await;
    core.start_reaction(REACTION)
        .await
        .expect("restart reaction");
    assert!(
        wait_for_reaction_status(&core, ComponentStatus::Running, Duration::from_secs(5)).await
    );
    tokio::time::sleep(Duration::from_millis(500)).await; // give any spurious replay a chance

    assert_eq!(
        sorted(names_received(&server).await),
        vec!["Alice", "Bob"],
        "a clean restart must not re-deliver acked events"
    );

    core.stop().await.expect("stop core");
    server.shutdown().await;
}

/// Repeated restarts must not trigger a false `config_hash` mismatch (which
/// under Strict would fail `start_reaction`). Regression guard for the
/// lazy-`config_hash` fix.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn config_hash_preserved_across_repeated_restarts() {
    let server = mock_server::start().await;
    let store = Arc::new(MemoryStateStoreProvider::new());
    let (core, handle) = build_core(
        server.endpoint.clone(),
        store,
        ReactionRecoveryPolicy::Strict,
    )
    .await;
    core.start().await.expect("start core");
    tokio::time::sleep(Duration::from_millis(300)).await;

    insert_person(&handle, "p1", "Alice").await;
    assert_eq!(
        server
            .recorder
            .wait_for_items(1, Duration::from_secs(10))
            .await,
        1
    );

    // Restart #1 (after the reaction has written its first checkpoint).
    stop_reaction_and_wait(&core).await;
    core.start_reaction(REACTION)
        .await
        .expect("restart #1 must succeed — no false config_hash mismatch");
    insert_person(&handle, "p2", "Bob").await;
    assert_eq!(
        server
            .recorder
            .wait_for_items(2, Duration::from_secs(10))
            .await,
        2
    );

    // Restart #2.
    stop_reaction_and_wait(&core).await;
    core.start_reaction(REACTION)
        .await
        .expect("restart #2 must succeed — no false config_hash mismatch");
    insert_person(&handle, "p3", "Carol").await;
    assert_eq!(
        server
            .recorder
            .wait_for_items(3, Duration::from_secs(10))
            .await,
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
    server.shutdown().await;
}

/// Under the default Strict policy a sustained outage drives the reaction to
/// `Error` without advancing the checkpoint; once the downstream recovers, a
/// restart replays the un-acked event (no loss).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn strict_fail_stops_on_sustained_failure_then_recovers_on_restart() {
    let server = mock_server::start().await;
    let store = Arc::new(MemoryStateStoreProvider::new());
    let (core, handle) = build_core(
        server.endpoint.clone(),
        store,
        ReactionRecoveryPolicy::Strict,
    )
    .await;
    core.start().await.expect("start core");
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Arrange: the downstream is down.
    server.set_failing(true);

    // Act: produce an event — the reaction exhausts its retries and fail-stops.
    insert_person(&handle, "p1", "Alice").await;
    assert!(
        wait_for_reaction_status(&core, ComponentStatus::Error, Duration::from_secs(15)).await,
        "Strict reaction must fail-stop on a sustained delivery failure"
    );
    assert!(
        names_received(&server).await.is_empty(),
        "nothing should have been delivered while the downstream was down"
    );

    // Recover: downstream comes back, operator restarts the reaction. A
    // fail-stopped reaction is in `Error`; `start_reaction` retries from there.
    server.set_failing(false);
    core.start_reaction(REACTION)
        .await
        .expect("restart reaction from Error");

    // Assert: the previously-failed event is replayed from the outbox.
    assert_eq!(
        server
            .recorder
            .wait_for_items(1, Duration::from_secs(10))
            .await,
        1
    );
    assert_eq!(
        names_received(&server).await,
        vec!["Alice"],
        "the un-acked event is replayed exactly once after recovery"
    );

    core.stop().await.expect("stop core");
    server.shutdown().await;
}

/// Under AutoSkipGap a sustained outage is skipped (favoring uptime) and the
/// reaction keeps running to deliver later events.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auto_skip_gap_keeps_running_and_skips_failed_batch() {
    let server = mock_server::start().await;
    let store = Arc::new(MemoryStateStoreProvider::new());
    let (core, handle) = build_core(
        server.endpoint.clone(),
        store,
        ReactionRecoveryPolicy::AutoSkipGap,
    )
    .await;
    core.start().await.expect("start core");
    tokio::time::sleep(Duration::from_millis(300)).await;

    // The downstream is down for the first event.
    server.set_failing(true);
    insert_person(&handle, "p1", "Alice").await;
    // Give the reaction time to exhaust retries and skip the batch.
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(
        wait_for_reaction_status(&core, ComponentStatus::Running, Duration::from_secs(2)).await,
        "AutoSkipGap reaction must stay Running after a skipped batch"
    );

    // The downstream recovers; later events are delivered.
    server.set_failing(false);
    insert_person(&handle, "p2", "Bob").await;
    assert_eq!(
        server
            .recorder
            .wait_for_items(1, Duration::from_secs(10))
            .await,
        1
    );
    assert_eq!(
        names_received(&server).await,
        vec!["Bob"],
        "the skipped event is dropped; later events are delivered"
    );

    core.stop().await.expect("stop core");
    server.shutdown().await;
}

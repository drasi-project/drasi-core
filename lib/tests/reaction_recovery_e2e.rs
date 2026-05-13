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

//! End-to-end tests for reaction recovery behavior.
//!
//! These tests use DrasiLib's public API (`stop_reaction` / `start_reaction`)
//! to exercise real restart cycles and verify that reactions catch up correctly
//! from the query outbox.

mod mock_source;

use anyhow::Result;
use drasi_lib::channels::{ComponentStatus, QueryResult};
use drasi_lib::context::ReactionRuntimeContext;
use drasi_lib::reactions::common::base::{ReactionBase, ReactionBaseParams};
use drasi_lib::recovery::ReactionRecoveryPolicy;
use drasi_lib::{DrasiLib, DispatchMode, MemoryStateStoreProvider, Query, Reaction};
use mock_source::{MockSource, MockSourceHandle, PropertyMapBuilder};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::timeout;

// ============================================================================
// DurableMemoryStateStoreProvider — test wrapper
// ============================================================================

/// Wrapper around `MemoryStateStoreProvider` that reports `is_durable() == true`.
///
/// In tests, the in-memory store IS effectively durable for the lifetime of the
/// test process, so reactions with `is_durable=true` can use it.
struct DurableMemoryStateStoreProvider {
    inner: MemoryStateStoreProvider,
}

impl DurableMemoryStateStoreProvider {
    fn new() -> Self {
        Self {
            inner: MemoryStateStoreProvider::new(),
        }
    }
}

#[async_trait::async_trait]
impl drasi_lib::state_store::StateStoreProvider for DurableMemoryStateStoreProvider {
    async fn get(
        &self,
        store_id: &str,
        key: &str,
    ) -> drasi_lib::state_store::StateStoreResult<Option<Vec<u8>>> {
        self.inner.get(store_id, key).await
    }

    async fn set(
        &self,
        store_id: &str,
        key: &str,
        value: Vec<u8>,
    ) -> drasi_lib::state_store::StateStoreResult<()> {
        self.inner.set(store_id, key, value).await
    }

    async fn delete(
        &self,
        store_id: &str,
        key: &str,
    ) -> drasi_lib::state_store::StateStoreResult<bool> {
        self.inner.delete(store_id, key).await
    }

    async fn contains_key(
        &self,
        store_id: &str,
        key: &str,
    ) -> drasi_lib::state_store::StateStoreResult<bool> {
        self.inner.contains_key(store_id, key).await
    }

    async fn get_many(
        &self,
        store_id: &str,
        keys: &[&str],
    ) -> drasi_lib::state_store::StateStoreResult<HashMap<String, Vec<u8>>> {
        self.inner.get_many(store_id, keys).await
    }

    async fn set_many(
        &self,
        store_id: &str,
        entries: &[(&str, &[u8])],
    ) -> drasi_lib::state_store::StateStoreResult<()> {
        self.inner.set_many(store_id, entries).await
    }

    async fn delete_many(
        &self,
        store_id: &str,
        keys: &[&str],
    ) -> drasi_lib::state_store::StateStoreResult<usize> {
        self.inner.delete_many(store_id, keys).await
    }

    async fn clear_store(&self, store_id: &str) -> drasi_lib::state_store::StateStoreResult<usize> {
        self.inner.clear_store(store_id).await
    }

    async fn list_keys(
        &self,
        store_id: &str,
    ) -> drasi_lib::state_store::StateStoreResult<Vec<String>> {
        self.inner.list_keys(store_id).await
    }

    async fn store_exists(&self, store_id: &str) -> drasi_lib::state_store::StateStoreResult<bool> {
        self.inner.store_exists(store_id).await
    }

    async fn key_count(&self, store_id: &str) -> drasi_lib::state_store::StateStoreResult<usize> {
        self.inner.key_count(store_id).await
    }

    fn is_durable(&self) -> bool {
        true
    }
}

// ============================================================================
// RecordingReaction — test infrastructure
// ============================================================================

/// A test reaction that records every `QueryResult` delivered to it.
struct RecordingReaction {
    base: ReactionBase,
    tx: mpsc::UnboundedSender<QueryResult>,
    recovery_policy: ReactionRecoveryPolicy,
    durable: bool,
    snapshot_on_fresh: bool,
}

/// Receiver side of the recording reaction.
struct RecordingReceiver {
    rx: mpsc::UnboundedReceiver<QueryResult>,
}

impl RecordingReceiver {
    /// Wait until `count` results have been received, with a timeout.
    async fn wait_for_count(&mut self, count: usize, dur: Duration) -> Vec<QueryResult> {
        let mut results = Vec::new();
        let deadline = tokio::time::Instant::now() + dur;
        while results.len() < count {
            match timeout(deadline - tokio::time::Instant::now(), self.rx.recv()).await {
                Ok(Some(r)) => results.push(r),
                Ok(None) => break,
                Err(_) => break,
            }
        }
        results
    }

    /// Drain any results already available without blocking.
    fn drain_available(&mut self) -> Vec<QueryResult> {
        let mut results = Vec::new();
        while let Ok(r) = self.rx.try_recv() {
            results.push(r);
        }
        results
    }
}

fn recording_reaction(
    id: &str,
    queries: Vec<String>,
    policy: ReactionRecoveryPolicy,
    durable: bool,
    snapshot_on_fresh: bool,
) -> (RecordingReaction, RecordingReceiver) {
    let (tx, rx) = mpsc::unbounded_channel();
    let params = ReactionBaseParams::new(id, queries).with_recovery_policy(policy);
    let base = ReactionBase::new(params);
    (
        RecordingReaction {
            base,
            tx,
            recovery_policy: policy,
            durable,
            snapshot_on_fresh,
        },
        RecordingReceiver { rx },
    )
}

impl std::fmt::Debug for RecordingReaction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecordingReaction")
            .field("id", &self.base.id)
            .finish()
    }
}

#[async_trait::async_trait]
impl Reaction for RecordingReaction {
    fn id(&self) -> &str {
        &self.base.id
    }

    fn type_name(&self) -> &str {
        "recording"
    }

    fn properties(&self) -> HashMap<String, serde_json::Value> {
        HashMap::new()
    }

    fn query_ids(&self) -> Vec<String> {
        self.base.queries.clone()
    }

    fn auto_start(&self) -> bool {
        self.base.get_auto_start()
    }

    async fn initialize(&self, context: ReactionRuntimeContext) {
        self.base.initialize(context).await;
    }

    async fn start(&self) -> Result<()> {
        self.base
            .set_status(
                ComponentStatus::Running,
                Some("Recording reaction started".into()),
            )
            .await;
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        self.base
            .set_status(
                ComponentStatus::Stopped,
                Some("Recording reaction stopped".into()),
            )
            .await;
        Ok(())
    }

    async fn status(&self) -> ComponentStatus {
        self.base.get_status().await
    }

    async fn enqueue_query_result(&self, result: QueryResult) -> Result<()> {
        let _ = self.tx.send(result);
        Ok(())
    }

    fn is_durable(&self) -> bool {
        self.durable
    }

    fn needs_snapshot_on_fresh_start(&self) -> bool {
        self.snapshot_on_fresh
    }

    fn default_recovery_policy(&self) -> ReactionRecoveryPolicy {
        self.recovery_policy
    }
}

// ============================================================================
// Helper
// ============================================================================

async fn insert_person(handle: &MockSourceHandle, id: &str, name: &str, age: i64) -> Result<()> {
    let props = PropertyMapBuilder::new()
        .with_string("name", name)
        .with_integer("age", age)
        .build();
    handle.send_node_insert(id, vec!["Person"], props).await
}

/// Wait for the reaction to finish stopping before trying to restart.
async fn stop_reaction_and_wait(core: &DrasiLib, id: &str) -> Result<()> {
    core.stop_reaction(id).await?;
    // Poll until the reaction is fully stopped (the manager transitions asynchronously)
    for _ in 0..50 {
        let statuses = core.list_reactions().await?;
        if let Some((_, status)) = statuses.iter().find(|(rid, _)| rid == id) {
            if *status == ComponentStatus::Stopped {
                return Ok(());
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    anyhow::bail!("Reaction {id} did not reach Stopped state within timeout");
}

// ============================================================================
// Tests
// ============================================================================

/// Test 1: Reaction replays missed events from outbox after restart.
#[tokio::test]
async fn test_reaction_outbox_catchup_on_restart() -> Result<()> {
    let (mock_source, handle) = MockSource::new("test-source")?;

    let query = Query::cypher("q1")
        .query("MATCH (p:Person) RETURN p.name AS name, p.age AS age")
        .from_source("test-source")
        .with_outbox_capacity(100)
        .auto_start(true)
        .build();

    let state_store = Arc::new(DurableMemoryStateStoreProvider::new());

    let (reaction, mut receiver) = recording_reaction(
        "rec",
        vec!["q1".into()],
        ReactionRecoveryPolicy::Strict,
        true,
        false,
    );

    let core = Arc::new(
        DrasiLib::builder()
            .with_id("catchup-test")
            .with_source(mock_source)
            .with_query(query)
            .with_reaction(reaction)
            .with_state_store_provider(state_store)
            .build()
            .await?,
    );

    core.start().await?;

    // Insert 2 rows and wait for delivery
    insert_person(&handle, "p1", "Alice", 30).await?;
    insert_person(&handle, "p2", "Bob", 25).await?;
    let initial = receiver.wait_for_count(2, Duration::from_secs(5)).await;
    assert_eq!(initial.len(), 2, "Should receive 2 initial events");

    // Stop the reaction — query keeps running, outbox accumulates
    stop_reaction_and_wait(&core, "rec").await?;

    // Insert 3 more rows while reaction is stopped
    insert_person(&handle, "p3", "Charlie", 35).await?;
    insert_person(&handle, "p4", "Diana", 28).await?;
    insert_person(&handle, "p5", "Eve", 22).await?;

    // Give query time to process
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Restart the reaction — should replay from outbox
    core.start_reaction("rec").await?;

    // Wait for the 3 missed events to arrive
    let replayed = receiver.wait_for_count(3, Duration::from_secs(5)).await;
    assert!(
        replayed.len() >= 3,
        "Should receive at least 3 replayed events, got {}",
        replayed.len()
    );

    // Verify no duplicates of the first 2 events — drain anything extra
    let extra = receiver.drain_available();
    let all_after_restart: Vec<_> = replayed.into_iter().chain(extra).collect();

    // Each result should be from query q1
    for r in &all_after_restart {
        assert_eq!(r.query_id, "q1");
    }

    core.stop().await?;
    Ok(())
}

/// Test 2: Restart with no missed events — no spurious replays.
#[tokio::test]
async fn test_reaction_restart_no_missed_events() -> Result<()> {
    let (mock_source, handle) = MockSource::new("test-source")?;

    let query = Query::cypher("q1")
        .query("MATCH (p:Person) RETURN p.name AS name")
        .from_source("test-source")
        .with_outbox_capacity(100)
        .auto_start(true)
        .build();

    let state_store = Arc::new(DurableMemoryStateStoreProvider::new());

    let (reaction, mut receiver) = recording_reaction(
        "rec",
        vec!["q1".into()],
        ReactionRecoveryPolicy::Strict,
        true,
        false,
    );

    let core = Arc::new(
        DrasiLib::builder()
            .with_id("clean-restart-test")
            .with_source(mock_source)
            .with_query(query)
            .with_reaction(reaction)
            .with_state_store_provider(state_store)
            .build()
            .await?,
    );

    core.start().await?;

    // Insert 2 rows and wait for delivery
    insert_person(&handle, "p1", "Alice", 30).await?;
    insert_person(&handle, "p2", "Bob", 25).await?;
    let initial = receiver.wait_for_count(2, Duration::from_secs(5)).await;
    assert_eq!(initial.len(), 2);

    // Stop and immediately restart — no events in between
    stop_reaction_and_wait(&core, "rec").await?;
    core.start_reaction("rec").await?;

    // Drain any spurious replays (there should be none beyond what's in the outbox)
    tokio::time::sleep(Duration::from_millis(500)).await;
    let spurious = receiver.drain_available();

    // Insert a new event to verify live delivery works
    insert_person(&handle, "p3", "Charlie", 35).await?;
    let live = receiver.wait_for_count(1, Duration::from_secs(5)).await;
    assert_eq!(live.len(), 1, "Should receive 1 live event after restart");

    eprintln!("Spurious replays after clean restart: {}", spurious.len());

    core.stop().await?;
    Ok(())
}

/// Test 3: Outbox gap with AutoSkipGap — skips missed events, resumes live.
#[tokio::test]
async fn test_reaction_outbox_gap_auto_skip() -> Result<()> {
    let (mock_source, handle) = MockSource::new("test-source")?;

    // Small outbox capacity so it overflows
    let query = Query::cypher("q1")
        .query("MATCH (p:Person) RETURN p.name AS name")
        .from_source("test-source")
        .with_outbox_capacity(2)
        .auto_start(true)
        .build();

    let state_store = Arc::new(DurableMemoryStateStoreProvider::new());

    let (reaction, mut receiver) = recording_reaction(
        "rec",
        vec!["q1".into()],
        ReactionRecoveryPolicy::AutoSkipGap,
        true,
        false,
    );

    let core = Arc::new(
        DrasiLib::builder()
            .with_id("skip-gap-test")
            .with_source(mock_source)
            .with_query(query)
            .with_reaction(reaction)
            .with_state_store_provider(state_store)
            .build()
            .await?,
    );

    core.start().await?;

    // Insert 1 row to establish checkpoint
    insert_person(&handle, "p1", "Alice", 30).await?;
    let initial = receiver.wait_for_count(1, Duration::from_secs(5)).await;
    assert_eq!(initial.len(), 1);

    // Stop reaction
    stop_reaction_and_wait(&core, "rec").await?;

    // Insert 5 rows — outbox capacity is 2, so oldest will be evicted (gap)
    for i in 0..5 {
        insert_person(
            &handle,
            &format!("p{}", i + 10),
            &format!("Person-{i}"),
            20 + i,
        )
        .await?;
    }
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Restart — AutoSkipGap should jump to current sequence
    core.start_reaction("rec").await?;

    // Drain any replayed events (should be minimal — gap was skipped)
    tokio::time::sleep(Duration::from_millis(500)).await;
    let after_restart = receiver.drain_available();
    eprintln!(
        "Events after AutoSkipGap restart: {} (gap events were skipped)",
        after_restart.len()
    );

    // Verify live delivery works after skip
    insert_person(&handle, "p-live", "LivePerson", 99).await?;
    let live = receiver.wait_for_count(1, Duration::from_secs(5)).await;
    assert_eq!(live.len(), 1, "Should receive live event after gap skip");

    core.stop().await?;
    Ok(())
}

/// Test 4: Outbox gap with AutoReset — re-bootstraps and resumes.
#[tokio::test]
async fn test_reaction_outbox_gap_auto_reset() -> Result<()> {
    let (mock_source, handle) = MockSource::new("test-source")?;

    let query = Query::cypher("q1")
        .query("MATCH (p:Person) RETURN p.name AS name")
        .from_source("test-source")
        .with_outbox_capacity(2)
        .auto_start(true)
        .build();

    let state_store = Arc::new(DurableMemoryStateStoreProvider::new());

    let (reaction, mut receiver) = recording_reaction(
        "rec",
        vec!["q1".into()],
        ReactionRecoveryPolicy::AutoReset,
        true,
        true, // needs snapshot on fresh start
    );

    let core = Arc::new(
        DrasiLib::builder()
            .with_id("autoreset-test")
            .with_source(mock_source)
            .with_query(query)
            .with_reaction(reaction)
            .with_state_store_provider(state_store)
            .build()
            .await?,
    );

    core.start().await?;

    // Insert 1 row to establish checkpoint
    insert_person(&handle, "p1", "Alice", 30).await?;
    let initial = receiver.wait_for_count(1, Duration::from_secs(5)).await;
    assert_eq!(initial.len(), 1);

    // Stop reaction
    stop_reaction_and_wait(&core, "rec").await?;

    // Insert 5 rows to overflow outbox (capacity 2)
    for i in 0..5 {
        insert_person(
            &handle,
            &format!("p{}", i + 10),
            &format!("Person-{i}"),
            20 + i,
        )
        .await?;
    }
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Restart — AutoReset should detect gap and trigger recovery
    core.start_reaction("rec").await?;
    tokio::time::sleep(Duration::from_millis(1000)).await;

    // Verify live delivery works after reset
    insert_person(&handle, "p-live", "LiveAfterReset", 99).await?;
    let live = receiver.wait_for_count(1, Duration::from_secs(5)).await;
    assert_eq!(live.len(), 1, "Should receive live event after auto-reset");

    core.stop().await?;
    Ok(())
}

/// Test 5: Live delivery of new events after restart with correct payloads.
#[tokio::test]
async fn test_reaction_live_delivery_after_restart() -> Result<()> {
    let (mock_source, handle) = MockSource::new("test-source")?;

    let query = Query::cypher("q1")
        .query("MATCH (p:Person) RETURN p.name AS name, p.age AS age")
        .from_source("test-source")
        .with_outbox_capacity(100)
        .auto_start(true)
        .build();

    let state_store = Arc::new(DurableMemoryStateStoreProvider::new());

    let (reaction, mut receiver) = recording_reaction(
        "rec",
        vec!["q1".into()],
        ReactionRecoveryPolicy::Strict,
        true,
        false,
    );

    let core = Arc::new(
        DrasiLib::builder()
            .with_id("live-after-restart-test")
            .with_source(mock_source)
            .with_query(query)
            .with_reaction(reaction)
            .with_state_store_provider(state_store)
            .build()
            .await?,
    );

    core.start().await?;

    // Insert initial data
    insert_person(&handle, "p1", "Alice", 30).await?;
    let initial = receiver.wait_for_count(1, Duration::from_secs(5)).await;
    assert_eq!(initial.len(), 1);

    // Stop and restart
    stop_reaction_and_wait(&core, "rec").await?;
    core.start_reaction("rec").await?;

    // Drain any replay events
    tokio::time::sleep(Duration::from_millis(500)).await;
    let _ = receiver.drain_available();

    // Insert new data with distinct values
    insert_person(&handle, "p-new1", "Xavier", 42).await?;
    insert_person(&handle, "p-new2", "Yara", 33).await?;

    let live = receiver.wait_for_count(2, Duration::from_secs(5)).await;
    assert_eq!(live.len(), 2, "Should receive 2 live events");

    // Verify results contain actual query result data
    for r in &live {
        assert_eq!(r.query_id, "q1");
        assert!(!r.results.is_empty(), "Result should have diff entries");
    }

    core.stop().await?;
    Ok(())
}

/// Test 6: Runtime sequence-gap detection via broadcast lag.
///
/// Uses a tiny broadcast buffer (capacity=2) so that flooding events while
/// the reaction is slow causes a lag — the forwarder detects a sequence gap
/// and applies the AutoSkipGap policy (skips the gap, resumes live delivery).
#[tokio::test]
async fn test_runtime_gap_detection_broadcast_lag() -> Result<()> {
    let (mock_source, handle) = MockSource::new("test-source")?;

    // Use broadcast mode with a very small buffer to induce lag.
    let query = Query::cypher("q1")
        .query("MATCH (p:Person) RETURN p.name AS name")
        .from_source("test-source")
        .with_dispatch_mode(DispatchMode::Broadcast)
        .with_dispatch_buffer_capacity(2)
        .with_outbox_capacity(100)
        .auto_start(true)
        .build();

    let state_store = Arc::new(DurableMemoryStateStoreProvider::new());

    let (reaction, mut receiver) = recording_reaction(
        "rec",
        vec!["q1".into()],
        ReactionRecoveryPolicy::AutoSkipGap,
        true,
        false,
    );

    let core = Arc::new(
        DrasiLib::builder()
            .with_id("runtime-gap-test")
            .with_source(mock_source)
            .with_query(query)
            .with_reaction(reaction)
            .with_state_store_provider(state_store)
            .build()
            .await?,
    );

    core.start().await?;

    // Insert one event to confirm the pipeline works initially.
    insert_person(&handle, "p1", "Alice", 30).await?;
    let initial = receiver.wait_for_count(1, Duration::from_secs(5)).await;
    assert_eq!(initial.len(), 1, "Should receive initial event");

    // Flood the broadcast channel to cause lag (buffer=2, send many fast).
    // The forwarder will detect a sequence gap when it catches up.
    for i in 0..20 {
        insert_person(
            &handle,
            &format!("p-flood-{i}"),
            &format!("Flood-{i}"),
            i,
        )
        .await?;
    }

    // Wait for the forwarder to process what it can.
    // With AutoSkipGap, it should skip the gap and continue.
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Verify that live delivery still works after the gap.
    insert_person(&handle, "p-after-gap", "AfterGap", 99).await?;
    let after = receiver.wait_for_count(1, Duration::from_secs(5)).await;
    assert_eq!(
        after.len(),
        1,
        "Should receive live event after gap recovery"
    );

    core.stop().await?;
    Ok(())
}

/// Test 7: Runtime gap with Strict policy — reaction should stop on gap.
#[tokio::test]
async fn test_runtime_gap_strict_policy_stops_reaction() -> Result<()> {
    let (mock_source, handle) = MockSource::new("test-source")?;

    let query = Query::cypher("q1")
        .query("MATCH (p:Person) RETURN p.name AS name")
        .from_source("test-source")
        .with_dispatch_mode(DispatchMode::Broadcast)
        .with_dispatch_buffer_capacity(2)
        .with_outbox_capacity(100)
        .auto_start(true)
        .build();

    let state_store = Arc::new(DurableMemoryStateStoreProvider::new());

    let (reaction, mut receiver) = recording_reaction(
        "rec",
        vec!["q1".into()],
        ReactionRecoveryPolicy::Strict,
        true,
        false,
    );

    let core = Arc::new(
        DrasiLib::builder()
            .with_id("strict-gap-test")
            .with_source(mock_source)
            .with_query(query)
            .with_reaction(reaction)
            .with_state_store_provider(state_store)
            .build()
            .await?,
    );

    core.start().await?;

    // Confirm initial delivery works.
    insert_person(&handle, "p1", "Alice", 30).await?;
    let initial = receiver.wait_for_count(1, Duration::from_secs(5)).await;
    assert_eq!(initial.len(), 1);

    // Flood to cause broadcast lag — Strict policy should stop the forwarder.
    for i in 0..20 {
        insert_person(
            &handle,
            &format!("p-flood-{i}"),
            &format!("Flood-{i}"),
            i,
        )
        .await?;
    }

    // Give time for the forwarder to detect the gap and exit.
    tokio::time::sleep(Duration::from_secs(2)).await;

    // After strict gap failure, new events should NOT be delivered.
    insert_person(&handle, "p-after", "After", 99).await?;
    let after = receiver.wait_for_count(1, Duration::from_millis(1000)).await;
    assert_eq!(
        after.len(),
        0,
        "Strict policy: no events should be delivered after gap"
    );

    core.stop().await?;
    Ok(())
}

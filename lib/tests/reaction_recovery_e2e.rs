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
use drasi_index_rocksdb::RocksDbIndexProvider;
use drasi_lib::channels::{ComponentStatus, QueryResult};
use drasi_lib::context::ReactionRuntimeContext;
use drasi_lib::reactions::common::base::{ReactionBase, ReactionBaseParams};
use drasi_lib::recovery::ReactionRecoveryPolicy;
use drasi_lib::{
    DispatchMode, DrasiLib, IndexBackendPlugin, MemoryStateStoreProvider, Query, Reaction,
    ReactionCheckpoint, StorageBackendRef,
};
use mock_source::{MockSource, MockSourceHandle, PropertyMapBuilder};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;
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
    recording_reaction_with_auto_start(id, queries, policy, durable, snapshot_on_fresh, true)
}

fn recording_reaction_with_auto_start(
    id: &str,
    queries: Vec<String>,
    policy: ReactionRecoveryPolicy,
    durable: bool,
    snapshot_on_fresh: bool,
    auto_start: bool,
) -> (RecordingReaction, RecordingReceiver) {
    let (tx, rx) = mpsc::unbounded_channel();
    let params = ReactionBaseParams::new(id, queries)
        .with_recovery_policy(policy)
        .with_auto_start(auto_start);
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
        let query_id = result.query_id.clone();
        let sequence = result.sequence;

        self.tx
            .send(result)
            .map_err(|_| anyhow::anyhow!("Recording reaction receiver closed"))?;

        // Advance only after the host has seeded a checkpoint. Writing
        // config_hash=0 here would clobber the real hash persisted at startup.
        if let Some(previous) = self.base.read_checkpoint(&query_id).await? {
            self.base
                .write_checkpoint(
                    &query_id,
                    &ReactionCheckpoint {
                        sequence,
                        config_hash: previous.config_hash,
                    },
                )
                .await?;
        }
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

async fn wait_for_query_result_count(
    core: &DrasiLib,
    query_id: &str,
    expected: usize,
) -> Result<()> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut last_count = None;
    loop {
        match core.get_query_results(query_id).await {
            Ok(results) if results.len() == expected => return Ok(()),
            Ok(results) if results.len() > expected => {
                anyhow::bail!(
                    "Query {query_id} produced {} results, expected {expected}",
                    results.len()
                );
            }
            Ok(results) => last_count = Some(results.len()),
            Err(_) => {}
        }
        if tokio::time::Instant::now() >= deadline {
            anyhow::bail!(
                "Query {query_id} did not reach {expected} results within timeout (last count: {last_count:?})"
            );
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
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
        false,
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
    assert_eq!(replayed.len(), 3, "Should receive exactly 3 missed events");

    // Verify no duplicates of the first 2 events — drain anything extra
    let extra = receiver.drain_available();
    let all_after_restart: Vec<_> = replayed.into_iter().chain(extra).collect();

    assert_eq!(
        all_after_restart.len(),
        3,
        "Previously checkpointed events must not be replayed"
    );
    assert_eq!(
        all_after_restart
            .iter()
            .map(|result| result.sequence)
            .collect::<Vec<_>>(),
        vec![3, 4, 5]
    );
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
        false,
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

    assert!(
        spurious.is_empty(),
        "Clean restart replayed checkpointed sequences: {:?}",
        spurious
            .iter()
            .map(|result| result.sequence)
            .collect::<Vec<_>>()
    );

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
        false,
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
    assert!(
        after_restart.is_empty(),
        "AutoSkipGap replayed unavailable sequences: {:?}",
        after_restart
            .iter()
            .map(|result| result.sequence)
            .collect::<Vec<_>>()
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
        false,
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
        false,
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
        false,
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
        insert_person(&handle, &format!("p-flood-{i}"), &format!("Flood-{i}"), i).await?;
    }

    // Verify that live delivery still works after the gap.
    // With AutoSkipGap, the forwarder skips the gap and resumes.
    // wait_for_count has its own timeout — no bare sleep needed.
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
        false,
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

    let mut event_rx = core.subscribe_all_component_events();

    core.start().await?;

    // Confirm initial delivery works.
    insert_person(&handle, "p1", "Alice", 30).await?;
    let initial = receiver.wait_for_count(1, Duration::from_secs(5)).await;
    assert_eq!(initial.len(), 1);

    // Flood to cause broadcast lag — Strict policy should stop the forwarder.
    for i in 0..20 {
        insert_person(&handle, &format!("p-flood-{i}"), &format!("Flood-{i}"), i).await?;
    }

    // Wait deterministically for the reaction to transition to Error state
    // (the supervisor fires this after the forwarder breaks on Strict gap).
    let error_event = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match event_rx.recv().await {
                Ok(event)
                    if event.component_id == "rec" && event.status == ComponentStatus::Error =>
                {
                    return event;
                }
                Ok(_) => continue,
                Err(_) => panic!("Event channel closed while waiting for Error status"),
            }
        }
    })
    .await
    .expect("Timed out waiting for reaction to reach Error status");
    assert_eq!(error_event.status, ComponentStatus::Error);

    // After strict gap failure, new events should NOT be delivered.
    insert_person(&handle, "p-after", "After", 99).await?;
    let after = receiver
        .wait_for_count(1, Duration::from_millis(1000))
        .await;
    assert_eq!(
        after.len(),
        0,
        "Strict policy: no events should be delivered after gap"
    );

    core.stop().await?;
    Ok(())
}

/// A trigger reaction added to a running query starts at the current head.
#[tokio::test]
async fn test_fresh_trigger_does_not_replay_retained_history() -> Result<()> {
    let (mock_source, handle) = MockSource::new("test-source")?;
    let query = Query::cypher("q1")
        .query("MATCH (p:Person) RETURN p.name AS name")
        .from_source("test-source")
        .with_outbox_capacity(100)
        .auto_start(true)
        .build();
    let state_store = Arc::new(DurableMemoryStateStoreProvider::new());
    let core = Arc::new(
        DrasiLib::builder()
            .with_id("fresh-trigger-test")
            .with_source(mock_source)
            .with_query(query)
            .with_state_store_provider(state_store)
            .build()
            .await?,
    );
    core.start().await?;

    insert_person(&handle, "p1", "Alice", 30).await?;
    insert_person(&handle, "p2", "Bob", 25).await?;
    wait_for_query_result_count(&core, "q1", 2).await?;

    let (reaction, mut receiver) = recording_reaction(
        "rec",
        vec!["q1".into()],
        ReactionRecoveryPolicy::Strict,
        false,
        false,
    );
    core.add_reaction(reaction).await?;
    for _ in 0..50 {
        if core.get_reaction_status("rec").await? == ComponentStatus::Running {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    insert_person(&handle, "p3", "Charlie", 35).await?;
    let mut received = receiver.wait_for_count(1, Duration::from_secs(5)).await;
    received.extend(receiver.wait_for_count(1, Duration::from_secs(1)).await);
    let sequences: Vec<_> = received.iter().map(|result| result.sequence).collect();
    assert_eq!(
        sequences,
        vec![3],
        "Fresh trigger should receive only the live result, got {sequences:?}"
    );

    core.stop().await?;
    Ok(())
}

/// A durable reaction cannot recover if its query output is volatile
/// (the query does not retain results, so history cannot be replayed).
#[tokio::test]
async fn test_durable_reaction_rejects_volatile_query() -> Result<()> {
    let (mock_source, _handle) = MockSource::new("test-source")?;
    let query = Query::cypher("q1")
        .query("MATCH (p:Person) RETURN p.name AS name")
        .from_source("test-source")
        .auto_start(true)
        .build();
    let state_store = Arc::new(DurableMemoryStateStoreProvider::new());
    let core = Arc::new(
        DrasiLib::builder()
            .with_id("durable-volatile-test")
            .with_source(mock_source)
            .with_query(query)
            .with_state_store_provider(state_store)
            .build()
            .await?,
    );
    core.start().await?;

    let (reaction, _receiver) = recording_reaction_with_auto_start(
        "rec",
        vec!["q1".into()],
        ReactionRecoveryPolicy::Strict,
        true,
        false,
        false,
    );
    core.add_reaction(reaction).await?;

    let result = core.start_reaction("rec").await;
    assert!(
        result.is_err(),
        "Durable reaction unexpectedly started with a volatile query"
    );
    let msg = format!("{}", result.unwrap_err());
    assert!(
        msg.contains("rec"),
        "Error should name the reaction id: {msg}"
    );
    assert!(
        msg.contains("volatile") || msg.contains("non-durable"),
        "Error should say the query is volatile / non-durable: {msg}"
    );
    assert_ne!(
        core.get_reaction_status("rec").await?,
        ComponentStatus::Running
    );
    let lifecycle = core.get_lifecycle_metrics().await?;
    assert!(
        lifecycle.startup_rejection_durable_on_volatile_query >= 1,
        "Expected durable-on-volatile-query rejection metric, got {}",
        lifecycle.startup_rejection_durable_on_volatile_query
    );

    core.stop().await?;
    Ok(())
}

fn volatile_person_query(id: &str) -> drasi_lib::config::QueryConfig {
    Query::cypher(id)
        .query("MATCH (p:Person) RETURN p.name AS name")
        .from_source("test-source")
        .auto_start(true)
        .build()
}

fn persistent_person_query(id: &str) -> drasi_lib::config::QueryConfig {
    Query::cypher(id)
        .query("MATCH (p:Person) RETURN p.name AS name")
        .from_source("test-source")
        .auto_start(true)
        .with_storage_backend(StorageBackendRef::Named("rocks".to_string()))
        .build()
}

async fn wait_for_reaction_status(
    core: &DrasiLib,
    id: &str,
    expected: ComponentStatus,
) -> Result<()> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if core.get_reaction_status(id).await? == expected {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            anyhow::bail!("Reaction {id} did not reach {expected:?} within timeout");
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Durable reaction + persistent query + durable store — start succeeds.
#[tokio::test]
async fn test_durable_reaction_starts_with_persistent_query() -> Result<()> {
    let tmp = TempDir::new()?;
    let (mock_source, _handle) = MockSource::new("test-source")?;
    let rocks: Arc<dyn IndexBackendPlugin> =
        Arc::new(RocksDbIndexProvider::new(tmp.path(), false, false));
    let state_store = Arc::new(DurableMemoryStateStoreProvider::new());
    let core = Arc::new(
        DrasiLib::builder()
            .with_id("durable-persistent-ok")
            .with_source(mock_source)
            .with_query(persistent_person_query("q1"))
            .with_index_provider("rocks", rocks)
            .with_state_store_provider(state_store)
            .build()
            .await?,
    );
    core.start().await?;

    let (reaction, _receiver) = recording_reaction_with_auto_start(
        "rec",
        vec!["q1".into()],
        ReactionRecoveryPolicy::Strict,
        true,
        false,
        false,
    );
    core.add_reaction(reaction).await?;
    core.start_reaction("rec").await?;
    wait_for_reaction_status(&core, "rec", ComponentStatus::Running).await?;

    core.stop().await?;
    Ok(())
}

/// Durable reaction + persistent query + volatile/missing store — still rejected.
#[tokio::test]
async fn test_durable_reaction_rejects_volatile_store_with_persistent_query() -> Result<()> {
    let tmp = TempDir::new()?;
    let (mock_source, _handle) = MockSource::new("test-source")?;
    let rocks: Arc<dyn IndexBackendPlugin> =
        Arc::new(RocksDbIndexProvider::new(tmp.path(), false, false));
    let state_store = Arc::new(MemoryStateStoreProvider::new());
    let core = Arc::new(
        DrasiLib::builder()
            .with_id("durable-volatile-store")
            .with_source(mock_source)
            .with_query(persistent_person_query("q1"))
            .with_index_provider("rocks", rocks)
            .with_state_store_provider(state_store)
            .build()
            .await?,
    );
    core.start().await?;

    let (reaction, _receiver) = recording_reaction_with_auto_start(
        "rec",
        vec!["q1".into()],
        ReactionRecoveryPolicy::Strict,
        true,
        false,
        false,
    );
    core.add_reaction(reaction).await?;

    let result = core.start_reaction("rec").await;
    assert!(
        result.is_err(),
        "Durable reaction unexpectedly started with a volatile store"
    );
    let msg = format!("{}", result.unwrap_err());
    assert!(
        msg.contains("rec"),
        "Error should name the reaction id: {msg}"
    );
    assert!(
        msg.contains("durable") && msg.contains("volatile"),
        "Error should mention durable reaction vs volatile store: {msg}"
    );
    assert_ne!(
        core.get_reaction_status("rec").await?,
        ComponentStatus::Running
    );

    core.stop().await?;
    Ok(())
}

/// Volatile reaction + volatile query remains allowed (at-most-once).
#[tokio::test]
async fn test_volatile_reaction_allows_volatile_query() -> Result<()> {
    let (mock_source, _handle) = MockSource::new("test-source")?;
    let core = Arc::new(
        DrasiLib::builder()
            .with_id("volatile-volatile-ok")
            .with_source(mock_source)
            .with_query(volatile_person_query("q1"))
            .build()
            .await?,
    );
    core.start().await?;

    let (reaction, _receiver) = recording_reaction_with_auto_start(
        "rec",
        vec!["q1".into()],
        ReactionRecoveryPolicy::Strict,
        false,
        false,
        false,
    );
    core.add_reaction(reaction).await?;
    core.start_reaction("rec").await?;
    wait_for_reaction_status(&core, "rec", ComponentStatus::Running).await?;

    core.stop().await?;
    Ok(())
}

/// Durable reaction subscribed to any volatile query is rejected as a whole.
#[tokio::test]
async fn test_durable_reaction_rejects_if_any_subscribed_query_is_volatile() -> Result<()> {
    let tmp = TempDir::new()?;
    let (mock_source, _handle) = MockSource::new("test-source")?;
    let rocks: Arc<dyn IndexBackendPlugin> =
        Arc::new(RocksDbIndexProvider::new(tmp.path(), false, false));
    let state_store = Arc::new(DurableMemoryStateStoreProvider::new());
    let core = Arc::new(
        DrasiLib::builder()
            .with_id("durable-mixed-queries")
            .with_source(mock_source)
            .with_query(persistent_person_query("q-persistent"))
            .with_query(volatile_person_query("q-volatile"))
            .with_index_provider("rocks", rocks)
            .with_state_store_provider(state_store)
            .build()
            .await?,
    );
    core.start().await?;

    let (reaction, _receiver) = recording_reaction_with_auto_start(
        "rec",
        vec!["q-persistent".into(), "q-volatile".into()],
        ReactionRecoveryPolicy::Strict,
        true,
        false,
        false,
    );
    core.add_reaction(reaction).await?;

    let result = core.start_reaction("rec").await;
    assert!(
        result.is_err(),
        "Durable reaction unexpectedly started with a mixed persistent/volatile subscription"
    );
    let msg = format!("{}", result.unwrap_err());
    assert!(
        msg.contains("rec"),
        "Error should name the reaction id: {msg}"
    );
    assert!(
        msg.contains("q-volatile"),
        "Error should name the volatile query: {msg}"
    );
    assert!(
        msg.contains("volatile") || msg.contains("non-durable"),
        "Error should say the query is volatile / non-durable: {msg}"
    );
    assert_ne!(
        core.get_reaction_status("rec").await?,
        ComponentStatus::Running
    );

    core.stop().await?;
    Ok(())
}

/// `add_reaction` with auto-start also rejects durable-on-volatile-query.
#[tokio::test]
async fn test_add_reaction_auto_start_rejects_durable_on_volatile_query() -> Result<()> {
    let (mock_source, _handle) = MockSource::new("test-source")?;
    let state_store = Arc::new(DurableMemoryStateStoreProvider::new());
    let core = Arc::new(
        DrasiLib::builder()
            .with_id("durable-volatile-autostart")
            .with_source(mock_source)
            .with_query(volatile_person_query("q1"))
            .with_state_store_provider(state_store)
            .build()
            .await?,
    );
    core.start().await?;

    let (reaction, _receiver) = recording_reaction_with_auto_start(
        "rec",
        vec!["q1".into()],
        ReactionRecoveryPolicy::Strict,
        true,
        false,
        true,
    );
    let result = core.add_reaction(reaction).await;
    assert!(
        result.is_err(),
        "add_reaction+auto-start unexpectedly succeeded for durable+volatile"
    );
    let msg = format!("{}", result.unwrap_err());
    assert!(
        msg.contains("rec"),
        "Error should name the reaction id: {msg}"
    );
    assert!(
        msg.contains("volatile") || msg.contains("non-durable"),
        "Error should say the query is volatile / non-durable: {msg}"
    );
    assert_ne!(
        core.get_reaction_status("rec").await?,
        ComponentStatus::Running
    );

    core.stop().await?;
    Ok(())
}

/// Backend change across restart: RocksDB query later recreated as memory-backed.
#[tokio::test]
async fn test_durable_reaction_rejects_after_query_backend_becomes_volatile() -> Result<()> {
    let tmp = TempDir::new()?;
    let (mock_source, _handle) = MockSource::new("test-source")?;
    let rocks: Arc<dyn IndexBackendPlugin> =
        Arc::new(RocksDbIndexProvider::new(tmp.path(), false, false));
    let state_store = Arc::new(DurableMemoryStateStoreProvider::new());
    let core = Arc::new(
        DrasiLib::builder()
            .with_id("durable-backend-change")
            .with_source(mock_source)
            .with_query(persistent_person_query("q1"))
            .with_index_provider("rocks", rocks)
            .with_state_store_provider(state_store)
            .build()
            .await?,
    );
    core.start().await?;

    let (reaction, _receiver) = recording_reaction_with_auto_start(
        "rec",
        vec!["q1".into()],
        ReactionRecoveryPolicy::Strict,
        true,
        false,
        false,
    );
    core.add_reaction(reaction).await?;
    core.start_reaction("rec").await?;
    wait_for_reaction_status(&core, "rec", ComponentStatus::Running).await?;

    stop_reaction_and_wait(&core, "rec").await?;
    core.update_query("q1", volatile_person_query("q1")).await?;

    let result = core.start_reaction("rec").await;
    assert!(
        result.is_err(),
        "Durable reaction unexpectedly started after query was recreated as memory-backed"
    );
    let msg = format!("{}", result.unwrap_err());
    assert!(
        msg.contains("rec"),
        "Error should name the reaction id: {msg}"
    );
    assert!(
        msg.contains("q1"),
        "Error should name the volatile query: {msg}"
    );
    assert!(
        msg.contains("volatile") || msg.contains("non-durable"),
        "Error should say the query is volatile / non-durable: {msg}"
    );
    assert_ne!(
        core.get_reaction_status("rec").await?,
        ComponentStatus::Running
    );

    core.stop().await?;
    Ok(())
}

/// Rule 1 (volatile store) fires before Rule 2 (volatile query) when both apply.
#[tokio::test]
async fn test_durable_reaction_rejects_volatile_store_before_volatile_query() -> Result<()> {
    let (mock_source, _handle) = MockSource::new("test-source")?;
    let state_store = Arc::new(MemoryStateStoreProvider::new());
    let core = Arc::new(
        DrasiLib::builder()
            .with_id("durable-both-volatile")
            .with_source(mock_source)
            .with_query(volatile_person_query("q1"))
            .with_state_store_provider(state_store)
            .build()
            .await?,
    );
    core.start().await?;

    let (reaction, _receiver) = recording_reaction_with_auto_start(
        "rec",
        vec!["q1".into()],
        ReactionRecoveryPolicy::Strict,
        true,
        false,
        false,
    );
    core.add_reaction(reaction).await?;

    let result = core.start_reaction("rec").await;
    assert!(
        result.is_err(),
        "Durable reaction unexpectedly started with volatile store and volatile query"
    );
    let msg = format!("{}", result.unwrap_err());
    assert!(
        msg.contains("rec"),
        "Error should name the reaction id: {msg}"
    );
    assert!(
        msg.contains("state store") && msg.contains("volatile"),
        "Rule 1 (volatile store) must fire before Rule 2 (volatile query): {msg}"
    );
    assert!(
        !msg.contains("subscribed query"),
        "Rule 2 query check must not run when Rule 1 already rejected: {msg}"
    );
    assert_ne!(
        core.get_reaction_status("rec").await?,
        ComponentStatus::Running
    );
    let lifecycle = core.get_lifecycle_metrics().await?;
    assert!(
        lifecycle.startup_rejection_durable_on_volatile >= 1,
        "Expected durable-on-volatile-store rejection, got {}",
        lifecycle.startup_rejection_durable_on_volatile
    );
    assert_eq!(
        lifecycle.startup_rejection_durable_on_volatile_query, 0,
        "Query rejection must not be recorded when store check already failed"
    );

    core.stop().await?;
    Ok(())
}

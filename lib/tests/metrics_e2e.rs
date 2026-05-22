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

//! Integration tests for observability metrics.
//!
//! These tests verify that metrics structs are correctly incremented during
//! normal operation using DrasiLib's public API.

mod mock_source;

use anyhow::Result;
use drasi_lib::channels::{ComponentStatus, QueryResult};
use drasi_lib::context::ReactionRuntimeContext;
use drasi_lib::reactions::common::base::{ReactionBase, ReactionBaseParams};
use drasi_lib::recovery::ReactionRecoveryPolicy;
use drasi_lib::{DrasiLib, MemoryStateStoreProvider, Query, Reaction};
use mock_source::{MockSource, MockSourceHandle, PropertyMapBuilder};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::timeout;

// ============================================================================
// DurableMemoryStateStoreProvider — test wrapper
// ============================================================================

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

struct RecordingReaction {
    base: ReactionBase,
    tx: mpsc::UnboundedSender<QueryResult>,
    recovery_policy: ReactionRecoveryPolicy,
    durable: bool,
    snapshot_on_fresh: bool,
}

impl std::fmt::Debug for RecordingReaction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecordingReaction")
            .field("id", &self.base.id)
            .finish()
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

struct RecordingReceiver {
    rx: mpsc::UnboundedReceiver<QueryResult>,
}

impl RecordingReceiver {
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

// ============================================================================
// Tests
// ============================================================================

/// Test: Query output metrics are updated after processing events.
#[tokio::test]
async fn query_output_metrics_updated_after_events() -> Result<()> {
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
            .with_id("metrics-test")
            .with_source(mock_source)
            .with_query(query)
            .with_reaction(reaction)
            .with_state_store_provider(state_store)
            .build()
            .await?,
    );

    core.start().await?;

    // Initially, metrics should show zero advances
    let initial = core.get_query_output_metrics("q1").await?;
    assert_eq!(initial.result_seq_advances, 0);

    // Insert some data
    insert_person(&handle, "p1", "Alice", 30).await?;
    insert_person(&handle, "p2", "Bob", 25).await?;
    insert_person(&handle, "p3", "Charlie", 35).await?;

    // Wait for delivery
    let results = receiver.wait_for_count(3, Duration::from_secs(5)).await;
    assert_eq!(results.len(), 3, "Should receive 3 events");

    // Metrics should now be updated
    let metrics = core.get_query_output_metrics("q1").await?;
    assert!(
        metrics.result_seq_advances >= 3,
        "Expected at least 3 sequence advances, got {}",
        metrics.result_seq_advances
    );
    assert!(
        metrics.live_results_count == 3,
        "Expected 3 live results, got {}",
        metrics.live_results_count
    );
    assert!(
        metrics.outbox_latest_seq >= 3,
        "Expected outbox latest seq >= 3, got {}",
        metrics.outbox_latest_seq
    );
    assert!(
        metrics.outer_transaction_duration_ns_last > 0,
        "Expected non-zero last transaction duration"
    );

    Ok(())
}

/// Test: Reaction metrics show checkpoint progress.
#[tokio::test]
async fn reaction_metrics_show_checkpoint_progress() -> Result<()> {
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
            .with_id("reaction-metrics-test")
            .with_source(mock_source)
            .with_query(query)
            .with_reaction(reaction)
            .with_state_store_provider(state_store)
            .build()
            .await?,
    );

    core.start().await?;

    // Insert events and wait for delivery
    insert_person(&handle, "p1", "Alice", 30).await?;
    insert_person(&handle, "p2", "Bob", 25).await?;
    let _ = receiver.wait_for_count(2, Duration::from_secs(5)).await;

    // Give forwarder time to update checkpoint metrics
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Reaction metrics should show checkpoint progress
    let metrics = core.get_reaction_metrics("rec").await?;
    assert!(
        !metrics.is_empty(),
        "Expected at least one entry in reaction metrics"
    );

    let q1_metrics = metrics.get("q1").expect("Expected metrics for query q1");
    assert!(
        q1_metrics.checkpoint_sequence >= 2,
        "Expected checkpoint_sequence >= 2, got {}",
        q1_metrics.checkpoint_sequence
    );

    Ok(())
}

/// Test: Lifecycle metrics count startup rejections.
#[tokio::test]
async fn lifecycle_metrics_count_startup_rejections() -> Result<()> {
    let (mock_source, _handle) = MockSource::new("test-source")?;

    let query = Query::cypher("q1")
        .query("MATCH (p:Person) RETURN p.name AS name, p.age AS age")
        .from_source("test-source")
        .with_outbox_capacity(100)
        .auto_start(true)
        .build();

    // Use a volatile store (the default MemoryStateStoreProvider)
    let state_store = Arc::new(MemoryStateStoreProvider::new());

    // Create a durable reaction with a volatile store — should be rejected
    let (reaction, _receiver) = recording_reaction(
        "durable-rec",
        vec!["q1".into()],
        ReactionRecoveryPolicy::Strict,
        true, // is_durable = true
        false,
    );

    let core = Arc::new(
        DrasiLib::builder()
            .with_id("lifecycle-metrics-test")
            .with_source(mock_source)
            .with_query(query)
            .with_reaction(reaction)
            .with_state_store_provider(state_store)
            .build()
            .await?,
    );

    // start() will fail because of the invalid config, but the metrics should still
    // have been incremented before the error was returned.
    let start_result = core.start().await;
    assert!(
        start_result.is_err(),
        "Expected start to fail due to durable-on-volatile"
    );

    let lifecycle = core.get_lifecycle_metrics().await?;
    assert!(
        lifecycle.startup_rejection_durable_on_volatile >= 1,
        "Expected at least 1 durable-on-volatile rejection, got {}",
        lifecycle.startup_rejection_durable_on_volatile
    );

    Ok(())
}

/// Test: Query output metrics returns ComponentNotFound for non-existent query.
#[tokio::test]
async fn query_output_metrics_error_for_nonexistent() -> Result<()> {
    let (mock_source, _handle) = MockSource::new("test-source")?;

    let query = Query::cypher("q1")
        .query("MATCH (p:Person) RETURN p.name AS name")
        .from_source("test-source")
        .auto_start(true)
        .build();

    let core = Arc::new(
        DrasiLib::builder()
            .with_id("no-metrics-test")
            .with_source(mock_source)
            .with_query(query)
            .build()
            .await?,
    );

    core.start().await?;

    let result = core.get_query_output_metrics("nonexistent").await;
    assert!(result.is_err(), "Expected error for non-existent query");

    Ok(())
}

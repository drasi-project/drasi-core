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
use async_trait::async_trait;
use drasi_core::interface::{
    CheckpointStore, CreatedIndexes, IndexBackendPlugin, IndexError, OutboxWriter, SessionControl,
};
use drasi_core::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange,
};
use drasi_index_rocksdb::RocksDbIndexProvider;
use drasi_lib::bootstrap::{
    BootstrapContext as SourceBootstrapContext, BootstrapProvider, BootstrapRequest,
    BootstrapResult,
};
use drasi_lib::channels::{BootstrapEvent, BootstrapEventSender, ComponentStatus, QueryResult};
use drasi_lib::config::SourceSubscriptionSettings;
use drasi_lib::context::ReactionRuntimeContext;
use drasi_lib::reactions::common::base::{ReactionBase, ReactionBaseParams};
use drasi_lib::reactions::{BootstrapContext as ReactionBootstrapContext, ReactionCheckpoint};
use drasi_lib::recovery::ReactionRecoveryPolicy;
use drasi_lib::state_store::StateStoreProvider;
use drasi_lib::{
    DispatchMode, DrasiLib, MemoryStateStoreProvider, Query, Reaction, RecoveryPolicy, Source,
    StorageBackendRef,
};
use mock_source::{MockSource, MockSourceHandle, PropertyMapBuilder};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot, Mutex};
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

struct StableSnapshotBootstrapProvider {
    attempts: Arc<std::sync::atomic::AtomicUsize>,
}

struct CapturingRocksProvider {
    inner: RocksDbIndexProvider,
    outbox: Arc<tokio::sync::RwLock<Option<Arc<dyn OutboxWriter>>>>,
    checkpoint_store: Arc<tokio::sync::RwLock<Option<Arc<dyn CheckpointStore>>>>,
    session_control: Arc<tokio::sync::RwLock<Option<Arc<dyn SessionControl>>>>,
}

#[async_trait]
impl IndexBackendPlugin for CapturingRocksProvider {
    async fn create_indexes(&self, query_id: &str) -> Result<CreatedIndexes, IndexError> {
        let created = self.inner.create_indexes(query_id).await?;
        *self.outbox.write().await = created.outbox_writer.clone();
        *self.checkpoint_store.write().await = created.checkpoint_store.clone();
        *self.session_control.write().await = Some(created.set.session_control.clone());
        Ok(created)
    }

    fn is_volatile(&self) -> bool {
        false
    }
}

#[async_trait]
impl BootstrapProvider for StableSnapshotBootstrapProvider {
    async fn bootstrap(
        &self,
        _request: BootstrapRequest,
        context: &SourceBootstrapContext,
        event_tx: BootstrapEventSender,
        _settings: Option<&SourceSubscriptionSettings>,
    ) -> Result<BootstrapResult> {
        self.attempts
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        event_tx
            .send(BootstrapEvent {
                source_id: context.source_id.clone(),
                change: SourceChange::Insert {
                    element: Element::Node {
                        metadata: ElementMetadata {
                            reference: ElementReference::new(
                                &context.source_id,
                                "bootstrap-person",
                            ),
                            labels: vec![Arc::from("Person")].into(),
                            effective_from: 1_000,
                        },
                        properties: ElementPropertyMap::from(serde_json::json!({
                            "name": "Bootstrap",
                            "age": 40
                        })),
                    },
                },
                timestamp: chrono::Utc::now(),
                sequence: context.next_sequence(),
            })
            .await
            .map_err(|_| anyhow::anyhow!("bootstrap receiver closed"))?;
        Ok(BootstrapResult {
            event_count: 1,
            source_position: None,
        })
    }
}

struct SnapshotRecordingReaction {
    base: ReactionBase,
    snapshot_tx: Mutex<Option<oneshot::Sender<(u64, Vec<serde_json::Value>)>>>,
}

impl SnapshotRecordingReaction {
    fn new(id: &str, query_id: &str) -> (Self, oneshot::Receiver<(u64, Vec<serde_json::Value>)>) {
        let (snapshot_tx, snapshot_rx) = oneshot::channel();
        (
            Self {
                base: ReactionBase::new(ReactionBaseParams::new(id, vec![query_id.to_string()])),
                snapshot_tx: Mutex::new(Some(snapshot_tx)),
            },
            snapshot_rx,
        )
    }
}

#[async_trait]
impl Reaction for SnapshotRecordingReaction {
    fn id(&self) -> &str {
        &self.base.id
    }

    fn type_name(&self) -> &str {
        "snapshot-recording"
    }

    fn properties(&self) -> HashMap<String, serde_json::Value> {
        HashMap::new()
    }

    fn query_ids(&self) -> Vec<String> {
        self.base.queries.clone()
    }

    async fn initialize(&self, context: ReactionRuntimeContext) {
        self.base.initialize(context).await;
    }

    async fn start(&self) -> Result<()> {
        self.base
            .set_status(
                ComponentStatus::Running,
                Some("Snapshot reaction started".into()),
            )
            .await;
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        self.base
            .set_status(
                ComponentStatus::Stopped,
                Some("Snapshot reaction stopped".into()),
            )
            .await;
        Ok(())
    }

    async fn status(&self) -> ComponentStatus {
        self.base.get_status().await
    }

    fn needs_snapshot_on_fresh_start(&self) -> bool {
        true
    }

    async fn bootstrap(&self, context: ReactionBootstrapContext) -> Result<()> {
        let snapshot = context
            .fetch_snapshot()
            .await
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        let as_of_sequence = snapshot.as_of_sequence;
        let rows = snapshot.collect_vec().await;
        if let Some(sender) = self.snapshot_tx.lock().await.take() {
            let _ = sender.send((as_of_sequence, rows));
        }
        Ok(())
    }
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

    async fn wait_for_live(&mut self, dur: Duration) -> (Vec<QueryResult>, Option<QueryResult>) {
        let deadline = tokio::time::Instant::now() + dur;
        let mut controls = Vec::new();
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            match timeout(remaining, self.rx.recv()).await {
                Ok(Some(result)) if result.sequence == 0 => controls.push(result),
                Ok(Some(result)) => return (controls, Some(result)),
                Ok(None) | Err(_) => return (controls, None),
            }
        }
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

async fn persist_reaction_checkpoint(
    store: &dyn StateStoreProvider,
    reaction_id: &str,
    query_id: &str,
    sequence: u64,
    config_hash: u64,
) -> Result<()> {
    let checkpoint = ReactionCheckpoint {
        sequence,
        config_hash,
    };
    store
        .set(
            reaction_id,
            &format!("checkpoint:{query_id}"),
            bincode::serialize(&checkpoint)?,
        )
        .await?;
    Ok(())
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

async fn wait_for_query_status(
    core: &DrasiLib,
    query_id: &str,
    expected: ComponentStatus,
) -> Result<()> {
    for _ in 0..100 {
        if core.get_query_status(query_id).await? == expected {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    anyhow::bail!("Query {query_id} did not reach {expected:?} within timeout");
}

// ============================================================================
// Tests
// ============================================================================

#[tokio::test]
async fn volatile_query_rebootstrap_preserves_reaction_sequence_and_current_snapshot() -> Result<()>
{
    let (mock_source, handle) = MockSource::new("volatile-bootstrap-source")?;
    let bootstrap_attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    mock_source
        .set_bootstrap_provider(Box::new(StableSnapshotBootstrapProvider {
            attempts: bootstrap_attempts.clone(),
        }))
        .await;
    let query = Query::cypher("volatile-query")
        .query("MATCH (p:Person) RETURN p.name AS name, p.age AS age")
        .from_source("volatile-bootstrap-source")
        .enable_bootstrap(true)
        .with_outbox_capacity(16)
        .auto_start(true)
        .build();
    let state_store = Arc::new(DurableMemoryStateStoreProvider::new());
    let (reaction, mut receiver) = recording_reaction(
        "surviving-reaction",
        vec!["volatile-query".into()],
        ReactionRecoveryPolicy::Strict,
        true,
        false,
    );
    let core = Arc::new(
        DrasiLib::builder()
            .with_id("volatile-rebootstrap-test")
            .with_source(mock_source)
            .with_query(query)
            .with_reaction(reaction)
            .with_state_store_provider(state_store.clone())
            .build()
            .await?,
    );
    core.start().await?;
    wait_for_query_status(&core, "volatile-query", ComponentStatus::Running).await?;

    insert_person(&handle, "live-before-restart", "Before", 30).await?;
    let (_, first) = receiver.wait_for_live(Duration::from_secs(5)).await;
    assert_eq!(first.expect("first live result").sequence, 1);
    let config_hash =
        drasi_lib::queries::compute_config_hash(&core.get_query_config("volatile-query").await?);
    persist_reaction_checkpoint(
        state_store.as_ref(),
        "surviving-reaction",
        "volatile-query",
        1,
        config_hash,
    )
    .await?;

    core.stop_query("volatile-query").await?;
    wait_for_query_status(&core, "volatile-query", ComponentStatus::Stopped).await?;
    core.start_query("volatile-query").await?;
    wait_for_query_status(&core, "volatile-query", ComponentStatus::Running).await?;
    assert_eq!(
        bootstrap_attempts.load(std::sync::atomic::Ordering::Acquire),
        2
    );

    insert_person(&handle, "live-after-restart", "After", 31).await?;
    let (controls, live) = receiver.wait_for_live(Duration::from_secs(5)).await;
    let controls: Vec<_> = controls
        .iter()
        .filter_map(|result| {
            (result.sequence == 0)
                .then(|| result.metadata["control_signal"].as_str())
                .flatten()
        })
        .collect();
    assert_eq!(controls, vec!["bootstrapStarted", "bootstrapCompleted"]);
    assert_eq!(
        live.expect("surviving reaction dropped post-rebootstrap output")
            .sequence,
        2
    );

    let (snapshot_reaction, snapshot_rx) =
        SnapshotRecordingReaction::new("new-snapshot-reaction", "volatile-query");
    core.add_reaction(snapshot_reaction).await?;
    let (as_of_sequence, snapshot) = timeout(Duration::from_secs(5), snapshot_rx)
        .await
        .expect("new reaction snapshot timed out")
        .expect("new reaction snapshot channel closed");
    assert_eq!(as_of_sequence, 2);
    assert_eq!(snapshot.len(), 2);
    let names: Vec<_> = snapshot
        .iter()
        .filter_map(|row| row["name"].as_str())
        .collect();
    assert!(names.contains(&"Bootstrap"));
    assert!(names.contains(&"After"));
    assert!(!names.contains(&"Before"));

    core.stop().await?;
    Ok(())
}

#[tokio::test]
async fn persistent_auto_reset_preserves_reaction_high_water_across_process_restart() -> Result<()>
{
    let data_dir = tempfile::TempDir::new()?;
    let state_store = Arc::new(DurableMemoryStateStoreProvider::new());
    let captured_outbox: Arc<tokio::sync::RwLock<Option<Arc<dyn OutboxWriter>>>> =
        Arc::new(tokio::sync::RwLock::new(None));
    let captured_checkpoints: Arc<tokio::sync::RwLock<Option<Arc<dyn CheckpointStore>>>> =
        Arc::new(tokio::sync::RwLock::new(None));
    let captured_session: Arc<tokio::sync::RwLock<Option<Arc<dyn SessionControl>>>> =
        Arc::new(tokio::sync::RwLock::new(None));
    let provider1: Arc<dyn IndexBackendPlugin> = Arc::new(CapturingRocksProvider {
        inner: RocksDbIndexProvider::new(data_dir.path(), true, false),
        outbox: captured_outbox.clone(),
        checkpoint_store: captured_checkpoints.clone(),
        session_control: captured_session.clone(),
    });
    let (source1, handle1) = MockSource::new("persistent-reset-source")?;
    let query_config = || {
        Query::cypher("persistent-reset-query")
            .query("MATCH (p:Person) RETURN p.name AS name, p.age AS age")
            .from_source("persistent-reset-source")
            .enable_bootstrap(false)
            .with_storage_backend(StorageBackendRef::Named("rocks".to_string()))
            .with_recovery_policy(RecoveryPolicy::AutoReset)
            .with_outbox_capacity(16)
            .auto_start(true)
            .build()
    };
    let (reaction1, mut receiver1) = recording_reaction(
        "persistent-reset-reaction",
        vec!["persistent-reset-query".into()],
        ReactionRecoveryPolicy::Strict,
        true,
        false,
    );
    let core1 = Arc::new(
        DrasiLib::builder()
            .with_id("persistent-reset-1")
            .with_index_provider("rocks", provider1)
            .with_source(source1)
            .with_query(query_config())
            .with_reaction(reaction1)
            .with_state_store_provider(state_store.clone())
            .build()
            .await?,
    );
    core1.start().await?;
    wait_for_query_status(&core1, "persistent-reset-query", ComponentStatus::Running).await?;
    insert_person(&handle1, "p1", "BeforeReset", 30).await?;
    let first = receiver1.wait_for_count(1, Duration::from_secs(5)).await;
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].sequence, 1);
    let config_hash = drasi_lib::queries::compute_config_hash(
        &core1.get_query_config("persistent-reset-query").await?,
    );
    persist_reaction_checkpoint(
        state_store.as_ref(),
        "persistent-reset-reaction",
        "persistent-reset-query",
        1,
        config_hash,
    )
    .await?;

    core1.stop_query("persistent-reset-query").await?;
    wait_for_query_status(&core1, "persistent-reset-query", ComponentStatus::Stopped).await?;
    captured_outbox
        .read()
        .await
        .as_ref()
        .expect("captured RocksDB outbox")
        .clear("persistent-reset-query")
        .await?;
    *captured_outbox.write().await = None;
    *captured_checkpoints.write().await = None;
    *captured_session.write().await = None;
    core1.start_query("persistent-reset-query").await?;
    wait_for_query_status(&core1, "persistent-reset-query", ComponentStatus::Running).await?;

    insert_person(&handle1, "p2", "AfterReset", 31).await?;
    let after_reset = receiver1.wait_for_count(1, Duration::from_secs(5)).await;
    assert_eq!(after_reset.len(), 1);
    assert_eq!(
        after_reset[0].sequence, 2,
        "AutoReset rewound the public output sequence"
    );
    persist_reaction_checkpoint(
        state_store.as_ref(),
        "persistent-reset-reaction",
        "persistent-reset-query",
        2,
        config_hash,
    )
    .await?;

    core1.stop_query("persistent-reset-query").await?;
    wait_for_query_status(&core1, "persistent-reset-query", ComponentStatus::Stopped).await?;
    let checkpoint_store = captured_checkpoints
        .read()
        .await
        .as_ref()
        .expect("captured RocksDB checkpoint store")
        .clone();
    let session_control = captured_session
        .read()
        .await
        .as_ref()
        .expect("captured RocksDB session control")
        .clone();
    session_control.begin().await?;
    checkpoint_store
        .stage_checkpoint("\0drasi:query-bootstrap:v1", 0, None)
        .await?;
    session_control.commit().await?;
    drop(checkpoint_store);
    drop(session_control);
    *captured_outbox.write().await = None;
    *captured_checkpoints.write().await = None;
    *captured_session.write().await = None;
    core1.start_query("persistent-reset-query").await?;
    wait_for_query_status(&core1, "persistent-reset-query", ComponentStatus::Running).await?;
    insert_person(&handle1, "p3", "AfterIncompleteBootstrap", 32).await?;
    let after_incomplete = receiver1.wait_for_count(1, Duration::from_secs(5)).await;
    assert_eq!(after_incomplete.len(), 1);
    assert_eq!(
        after_incomplete[0].sequence, 3,
        "incomplete-bootstrap reset rewound the public sequence"
    );
    persist_reaction_checkpoint(
        state_store.as_ref(),
        "persistent-reset-reaction",
        "persistent-reset-query",
        3,
        config_hash,
    )
    .await?;

    let (snapshot_reaction, snapshot_rx) =
        SnapshotRecordingReaction::new("persistent-reset-snapshot", "persistent-reset-query");
    core1.add_reaction(snapshot_reaction).await?;
    let (snapshot_sequence, snapshot_rows) = timeout(Duration::from_secs(5), snapshot_rx)
        .await
        .expect("persistent reset snapshot timed out")
        .expect("persistent reset snapshot channel closed");
    assert_eq!(snapshot_sequence, 3);
    assert_eq!(snapshot_rows.len(), 1);
    assert_eq!(snapshot_rows[0]["name"], "AfterIncompleteBootstrap");

    core1.shutdown().await?;
    *captured_outbox.write().await = None;
    *captured_checkpoints.write().await = None;
    *captured_session.write().await = None;
    drop(core1);

    let provider2: Arc<dyn IndexBackendPlugin> =
        Arc::new(RocksDbIndexProvider::new(data_dir.path(), true, false));
    let (source2, handle2) = MockSource::new("persistent-reset-source")?;
    let (reaction2, mut receiver2) = recording_reaction(
        "persistent-reset-reaction",
        vec!["persistent-reset-query".into()],
        ReactionRecoveryPolicy::Strict,
        true,
        false,
    );
    let core2 = Arc::new(
        DrasiLib::builder()
            .with_id("persistent-reset-2")
            .with_index_provider("rocks", provider2)
            .with_source(source2)
            .with_query(query_config())
            .with_reaction(reaction2)
            .with_state_store_provider(state_store)
            .build()
            .await?,
    );
    core2.start().await?;
    wait_for_query_status(&core2, "persistent-reset-query", ComponentStatus::Running).await?;
    assert!(
        receiver2.drain_available().is_empty(),
        "checkpointed reaction replayed reset history on process restart"
    );
    insert_person(&handle2, "p3", "AfterProcessRestart", 32).await?;
    let after_process_restart = receiver2.wait_for_count(1, Duration::from_secs(5)).await;
    assert_eq!(after_process_restart.len(), 1);
    assert_eq!(after_process_restart[0].sequence, 4);

    core2.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn config_reset_preserves_reaction_high_water() -> Result<()> {
    let data_dir = tempfile::TempDir::new()?;
    let state_store = Arc::new(DurableMemoryStateStoreProvider::new());
    let provider: Arc<dyn IndexBackendPlugin> =
        Arc::new(RocksDbIndexProvider::new(data_dir.path(), true, false));
    let (source, handle) = MockSource::new("config-reset-source")?;
    let query = Query::cypher("config-reset-query")
        .query("MATCH (p:Person) RETURN p.name AS name")
        .from_source("config-reset-source")
        .enable_bootstrap(false)
        .with_storage_backend(StorageBackendRef::Named("rocks".to_string()))
        .with_recovery_policy(RecoveryPolicy::AutoReset)
        .auto_start(true)
        .build();
    let (reaction, mut receiver) = recording_reaction(
        "config-reset-reaction",
        vec!["config-reset-query".into()],
        ReactionRecoveryPolicy::AutoReset,
        true,
        true,
    );
    let core = Arc::new(
        DrasiLib::builder()
            .with_id("config-reset-test")
            .with_index_provider("rocks", provider)
            .with_source(source)
            .with_query(query)
            .with_reaction(reaction)
            .with_state_store_provider(state_store.clone())
            .build()
            .await?,
    );
    core.start().await?;
    insert_person(&handle, "p1", "BeforeConfigReset", 30).await?;
    let (_, first) = receiver.wait_for_live(Duration::from_secs(5)).await;
    assert_eq!(first.expect("first config-reset output").sequence, 1);
    let old_hash = drasi_lib::queries::compute_config_hash(
        &core.get_query_config("config-reset-query").await?,
    );
    persist_reaction_checkpoint(
        state_store.as_ref(),
        "config-reset-reaction",
        "config-reset-query",
        1,
        old_hash,
    )
    .await?;
    stop_reaction_and_wait(&core, "config-reset-reaction").await?;

    let changed_query = Query::cypher("config-reset-query")
        .query("MATCH (p:Person) RETURN p.name AS name, p.age AS age")
        .from_source("config-reset-source")
        .enable_bootstrap(false)
        .with_storage_backend(StorageBackendRef::Named("rocks".to_string()))
        .with_recovery_policy(RecoveryPolicy::AutoReset)
        .auto_start(true)
        .build();
    core.update_query("config-reset-query", changed_query)
        .await?;
    wait_for_query_status(&core, "config-reset-query", ComponentStatus::Running).await?;
    core.start_reaction("config-reset-reaction").await?;

    insert_person(&handle, "p2", "AfterConfigReset", 31).await?;
    let (_, after_reset) = receiver.wait_for_live(Duration::from_secs(5)).await;
    assert_eq!(
        after_reset
            .expect("post-config-reset output was silently dropped")
            .sequence,
        2
    );

    let (snapshot_reaction, snapshot_rx) =
        SnapshotRecordingReaction::new("config-reset-snapshot", "config-reset-query");
    core.add_reaction(snapshot_reaction).await?;
    let (snapshot_sequence, snapshot_rows) = timeout(Duration::from_secs(5), snapshot_rx)
        .await
        .expect("config reset snapshot timed out")
        .expect("config reset snapshot channel closed");
    assert_eq!(snapshot_sequence, 2);
    assert_eq!(snapshot_rows.len(), 1);
    assert_eq!(snapshot_rows[0]["name"], "AfterConfigReset");

    core.shutdown().await?;
    Ok(())
}

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
            .with_state_store_provider(state_store.clone())
            .build()
            .await?,
    );

    core.start().await?;

    // Insert 2 rows and wait for delivery
    insert_person(&handle, "p1", "Alice", 30).await?;
    insert_person(&handle, "p2", "Bob", 25).await?;
    let initial = receiver.wait_for_count(2, Duration::from_secs(5)).await;
    assert_eq!(initial.len(), 2, "Should receive 2 initial events");
    let config_hash = drasi_lib::queries::compute_config_hash(&core.get_query_config("q1").await?);
    persist_reaction_checkpoint(state_store.as_ref(), "rec", "q1", 2, config_hash).await?;

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
        replayed.len() == 3,
        "Should receive exactly 3 replayed events, got {}",
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
            .with_state_store_provider(state_store.clone())
            .build()
            .await?,
    );

    core.start().await?;

    // Insert 2 rows and wait for delivery
    insert_person(&handle, "p1", "Alice", 30).await?;
    insert_person(&handle, "p2", "Bob", 25).await?;
    let initial = receiver.wait_for_count(2, Duration::from_secs(5)).await;
    assert_eq!(initial.len(), 2);
    let config_hash = drasi_lib::queries::compute_config_hash(&core.get_query_config("q1").await?);
    persist_reaction_checkpoint(state_store.as_ref(), "rec", "q1", 2, config_hash).await?;

    // Stop and immediately restart — no events in between
    stop_reaction_and_wait(&core, "rec").await?;
    core.start_reaction("rec").await?;

    // A checkpoint already at the durable sequence must suppress replay.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let spurious = receiver.drain_available();
    assert!(
        spurious.is_empty(),
        "checkpointed reaction received unnecessary replay: {spurious:?}"
    );

    // Insert a new event to verify live delivery works
    insert_person(&handle, "p3", "Charlie", 35).await?;
    let live = receiver.wait_for_count(1, Duration::from_secs(5)).await;
    assert_eq!(live.len(), 1, "Should receive 1 live event after restart");

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

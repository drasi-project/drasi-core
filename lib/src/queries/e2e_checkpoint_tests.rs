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

//! End-to-end tests for checkpoint and recovery features.
//!
//! These tests exercise the **public `DrasiLib` API** (`DrasiLib::builder()`,
//! `start_query()`, `stop_query()`, `remove_query()`, etc.) with the real
//! RocksDB index backend to validate the full checkpoint/recovery stack.
//!
//! Compared to the unit tests in `checkpoint_tests.rs` (which test internal
//! `QueryManager` wiring), these tests verify that config propagation, index
//! factory routing, and component lifecycle work end-to-end.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use tokio::sync::{mpsc, RwLock};

use crate::channels::events::SourceEventWrapper;
use crate::channels::{ChannelChangeReceiver, ComponentStatus, SubscriptionResponse};
use crate::config::{QueryConfig, SourceSubscriptionSettings};
use crate::context::SourceRuntimeContext;
use crate::indexes::{IndexBackendPlugin, StorageBackendRef, StorageBackendSpec};
use crate::sources::{Source, SourceBase, SourceBaseParams, SourceError};
use crate::{DrasiLib, Query, RecoveryPolicy};

use drasi_index_rocksdb::RocksDbIndexProvider;

// ============================================================================
// E2E test source
// ============================================================================

struct E2eTestSource {
    base: SourceBase,
    replay_capable: bool,
    /// Records the most recent `resume_from` received in subscribe().
    last_resume_from: Arc<RwLock<Option<Bytes>>>,
    /// Records the most recent `last_sequence` received in subscribe().
    last_sequence_seen: Arc<RwLock<Option<u64>>>,
    /// Number of times subscribe was called.
    subscribe_count: Arc<AtomicU32>,
    /// Number of remaining subscribe failures (decremented each call).
    remaining_failures: Arc<AtomicU32>,
    /// Number of times remove_position_handle was called.
    position_handle_removed: Arc<AtomicU32>,
    /// Sender for injecting events. Recreated in subscribe() for each subscription.
    event_tx: Arc<RwLock<Option<mpsc::Sender<Arc<SourceEventWrapper>>>>>,
}

impl E2eTestSource {
    fn new(id: &str, replay_capable: bool) -> anyhow::Result<Self> {
        Ok(Self {
            base: SourceBase::new(SourceBaseParams::new(id))?,
            replay_capable,
            last_resume_from: Arc::new(RwLock::new(None)),
            last_sequence_seen: Arc::new(RwLock::new(None)),
            subscribe_count: Arc::new(AtomicU32::new(0)),
            remaining_failures: Arc::new(AtomicU32::new(0)),
            position_handle_removed: Arc::new(AtomicU32::new(0)),
            event_tx: Arc::new(RwLock::new(None)),
        })
    }

    fn last_resume_from(&self) -> Arc<RwLock<Option<Bytes>>> {
        self.last_resume_from.clone()
    }

    fn last_sequence_seen(&self) -> Arc<RwLock<Option<u64>>> {
        self.last_sequence_seen.clone()
    }

    fn subscribe_count_handle(&self) -> Arc<AtomicU32> {
        self.subscribe_count.clone()
    }

    fn position_handle_removed_handle(&self) -> Arc<AtomicU32> {
        self.position_handle_removed.clone()
    }

    fn remaining_failures_handle(&self) -> Arc<AtomicU32> {
        self.remaining_failures.clone()
    }

    fn event_sender(&self) -> Arc<RwLock<Option<mpsc::Sender<Arc<SourceEventWrapper>>>>> {
        self.event_tx.clone()
    }

    fn set_fail_count(&self, count: u32) {
        self.remaining_failures.store(count, Ordering::Release);
    }
}

#[async_trait]
impl Source for E2eTestSource {
    fn id(&self) -> &str {
        self.base.get_id()
    }

    fn type_name(&self) -> &str {
        "e2e-test"
    }

    fn properties(&self) -> HashMap<String, serde_json::Value> {
        HashMap::new()
    }

    fn supports_replay(&self) -> bool {
        self.replay_capable
    }

    fn auto_start(&self) -> bool {
        false
    }

    async fn initialize(&self, context: SourceRuntimeContext) {
        self.base.initialize(context).await;
    }

    async fn start(&self) -> anyhow::Result<()> {
        self.base
            .set_status(ComponentStatus::Starting, Some("Starting".into()))
            .await;
        self.base
            .set_status(ComponentStatus::Running, Some("Running".into()))
            .await;
        Ok(())
    }

    async fn stop(&self) -> anyhow::Result<()> {
        self.base
            .set_status(ComponentStatus::Stopping, Some("Stopping".into()))
            .await;
        self.base
            .set_status(ComponentStatus::Stopped, Some("Stopped".into()))
            .await;
        Ok(())
    }

    async fn status(&self) -> ComponentStatus {
        self.base.status_handle().get_status().await
    }

    async fn subscribe(
        &self,
        settings: SourceSubscriptionSettings,
    ) -> anyhow::Result<SubscriptionResponse> {
        self.subscribe_count.fetch_add(1, Ordering::AcqRel);

        // Record what the query sent us
        *self.last_resume_from.write().await = settings.resume_from.clone();
        *self.last_sequence_seen.write().await = settings.last_sequence;

        // Check for injected failures
        if self
            .remaining_failures
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                if n > 0 {
                    Some(n - 1)
                } else {
                    None
                }
            })
            .is_ok()
        {
            return Err(SourceError::PositionUnavailable {
                source_id: self.id().to_string(),
                requested: settings.resume_from.unwrap_or_default(),
                earliest_available: None,
            }
            .into());
        }

        // Create a new channel pair for this subscription
        let (tx, rx) = mpsc::channel(1000);
        *self.event_tx.write().await = Some(tx);

        // Position handle
        let position_handle = if settings.request_position_handle {
            Some(Arc::new(AtomicU64::new(u64::MAX)))
        } else {
            None
        };

        Ok(SubscriptionResponse {
            query_id: settings.query_id,
            source_id: self.id().to_string(),
            receiver: Box::new(ChannelChangeReceiver::new(rx)),
            bootstrap_receiver: None,
            position_handle,
            bootstrap_result_receiver: None,
        })
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn remove_position_handle(&self, _query_id: &str) {
        self.position_handle_removed.fetch_add(1, Ordering::AcqRel);
    }
}

// ============================================================================
// Helper: build a DrasiLib with persistent plugin
// ============================================================================

async fn build_e2e_lib(
    id: &str,
    tmp_dir: &tempfile::TempDir,
    default_recovery_policy: Option<RecoveryPolicy>,
) -> anyhow::Result<Arc<DrasiLib>> {
    let provider = RocksDbIndexProvider::new(tmp_dir.path(), false, false);
    let mut builder = DrasiLib::builder()
        .with_id(id)
        .with_index_provider(Arc::new(provider) as Arc<dyn IndexBackendPlugin>);

    if let Some(policy) = default_recovery_policy {
        builder = builder.with_default_recovery_policy(policy);
    }

    let lib = builder.build().await?;
    Ok(Arc::new(lib))
}

fn make_persistent_query(
    id: &str,
    source_id: &str,
    recovery_policy: Option<RecoveryPolicy>,
) -> QueryConfig {
    let mut q = Query::cypher(id)
        .query("MATCH (n:Node) RETURN n.id AS id, n.value AS value")
        .from_source(source_id)
        .auto_start(false)
        .enable_bootstrap(false)
        .with_storage_backend(StorageBackendRef::Inline(StorageBackendSpec::RocksDb {
            path: "/tmp/e2e-test-drasi".to_string(),
            enable_archive: false,
            direct_io: false,
        }));

    if let Some(policy) = recovery_policy {
        q = q.with_recovery_policy(policy);
    }

    q.build()
}

fn make_volatile_query(id: &str, source_id: &str) -> QueryConfig {
    Query::cypher(id)
        .query("MATCH (n:Node) RETURN n.id AS id, n.value AS value")
        .from_source(source_id)
        .auto_start(false)
        .enable_bootstrap(false)
        .build()
}

/// Send a source change event and wait briefly for processing.
async fn send_event(
    tx: &Arc<RwLock<Option<mpsc::Sender<Arc<SourceEventWrapper>>>>>,
    source_id: &str,
    sequence: u64,
    position: &[u8],
) {
    use drasi_core::models::{
        Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange,
    };

    let node_id = format!("node-{sequence}");
    let mut props = ElementPropertyMap::new();
    props.insert(
        "id",
        drasi_core::models::ElementValue::String(node_id.clone().into()),
    );
    props.insert(
        "value",
        drasi_core::models::ElementValue::Integer(sequence as i64),
    );

    let element = Element::Node {
        metadata: ElementMetadata {
            reference: ElementReference::new(source_id, &node_id),
            labels: Arc::from(vec![Arc::from("Node")]),
            effective_from: 0,
        },
        properties: props,
    };

    let change = SourceChange::Insert { element };

    let mut event = SourceEventWrapper::new(
        source_id.to_string(),
        crate::channels::events::SourceEvent::Change(change),
        chrono::Utc::now(),
    );
    event.sequence = Some(sequence);
    event.source_position = Some(Bytes::from(position.to_vec()));

    if let Some(sender) = tx.read().await.as_ref() {
        sender.send(Arc::new(event)).await.unwrap();
    }
    // Brief pause for processing
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
}

async fn wait_for_status(core: &DrasiLib, component_id: &str, expected: ComponentStatus) {
    let graph = core.component_graph();
    crate::component_graph::wait_for_status(
        &graph,
        component_id,
        &[expected],
        std::time::Duration::from_secs(5),
    )
    .await
    .unwrap_or_else(|e| panic!("wait_for_status({component_id}, {expected:?}) failed: {e}"));
}

// ============================================================================
// E2E Tests
// ============================================================================

/// Full lifecycle: build → start → feed events → stop → restart → verify resume_from.
#[tokio::test]
async fn test_e2e_checkpoint_round_trip() {
    let tmp_dir = tempfile::TempDir::new().unwrap();
    let core = build_e2e_lib("e2e-rt", &tmp_dir, None).await.unwrap();

    // Create source and grab shared handles before moving
    let source = E2eTestSource::new("e2e-src", true).unwrap();
    let resume_from = source.last_resume_from();
    let last_seq = source.last_sequence_seen();
    let event_tx = source.event_sender();
    let sub_count = source.subscribe_count_handle();

    // Add and start source
    core.add_source(source).await.unwrap();
    core.start_source("e2e-src").await.unwrap();
    wait_for_status(&core, "e2e-src", ComponentStatus::Running).await;

    // Add and start persistent query
    let query = make_persistent_query("e2e-q", "e2e-src", None);
    core.add_query(query).await.unwrap();
    core.start_query("e2e-q").await.unwrap();
    wait_for_status(&core, "e2e-q", ComponentStatus::Running).await;

    // First subscribe: no resume_from (first start)
    assert!(
        resume_from.read().await.is_none(),
        "First start should have no resume_from"
    );
    assert_eq!(sub_count.load(Ordering::Acquire), 1);

    // Feed events with positions
    send_event(&event_tx, "e2e-src", 1, b"pos-1").await;
    send_event(&event_tx, "e2e-src", 2, b"pos-2").await;
    send_event(&event_tx, "e2e-src", 3, b"pos-3").await;

    // Wait for checkpoint processing
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // Stop the query
    core.stop_query("e2e-q").await.unwrap();
    wait_for_status(&core, "e2e-q", ComponentStatus::Stopped).await;

    // Restart the query
    core.start_query("e2e-q").await.unwrap();
    wait_for_status(&core, "e2e-q", ComponentStatus::Running).await;

    // Second subscribe: should have resume_from with the last checkpointed position
    assert_eq!(
        sub_count.load(Ordering::Acquire),
        2,
        "Should have subscribed twice"
    );

    let resumed = resume_from.read().await;
    assert!(
        resumed.is_some(),
        "After restart, resume_from should be set from checkpoint"
    );

    // Also verify last_sequence was passed
    let seq = last_seq.read().await;
    assert!(seq.is_some(), "last_sequence should be set from checkpoint");

    core.stop_query("e2e-q").await.unwrap();
}

/// Volatile query (no storage backend) should never send resume_from.
#[tokio::test]
async fn test_e2e_volatile_query_no_checkpoints() {
    let tmp_dir = tempfile::TempDir::new().unwrap();
    let core = build_e2e_lib("e2e-vol", &tmp_dir, None).await.unwrap();

    let source = E2eTestSource::new("vol-src", true).unwrap();
    let resume_from = source.last_resume_from();
    let event_tx = source.event_sender();

    core.add_source(source).await.unwrap();
    core.start_source("vol-src").await.unwrap();
    wait_for_status(&core, "vol-src", ComponentStatus::Running).await;

    // Volatile query — no storage backend
    let query = make_volatile_query("vol-q", "vol-src");
    core.add_query(query).await.unwrap();
    core.start_query("vol-q").await.unwrap();
    wait_for_status(&core, "vol-q", ComponentStatus::Running).await;

    send_event(&event_tx, "vol-src", 1, b"pos-1").await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    core.stop_query("vol-q").await.unwrap();
    wait_for_status(&core, "vol-q", ComponentStatus::Stopped).await;
    core.start_query("vol-q").await.unwrap();
    wait_for_status(&core, "vol-q", ComponentStatus::Running).await;

    // Volatile: no resume_from even after restart
    assert!(
        resume_from.read().await.is_none(),
        "Volatile query should never set resume_from"
    );

    core.stop_query("vol-q").await.unwrap();
}

/// Persistent query + volatile source (supports_replay=false) → rejected.
#[tokio::test]
async fn test_e2e_persistent_query_volatile_source_rejected() {
    let tmp_dir = tempfile::TempDir::new().unwrap();
    let core = build_e2e_lib("e2e-compat", &tmp_dir, None).await.unwrap();

    // Source that does NOT support replay
    let source = E2eTestSource::new("compat-src", false).unwrap();
    core.add_source(source).await.unwrap();
    core.start_source("compat-src").await.unwrap();
    wait_for_status(&core, "compat-src", ComponentStatus::Running).await;

    let query = make_persistent_query("compat-q", "compat-src", None);
    core.add_query(query).await.unwrap();

    let result = core.start_query("compat-q").await;
    assert!(
        result.is_err(),
        "Persistent query + volatile source should be rejected"
    );
}

/// Strict policy + PositionUnavailable → query goes to Error state.
#[tokio::test]
async fn test_e2e_strict_policy_position_unavailable() {
    let tmp_dir = tempfile::TempDir::new().unwrap();
    let core = build_e2e_lib("e2e-strict", &tmp_dir, None).await.unwrap();

    let source = E2eTestSource::new("strict-src", true).unwrap();
    source.set_fail_count(u32::MAX); // always fail
    core.add_source(source).await.unwrap();
    core.start_source("strict-src").await.unwrap();
    wait_for_status(&core, "strict-src", ComponentStatus::Running).await;

    let query = make_persistent_query("strict-q", "strict-src", Some(RecoveryPolicy::Strict));
    core.add_query(query).await.unwrap();

    let result = core.start_query("strict-q").await;
    assert!(
        result.is_err(),
        "Strict policy + PositionUnavailable should fail"
    );
}

/// AutoReset + PositionUnavailable → wipes state and re-bootstraps.
#[tokio::test]
async fn test_e2e_auto_reset_position_unavailable() {
    let tmp_dir = tempfile::TempDir::new().unwrap();
    let core = build_e2e_lib("e2e-reset", &tmp_dir, None).await.unwrap();

    let source = E2eTestSource::new("reset-src", true).unwrap();
    source.set_fail_count(1); // fail once, succeed on retry
    let sub_count = source.subscribe_count_handle();
    let resume_from = source.last_resume_from();

    core.add_source(source).await.unwrap();
    core.start_source("reset-src").await.unwrap();
    wait_for_status(&core, "reset-src", ComponentStatus::Running).await;

    let query = make_persistent_query("reset-q", "reset-src", Some(RecoveryPolicy::AutoReset));
    core.add_query(query).await.unwrap();

    let result = core.start_query("reset-q").await;
    assert!(
        result.is_ok(),
        "AutoReset should retry and succeed: {result:?}"
    );

    // Subscribed twice: first fail, then retry
    assert_eq!(sub_count.load(Ordering::Acquire), 2);

    // On retry, resume_from should be None (fresh bootstrap)
    assert!(
        resume_from.read().await.is_none(),
        "AutoReset retry should clear resume_from"
    );

    wait_for_status(&core, "reset-q", ComponentStatus::Running).await;
    core.stop_query("reset-q").await.unwrap();
}

/// remove_query() clears persistent state so re-adding starts fresh.
#[tokio::test]
async fn test_e2e_remove_query_clears_persistent_state() {
    let tmp_dir = tempfile::TempDir::new().unwrap();
    let core = build_e2e_lib("e2e-remove", &tmp_dir, None).await.unwrap();

    let source = E2eTestSource::new("rm-src", true).unwrap();
    let resume_from = source.last_resume_from();
    let event_tx = source.event_sender();

    core.add_source(source).await.unwrap();
    core.start_source("rm-src").await.unwrap();
    wait_for_status(&core, "rm-src", ComponentStatus::Running).await;

    // First lifecycle: add → start → feed events → stop
    let query = make_persistent_query("rm-q", "rm-src", None);
    core.add_query(query).await.unwrap();
    core.start_query("rm-q").await.unwrap();
    wait_for_status(&core, "rm-q", ComponentStatus::Running).await;

    send_event(&event_tx, "rm-src", 1, b"pos-1").await;
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    core.stop_query("rm-q").await.unwrap();
    wait_for_status(&core, "rm-q", ComponentStatus::Stopped).await;

    // Remove the query (should clear persistent state)
    core.remove_query("rm-q").await.unwrap();

    // Re-add the same query and start
    let query2 = make_persistent_query("rm-q", "rm-src", None);
    core.add_query(query2).await.unwrap();
    core.start_query("rm-q").await.unwrap();
    wait_for_status(&core, "rm-q", ComponentStatus::Running).await;

    // After removal + re-add, resume_from should be None (state was cleared)
    assert!(
        resume_from.read().await.is_none(),
        "After remove + re-add, resume_from should be None (persistent state cleared)"
    );

    core.stop_query("rm-q").await.unwrap();
}

/// stop() releases position handles on the source.
#[tokio::test]
async fn test_e2e_stop_releases_position_handles() {
    let tmp_dir = tempfile::TempDir::new().unwrap();
    let core = build_e2e_lib("e2e-handles", &tmp_dir, None).await.unwrap();

    let source = E2eTestSource::new("handle-src", true).unwrap();
    let removed_count = source.position_handle_removed_handle();

    core.add_source(source).await.unwrap();
    core.start_source("handle-src").await.unwrap();
    wait_for_status(&core, "handle-src", ComponentStatus::Running).await;

    let query = make_persistent_query("handle-q", "handle-src", None);
    core.add_query(query).await.unwrap();
    core.start_query("handle-q").await.unwrap();
    wait_for_status(&core, "handle-q", ComponentStatus::Running).await;

    assert_eq!(
        removed_count.load(Ordering::Acquire),
        0,
        "No position handle removal yet"
    );

    // Stop should release position handles
    core.stop_query("handle-q").await.unwrap();

    assert!(
        removed_count.load(Ordering::Acquire) >= 1,
        "Stop should have released position handle"
    );
}

/// Config change (different query string) triggers full re-bootstrap instead of resume.
#[tokio::test]
async fn test_e2e_config_change_triggers_rebootstrap() {
    let tmp_dir = tempfile::TempDir::new().unwrap();
    let core = build_e2e_lib("e2e-cfghash", &tmp_dir, None).await.unwrap();

    let source = E2eTestSource::new("cfg-src", true).unwrap();
    let resume_from = source.last_resume_from();
    let event_tx = source.event_sender();
    let sub_count = source.subscribe_count_handle();

    core.add_source(source).await.unwrap();
    core.start_source("cfg-src").await.unwrap();
    wait_for_status(&core, "cfg-src", ComponentStatus::Running).await;

    // Start with initial query
    let query1 = Query::cypher("cfg-q")
        .query("MATCH (n:Node) RETURN n.id AS id")
        .from_source("cfg-src")
        .auto_start(false)
        .enable_bootstrap(false)
        .with_storage_backend(StorageBackendRef::Inline(StorageBackendSpec::RocksDb {
            path: "/tmp/e2e-test-drasi".to_string(),
            enable_archive: false,
            direct_io: false,
        }))
        .build();
    core.add_query(query1).await.unwrap();
    core.start_query("cfg-q").await.unwrap();
    wait_for_status(&core, "cfg-q", ComponentStatus::Running).await;

    // Feed events to create checkpoints
    send_event(&event_tx, "cfg-src", 1, b"pos-1").await;
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    core.stop_query("cfg-q").await.unwrap();
    wait_for_status(&core, "cfg-q", ComponentStatus::Stopped).await;
    assert_eq!(sub_count.load(Ordering::Acquire), 1);

    // Update query with different query string (config hash changes)
    let query2 = Query::cypher("cfg-q")
        .query("MATCH (n:Node) RETURN n.id AS id, n.value AS value")
        .from_source("cfg-src")
        .auto_start(false)
        .enable_bootstrap(false)
        .with_storage_backend(StorageBackendRef::Inline(StorageBackendSpec::RocksDb {
            path: "/tmp/e2e-test-drasi".to_string(),
            enable_archive: false,
            direct_io: false,
        }))
        .build();
    core.update_query("cfg-q", query2).await.unwrap();

    // Restart — config hash changed, so should NOT resume from checkpoint
    core.start_query("cfg-q").await.unwrap();
    wait_for_status(&core, "cfg-q", ComponentStatus::Running).await;

    assert_eq!(sub_count.load(Ordering::Acquire), 2);

    // resume_from should be None because config changed → full re-bootstrap
    assert!(
        resume_from.read().await.is_none(),
        "Config change should trigger fresh start (no resume_from)"
    );

    core.stop_query("cfg-q").await.unwrap();
}

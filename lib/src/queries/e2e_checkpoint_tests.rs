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
use serial_test::serial;
use tokio::sync::{mpsc, RwLock};

use crate::channels::events::SourceEventWrapper;
use crate::channels::{ChannelChangeReceiver, ComponentStatus, SubscriptionResponse};
use crate::config::{QueryConfig, SourceSubscriptionSettings};
use crate::context::SourceRuntimeContext;
use crate::indexes::{IndexBackendPlugin, StorageBackendRef};
use crate::sources::{Source, SourceBase, SourceBaseParams, SourceError};
use crate::{DrasiLib, Query, RecoveryPolicy};

use drasi_index_rocksdb::RocksDbIndexProvider;

// ============================================================================
// E2E test source
// ============================================================================

struct E2eTestSource {
    base: SourceBase,
    replay_capable: bool,
    control: Arc<E2eSourceControl>,
}

#[derive(Default)]
struct E2eSourceControl {
    /// Records the most recent `resume_from` received in subscribe().
    last_resume_from: Arc<RwLock<Option<Bytes>>>,
    /// Records the most recent `resume_sequence` received in subscribe().
    last_resume_sequence: Arc<RwLock<Option<u64>>>,
    /// Number of times subscribe was called.
    subscribe_count: Arc<AtomicU32>,
    /// Number of remaining subscribe failures (decremented each call).
    remaining_failures: Arc<AtomicU32>,
    /// Number of times remove_position_handle was called.
    position_handle_removed: Arc<AtomicU32>,
    /// Sender for injecting events. Recreated in subscribe() for each subscription.
    event_tx: Arc<RwLock<Option<mpsc::Sender<Arc<SourceEventWrapper>>>>>,
    /// Supply a one-row snapshot to fresh bootstrap subscriptions.
    bootstrap: std::sync::atomic::AtomicBool,
    /// Supply an empty snapshot instead of one row.
    bootstrap_empty: std::sync::atomic::AtomicBool,
    /// Omit the snapshot boundary cursor from the bootstrap result.
    bootstrap_cursorless: std::sync::atomic::AtomicBool,
    /// Element ID of the snapshot row (defaults to `snapshot`).
    bootstrap_node_id: std::sync::Mutex<Option<&'static str>>,
    /// Source sequence carried by the snapshot row.
    bootstrap_sequence: AtomicU64,
    /// When set, the bootstrap result is withheld and its sender stored here.
    bootstrap_completion: std::sync::Mutex<Option<BootstrapCompletion>>,
    /// The latest position handle; the query confirms committed sequences here.
    position_handle: std::sync::Mutex<Option<Arc<AtomicU64>>>,
}

type BootstrapCompletion = Arc<
    tokio::sync::Mutex<
        Option<tokio::sync::oneshot::Sender<anyhow::Result<crate::bootstrap::BootstrapResult>>>,
    >,
>;

struct E2eSourceFixture {
    id: crate::ComponentId,
    name: crate::ComponentName,
    replay_capable: bool,
    control: Arc<E2eSourceControl>,
}

impl std::ops::Deref for E2eTestSource {
    type Target = E2eSourceControl;
    fn deref(&self) -> &Self::Target {
        &self.control
    }
}

impl std::ops::Deref for E2eSourceFixture {
    type Target = E2eSourceControl;
    fn deref(&self) -> &Self::Target {
        &self.control
    }
}

impl E2eSourceFixture {
    async fn add_to(self, core: &DrasiLib) -> crate::Result<()> {
        core.create_source_async(self.name, Some(self.id), false, move |id| async move {
            Ok((
                E2eTestSource {
                    base: SourceBase::new(SourceBaseParams::new(id))?,
                    replay_capable: self.replay_capable,
                    control: self.control,
                },
                std::collections::HashMap::new(),
            ))
        })
        .await
        .map(|_| ())
    }

    fn new(id: crate::ComponentId, name: crate::ComponentName, replay_capable: bool) -> Self {
        Self {
            id,
            name,
            replay_capable,
            control: Arc::new(E2eSourceControl::default()),
        }
    }
}

impl E2eSourceControl {
    fn last_resume_from(&self) -> Arc<RwLock<Option<Bytes>>> {
        self.last_resume_from.clone()
    }

    fn last_resume_sequence(&self) -> Arc<RwLock<Option<u64>>> {
        self.last_resume_sequence.clone()
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

    fn with_bootstrap(&self) {
        self.bootstrap.store(true, Ordering::Release);
    }

    /// Withhold the bootstrap result; the test completes or drops it.
    fn defer_bootstrap_completion(&self) -> BootstrapCompletion {
        let completion = Arc::new(tokio::sync::Mutex::new(None));
        *self.bootstrap_completion.lock().unwrap() = Some(completion.clone());
        completion
    }

    async fn bootstrap_response(
        &self,
        source_id: &crate::ComponentId,
    ) -> (
        Option<crate::channels::BootstrapEventReceiver>,
        Option<tokio::sync::oneshot::Receiver<anyhow::Result<crate::bootstrap::BootstrapResult>>>,
    ) {
        use drasi_core::models::{
            Element, ElementMetadata, ElementPropertyMap, ElementReference, ElementValue,
            SourceChange,
        };
        let (events, receiver) = mpsc::channel(1);
        let (result, result_receiver) = tokio::sync::oneshot::channel();
        let empty = self.bootstrap_empty.load(Ordering::Acquire);
        if !empty {
            let mut properties = ElementPropertyMap::new();
            properties.insert("id", ElementValue::String(source_id.as_str().into()));
            properties.insert("value", ElementValue::Integer(42));
            let node = self.bootstrap_node_id.lock().unwrap().unwrap_or("snapshot");
            events
                .send(crate::channels::BootstrapEvent {
                    source_id: source_id.clone(),
                    change: SourceChange::Insert {
                        element: Element::Node {
                            metadata: ElementMetadata {
                                reference: ElementReference::new(source_id.as_str(), node),
                                labels: Arc::from(vec![Arc::from("Node")]),
                                effective_from: 0,
                            },
                            properties,
                        },
                    },
                    timestamp: chrono::Utc::now(),
                    sequence: self.bootstrap_sequence.load(Ordering::Acquire),
                })
                .await
                .unwrap();
        }
        let completion = self.bootstrap_completion.lock().unwrap().clone();
        match completion {
            Some(completion) => *completion.lock().await = Some(result),
            None => result
                .send(Ok(crate::bootstrap::BootstrapResult {
                    event_count: usize::from(!empty),
                    source_position: (!self.bootstrap_cursorless.load(Ordering::Acquire))
                        .then(|| snapshot_cursor(source_id)),
                }))
                .unwrap(),
        }
        (Some(receiver), Some(result_receiver))
    }
}

fn snapshot_cursor(source_id: &crate::ComponentId) -> Bytes {
    Bytes::from(format!("snapshot-{source_id}"))
}

#[async_trait]
impl Source for E2eTestSource {
    fn id(&self) -> &drasi_core_types::ComponentId {
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

    async fn bind_runtime_context(&self, context: SourceRuntimeContext) {
        self.base.bind_runtime_context(context).await;
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
        *self.last_resume_sequence.write().await = settings.resume_sequence;

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
                source_id: self.id().clone(),
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
        self.position_handle
            .lock()
            .unwrap()
            .clone_from(&position_handle);

        // Like a replay-capable source: a native cursor or a framework sequence
        // resumes the stream; only a fresh subscription bootstraps.
        let (bootstrap_receiver, bootstrap_result_receiver) =
            if self.bootstrap.load(Ordering::Acquire)
                && settings.enable_bootstrap
                && settings.resume_from.is_none()
                && settings.resume_sequence.is_none()
            {
                self.bootstrap_response(self.id()).await
            } else {
                (None, None)
            };

        Ok(SubscriptionResponse {
            query_id: settings.query_id,
            source_id: self.id().clone(),
            receiver: Box::new(ChannelChangeReceiver::new(rx)),
            bootstrap_receiver,
            position_handle,
            bootstrap_result_receiver,
        })
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn remove_position_handle(&self, _query_id: &drasi_core_types::ComponentId) {
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
    build_e2e_lib_with(
        id,
        Arc::new(RocksDbIndexProvider::new(tmp_dir.path(), false, false)),
        default_recovery_policy,
    )
    .await
}

async fn build_e2e_lib_with(
    id: &str,
    provider: Arc<dyn IndexBackendPlugin>,
    default_recovery_policy: Option<RecoveryPolicy>,
) -> anyhow::Result<Arc<DrasiLib>> {
    let mut builder = DrasiLib::builder()
        .with_allow_external_component_ids(true)
        .with_query_factory(
            crate::computation::v1::FactoryRegistry::standard()
                .get(
                    &crate::computation::v1::ImplementationIdentity::try_new(
                        "drasi/continuous-query",
                        "1",
                    )
                    .expect("reference query identity"),
                )
                .expect("reference query factory")
                .clone(),
        )
        .with_id(id)
        .with_index_provider("persistent", provider);

    if let Some(policy) = default_recovery_policy {
        builder = builder.with_default_recovery_policy(policy);
    }

    let lib = builder.build().await?;
    Ok(Arc::new(lib))
}

fn make_persistent_query(
    id: crate::ComponentId,
    name: crate::ComponentName,
    source_id: drasi_core_types::ComponentName,
    recovery_policy: Option<RecoveryPolicy>,
) -> QueryConfig {
    let mut q = Query::cypher(name)
        .with_id(id)
        .query("MATCH (n:Node) RETURN n.id AS id, n.value AS value")
        .from_input_by_name(source_id)
        .auto_start(false)
        .enable_bootstrap(false)
        .with_storage_backend(StorageBackendRef::Named("persistent".to_string()));

    if let Some(policy) = recovery_policy {
        q = q.with_recovery_policy(policy);
    }

    q.build().unwrap()
}

fn make_volatile_query(
    id: drasi_core_types::ComponentName,
    source_id: drasi_core_types::ComponentName,
) -> QueryConfig {
    Query::cypher(id)
        .query("MATCH (n:Node) RETURN n.id AS id, n.value AS value")
        .from_input_by_name(source_id)
        .auto_start(false)
        .enable_bootstrap(false)
        .build()
        .unwrap()
}

/// Send a source change event and wait briefly for processing.
async fn send_event(
    tx: &Arc<RwLock<Option<mpsc::Sender<Arc<SourceEventWrapper>>>>>,
    source_id: drasi_core_types::ComponentId,
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
            reference: ElementReference::new(source_id.as_str(), &node_id),
            labels: Arc::from(vec![Arc::from("Node")]),
            effective_from: 0,
        },
        properties: props,
    };

    let change = SourceChange::Insert { element };

    let event = SourceEventWrapper::new(
        source_id,
        crate::channels::events::SourceEvent::Change(change),
        chrono::Utc::now(),
        sequence,
    )
    .with_source_position(Bytes::from(position.to_vec()));

    if let Some(sender) = tx.read().await.as_ref() {
        sender.send(Arc::new(event)).await.unwrap();
    }
    // Brief pause for processing
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
}

async fn wait_for_status(core: &DrasiLib, name: &crate::ComponentName, expected: ComponentStatus) {
    let graph = core.component_graph();
    crate::component_graph::wait_for_status_by_name(
        &graph,
        name,
        &[expected],
        std::time::Duration::from_secs(5),
    )
    .await
    .unwrap_or_else(|e| panic!("wait_for_status({name}, {expected:?}) failed: {e}"));
}

async fn wait_for_outbox_sequence(
    core: &DrasiLib,
    query_id: &drasi_core_types::ComponentName,
    sequence: u64,
) -> super::OutboxResponse {
    let query = core
        .query_manager()
        .get_query_instance_by_name(query_id)
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let outbox = query.fetch_outbox(0).await.unwrap();
            if outbox.latest_sequence >= sequence {
                return outbox;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("query must commit the expected output sequence")
}

// ============================================================================
// E2E Tests
// ============================================================================

#[rstest::rstest]
#[case::insert_only(1)]
#[case::named_update(2)]
#[tokio::test]
#[serial]
async fn test_e2e_outbox_persistent_reopen(#[case] sequence: u64) {
    use crate::channels::{ResultDiff, SourceEvent};
    use drasi_core::models::{
        Element, ElementMetadata, ElementPropertyMap, ElementReference, ElementValue, SourceChange,
    };
    use serde_json::json;

    let tmp_dir = tempfile::TempDir::new().unwrap();
    let core = build_e2e_lib("outbox-reopen", &tmp_dir, Some(RecoveryPolicy::Strict))
        .await
        .unwrap();
    let source = E2eSourceFixture::new(
        crate::ComponentId::try_new("00000000-0000-0000-0000-0000000001d5").expect("fixture UUID"),
        crate::ComponentName::try_new("outbox-src").expect("fixture component name"),
        true,
    );
    let event_tx = source.event_sender();
    source.add_to(&core).await.unwrap();
    let source_id = core
        .component_by_name(&crate::ComponentName::try_new("outbox-src").unwrap())
        .unwrap()
        .id()
        .clone();
    core.add_query(make_persistent_query(
        crate::ComponentId::try_new("00000000-0000-0000-0000-00000000044f").expect("fixture UUID"),
        drasi_core_types::ComponentName::try_new("outbox-q").expect("fixture component name"),
        drasi_core_types::ComponentName::try_new("outbox-src").expect("fixture component name"),
        None,
    ))
    .await
    .unwrap();
    core.start().await.unwrap();
    core.start_source_by_name(
        &drasi_core_types::ComponentName::try_new("outbox-src").expect("fixture component name"),
    )
    .await
    .unwrap();
    core.start_query_by_name(
        &drasi_core_types::ComponentName::try_new("outbox-q").expect("fixture component name"),
    )
    .await
    .unwrap();
    send_event(
        &event_tx,
        drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000001d5")
            .expect("fixture UUID"),
        1,
        b"pos-1",
    )
    .await;
    if sequence == 2 {
        let mut properties = ElementPropertyMap::new();
        properties.insert("id", ElementValue::String("node-1".into()));
        properties.insert("value", ElementValue::Integer(2));
        let mut event = SourceEventWrapper::new(
            drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000001d5")
                .expect("fixture UUID"),
            SourceEvent::Change(SourceChange::Update {
                removed_properties: Default::default(),
                element: Element::Node {
                    metadata: ElementMetadata {
                        reference: ElementReference::new(source_id.as_str(), "node-1"),
                        labels: Arc::from(vec![Arc::from("Node")]),
                        effective_from: 1,
                    },
                    properties,
                },
            }),
            chrono::Utc::now(),
            2,
        );
        event.source_position = Some(Bytes::from_static(b"pos-2"));
        event_tx
            .read()
            .await
            .as_ref()
            .expect("subscribed source")
            .send(Arc::new(event))
            .await
            .unwrap();
    }
    let before = wait_for_outbox_sequence(
        &core,
        &drasi_core_types::ComponentName::try_new("outbox-q").expect("fixture component name"),
        sequence,
    )
    .await;
    assert_eq!(before.latest_sequence, sequence);
    assert_eq!(before.results.len(), sequence as usize);
    let signature = match &before.results[0].results[0] {
        ResultDiff::Add {
            data,
            row_signature,
        } => {
            assert_eq!(*data, json!({"id": "node-1", "value": 1}));
            *row_signature
        }
        other => panic!("expected initial insert, got {other:?}"),
    };
    if sequence == 2 {
        assert_eq!(
            before.results[1].results,
            vec![ResultDiff::Update {
                data: json!({"id": "node-1", "value": 2}),
                before: json!({"id": "node-1", "value": 1}),
                after: json!({"id": "node-1", "value": 2}),
                grouping_keys: None,
                row_signature: signature,
            }]
        );
    }
    core.shutdown().await.unwrap();
    drop(core);

    let reopened = build_e2e_lib("outbox-reopen", &tmp_dir, Some(RecoveryPolicy::Strict))
        .await
        .unwrap();
    let source = E2eSourceFixture::new(
        crate::ComponentId::try_new("00000000-0000-0000-0000-0000000001d5").expect("fixture UUID"),
        crate::ComponentName::try_new("outbox-src").expect("fixture component name"),
        true,
    );
    let resume_sequence = source.last_resume_sequence();
    let event_tx = source.event_sender();
    source.add_to(&reopened).await.unwrap();
    reopened
        .add_query(make_persistent_query(
            crate::ComponentId::try_new("00000000-0000-0000-0000-00000000044f")
                .expect("fixture UUID"),
            drasi_core_types::ComponentName::try_new("outbox-q").expect("fixture component name"),
            drasi_core_types::ComponentName::try_new("outbox-src").expect("fixture component name"),
            None,
        ))
        .await
        .unwrap();
    reopened.start().await.unwrap();
    reopened
        .start_source_by_name(
            &drasi_core_types::ComponentName::try_new("outbox-src")
                .expect("fixture component name"),
        )
        .await
        .unwrap();
    let started = reopened
        .start_query_by_name(
            &drasi_core_types::ComponentName::try_new("outbox-q").expect("fixture component name"),
        )
        .await;
    started.expect("fresh persistent reopen must hydrate ordinary query updates");
    assert_eq!(*resume_sequence.read().await, Some(sequence));
    let query = reopened
        .query_manager()
        .get_query_instance_by_name(
            &drasi_core_types::ComponentName::try_new("outbox-q").expect("fixture component name"),
        )
        .await
        .unwrap();
    let snapshot = query.fetch_snapshot().await.unwrap();
    assert_eq!(snapshot.as_of_sequence, sequence);
    assert_eq!(
        snapshot.to_vec(),
        vec![json!({"id": "node-1", "value": sequence})]
    );
    let restored = query.fetch_outbox(0).await.unwrap();
    assert_eq!(restored.latest_sequence, sequence);
    assert_eq!(restored.config_hash, before.config_hash);
    assert_eq!(restored.output_generation, before.output_generation);
    assert_eq!(restored.results.len(), before.results.len());
    for (index, (restored, original)) in restored.results.iter().zip(&before.results).enumerate() {
        assert_eq!(restored.sequence, index as u64 + 1);
        assert_eq!(restored.query_id, original.query_id);
        assert_eq!(restored.timestamp, original.timestamp);
        assert_eq!(restored.results, original.results);
        assert_eq!(restored.metadata, original.metadata);
        assert!(restored.profiling.is_some());
    }
    assert_eq!(
        query
            .fetch_outbox(sequence - 1)
            .await
            .unwrap()
            .results
            .len(),
        1
    );
    send_event(
        &event_tx,
        drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000001d5")
            .expect("fixture UUID"),
        sequence + 1,
        b"pos-next",
    )
    .await;
    let continued = wait_for_outbox_sequence(
        &reopened,
        &drasi_core_types::ComponentName::try_new("outbox-q").expect("fixture component name"),
        sequence + 1,
    )
    .await;
    assert_eq!(continued.latest_sequence, sequence + 1);
    assert_eq!(continued.results.last().unwrap().sequence, sequence + 1);
    reopened.shutdown().await.unwrap();
}

/// Full lifecycle: build → start → feed events → stop → restart → verify resume_from.
#[tokio::test]
#[serial]
async fn test_e2e_checkpoint_round_trip() {
    let tmp_dir = tempfile::TempDir::new().unwrap();
    let core = build_e2e_lib("e2e-rt", &tmp_dir, None).await.unwrap();

    // Create source and grab shared handles before moving
    let source = E2eSourceFixture::new(
        crate::ComponentId::try_new("00000000-0000-0000-0000-000000000237").expect("fixture UUID"),
        crate::ComponentName::try_new("e2e-src").expect("fixture component name"),
        true,
    );
    let resume_from = source.last_resume_from();
    let event_tx = source.event_sender();
    let sub_count = source.subscribe_count_handle();

    // Add and start source
    source.add_to(&core).await.unwrap();
    core.start_source_by_name(
        &drasi_core_types::ComponentName::try_new("e2e-src").expect("fixture component name"),
    )
    .await
    .unwrap();
    wait_for_status(
        &core,
        &drasi_core_types::ComponentName::try_new("e2e-src").expect("fixture component name"),
        ComponentStatus::Running,
    )
    .await;

    // Add and start persistent query
    let query = make_persistent_query(
        crate::ComponentId::try_new("00000000-0000-0000-0000-000000000450").expect("fixture UUID"),
        drasi_core_types::ComponentName::try_new("e2e-q").expect("fixture component name"),
        drasi_core_types::ComponentName::try_new("e2e-src").expect("fixture component name"),
        None,
    );
    core.add_query(query).await.unwrap();
    core.start_query_by_name(
        &drasi_core_types::ComponentName::try_new("e2e-q").expect("fixture component name"),
    )
    .await
    .unwrap();
    wait_for_status(
        &core,
        &drasi_core_types::ComponentName::try_new("e2e-q").expect("fixture component name"),
        ComponentStatus::Running,
    )
    .await;

    // First subscribe: no resume_from (first start)
    assert!(
        resume_from.read().await.is_none(),
        "First start should have no resume_from"
    );
    assert_eq!(sub_count.load(Ordering::Acquire), 1);

    // Feed events with positions
    send_event(
        &event_tx,
        drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000237")
            .expect("fixture UUID"),
        1,
        b"pos-1",
    )
    .await;
    send_event(
        &event_tx,
        drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000237")
            .expect("fixture UUID"),
        2,
        b"pos-2",
    )
    .await;
    send_event(
        &event_tx,
        drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000237")
            .expect("fixture UUID"),
        3,
        b"pos-3",
    )
    .await;

    // Wait for checkpoint processing
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // Stop the query
    core.stop_query_by_name(
        &drasi_core_types::ComponentName::try_new("e2e-q").expect("fixture component name"),
    )
    .await
    .unwrap();
    wait_for_status(
        &core,
        &drasi_core_types::ComponentName::try_new("e2e-q").expect("fixture component name"),
        ComponentStatus::Stopped,
    )
    .await;

    // Restart the query
    core.start_query_by_name(
        &drasi_core_types::ComponentName::try_new("e2e-q").expect("fixture component name"),
    )
    .await
    .unwrap();
    wait_for_status(
        &core,
        &drasi_core_types::ComponentName::try_new("e2e-q").expect("fixture component name"),
        ComponentStatus::Running,
    )
    .await;

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

    core.stop_query_by_name(
        &drasi_core_types::ComponentName::try_new("e2e-q").expect("fixture component name"),
    )
    .await
    .unwrap();
}

/// Restart monotonicity (issue #827): after a checkpoint at sequence N, the query
/// manager must carry `resume_sequence = Some(N)` back to the source on resubscribe
/// so a native-cursor source can raise its sequence counter above the dedup
/// high-water. This verifies the QueryManager → source wiring end-to-end.
#[tokio::test]
#[serial]
async fn test_e2e_resume_sequence_carried_back() {
    let tmp_dir = tempfile::TempDir::new().unwrap();
    let core = build_e2e_lib("e2e-rs", &tmp_dir, None).await.unwrap();

    let source = E2eSourceFixture::new(
        crate::ComponentId::try_new("00000000-0000-0000-0000-000000000407").expect("fixture UUID"),
        crate::ComponentName::try_new("e2e-rs-src").expect("fixture component name"),
        true,
    );
    let resume_sequence = source.last_resume_sequence();
    let event_tx = source.event_sender();
    let sub_count = source.subscribe_count_handle();

    source.add_to(&core).await.unwrap();
    core.start_source_by_name(
        &drasi_core_types::ComponentName::try_new("e2e-rs-src").expect("fixture component name"),
    )
    .await
    .unwrap();
    wait_for_status(
        &core,
        &drasi_core_types::ComponentName::try_new("e2e-rs-src").expect("fixture component name"),
        ComponentStatus::Running,
    )
    .await;

    let query = make_persistent_query(
        crate::ComponentId::try_new("00000000-0000-0000-0000-000000000451").expect("fixture UUID"),
        drasi_core_types::ComponentName::try_new("e2e-rs-q").expect("fixture component name"),
        drasi_core_types::ComponentName::try_new("e2e-rs-src").expect("fixture component name"),
        None,
    );
    core.add_query(query).await.unwrap();
    core.start_query_by_name(
        &drasi_core_types::ComponentName::try_new("e2e-rs-q").expect("fixture component name"),
    )
    .await
    .unwrap();
    wait_for_status(
        &core,
        &drasi_core_types::ComponentName::try_new("e2e-rs-q").expect("fixture component name"),
        ComponentStatus::Running,
    )
    .await;

    // First start: no resume_sequence.
    assert!(
        resume_sequence.read().await.is_none(),
        "First start should have no resume_sequence"
    );
    assert_eq!(sub_count.load(Ordering::Acquire), 1);

    // Feed events; the last checkpointed sequence should be 3.
    send_event(
        &event_tx,
        drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000407")
            .expect("fixture UUID"),
        1,
        b"pos-1",
    )
    .await;
    send_event(
        &event_tx,
        drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000407")
            .expect("fixture UUID"),
        2,
        b"pos-2",
    )
    .await;
    send_event(
        &event_tx,
        drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000407")
            .expect("fixture UUID"),
        3,
        b"pos-3",
    )
    .await;
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // Stop and restart the query.
    core.stop_query_by_name(
        &drasi_core_types::ComponentName::try_new("e2e-rs-q").expect("fixture component name"),
    )
    .await
    .unwrap();
    wait_for_status(
        &core,
        &drasi_core_types::ComponentName::try_new("e2e-rs-q").expect("fixture component name"),
        ComponentStatus::Stopped,
    )
    .await;
    core.start_query_by_name(
        &drasi_core_types::ComponentName::try_new("e2e-rs-q").expect("fixture component name"),
    )
    .await
    .unwrap();
    wait_for_status(
        &core,
        &drasi_core_types::ComponentName::try_new("e2e-rs-q").expect("fixture component name"),
        ComponentStatus::Running,
    )
    .await;

    assert_eq!(
        sub_count.load(Ordering::Acquire),
        2,
        "Should have subscribed twice"
    );

    // After restart, the source must receive the checkpointed sequence so it can
    // raise its counter above the dedup high-water.
    let resumed_seq = *resume_sequence.read().await;
    assert_eq!(
        resumed_seq,
        Some(3),
        "After restart, resume_sequence must carry the last checkpointed sequence"
    );

    core.stop_query_by_name(
        &drasi_core_types::ComponentName::try_new("e2e-rs-q").expect("fixture component name"),
    )
    .await
    .unwrap();
}

/// Auto-reset must clear `resume_sequence` (not just `resume_from`): after a
/// checkpoint exists, a forced auto-reset re-subscribe should hand the source a
/// fresh-start `resume_sequence = None`, matching the cleared `resume_from`.
#[tokio::test]
#[serial]
async fn test_e2e_auto_reset_clears_resume_sequence() {
    let tmp_dir = tempfile::TempDir::new().unwrap();
    let core = build_e2e_lib("e2e-rs-reset", &tmp_dir, None).await.unwrap();

    let source = E2eSourceFixture::new(
        crate::ComponentId::try_new("00000000-0000-0000-0000-000000000408").expect("fixture UUID"),
        crate::ComponentName::try_new("rs-reset-src").expect("fixture component name"),
        true,
    );
    let resume_sequence = source.last_resume_sequence();
    let resume_from = source.last_resume_from();
    let event_tx = source.event_sender();
    let sub_count = source.subscribe_count_handle();
    let remaining_failures = source.remaining_failures_handle();

    source.add_to(&core).await.unwrap();
    core.start_source_by_name(
        &drasi_core_types::ComponentName::try_new("rs-reset-src").expect("fixture component name"),
    )
    .await
    .unwrap();
    wait_for_status(
        &core,
        &drasi_core_types::ComponentName::try_new("rs-reset-src").expect("fixture component name"),
        ComponentStatus::Running,
    )
    .await;

    // First run: establish a checkpoint at sequence 3.
    let query = make_persistent_query(
        crate::ComponentId::try_new("00000000-0000-0000-0000-000000000452").expect("fixture UUID"),
        drasi_core_types::ComponentName::try_new("rs-reset-q").expect("fixture component name"),
        drasi_core_types::ComponentName::try_new("rs-reset-src").expect("fixture component name"),
        Some(RecoveryPolicy::AutoReset),
    );
    core.add_query(query).await.unwrap();
    core.start_query_by_name(
        &drasi_core_types::ComponentName::try_new("rs-reset-q").expect("fixture component name"),
    )
    .await
    .unwrap();
    wait_for_status(
        &core,
        &drasi_core_types::ComponentName::try_new("rs-reset-q").expect("fixture component name"),
        ComponentStatus::Running,
    )
    .await;

    send_event(
        &event_tx,
        drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000408")
            .expect("fixture UUID"),
        1,
        b"pos-1",
    )
    .await;
    send_event(
        &event_tx,
        drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000408")
            .expect("fixture UUID"),
        2,
        b"pos-2",
    )
    .await;
    send_event(
        &event_tx,
        drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000408")
            .expect("fixture UUID"),
        3,
        b"pos-3",
    )
    .await;
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    core.stop_query_by_name(
        &drasi_core_types::ComponentName::try_new("rs-reset-q").expect("fixture component name"),
    )
    .await
    .unwrap();
    wait_for_status(
        &core,
        &drasi_core_types::ComponentName::try_new("rs-reset-q").expect("fixture component name"),
        ComponentStatus::Stopped,
    )
    .await;

    // Force the next resubscribe to fail with PositionUnavailable so AutoReset
    // clears persistent state and retries from a fresh bootstrap.
    remaining_failures.store(1, std::sync::atomic::Ordering::Release);
    let count_before = sub_count.load(Ordering::Acquire);

    core.start_query_by_name(
        &drasi_core_types::ComponentName::try_new("rs-reset-q").expect("fixture component name"),
    )
    .await
    .unwrap();
    wait_for_status(
        &core,
        &drasi_core_types::ComponentName::try_new("rs-reset-q").expect("fixture component name"),
        ComponentStatus::Running,
    )
    .await;

    // Two more subscribes: the failing resume attempt, then the auto-reset retry.
    assert_eq!(
        sub_count.load(Ordering::Acquire),
        count_before + 2,
        "AutoReset should retry after the failed resume"
    );

    // The retry is a fresh bootstrap: both resume signals cleared.
    assert!(
        resume_from.read().await.is_none(),
        "AutoReset retry must clear resume_from"
    );
    assert!(
        resume_sequence.read().await.is_none(),
        "AutoReset retry must clear resume_sequence"
    );

    core.stop_query_by_name(
        &drasi_core_types::ComponentName::try_new("rs-reset-q").expect("fixture component name"),
    )
    .await
    .unwrap();
}

#[tokio::test]
#[serial]
async fn test_e2e_volatile_query_no_checkpoints() {
    let tmp_dir = tempfile::TempDir::new().unwrap();
    let core = build_e2e_lib("e2e-vol", &tmp_dir, None).await.unwrap();

    let source = E2eSourceFixture::new(
        crate::ComponentId::try_new("00000000-0000-0000-0000-00000000021b").expect("fixture UUID"),
        crate::ComponentName::try_new("vol-src").expect("fixture component name"),
        true,
    );
    let resume_from = source.last_resume_from();
    let event_tx = source.event_sender();

    source.add_to(&core).await.unwrap();
    core.start_source_by_name(
        &drasi_core_types::ComponentName::try_new("vol-src").expect("fixture component name"),
    )
    .await
    .unwrap();
    wait_for_status(
        &core,
        &drasi_core_types::ComponentName::try_new("vol-src").expect("fixture component name"),
        ComponentStatus::Running,
    )
    .await;

    // Volatile query — no storage backend
    let query = make_volatile_query(
        drasi_core_types::ComponentName::try_new("vol-q").expect("fixture component name"),
        drasi_core_types::ComponentName::try_new("vol-src").expect("fixture component name"),
    );
    core.add_query(query).await.unwrap();
    core.start_query_by_name(
        &drasi_core_types::ComponentName::try_new("vol-q").expect("fixture component name"),
    )
    .await
    .unwrap();
    wait_for_status(
        &core,
        &drasi_core_types::ComponentName::try_new("vol-q").expect("fixture component name"),
        ComponentStatus::Running,
    )
    .await;

    send_event(
        &event_tx,
        drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000021b")
            .expect("fixture UUID"),
        1,
        b"pos-1",
    )
    .await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    core.stop_query_by_name(
        &drasi_core_types::ComponentName::try_new("vol-q").expect("fixture component name"),
    )
    .await
    .unwrap();
    wait_for_status(
        &core,
        &drasi_core_types::ComponentName::try_new("vol-q").expect("fixture component name"),
        ComponentStatus::Stopped,
    )
    .await;
    core.start_query_by_name(
        &drasi_core_types::ComponentName::try_new("vol-q").expect("fixture component name"),
    )
    .await
    .unwrap();
    wait_for_status(
        &core,
        &drasi_core_types::ComponentName::try_new("vol-q").expect("fixture component name"),
        ComponentStatus::Running,
    )
    .await;

    // Volatile: no resume_from even after restart
    assert!(
        resume_from.read().await.is_none(),
        "Volatile query should never set resume_from"
    );

    core.stop_query_by_name(
        &drasi_core_types::ComponentName::try_new("vol-q").expect("fixture component name"),
    )
    .await
    .unwrap();
}

/// Persistent query + volatile source (supports_replay=false) → rejected.
#[tokio::test]
#[serial]
async fn test_e2e_persistent_query_volatile_source_rejected() {
    let tmp_dir = tempfile::TempDir::new().unwrap();
    let core = build_e2e_lib("e2e-compat", &tmp_dir, None).await.unwrap();

    // Source that does NOT support replay
    let source = E2eSourceFixture::new(
        crate::ComponentId::try_new("00000000-0000-0000-0000-000000000453").expect("fixture UUID"),
        crate::ComponentName::try_new("compat-src").expect("fixture component name"),
        false,
    );
    source.add_to(&core).await.unwrap();
    core.start_source_by_name(
        &drasi_core_types::ComponentName::try_new("compat-src").expect("fixture component name"),
    )
    .await
    .unwrap();
    wait_for_status(
        &core,
        &drasi_core_types::ComponentName::try_new("compat-src").expect("fixture component name"),
        ComponentStatus::Running,
    )
    .await;

    let query = make_persistent_query(
        crate::ComponentId::try_new("00000000-0000-0000-0000-000000000454").expect("fixture UUID"),
        drasi_core_types::ComponentName::try_new("compat-q").expect("fixture component name"),
        drasi_core_types::ComponentName::try_new("compat-src").expect("fixture component name"),
        None,
    );
    core.add_query(query).await.unwrap();

    let result = core
        .start_query_by_name(
            &drasi_core_types::ComponentName::try_new("compat-q").expect("fixture component name"),
        )
        .await;
    assert!(
        result.is_err(),
        "Persistent query + volatile source should be rejected"
    );
}

/// Strict policy + PositionUnavailable → query goes to Error state.
#[tokio::test]
#[serial]
async fn test_e2e_strict_policy_position_unavailable() {
    let tmp_dir = tempfile::TempDir::new().unwrap();
    let core = build_e2e_lib("e2e-strict", &tmp_dir, None).await.unwrap();

    let source = E2eSourceFixture::new(
        crate::ComponentId::try_new("00000000-0000-0000-0000-000000000220").expect("fixture UUID"),
        crate::ComponentName::try_new("strict-src").expect("fixture component name"),
        true,
    );
    source.set_fail_count(u32::MAX); // always fail
    source.add_to(&core).await.unwrap();
    core.start_source_by_name(
        &drasi_core_types::ComponentName::try_new("strict-src").expect("fixture component name"),
    )
    .await
    .unwrap();
    wait_for_status(
        &core,
        &drasi_core_types::ComponentName::try_new("strict-src").expect("fixture component name"),
        ComponentStatus::Running,
    )
    .await;

    let query = make_persistent_query(
        crate::ComponentId::try_new("00000000-0000-0000-0000-000000000455").expect("fixture UUID"),
        drasi_core_types::ComponentName::try_new("strict-q").expect("fixture component name"),
        drasi_core_types::ComponentName::try_new("strict-src").expect("fixture component name"),
        Some(RecoveryPolicy::Strict),
    );
    core.add_query(query).await.unwrap();

    let result = core
        .start_query_by_name(
            &drasi_core_types::ComponentName::try_new("strict-q").expect("fixture component name"),
        )
        .await;
    assert!(
        result.is_err(),
        "Strict policy + PositionUnavailable should fail"
    );
}

/// AutoReset + PositionUnavailable → wipes state and re-bootstraps.
#[tokio::test]
#[serial]
async fn test_e2e_auto_reset_position_unavailable() {
    let tmp_dir = tempfile::TempDir::new().unwrap();
    let core = build_e2e_lib("e2e-reset", &tmp_dir, None).await.unwrap();

    let source = E2eSourceFixture::new(
        crate::ComponentId::try_new("00000000-0000-0000-0000-000000000222").expect("fixture UUID"),
        crate::ComponentName::try_new("reset-src").expect("fixture component name"),
        true,
    );
    source.set_fail_count(1); // fail once, succeed on retry
    let sub_count = source.subscribe_count_handle();
    let resume_from = source.last_resume_from();

    source.add_to(&core).await.unwrap();
    core.start_source_by_name(
        &drasi_core_types::ComponentName::try_new("reset-src").expect("fixture component name"),
    )
    .await
    .unwrap();
    wait_for_status(
        &core,
        &drasi_core_types::ComponentName::try_new("reset-src").expect("fixture component name"),
        ComponentStatus::Running,
    )
    .await;

    let query = make_persistent_query(
        crate::ComponentId::try_new("00000000-0000-0000-0000-000000000456").expect("fixture UUID"),
        drasi_core_types::ComponentName::try_new("reset-q").expect("fixture component name"),
        drasi_core_types::ComponentName::try_new("reset-src").expect("fixture component name"),
        Some(RecoveryPolicy::AutoReset),
    );
    core.add_query(query).await.unwrap();

    let result = core
        .start_query_by_name(
            &drasi_core_types::ComponentName::try_new("reset-q").expect("fixture component name"),
        )
        .await;
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

    wait_for_status(
        &core,
        &drasi_core_types::ComponentName::try_new("reset-q").expect("fixture component name"),
        ComponentStatus::Running,
    )
    .await;
    core.stop_query_by_name(
        &drasi_core_types::ComponentName::try_new("reset-q").expect("fixture component name"),
    )
    .await
    .unwrap();
}

/// remove_query() clears persistent state so re-adding starts fresh.
#[tokio::test]
#[serial]
async fn test_e2e_remove_query_clears_persistent_state() {
    let tmp_dir = tempfile::TempDir::new().unwrap();
    let core = build_e2e_lib("e2e-remove", &tmp_dir, None).await.unwrap();

    let source = E2eSourceFixture::new(
        crate::ComponentId::try_new("00000000-0000-0000-0000-000000000409").expect("fixture UUID"),
        crate::ComponentName::try_new("rm-src").expect("fixture component name"),
        true,
    );
    let resume_from = source.last_resume_from();
    let event_tx = source.event_sender();

    source.add_to(&core).await.unwrap();
    core.start_source_by_name(
        &drasi_core_types::ComponentName::try_new("rm-src").expect("fixture component name"),
    )
    .await
    .unwrap();
    wait_for_status(
        &core,
        &drasi_core_types::ComponentName::try_new("rm-src").expect("fixture component name"),
        ComponentStatus::Running,
    )
    .await;

    // First lifecycle: add → start → feed events → stop
    let query = make_persistent_query(
        crate::ComponentId::try_new("00000000-0000-0000-0000-000000000457").expect("fixture UUID"),
        drasi_core_types::ComponentName::try_new("rm-q").expect("fixture component name"),
        drasi_core_types::ComponentName::try_new("rm-src").expect("fixture component name"),
        None,
    );
    core.add_query(query).await.unwrap();
    core.start_query_by_name(
        &drasi_core_types::ComponentName::try_new("rm-q").expect("fixture component name"),
    )
    .await
    .unwrap();
    wait_for_status(
        &core,
        &drasi_core_types::ComponentName::try_new("rm-q").expect("fixture component name"),
        ComponentStatus::Running,
    )
    .await;

    send_event(
        &event_tx,
        drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000409")
            .expect("fixture UUID"),
        1,
        b"pos-1",
    )
    .await;
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    core.stop_query_by_name(
        &drasi_core_types::ComponentName::try_new("rm-q").expect("fixture component name"),
    )
    .await
    .unwrap();
    wait_for_status(
        &core,
        &drasi_core_types::ComponentName::try_new("rm-q").expect("fixture component name"),
        ComponentStatus::Stopped,
    )
    .await;

    // Remove the query (should clear persistent state)
    core.remove_query_by_name(
        &drasi_core_types::ComponentName::try_new("rm-q").expect("fixture component name"),
    )
    .await
    .unwrap();

    // Re-add the same query and start
    let query2 = make_persistent_query(
        crate::ComponentId::try_new("00000000-0000-0000-0000-000000000457").expect("fixture UUID"),
        drasi_core_types::ComponentName::try_new("rm-q").expect("fixture component name"),
        drasi_core_types::ComponentName::try_new("rm-src").expect("fixture component name"),
        None,
    );
    core.add_query(query2).await.unwrap();
    core.start_query_by_name(
        &drasi_core_types::ComponentName::try_new("rm-q").expect("fixture component name"),
    )
    .await
    .unwrap();
    wait_for_status(
        &core,
        &drasi_core_types::ComponentName::try_new("rm-q").expect("fixture component name"),
        ComponentStatus::Running,
    )
    .await;

    // After removal + re-add, resume_from should be None (state was cleared)
    assert!(
        resume_from.read().await.is_none(),
        "After remove + re-add, resume_from should be None (persistent state cleared)"
    );

    core.stop_query_by_name(
        &drasi_core_types::ComponentName::try_new("rm-q").expect("fixture component name"),
    )
    .await
    .unwrap();
}

/// stop() releases position handles on the source.
#[tokio::test]
#[serial]
async fn test_e2e_stop_releases_position_handles() {
    let tmp_dir = tempfile::TempDir::new().unwrap();
    let core = build_e2e_lib("e2e-handles", &tmp_dir, None).await.unwrap();

    let source = E2eSourceFixture::new(
        crate::ComponentId::try_new("00000000-0000-0000-0000-000000000458").expect("fixture UUID"),
        crate::ComponentName::try_new("handle-src").expect("fixture component name"),
        true,
    );
    let removed_count = source.position_handle_removed_handle();

    source.add_to(&core).await.unwrap();
    core.start_source_by_name(
        &drasi_core_types::ComponentName::try_new("handle-src").expect("fixture component name"),
    )
    .await
    .unwrap();
    wait_for_status(
        &core,
        &drasi_core_types::ComponentName::try_new("handle-src").expect("fixture component name"),
        ComponentStatus::Running,
    )
    .await;

    let query = make_persistent_query(
        crate::ComponentId::try_new("00000000-0000-0000-0000-000000000459").expect("fixture UUID"),
        drasi_core_types::ComponentName::try_new("handle-q").expect("fixture component name"),
        drasi_core_types::ComponentName::try_new("handle-src").expect("fixture component name"),
        None,
    );
    core.add_query(query).await.unwrap();
    core.start_query_by_name(
        &drasi_core_types::ComponentName::try_new("handle-q").expect("fixture component name"),
    )
    .await
    .unwrap();
    wait_for_status(
        &core,
        &drasi_core_types::ComponentName::try_new("handle-q").expect("fixture component name"),
        ComponentStatus::Running,
    )
    .await;

    assert_eq!(
        removed_count.load(Ordering::Acquire),
        0,
        "No position handle removal yet"
    );

    // Stop should release position handles
    core.stop_query_by_name(
        &drasi_core_types::ComponentName::try_new("handle-q").expect("fixture component name"),
    )
    .await
    .unwrap();

    assert!(
        removed_count.load(Ordering::Acquire) >= 1,
        "Stop should have released position handle"
    );
}

/// A new query identity cannot resume the checkpoint of a retired query with the same name.
#[tokio::test]
#[serial]
async fn test_e2e_query_recreation_does_not_resume_the_retired_identity() {
    let tmp_dir = tempfile::TempDir::new().unwrap();
    let core = build_e2e_lib("e2e-cfghash", &tmp_dir, None).await.unwrap();

    let source = E2eSourceFixture::new(
        crate::ComponentId::try_new("00000000-0000-0000-0000-00000000040a").expect("fixture UUID"),
        crate::ComponentName::try_new("cfg-src").expect("fixture component name"),
        true,
    );
    let resume_from = source.last_resume_from();
    let event_tx = source.event_sender();
    let sub_count = source.subscribe_count_handle();

    source.add_to(&core).await.unwrap();
    core.start_source_by_name(
        &drasi_core_types::ComponentName::try_new("cfg-src").expect("fixture component name"),
    )
    .await
    .unwrap();
    wait_for_status(
        &core,
        &drasi_core_types::ComponentName::try_new("cfg-src").expect("fixture component name"),
        ComponentStatus::Running,
    )
    .await;

    // Start with initial query
    let query1 = Query::cypher(
        drasi_core_types::ComponentName::try_new("cfg-q").expect("fixture component name"),
    )
    .query("MATCH (n:Node) RETURN n.id AS id")
    .from_input_by_name(
        drasi_core_types::ComponentName::try_new("cfg-src").expect("fixture component name"),
    )
    .auto_start(false)
    .enable_bootstrap(false)
    .with_storage_backend(StorageBackendRef::Named("persistent".to_string()))
    .build()
    .unwrap();
    let original = core.add_query_with_handle(query1).await.unwrap();
    core.start_query_by_name(
        &drasi_core_types::ComponentName::try_new("cfg-q").expect("fixture component name"),
    )
    .await
    .unwrap();
    wait_for_status(
        &core,
        &drasi_core_types::ComponentName::try_new("cfg-q").expect("fixture component name"),
        ComponentStatus::Running,
    )
    .await;

    // Feed events to create checkpoints
    send_event(
        &event_tx,
        drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000040a")
            .expect("fixture UUID"),
        1,
        b"pos-1",
    )
    .await;
    wait_for_outbox_sequence(&core, &crate::ComponentName::try_new("cfg-q").unwrap(), 1).await;

    core.stop_query_by_name(
        &drasi_core_types::ComponentName::try_new("cfg-q").expect("fixture component name"),
    )
    .await
    .unwrap();
    wait_for_status(
        &core,
        &drasi_core_types::ComponentName::try_new("cfg-q").expect("fixture component name"),
        ComponentStatus::Stopped,
    )
    .await;
    assert_eq!(sub_count.load(Ordering::Acquire), 1);

    core.remove_query_by_id(original.id()).await.unwrap();
    let query2 = Query::cypher(
        drasi_core_types::ComponentName::try_new("cfg-q").expect("fixture component name"),
    )
    .query("MATCH (n:Node) RETURN n.id AS id, n.value AS value")
    .from_input_by_name(
        drasi_core_types::ComponentName::try_new("cfg-src").expect("fixture component name"),
    )
    .auto_start(false)
    .enable_bootstrap(false)
    .with_storage_backend(StorageBackendRef::Named("persistent".to_string()))
    .build()
    .unwrap();
    let replacement = core.add_query_with_handle(query2).await.unwrap();
    replacement.wait_created().await.unwrap();
    assert_ne!(original.id(), replacement.id());

    core.start_query_by_name(
        &drasi_core_types::ComponentName::try_new("cfg-q").expect("fixture component name"),
    )
    .await
    .unwrap();
    wait_for_status(
        &core,
        &drasi_core_types::ComponentName::try_new("cfg-q").expect("fixture component name"),
        ComponentStatus::Running,
    )
    .await;

    assert_eq!(sub_count.load(Ordering::Acquire), 2);

    assert!(
        resume_from.read().await.is_none(),
        "A new component must not reuse the retired component's checkpoint"
    );

    core.stop_query_by_name(
        &drasi_core_types::ComponentName::try_new("cfg-q").expect("fixture component name"),
    )
    .await
    .unwrap();
    core.shutdown().await.unwrap();
}

mod aggregate_snapshot_tests {
    use super::*;
    use crate::channels::{QuerySubscriptionResponse, ResultDiff};
    use drasi_core::models::{
        Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange,
    };
    use serde_json::{json, Value};
    use std::time::Duration;
    use tokio::time::timeout;
    use tokio_stream::StreamExt;

    const SOURCE: &str = "aggregate-src";
    const TIMEOUT: Duration = Duration::from_secs(10);

    struct PersistentAggregate {
        core: Arc<DrasiLib>,
        query: Arc<dyn crate::queries::Query>,
        event_tx: Arc<RwLock<Option<mpsc::Sender<Arc<SourceEventWrapper>>>>>,
        subscription: QuerySubscriptionResponse,
    }

    impl PersistentAggregate {
        async fn open(
            tmp: &tempfile::TempDir,
            query_id: crate::ComponentId,
            query_name: crate::ComponentName,
        ) -> Self {
            Self::open_query(
                tmp,
                query_id,
                query_name,
                "MATCH (n:Node)
                 WITH sum(n.value) AS totalValue, sum(n.cost) AS totalCost,
                      count(n) AS positionCount
                 RETURN totalValue, totalCost, positionCount",
            )
            .await
        }

        async fn open_query(
            tmp: &tempfile::TempDir,
            query_id: crate::ComponentId,
            query_name: crate::ComponentName,
            query_text: &str,
        ) -> Self {
            let core = build_e2e_lib("aggregate-persistence", tmp, None)
                .await
                .unwrap();
            core.start().await.unwrap();
            let source = E2eSourceFixture::new(
                crate::ComponentId::try_new("00000000-0000-0000-0000-000000000364")
                    .expect("fixture UUID"),
                crate::ComponentName::try_new("aggregate-src").expect("fixture component name"),
                true,
            );
            let event_tx = source.event_sender();
            source.add_to(&core).await.unwrap();
            core.start_source_by_name(
                &drasi_core_types::ComponentName::try_new("aggregate-src")
                    .expect("fixture component name"),
            )
            .await
            .unwrap();
            wait_for_status(
                &core,
                &drasi_core_types::ComponentName::try_new("aggregate-src")
                    .expect("fixture component name"),
                ComponentStatus::Running,
            )
            .await;

            let config = Query::cypher(query_name.clone())
                .with_id(query_id)
                .query(query_text)
                .from_input_by_name(
                    drasi_core_types::ComponentName::try_new("aggregate-src")
                        .expect("fixture component name"),
                )
                .auto_start(false)
                .enable_bootstrap(false)
                .with_storage_backend(StorageBackendRef::Named("persistent".to_string()))
                .build()
                .unwrap();
            core.add_query(config).await.unwrap();
            core.start_query_by_name(&query_name).await.unwrap();
            wait_for_status(&core, &query_name, ComponentStatus::Running).await;
            let query = core
                .query_manager()
                .get_query_instance_by_name(&query_name)
                .await
                .unwrap();
            let subscription = query
                .subscribe(
                    drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000363")
                        .expect("fixture UUID"),
                )
                .await
                .unwrap();
            Self {
                core,
                query,
                event_tx,
                subscription,
            }
        }

        async fn send(&mut self, change: SourceChange, sequence: u64) -> Vec<ResultDiff> {
            let event = SourceEventWrapper::new(
                drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000364")
                    .expect("fixture UUID"),
                crate::channels::SourceEvent::Change(change),
                chrono::Utc::now(),
                sequence,
            )
            .with_source_position(Bytes::copy_from_slice(&sequence.to_be_bytes()));
            let sender = self.event_tx.read().await.as_ref().unwrap().clone();
            sender.send(Arc::new(event)).await.unwrap();
            timeout(TIMEOUT, self.subscription.receiver.recv())
                .await
                .expect("the source change must produce a live delta")
                .unwrap()
                .results
                .clone()
        }

        async fn assert_snapshot(&self, expected: HashMap<u64, Value>) {
            let snapshot = timeout(TIMEOUT, self.query.fetch_snapshot())
                .await
                .unwrap()
                .unwrap();
            let keyed: HashMap<_, _> = snapshot.stream_keyed().collect().await;
            assert_eq!(keyed, expected);
            let mut actual = self
                .core
                .get_query_results_by_id(&self.query.get_config().id)
                .await
                .unwrap();
            let mut expected: Vec<_> = expected.into_values().collect();
            actual.sort_by_key(Value::to_string);
            expected.sort_by_key(Value::to_string);
            assert_eq!(actual, expected);
        }

        async fn shutdown(self) {
            self.core.shutdown().await.unwrap();
        }
    }

    fn position(id: &str, value: i64, cost: i64, time: u64) -> Element {
        Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new(SOURCE, id),
                labels: Arc::from([Arc::from("Node")]),
                effective_from: time,
            },
            properties: ElementPropertyMap::from(json!({"value": value, "cost": cost})),
        }
    }

    fn summary(value: f64, cost: f64, count: i64) -> Value {
        json!({"totalValue": value, "totalCost": cost, "positionCount": count})
    }

    #[tokio::test]
    #[serial]
    async fn grouping_numeric_persistent_compound_key_reopens_and_drains() {
        const QUERY: &str = "MATCH (n:Node)
            WITH n.groupKey AS groupKey, sum(n.value) AS totalValue,
                 sum(n.cost) AS totalCost, count(n) AS positionCount
            RETURN totalValue, totalCost, positionCount";
        fn grouped(id: &str, group: Value, value: i64, cost: i64, time: u64) -> Element {
            let mut node = position(id, value, cost, time);
            if let Element::Node { properties, .. } = &mut node {
                *properties = ElementPropertyMap::from(json!({
                    "groupKey": group, "value": value, "cost": cost,
                }));
            }
            node
        }
        let integer = json!([1, {"bucket": 0}]);
        let float = json!([1.0, {"bucket": -0.0}]);
        let tmp = tempfile::TempDir::new().unwrap();
        let mut subject = PersistentAggregate::open_query(
            &tmp,
            crate::ComponentId::try_new("00000000-0000-0000-0000-00000000045a")
                .expect("fixture UUID"),
            crate::ComponentName::try_new("numeric-summary").expect("fixture component name"),
            QUERY,
        )
        .await;
        let added = subject
            .send(
                SourceChange::Insert {
                    element: grouped("p", integer, 10, 8, 1),
                },
                1,
            )
            .await;
        let signature = match added.as_slice() {
            [ResultDiff::Add {
                row_signature,
                data,
            }] => {
                assert_eq!(data, &summary(10.0, 8.0, 1));
                *row_signature
            }
            other => panic!("expected a group addition, got {other:?}"),
        };
        assert_eq!(
            subject
                .send(
                    SourceChange::Insert {
                        element: grouped("q", float.clone(), 5, 4, 2),
                    },
                    2
                )
                .await,
            update(signature, summary(10.0, 8.0, 1), summary(15.0, 12.0, 2))
        );
        subject
            .assert_snapshot(HashMap::from([(signature, summary(15.0, 12.0, 2))]))
            .await;
        subject.shutdown().await;

        let mut subject = PersistentAggregate::open_query(
            &tmp,
            crate::ComponentId::try_new("00000000-0000-0000-0000-00000000045a")
                .expect("fixture UUID"),
            crate::ComponentName::try_new("numeric-summary").expect("fixture component name"),
            QUERY,
        )
        .await;
        subject
            .assert_snapshot(HashMap::from([(signature, summary(15.0, 12.0, 2))]))
            .await;
        assert_eq!(
            subject
                .send(
                    SourceChange::Update {
                        removed_properties: Default::default(),
                        element: grouped("p", float, 20, 8, 3),
                    },
                    3
                )
                .await,
            update(signature, summary(15.0, 12.0, 2), summary(25.0, 12.0, 2))
        );
        subject
            .assert_snapshot(HashMap::from([(signature, summary(25.0, 12.0, 2))]))
            .await;
        assert_eq!(
            subject
                .send(
                    SourceChange::Delete {
                        metadata: position("q", 5, 4, 4).get_metadata().clone(),
                    },
                    4
                )
                .await,
            update(signature, summary(25.0, 12.0, 2), summary(20.0, 8.0, 1))
        );
        assert_eq!(
            subject
                .send(
                    SourceChange::Delete {
                        metadata: position("p", 20, 8, 5).get_metadata().clone(),
                    },
                    5
                )
                .await,
            vec![ResultDiff::Delete {
                data: summary(20.0, 8.0, 1),
                row_signature: signature
            }]
        );
        subject.assert_snapshot(HashMap::new()).await;
        subject.shutdown().await;
        let subject = PersistentAggregate::open_query(
            &tmp,
            crate::ComponentId::try_new("00000000-0000-0000-0000-00000000045a")
                .expect("fixture UUID"),
            crate::ComponentName::try_new("numeric-summary").expect("fixture component name"),
            QUERY,
        )
        .await;
        subject.assert_snapshot(HashMap::new()).await;
        subject.shutdown().await;
    }

    fn update(signature: u64, before: Value, after: Value) -> Vec<ResultDiff> {
        vec![ResultDiff::Update {
            data: after.clone(),
            before,
            after,
            grouping_keys: None,
            row_signature: signature,
        }]
    }

    async fn seed(subject: &mut PersistentAggregate, aapl_value: i64) -> u64 {
        let first = subject
            .send(
                SourceChange::Insert {
                    element: position("aapl", aapl_value, 800, 1),
                },
                1,
            )
            .await;
        let signature = match first.as_slice() {
            [ResultDiff::Add {
                data,
                row_signature,
            }] => {
                assert_eq!(data, &summary(aapl_value as f64, 800.0, 1));
                *row_signature
            }
            _ => panic!("expected one initial aggregate Add, got {first:?}"),
        };
        subject
            .assert_snapshot(HashMap::from([(
                signature,
                summary(aapl_value as f64, 800.0, 1),
            )]))
            .await;
        let second = subject
            .send(
                SourceChange::Insert {
                    element: position("msft", 900, 1000, 2),
                },
                2,
            )
            .await;
        assert_eq!(
            second,
            update(
                signature,
                summary(aapl_value as f64, 800.0, 1),
                summary((aapl_value + 900) as f64, 1800.0, 2),
            )
        );
        subject
            .assert_snapshot(HashMap::from([(
                signature,
                summary((aapl_value + 900) as f64, 1800.0, 2),
            )]))
            .await;
        signature
    }

    #[tokio::test]
    #[serial]
    async fn aggregate_snapshot_persistent_reopen_keeps_group_identity() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut subject = PersistentAggregate::open(
            &tmp,
            crate::ComponentId::try_new("00000000-0000-0000-0000-00000000045b")
                .expect("fixture UUID"),
            crate::ComponentName::try_new("persistent-summary").expect("fixture component name"),
        )
        .await;
        let signature = seed(&mut subject, 1100).await;
        assert_eq!(
            subject
                .send(
                    SourceChange::Update {
                        removed_properties: Default::default(),
                        element: position("aapl", 1150, 800, 3),
                    },
                    3,
                )
                .await,
            update(
                signature,
                summary(2000.0, 1800.0, 2),
                summary(2050.0, 1800.0, 2)
            )
        );
        subject
            .assert_snapshot(HashMap::from([(signature, summary(2050.0, 1800.0, 2))]))
            .await;
        subject.shutdown().await;

        let mut subject = PersistentAggregate::open(
            &tmp,
            crate::ComponentId::try_new("00000000-0000-0000-0000-00000000045b")
                .expect("fixture UUID"),
            crate::ComponentName::try_new("persistent-summary").expect("fixture component name"),
        )
        .await;
        subject
            .assert_snapshot(HashMap::from([(signature, summary(2050.0, 1800.0, 2))]))
            .await;
        // A different contributor updates the same persisted group after reopen.
        assert_eq!(
            subject
                .send(
                    SourceChange::Update {
                        removed_properties: Default::default(),
                        element: position("msft", 1000, 1000, 4),
                    },
                    4,
                )
                .await,
            update(
                signature,
                summary(2050.0, 1800.0, 2),
                summary(2150.0, 1800.0, 2)
            )
        );
        subject
            .assert_snapshot(HashMap::from([(signature, summary(2150.0, 1800.0, 2))]))
            .await;
        assert_eq!(
            subject
                .send(
                    SourceChange::Delete {
                        metadata: position("msft", 1000, 1000, 5).get_metadata().clone(),
                    },
                    5,
                )
                .await,
            update(
                signature,
                summary(2150.0, 1800.0, 2),
                summary(1150.0, 800.0, 1)
            )
        );
        subject
            .assert_snapshot(HashMap::from([(signature, summary(1150.0, 800.0, 1))]))
            .await;
        assert_eq!(
            subject
                .send(
                    SourceChange::Delete {
                        metadata: position("aapl", 1150, 800, 6).get_metadata().clone(),
                    },
                    6,
                )
                .await,
            vec![ResultDiff::Delete {
                data: summary(1150.0, 800.0, 1),
                row_signature: signature,
            }]
        );
        subject.assert_snapshot(HashMap::new()).await;
        subject.shutdown().await;

        let subject = PersistentAggregate::open(
            &tmp,
            crate::ComponentId::try_new("00000000-0000-0000-0000-00000000045b")
                .expect("fixture UUID"),
            crate::ComponentName::try_new("persistent-summary").expect("fixture component name"),
        )
        .await;
        subject.assert_snapshot(HashMap::new()).await;
        subject.shutdown().await;
    }

    #[tokio::test]
    #[serial]
    async fn aggregate_snapshot_legacy_contributor_namespace_is_never_adopted() {
        use drasi_core::{
            evaluation::functions::FunctionRegistry, interface::RowMutation, query::QueryBuilder,
        };
        use drasi_query_cypher::CypherParser;

        let tmp = tempfile::TempDir::new().unwrap();
        let mut subject = PersistentAggregate::open(
            &tmp,
            crate::ComponentId::try_new("00000000-0000-0000-0000-00000000022a")
                .expect("fixture UUID"),
            crate::ComponentName::try_new("legacy-summary").expect("fixture component name"),
        )
        .await;
        let signature = seed(&mut subject, 1100).await;
        subject.shutdown().await;

        let ordinary = QueryBuilder::new(
            "MATCH (n:Node) RETURN n.value",
            Arc::new(CypherParser::new(Arc::new(FunctionRegistry::new()))),
        )
        .build()
        .await;
        let mut contributor_signatures = Vec::new();
        for element in [position("aapl", 1100, 800, 1), position("msft", 900, 1000, 2)] {
            let diffs = ordinary
                .process_source_change(SourceChange::Insert { element })
                .await
                .unwrap();
            assert_eq!(diffs.len(), 1);
            contributor_signatures.push(diffs[0].row_signature());
        }
        let aapl = contributor_signatures[0];
        let msft = contributor_signatures[1];
        assert_ne!(aapl, msft);
        assert_ne!(signature, aapl);
        assert_ne!(signature, msft);

        // Legacy processing namespaces are outside the graph's storage scope.
        // Do not adopt their stale contributor rows or mutate them during recovery.
        let stale = rmp_serde::to_vec(&summary(1100.0, 800.0, 1)).unwrap();
        let current = rmp_serde::to_vec(&summary(2000.0, 1800.0, 2)).unwrap();
        {
            let indexes = RocksDbIndexProvider::new(tmp.path(), false, false)
                .create_indexes(
                    &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000022a")
                        .expect("fixture UUID"),
                )
                .await
                .unwrap();
            indexes
                .live_results_writer
                .as_ref()
                .unwrap()
                .apply_mutations(
                    &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000022a")
                        .expect("fixture UUID"),
                    &[
                        RowMutation {
                            row_signature: signature,
                            data: None,
                        },
                        RowMutation {
                            row_signature: aapl,
                            data: Some(&stale),
                        },
                        RowMutation {
                            row_signature: msft,
                            data: Some(&current),
                        },
                    ],
                )
                .await
                .unwrap();
        }

        let mut subject = PersistentAggregate::open(
            &tmp,
            crate::ComponentId::try_new("00000000-0000-0000-0000-00000000022a")
                .expect("fixture UUID"),
            crate::ComponentName::try_new("legacy-summary").expect("fixture component name"),
        )
        .await;
        subject
            .assert_snapshot(HashMap::from([(signature, summary(2000.0, 1800.0, 2))]))
            .await;
        assert_eq!(
            subject
                .send(
                    SourceChange::Update {
                        removed_properties: Default::default(),
                        element: position("aapl", 1150, 800, 3),
                    },
                    3
                )
                .await,
            update(
                signature,
                summary(2000.0, 1800.0, 2),
                summary(2050.0, 1800.0, 2)
            )
        );
        subject.shutdown().await;

        // Only the graph's committed group survives its reopen.
        let subject = PersistentAggregate::open(
            &tmp,
            crate::ComponentId::try_new("00000000-0000-0000-0000-00000000022a")
                .expect("fixture UUID"),
            crate::ComponentName::try_new("legacy-summary").expect("fixture component name"),
        )
        .await;
        subject
            .assert_snapshot(HashMap::from([(signature, summary(2050.0, 1800.0, 2))]))
            .await;
        subject.shutdown().await;

        // A new query namespace and complete input reconstruction are an explicit
        // rebuild, leaving the old namespace intact rather than guessing by value.
        let mut rebuilt = PersistentAggregate::open(
            &tmp,
            crate::ComponentId::try_new("00000000-0000-0000-0000-00000000045c")
                .expect("fixture UUID"),
            crate::ComponentName::try_new("rebuilt-summary").expect("fixture component name"),
        )
        .await;
        rebuilt.assert_snapshot(HashMap::new()).await;
        assert_eq!(seed(&mut rebuilt, 1150).await, signature);
        rebuilt.shutdown().await;
        let indexes = RocksDbIndexProvider::new(tmp.path(), false, false)
            .create_indexes(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000022a")
                    .expect("fixture UUID"),
            )
            .await
            .unwrap();
        assert_eq!(
            indexes
                .live_results_writer
                .as_ref()
                .unwrap()
                .read_snapshot(
                    &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000022a")
                        .expect("fixture UUID")
                )
                .await
                .unwrap()
                .into_iter()
                .collect::<std::collections::BTreeMap<_, _>>(),
            std::collections::BTreeMap::from([(aapl, stale), (msft, current)])
        );
    }
}

/// Bootstrap handover, restart and replacement guarantees for persistent
/// queries whose sources supply snapshots through ordinary subscriptions.
mod durable_bootstrap_tests {
    use super::*;
    use drasi_core::interface::{
        CheckpointKey, CheckpointStore, CreatedIndexes, IndexError, LiveResultsWriter,
        OutputPartition, RowMutation, SourceCheckpoint,
    };

    const BOOTSTRAP_MARKER: &str = "\0computation:query-bootstrap:v1";

    fn name(value: &str) -> crate::ComponentName {
        crate::ComponentName::try_new(value).expect("fixture component name")
    }

    fn id(value: u128) -> crate::ComponentId {
        crate::test_helpers::component_id(0xb007_0000 + value)
    }

    struct Sources {
        controls: Vec<Arc<E2eSourceControl>>,
        ids: Vec<crate::ComponentId>,
    }

    /// Adds `left` and `right`; `configure` prepares each control before use.
    async fn add_sources(
        core: &DrasiLib,
        names: &[&str],
        configure: impl Fn(usize, &E2eSourceControl),
    ) -> Sources {
        let mut sources = Sources {
            controls: Vec::new(),
            ids: Vec::new(),
        };
        for (index, source_name) in names.iter().enumerate() {
            let fixture = E2eSourceFixture::new(id(index as u128 + 1), name(source_name), true);
            configure(index, &fixture.control);
            sources.controls.push(fixture.control.clone());
            sources.ids.push(fixture.id.clone());
            fixture.add_to(core).await.unwrap();
            core.start_source_by_name(&name(source_name)).await.unwrap();
        }
        sources
    }

    fn query_config(
        query_name: &str,
        text: &str,
        inputs: &[&str],
        policy: RecoveryPolicy,
    ) -> QueryConfig {
        let mut builder = Query::cypher(name(query_name))
            .with_id(id(0x100))
            .query(text)
            .enable_bootstrap(true)
            .auto_start(false)
            .with_storage_backend(StorageBackendRef::Named("persistent".into()))
            .with_recovery_policy(policy);
        for input in inputs {
            builder = builder.from_input_by_name(name(input));
        }
        builder.build().unwrap()
    }

    async fn query_handle(
        core: &DrasiLib,
        query_name: &str,
    ) -> Arc<dyn super::super::traits::Query> {
        core.query_manager()
            .get_query_instance_by_name(&name(query_name))
            .await
            .unwrap()
    }

    async fn wait_for_confirmed(control: &E2eSourceControl, sequence: u64) {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let handle = control.position_handle.lock().unwrap().clone();
                if handle.is_some_and(|handle| handle.load(Ordering::Acquire) == sequence) {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("query must confirm the durable input checkpoint")
    }

    async fn subscriptions_closed(controls: &[Arc<E2eSourceControl>]) -> bool {
        for control in controls {
            if control
                .event_tx
                .read()
                .await
                .as_ref()
                .is_some_and(|sender| !sender.is_closed())
            {
                return false;
            }
        }
        true
    }

    /// A completed bootstrap is reopened from its committed boundary without
    /// re-bootstrapping: each snapshot source resumes from its snapshot cursor
    /// and every source resumes from framework sequence zero, so input admitted
    /// after the snapshot is replayed rather than skipped. This holds for empty
    /// snapshots and for sources that supply no snapshot at all, and for a
    /// replacement bootstrap after an authorized configuration change.
    #[rstest::rstest]
    #[case::rows(false, 2, false)]
    #[case::empty_snapshot(true, 2, false)]
    #[case::mixed_sources(false, 1, false)]
    #[case::no_bootstrap_providers(false, 0, false)]
    #[case::replacement_rows(false, 2, true)]
    #[case::replacement_mixed_sources(false, 1, true)]
    #[tokio::test]
    #[serial]
    async fn bootstrap_snapshot_reopens_without_live_events(
        #[case] empty: bool,
        #[case] bootstrap_sources: usize,
        #[case] replacement: bool,
    ) {
        let directory = tempfile::TempDir::new().unwrap();
        // A replacement is a configuration change, which only AutoReset
        // authorizes; first creation and reopen behave the same under both.
        let policy = if replacement {
            RecoveryPolicy::AutoReset
        } else {
            RecoveryPolicy::Strict
        };
        let last_run = if replacement { 2 } else { 1 };
        let mut generation = None;
        for run in 0..=last_run {
            let seed = replacement && run == 0;
            let restart = run == last_run;
            let core = build_e2e_lib("bootstrap-reopen", &directory, None)
                .await
                .unwrap();
            let sources = add_sources(&core, &["left", "right"], |index, control| {
                if index < bootstrap_sources {
                    control.with_bootstrap();
                }
                control.bootstrap_empty.store(empty, Ordering::Release);
            })
            .await;
            core.add_query(query_config(
                "bootstrap-q",
                if seed {
                    "MATCH (n:Node) RETURN n.id AS id, n.value AS previous_value"
                } else {
                    "MATCH (n:Node) RETURN n.id AS id, n.value AS value"
                },
                &["left", "right"],
                policy,
            ))
            .await
            .unwrap();
            core.start_query_by_name(&name("bootstrap-q"))
                .await
                .unwrap();
            let query = query_handle(&core, "bootstrap-q").await;
            let snapshot = query.fetch_snapshot().await.unwrap();
            assert_eq!(snapshot.as_of_sequence, 0, "run {run}");
            if seed {
                generation = Some(snapshot.output_generation);
                core.shutdown().await.unwrap();
                continue;
            }
            match (restart, generation) {
                // A reopen keeps the completed bootstrap's output generation.
                (true, Some(previous)) => assert_eq!(snapshot.output_generation, previous),
                // A replacement bootstrap starts a new output generation.
                (false, Some(previous)) => assert!(snapshot.output_generation > previous),
                (_, None) => {}
            }
            generation = Some(snapshot.output_generation);
            let mut rows = snapshot.to_vec();
            rows.sort_by_key(|row| row["id"].as_str().unwrap().to_string());
            let expected: Vec<_> = if empty {
                Vec::new()
            } else {
                sources
                    .ids
                    .iter()
                    .take(bootstrap_sources)
                    .map(|source| serde_json::json!({"id": source.as_str(), "value": 42}))
                    .collect()
            };
            assert_eq!(rows, expected, "run {run}");
            assert!(query.fetch_outbox(0).await.unwrap().results.is_empty());
            for (index, control) in sources.controls.iter().enumerate() {
                assert_eq!(
                    *control.last_resume_from.read().await,
                    (restart && index < bootstrap_sources)
                        .then(|| snapshot_cursor(&sources.ids[index])),
                    "run {run} source {index}"
                );
                assert_eq!(
                    *control.last_resume_sequence.read().await,
                    restart.then_some(0),
                    "run {run} source {index}"
                );
                assert_eq!(control.subscribe_count.load(Ordering::Acquire), 1);
            }
            core.shutdown().await.unwrap();
        }
    }

    /// An event that changes no result still advances the durable input
    /// checkpoint; a restart resumes after it (cursor and sequence-only
    /// sources alike) while the output remains at the bootstrap boundary.
    #[rstest::rstest]
    #[case::cursor_only(false, RecoveryPolicy::Strict)]
    #[case::sequence_only_strict(true, RecoveryPolicy::Strict)]
    #[case::sequence_only_auto_reset(true, RecoveryPolicy::AutoReset)]
    #[tokio::test]
    #[serial]
    async fn bootstrap_snapshot_reopens_after_filtered_event(
        #[case] sequence_only: bool,
        #[case] policy: RecoveryPolicy,
    ) {
        use drasi_core::models::{Element, ElementMetadata, ElementReference, SourceChange};
        let directory = tempfile::TempDir::new().unwrap();
        let names: &[&str] = if sequence_only {
            &["source", "sequence-only"]
        } else {
            &["source"]
        };
        for restart in [false, true] {
            let core = build_e2e_lib("bootstrap-filtered", &directory, None)
                .await
                .unwrap();
            let sources = add_sources(&core, names, |index, control| {
                if index == 0 {
                    control.with_bootstrap();
                }
            })
            .await;
            core.add_query(query_config(
                "filtered",
                "MATCH (n:Node) WHERE n.value = 42 RETURN n.id AS id, n.value AS value",
                names,
                policy,
            ))
            .await
            .unwrap();
            core.start_query_by_name(&name("filtered")).await.unwrap();
            let source = &sources.controls[0];
            assert_eq!(
                *source.last_resume_from.read().await,
                restart.then(|| Bytes::from_static(b"filtered-1"))
            );
            assert_eq!(
                *source.last_resume_sequence.read().await,
                restart.then_some(1)
            );
            if sequence_only {
                assert_eq!(
                    *sources.controls[1].last_resume_sequence.read().await,
                    Some(if restart { 7 } else { 0 }).filter(|_| restart)
                );
            }
            let query = query_handle(&core, "filtered").await;
            if !restart {
                send_event(&source.event_tx, sources.ids[0].clone(), 1, b"filtered-1").await;
                wait_for_confirmed(source, 1).await;
                if sequence_only {
                    let event = SourceEventWrapper::new(
                        sources.ids[1].clone(),
                        crate::channels::events::SourceEvent::Change(SourceChange::Insert {
                            element: Element::Node {
                                metadata: ElementMetadata {
                                    reference: ElementReference::new(
                                        sources.ids[1].as_str(),
                                        "filtered",
                                    ),
                                    labels: Arc::from(vec![Arc::from("Excluded")]),
                                    effective_from: 0,
                                },
                                properties: Default::default(),
                            },
                        }),
                        chrono::Utc::now(),
                        7,
                    );
                    let sender = sources.controls[1].event_tx.read().await.clone().unwrap();
                    sender.send(Arc::new(event)).await.unwrap();
                    wait_for_confirmed(&sources.controls[1], 7).await;
                }
            }
            let snapshot = query.fetch_snapshot().await.unwrap();
            assert_eq!(snapshot.as_of_sequence, 0);
            assert_eq!(snapshot.output_generation, 0);
            assert_eq!(
                snapshot.to_vec(),
                vec![serde_json::json!({"id": sources.ids[0].as_str(), "value": 42})]
            );
            assert!(query.fetch_outbox(0).await.unwrap().results.is_empty());
            if restart {
                send_event(&source.event_tx, sources.ids[0].clone(), 42, b"matching-42").await;
                let outbox = wait_for_outbox_sequence(&core, &name("filtered"), 1).await;
                assert_eq!(outbox.latest_sequence, 1);
                let rows = query.fetch_snapshot().await.unwrap().to_vec();
                assert_eq!(rows.len(), 2);
                assert!(rows.contains(&serde_json::json!({"id": "node-42", "value": 42})));
            }
            core.shutdown().await.unwrap();
        }
    }

    /// Source-native cursors, bootstrap ordinals and live framework sequences
    /// are separate identities: the result sequence counts committed outputs
    /// and the restart resumes from the last live event's sequence and cursor.
    #[tokio::test]
    #[serial]
    async fn bootstrap_and_live_sequences_remain_independent_of_source_cursor_on_restart() {
        const BOOTSTRAP_SEQUENCE: u64 = 1_000_000;
        const FIRST_CURSOR: &[u8] = b"local-epoch:7";
        const SECOND_CURSOR: &[u8] = b"local-epoch:8";
        let directory = tempfile::TempDir::new().unwrap();
        for restart in [false, true] {
            let core = build_e2e_lib("sequence-instance", &directory, None)
                .await
                .unwrap();
            let sources = add_sources(&core, &["cursor-source"], |_, control| {
                control.with_bootstrap();
                control
                    .bootstrap_sequence
                    .store(BOOTSTRAP_SEQUENCE, Ordering::Release);
            })
            .await;
            let source = &sources.controls[0];
            core.add_query(query_config(
                "cursor-query",
                "MATCH (n:Node) RETURN n.id AS id, n.value AS value",
                &["cursor-source"],
                RecoveryPolicy::Strict,
            ))
            .await
            .unwrap();
            core.start_query_by_name(&name("cursor-query"))
                .await
                .unwrap();
            assert_eq!(
                *source.last_resume_from.read().await,
                restart.then(|| Bytes::from_static(FIRST_CURSOR))
            );
            assert_eq!(
                *source.last_resume_sequence.read().await,
                restart.then_some(BOOTSTRAP_SEQUENCE + 1)
            );
            let query = query_handle(&core, "cursor-query").await;
            if restart {
                let restored = query.fetch_snapshot().await.unwrap();
                assert_eq!(restored.as_of_sequence, 1);
                assert_eq!(restored.to_vec().len(), 2);
            }
            let result_sequence = if restart { 2 } else { 1 };
            let event_sequence = BOOTSTRAP_SEQUENCE + result_sequence;
            let cursor = if restart { SECOND_CURSOR } else { FIRST_CURSOR };
            send_event(
                &source.event_tx,
                sources.ids[0].clone(),
                event_sequence,
                cursor,
            )
            .await;
            let outbox =
                wait_for_outbox_sequence(&core, &name("cursor-query"), result_sequence).await;
            assert_eq!(outbox.latest_sequence, result_sequence);
            let rows = query.fetch_snapshot().await.unwrap().to_vec();
            assert_eq!(rows.len(), result_sequence as usize + 1);
            for live in 1..=result_sequence {
                assert!(rows.contains(&serde_json::json!({
                    "id": format!("node-{}", BOOTSTRAP_SEQUENCE + live),
                    "value": BOOTSTRAP_SEQUENCE + live,
                })));
            }
            wait_for_confirmed(source, event_sequence).await;
            core.shutdown().await.unwrap();
        }
    }

    #[derive(Default)]
    struct Fault {
        handover_checkpoint: std::sync::atomic::AtomicBool,
        snapshot_write: std::sync::atomic::AtomicBool,
    }

    struct FaultStore {
        inner: Arc<dyn CheckpointStore>,
        fault: Arc<Fault>,
    }

    #[async_trait]
    impl CheckpointStore for FaultStore {
        fn is_persistent(&self) -> bool {
            self.inner.is_persistent()
        }
        async fn stage_checkpoint(
            &self,
            key: &CheckpointKey,
            sequence: u64,
            position: Option<&Bytes>,
        ) -> Result<(), IndexError> {
            if key.metadata_name() == Some(BOOTSTRAP_MARKER)
                && sequence == 1
                && self.fault.handover_checkpoint.load(Ordering::Acquire)
            {
                return Err(IndexError::IOError);
            }
            self.inner.stage_checkpoint(key, sequence, position).await
        }
        async fn read_checkpoint(
            &self,
            key: &CheckpointKey,
        ) -> Result<Option<SourceCheckpoint>, IndexError> {
            self.inner.read_checkpoint(key).await
        }
        async fn read_all_checkpoints(
            &self,
        ) -> Result<HashMap<CheckpointKey, SourceCheckpoint>, IndexError> {
            self.inner.read_all_checkpoints().await
        }
        async fn clear_checkpoints(&self) -> Result<(), IndexError> {
            self.inner.clear_checkpoints().await
        }
        async fn write_config_hash(&self, hash: u64) -> Result<(), IndexError> {
            self.inner.write_config_hash(hash).await
        }
        async fn read_config_hash(&self) -> Result<Option<u64>, IndexError> {
            self.inner.read_config_hash().await
        }
        async fn stage_result_sequence(
            &self,
            id: &OutputPartition,
            sequence: u64,
        ) -> Result<(), IndexError> {
            self.inner.stage_result_sequence(id, sequence).await
        }
        async fn write_result_sequence(
            &self,
            id: &OutputPartition,
            sequence: u64,
        ) -> Result<(), IndexError> {
            self.inner.write_result_sequence(id, sequence).await
        }
        async fn read_result_sequence(
            &self,
            id: &OutputPartition,
        ) -> Result<Option<u64>, IndexError> {
            self.inner.read_result_sequence(id).await
        }
        async fn write_output_generation(
            &self,
            id: &OutputPartition,
            generation: u64,
        ) -> Result<(), IndexError> {
            self.inner.write_output_generation(id, generation).await
        }
        async fn read_output_generation(
            &self,
            id: &OutputPartition,
        ) -> Result<Option<u64>, IndexError> {
            self.inner.read_output_generation(id).await
        }
    }

    struct FaultLiveResults {
        inner: Arc<dyn LiveResultsWriter>,
        fault: Arc<Fault>,
    }

    #[async_trait]
    impl LiveResultsWriter for FaultLiveResults {
        async fn apply_mutations(
            &self,
            id: &crate::ComponentId,
            rows: &[RowMutation<'_>],
        ) -> Result<(), IndexError> {
            if self.fault.snapshot_write.load(Ordering::Acquire) {
                return Err(IndexError::IOError);
            }
            self.inner.apply_mutations(id, rows).await
        }
        async fn read_snapshot(
            &self,
            id: &crate::ComponentId,
        ) -> Result<Vec<(u64, Vec<u8>)>, IndexError> {
            self.inner.read_snapshot(id).await
        }
        async fn clear(&self, id: &crate::ComponentId) -> Result<(), IndexError> {
            self.inner.clear(id).await
        }
        async fn row_count(&self, id: &crate::ComponentId) -> Result<usize, IndexError> {
            self.inner.row_count(id).await
        }
    }

    struct FaultProvider {
        inner: RocksDbIndexProvider,
        fault: Arc<Fault>,
    }

    impl FaultProvider {
        fn wrap(&self, mut indexes: CreatedIndexes) -> CreatedIndexes {
            indexes.checkpoint_store = indexes.checkpoint_store.map(|inner| {
                Arc::new(FaultStore {
                    inner,
                    fault: self.fault.clone(),
                }) as Arc<dyn CheckpointStore>
            });
            indexes.live_results_writer = indexes.live_results_writer.map(|inner| {
                Arc::new(FaultLiveResults {
                    inner,
                    fault: self.fault.clone(),
                }) as Arc<dyn LiveResultsWriter>
            });
            indexes
        }
    }

    #[async_trait]
    impl IndexBackendPlugin for FaultProvider {
        async fn create_indexes(
            &self,
            component_id: &crate::ComponentId,
        ) -> Result<CreatedIndexes, IndexError> {
            Ok(self.wrap(self.inner.create_indexes(component_id).await?))
        }
        async fn create_scoped_indexes(
            &self,
            storage_scope: &drasi_core::interface::StorageScope,
            component_id: &crate::ComponentId,
        ) -> Result<CreatedIndexes, IndexError> {
            Ok(self.wrap(
                self.inner
                    .create_scoped_indexes(storage_scope, component_id)
                    .await?,
            ))
        }
        fn is_volatile(&self) -> bool {
            self.inner.is_volatile()
        }
        fn durability(&self) -> drasi_core::interface::StorageDurability {
            self.inner.durability()
        }
        fn supports_atomic_query_output(&self) -> bool {
            self.inner.supports_atomic_query_output()
        }
    }

    #[derive(Clone, Copy, Debug)]
    enum HandoverFailure {
        /// One provider reports failure while the other never completes.
        Provider { failing: usize },
        /// One provider drops its result while the other never completes.
        DroppedProvider { failing: usize },
        /// The atomic handover commit (boundaries, marker, output) fails.
        HandoverCommit,
        /// Writing the durable snapshot rows fails.
        SnapshotWrite,
    }

    /// A failed bootstrap handover never authorizes the queued live tail,
    /// releases its source subscriptions, and leaves no partial boundary: a
    /// restart refuses the incomplete snapshot under Strict and rebuilds it
    /// from a new snapshot under AutoReset.
    #[rstest::rstest]
    #[case::provider_failure_first(HandoverFailure::Provider { failing: 0 })]
    #[case::provider_failure_second(HandoverFailure::Provider { failing: 1 })]
    #[case::dropped_provider_first(HandoverFailure::DroppedProvider { failing: 0 })]
    #[case::dropped_provider_second(HandoverFailure::DroppedProvider { failing: 1 })]
    #[case::handover_commit(HandoverFailure::HandoverCommit)]
    #[case::snapshot_write(HandoverFailure::SnapshotWrite)]
    #[tokio::test]
    #[serial]
    async fn bootstrap_handover_failure_cancels_tail_and_rolls_back(
        #[case] failure: HandoverFailure,
    ) {
        let directory = tempfile::TempDir::new().unwrap();
        let fault = Arc::new(Fault::default());
        let core = build_e2e_lib_with(
            "handover",
            Arc::new(FaultProvider {
                inner: RocksDbIndexProvider::new(directory.path(), false, false),
                fault: fault.clone(),
            }),
            None,
        )
        .await
        .unwrap();
        let completions = std::sync::Mutex::new(Vec::new());
        let sources = add_sources(&core, &["left", "right"], |_, control| {
            control.with_bootstrap();
            completions
                .lock()
                .unwrap()
                .push(control.defer_bootstrap_completion());
        })
        .await;
        let completions = completions.into_inner().unwrap();
        core.add_query(query_config(
            "handover",
            "MATCH (n:Node) RETURN n.id AS id, n.value AS value",
            &["left", "right"],
            RecoveryPolicy::Strict,
        ))
        .await
        .unwrap();
        fault.snapshot_write.store(
            matches!(failure, HandoverFailure::SnapshotWrite),
            Ordering::Release,
        );
        fault.handover_checkpoint.store(
            matches!(failure, HandoverFailure::HandoverCommit),
            Ordering::Release,
        );
        let started = {
            let core = core.clone();
            tokio::spawn(async move { core.start_query_by_name(&name("handover")).await })
        };
        // Both subscriptions are open and their providers are running.
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let mut ready = true;
                for completion in &completions {
                    ready &= completion.lock().await.is_some();
                }
                if ready {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("both bootstrap providers must start");
        // The tail is queued before the handover fails (a snapshot write
        // failure may already have closed the subscription).
        let tail = sources.controls[0].event_tx.read().await.clone().unwrap();
        let _ = tail
            .send(Arc::new(
                SourceEventWrapper::new(
                    sources.ids[0].clone(),
                    crate::channels::events::SourceEvent::Change(
                        drasi_core::models::SourceChange::Insert {
                            element: drasi_core::models::Element::Node {
                                metadata: drasi_core::models::ElementMetadata {
                                    reference: drasi_core::models::ElementReference::new(
                                        sources.ids[0].as_str(),
                                        "live-1",
                                    ),
                                    labels: Arc::from(vec![Arc::from("Node")]),
                                    effective_from: 0,
                                },
                                properties: Default::default(),
                            },
                        },
                    ),
                    chrono::Utc::now(),
                    1,
                )
                .with_source_position(Bytes::from_static(b"live-1")),
            ))
            .await;
        drop(tail);
        for (index, completion) in completions.iter().enumerate() {
            let sender = completion.lock().await.take().unwrap();
            match failure {
                HandoverFailure::Provider { failing } if index == failing => sender
                    .send(Err(anyhow::anyhow!("injected provider failure")))
                    .unwrap(),
                HandoverFailure::DroppedProvider { failing } if index == failing => drop(sender),
                // The other provider never finishes; the failure must cancel it.
                HandoverFailure::Provider { .. } | HandoverFailure::DroppedProvider { .. } => {
                    std::mem::forget(sender)
                }
                // A snapshot write failure may already have dropped the receiver.
                HandoverFailure::HandoverCommit | HandoverFailure::SnapshotWrite => {
                    let _ = sender.send(Ok(crate::bootstrap::BootstrapResult {
                        event_count: 1,
                        source_position: Some(snapshot_cursor(&sources.ids[index])),
                    }));
                }
            }
        }
        // The start request may settle before or after the handover; either
        // way the query must reach Error without waiting for the provider
        // that never finishes.
        let _ = tokio::time::timeout(std::time::Duration::from_secs(10), started)
            .await
            .expect("a failed handover must not wait for the unfinished provider");
        wait_for_status(&core, &name("handover"), ComponentStatus::Error).await;
        let query = query_handle(&core, "handover").await;
        assert!(
            query.fetch_snapshot().await.is_err(),
            "a failed bootstrap must not publish a snapshot"
        );
        assert!(
            query
                .fetch_outbox(0)
                .await
                .map(|outbox| outbox.results.is_empty())
                .unwrap_or(true),
            "the queued tail must not run after the failure"
        );
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while !subscriptions_closed(&sources.controls).await {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("source forwarders must be cancelled");
        drop(query);
        core.shutdown().await.unwrap();
        drop(core);

        for policy in [RecoveryPolicy::Strict, RecoveryPolicy::AutoReset] {
            let core = build_e2e_lib("handover", &directory, None).await.unwrap();
            let sources = add_sources(&core, &["left", "right"], |_, control| {
                control.with_bootstrap()
            })
            .await;
            core.add_query(query_config(
                "handover",
                "MATCH (n:Node) RETURN n.id AS id, n.value AS value",
                &["left", "right"],
                policy,
            ))
            .await
            .unwrap();
            let started = core.start_query_by_name(&name("handover")).await;
            for control in &sources.controls {
                assert_eq!(
                    *control.last_resume_from.read().await,
                    None,
                    "no source boundary may survive a failed handover"
                );
                assert_eq!(*control.last_resume_sequence.read().await, None);
            }
            match policy {
                RecoveryPolicy::Strict => {
                    let error = format!("{:?}", started.unwrap_err());
                    assert!(error.contains("bootstrap did not complete"), "{error}");
                }
                RecoveryPolicy::AutoReset => {
                    started.unwrap();
                    let snapshot = query_handle(&core, "handover")
                        .await
                        .fetch_snapshot()
                        .await
                        .unwrap();
                    let mut rows = snapshot.to_vec();
                    rows.sort_by_key(|row| row["id"].as_str().unwrap().to_string());
                    assert_eq!(
                        rows,
                        sources
                            .ids
                            .iter()
                            .map(|source| serde_json::json!({"id": source.as_str(), "value": 42}))
                            .collect::<Vec<_>>()
                    );
                    assert_eq!(snapshot.as_of_sequence, 0);
                }
            }
            core.shutdown().await.unwrap();
        }
    }

    #[derive(Clone, Copy, Debug)]
    enum ReplacementCause {
        ConfigChange,
        IncompleteBootstrap,
        SourceGap,
    }

    /// A replacement bootstrap (after a configuration change, an interrupted
    /// bootstrap, or a source history gap) that is itself interrupted leaves
    /// an incomplete durable bootstrap. Strict refuses it; AutoReset discards
    /// the partially populated indexes and rebuilds from a complete snapshot.
    #[rstest::rstest]
    #[case::config_change_strict(ReplacementCause::ConfigChange, RecoveryPolicy::Strict)]
    #[case::config_change_auto_reset(ReplacementCause::ConfigChange, RecoveryPolicy::AutoReset)]
    #[case::incomplete_strict(ReplacementCause::IncompleteBootstrap, RecoveryPolicy::Strict)]
    #[case::incomplete_auto_reset(ReplacementCause::IncompleteBootstrap, RecoveryPolicy::AutoReset)]
    #[case::source_gap_strict(ReplacementCause::SourceGap, RecoveryPolicy::Strict)]
    #[case::source_gap_auto_reset(ReplacementCause::SourceGap, RecoveryPolicy::AutoReset)]
    #[tokio::test]
    #[serial]
    async fn interrupted_replacement_bootstrap_obeys_recovery_policy(
        #[case] cause: ReplacementCause,
        #[case] policy: RecoveryPolicy,
    ) {
        let directory = tempfile::TempDir::new().unwrap();
        let text = |original: bool| {
            if original && matches!(cause, ReplacementCause::ConfigChange) {
                "MATCH (n:Node) RETURN count(n) AS previous_count"
            } else {
                "MATCH (n:Node) RETURN count(n) AS count"
            }
        };
        let mut previous_generation = 0;
        for original in [true, false] {
            let core = build_e2e_lib("replacement-instance", &directory, None)
                .await
                .unwrap();
            let incomplete = !original || matches!(cause, ReplacementCause::IncompleteBootstrap);
            let mut completions = Vec::new();
            let sources = add_sources(&core, &["source"], |_, control| {
                control.with_bootstrap();
                *control.bootstrap_node_id.lock().unwrap() =
                    Some(if original { "original" } else { "interrupted" });
                if !original && matches!(cause, ReplacementCause::SourceGap) {
                    control.set_fail_count(1);
                }
            })
            .await;
            if incomplete {
                completions.push(sources.controls[0].defer_bootstrap_completion());
            }
            core.add_query(query_config(
                "replacement",
                text(original),
                &["source"],
                RecoveryPolicy::AutoReset,
            ))
            .await
            .unwrap();
            if incomplete {
                let start = {
                    let core = core.clone();
                    tokio::spawn(
                        async move { core.start_query_by_name(&name("replacement")).await },
                    )
                };
                // Wait until the snapshot row has been processed and the
                // provider is still incomplete: the durable state of a crash
                // after an indexed bootstrap row.
                tokio::time::timeout(std::time::Duration::from_secs(5), async {
                    while completions[0].lock().await.is_none() {
                        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    }
                })
                .await
                .unwrap();
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                core.shutdown().await.unwrap();
                let _ = start.await;
            } else {
                core.start_query_by_name(&name("replacement"))
                    .await
                    .unwrap();
                let snapshot = query_handle(&core, "replacement")
                    .await
                    .fetch_snapshot()
                    .await
                    .unwrap();
                previous_generation = snapshot.output_generation;
                core.shutdown().await.unwrap();
            }
        }

        let core = build_e2e_lib("replacement-instance", &directory, None)
            .await
            .unwrap();
        let sources = add_sources(&core, &["source"], |_, control| {
            control.with_bootstrap();
            *control.bootstrap_node_id.lock().unwrap() = Some("complete");
        })
        .await;
        core.add_query(query_config(
            "replacement",
            text(false),
            &["source"],
            policy,
        ))
        .await
        .unwrap();
        let started = core.start_query_by_name(&name("replacement")).await;
        match policy {
            RecoveryPolicy::Strict => {
                let error = format!("{:?}", started.unwrap_err());
                assert!(error.contains("bootstrap did not complete"), "{error}");
            }
            RecoveryPolicy::AutoReset => {
                started.unwrap();
                let snapshot = query_handle(&core, "replacement")
                    .await
                    .fetch_snapshot()
                    .await
                    .unwrap();
                assert_eq!(
                    snapshot.to_vec(),
                    vec![serde_json::json!({"count": 1})],
                    "replacement must discard partially populated indexes"
                );
                assert!(snapshot.output_generation > previous_generation);
                assert_eq!(*sources.controls[0].last_resume_from.read().await, None);
            }
        }
        core.shutdown().await.unwrap();
    }
}

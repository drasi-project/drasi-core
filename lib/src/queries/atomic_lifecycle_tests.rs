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

use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

use anyhow::Result;
use async_trait::async_trait;
use bytes::Bytes;
use drasi_core::{
    interface::{
        CheckpointStore, CreatedIndexes, IndexBackendPlugin, IndexError, LiveResultsWriter,
        OutboxWriter, RowMutation, SourceCheckpoint,
    },
    middleware::MiddlewareTypeRegistry,
    models::{Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange},
};
use drasi_index_rocksdb::RocksDbIndexProvider;
use serde_json::json;
use tokio::sync::{oneshot, Mutex, Notify, RwLock};

use super::{
    manager::{DrasiQuery, Query, QueryManager},
    query_composite_host::{
        read_legacy_pending_state, LegacyPendingState, QueryIngressFence, QueryProcessingObserver,
        LEGACY_OUTPUT_PENDING_MARKER_V1, QUERY_BOOTSTRAP_MARKER_V1,
    },
};
use crate::{
    channels::{
        ChangeReceiver, ComponentStatus, DispatchMode, QueryResult, ResultDiff, SourceEvent,
        SourceEventWrapper, SubscriptionResponse,
    },
    component_graph::ComponentGraph,
    config::{QueryConfig, QueryLanguage, SourceSubscriptionConfig},
    indexes::{IndexFactory, StorageBackendRef},
    managers::get_or_init_global_registry,
    sources::{Source, SourceBase, SourceBaseParams, SourceManager},
};

const SOURCE_ID: &str = "atomic-lifecycle-source";
const BLOCKING_SOURCE_ID: &str = "atomic-blocking-source";
const BACKEND_NAME: &str = "atomic-lifecycle-rocks";

struct BoundedLifecycleSource {
    base: SourceBase,
    removed_position_handles: Arc<AtomicUsize>,
}

impl Clone for BoundedLifecycleSource {
    fn clone(&self) -> Self {
        Self {
            base: self.base.clone_shared(),
            removed_position_handles: self.removed_position_handles.clone(),
        }
    }
}

impl BoundedLifecycleSource {
    fn new() -> Self {
        Self {
            base: SourceBase::new(
                SourceBaseParams::new(SOURCE_ID)
                    .with_dispatch_mode(DispatchMode::Channel)
                    .with_dispatch_buffer_capacity(1)
                    .with_auto_start(false),
            )
            .expect("create bounded lifecycle source"),
            removed_position_handles: Arc::new(AtomicUsize::new(0)),
        }
    }

    async fn inject(&self, sequence: u64, name: &str) -> Result<()> {
        let change = SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new(SOURCE_ID, &format!("person-{sequence}")),
                    labels: vec![Arc::from("Person")].into(),
                    effective_from: sequence.saturating_mul(1_000),
                },
                properties: ElementPropertyMap::from(json!({ "name": name })),
            },
        };
        let timestamp = chrono::DateTime::from_timestamp_millis(change.get_realtime() as i64)
            .expect("valid lifecycle event timestamp");
        let mut event = SourceEventWrapper::with_sequence(
            SOURCE_ID.to_string(),
            SourceEvent::Change(change),
            timestamp,
            sequence,
            None,
        );
        event.set_source_position(Bytes::from(format!("position-{sequence}")));
        self.base.dispatch_event(event).await
    }

    fn removed_position_handles(&self) -> usize {
        self.removed_position_handles.load(Ordering::Acquire)
    }
}

#[async_trait]
impl Source for BoundedLifecycleSource {
    fn id(&self) -> &str {
        SOURCE_ID
    }

    fn type_name(&self) -> &str {
        "atomic-lifecycle"
    }

    fn properties(&self) -> HashMap<String, serde_json::Value> {
        HashMap::new()
    }

    fn dispatch_mode(&self) -> DispatchMode {
        DispatchMode::Channel
    }

    fn auto_start(&self) -> bool {
        false
    }

    async fn start(&self) -> Result<()> {
        self.base
            .set_status(ComponentStatus::Running, Some("Running".to_string()))
            .await;
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        self.base.stop_common().await
    }

    async fn status(&self) -> ComponentStatus {
        self.base.status_handle().get_status().await
    }

    async fn subscribe(
        &self,
        settings: crate::config::SourceSubscriptionSettings,
    ) -> Result<SubscriptionResponse> {
        self.base
            .subscribe_with_bootstrap(&settings, self.type_name())
            .await
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn initialize(&self, context: crate::context::SourceRuntimeContext) {
        self.base.initialize(context).await;
    }

    async fn remove_position_handle(&self, query_id: &str) {
        self.removed_position_handles.fetch_add(1, Ordering::AcqRel);
        self.base.remove_position_handle(query_id).await;
    }
}

struct BlockingSubscribeSource {
    base: SourceBase,
    entered_tx: Arc<Mutex<Option<oneshot::Sender<()>>>>,
    release: Arc<Notify>,
}

impl Clone for BlockingSubscribeSource {
    fn clone(&self) -> Self {
        Self {
            base: self.base.clone_shared(),
            entered_tx: self.entered_tx.clone(),
            release: self.release.clone(),
        }
    }
}

impl BlockingSubscribeSource {
    fn new() -> (Self, oneshot::Receiver<()>) {
        let (entered_tx, entered_rx) = oneshot::channel();
        (
            Self {
                base: SourceBase::new(
                    SourceBaseParams::new(BLOCKING_SOURCE_ID)
                        .with_dispatch_mode(DispatchMode::Channel)
                        .with_dispatch_buffer_capacity(1)
                        .with_auto_start(false),
                )
                .expect("create blocking lifecycle source"),
                entered_tx: Arc::new(Mutex::new(Some(entered_tx))),
                release: Arc::new(Notify::new()),
            },
            entered_rx,
        )
    }
}

#[async_trait]
impl Source for BlockingSubscribeSource {
    fn id(&self) -> &str {
        BLOCKING_SOURCE_ID
    }

    fn type_name(&self) -> &str {
        "atomic-blocking"
    }

    fn properties(&self) -> HashMap<String, serde_json::Value> {
        HashMap::new()
    }

    fn dispatch_mode(&self) -> DispatchMode {
        DispatchMode::Channel
    }

    fn auto_start(&self) -> bool {
        false
    }

    async fn start(&self) -> Result<()> {
        self.base
            .set_status(ComponentStatus::Running, Some("Running".to_string()))
            .await;
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        self.base.stop_common().await
    }

    async fn status(&self) -> ComponentStatus {
        self.base.status_handle().get_status().await
    }

    async fn subscribe(
        &self,
        settings: crate::config::SourceSubscriptionSettings,
    ) -> Result<SubscriptionResponse> {
        if let Some(entered_tx) = self.entered_tx.lock().await.take() {
            let _ = entered_tx.send(());
        }
        self.release.notified().await;
        self.base
            .subscribe_with_bootstrap(&settings, self.type_name())
            .await
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn initialize(&self, context: crate::context::SourceRuntimeContext) {
        self.base.initialize(context).await;
    }

    async fn remove_position_handle(&self, query_id: &str) {
        self.base.remove_position_handle(query_id).await;
    }
}

struct FailBeforeCommit;

#[async_trait]
impl QueryProcessingObserver for FailBeforeCommit {
    async fn after_output_staged(&self) -> std::result::Result<(), IndexError> {
        Err(IndexError::CorruptedData)
    }
}

struct FailAfterCommit;

#[async_trait]
impl QueryProcessingObserver for FailAfterCommit {
    async fn after_commit_before_publish(&self) -> Result<()> {
        Err(anyhow::anyhow!(
            "injected manager failure after commit before publication"
        ))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LegacyFailureStage {
    Outbox,
    LiveResults,
    ResultSequence,
    MarkerStage,
    MarkerClear,
}

struct LegacyFailureProvider {
    inner: RocksDbIndexProvider,
    stage: LegacyFailureStage,
    failed: Arc<AtomicBool>,
}

#[async_trait]
impl IndexBackendPlugin for LegacyFailureProvider {
    async fn create_indexes(&self, query_id: &str) -> Result<CreatedIndexes, IndexError> {
        let mut created = self.inner.create_indexes(query_id).await?;
        let outbox = created.outbox_writer.take().expect("RocksDB outbox");
        created.outbox_writer = Some(Arc::new(LegacyFailureOutbox {
            inner: outbox,
            stage: self.stage,
            failed: self.failed.clone(),
        }));
        let live_results = created
            .live_results_writer
            .take()
            .expect("RocksDB live-results writer");
        created.live_results_writer = Some(Arc::new(LegacyFailureLiveResults {
            inner: live_results,
            stage: self.stage,
            failed: self.failed.clone(),
        }));
        let checkpoints = created
            .checkpoint_store
            .take()
            .expect("RocksDB checkpoint store");
        created.checkpoint_store = Some(Arc::new(LegacyFailureCheckpointStore {
            inner: checkpoints,
            stage: self.stage,
            failed: self.failed.clone(),
        }));
        Ok(created)
    }

    fn is_volatile(&self) -> bool {
        false
    }
}

struct LegacyFailureOutbox {
    inner: Arc<dyn OutboxWriter>,
    stage: LegacyFailureStage,
    failed: Arc<AtomicBool>,
}

#[async_trait]
impl OutboxWriter for LegacyFailureOutbox {
    fn transaction_domain(&self) -> Option<drasi_core::interface::TransactionDomain> {
        None
    }

    async fn append(&self, query_id: &str, sequence: u64, data: &[u8]) -> Result<(), IndexError> {
        if self.stage == LegacyFailureStage::Outbox && !self.failed.swap(true, Ordering::AcqRel) {
            return Err(IndexError::other(std::io::Error::other(
                "injected persistent Legacy outbox failure",
            )));
        }
        self.inner.append(query_id, sequence, data).await
    }

    async fn read_from(
        &self,
        query_id: &str,
        after_sequence: u64,
    ) -> Result<Vec<(u64, Vec<u8>)>, IndexError> {
        self.inner.read_from(query_id, after_sequence).await
    }

    async fn read_latest_sequence(&self, query_id: &str) -> Result<Option<u64>, IndexError> {
        self.inner.read_latest_sequence(query_id).await
    }

    async fn clear(&self, query_id: &str) -> Result<(), IndexError> {
        self.inner.clear(query_id).await
    }

    async fn trim_to_capacity(&self, query_id: &str, capacity: usize) -> Result<usize, IndexError> {
        self.inner.trim_to_capacity(query_id, capacity).await
    }
}

struct LegacyFailureLiveResults {
    inner: Arc<dyn LiveResultsWriter>,
    stage: LegacyFailureStage,
    failed: Arc<AtomicBool>,
}

#[async_trait]
impl LiveResultsWriter for LegacyFailureLiveResults {
    fn transaction_domain(&self) -> Option<drasi_core::interface::TransactionDomain> {
        self.inner.transaction_domain()
    }

    async fn apply_mutations(
        &self,
        query_id: &str,
        mutations: &[RowMutation<'_>],
    ) -> Result<(), IndexError> {
        if self.stage == LegacyFailureStage::LiveResults
            && !self.failed.swap(true, Ordering::AcqRel)
        {
            return Err(IndexError::other(std::io::Error::other(
                "injected persistent Legacy live-result failure",
            )));
        }
        self.inner.apply_mutations(query_id, mutations).await
    }

    async fn read_snapshot(&self, query_id: &str) -> Result<Vec<(u64, Vec<u8>)>, IndexError> {
        self.inner.read_snapshot(query_id).await
    }

    async fn clear(&self, query_id: &str) -> Result<(), IndexError> {
        self.inner.clear(query_id).await
    }

    async fn row_count(&self, query_id: &str) -> Result<usize, IndexError> {
        self.inner.row_count(query_id).await
    }
}

struct LegacyFailureCheckpointStore {
    inner: Arc<dyn CheckpointStore>,
    stage: LegacyFailureStage,
    failed: Arc<AtomicBool>,
}

#[async_trait]
impl CheckpointStore for LegacyFailureCheckpointStore {
    fn transaction_domain(&self) -> Option<drasi_core::interface::TransactionDomain> {
        self.inner.transaction_domain()
    }

    fn is_persistent(&self) -> bool {
        true
    }

    async fn stage_checkpoint(
        &self,
        source_id: &str,
        sequence: u64,
        source_position: Option<&Bytes>,
    ) -> Result<(), IndexError> {
        if self.stage == LegacyFailureStage::MarkerStage
            && source_id == LEGACY_OUTPUT_PENDING_MARKER_V1
            && sequence == 1
            && !self.failed.swap(true, Ordering::AcqRel)
        {
            return Err(IndexError::other(std::io::Error::other(
                "injected Legacy pending-marker stage failure",
            )));
        }
        if self.stage == LegacyFailureStage::MarkerClear
            && source_id == LEGACY_OUTPUT_PENDING_MARKER_V1
            && sequence == 0
            && !self.failed.swap(true, Ordering::AcqRel)
        {
            return Err(IndexError::other(std::io::Error::other(
                "injected Legacy pending-marker clear failure",
            )));
        }
        self.inner
            .stage_checkpoint(source_id, sequence, source_position)
            .await
    }

    async fn read_checkpoint(
        &self,
        source_id: &str,
    ) -> Result<Option<SourceCheckpoint>, IndexError> {
        self.inner.read_checkpoint(source_id).await
    }

    async fn read_all_checkpoints(&self) -> Result<HashMap<String, SourceCheckpoint>, IndexError> {
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

    async fn write_result_sequence(&self, query_id: &str, sequence: u64) -> Result<(), IndexError> {
        if self.stage == LegacyFailureStage::ResultSequence
            && sequence > 0
            && !self.failed.swap(true, Ordering::AcqRel)
        {
            return Err(IndexError::other(std::io::Error::other(
                "injected persistent Legacy result-sequence failure",
            )));
        }
        self.inner.write_result_sequence(query_id, sequence).await
    }

    async fn read_result_sequence(&self, query_id: &str) -> Result<Option<u64>, IndexError> {
        self.inner.read_result_sequence(query_id).await
    }
}

struct LifecycleHarness {
    manager: Arc<QueryManager>,
    source_manager: Arc<SourceManager>,
    graph: Arc<RwLock<ComponentGraph>>,
    source: BoundedLifecycleSource,
}

impl LifecycleHarness {
    async fn new(path: &std::path::Path) -> Self {
        Self::new_with_provider(Arc::new(RocksDbIndexProvider::new(path, true, false))).await
    }

    async fn new_with_provider(provider: Arc<dyn IndexBackendPlugin>) -> Self {
        let log_registry = get_or_init_global_registry();
        let (graph, mut update_rx) = ComponentGraph::new("atomic-lifecycle");
        let update_tx = graph.update_sender();
        let graph = Arc::new(RwLock::new(graph));
        let update_graph = graph.clone();
        tokio::spawn(async move {
            while let Some(update) = update_rx.recv().await {
                update_graph.write().await.apply_update(update);
            }
        });

        let source_manager = Arc::new(SourceManager::new(
            "atomic-lifecycle",
            log_registry.clone(),
            graph.clone(),
            update_tx.clone(),
        ));
        let index_factory = Arc::new(IndexFactory::new(
            vec![],
            HashMap::from([(BACKEND_NAME.to_string(), provider)]),
        ));
        let manager = Arc::new(QueryManager::new(
            "atomic-lifecycle",
            source_manager.clone(),
            index_factory,
            Arc::new(MiddlewareTypeRegistry::new()),
            log_registry,
            graph.clone(),
            update_tx,
            None,
        ));
        let source = BoundedLifecycleSource::new();

        graph
            .write()
            .await
            .register_source(SOURCE_ID, HashMap::new())
            .expect("register lifecycle source");
        source_manager
            .provision_source(source.clone())
            .await
            .expect("provision lifecycle source");
        source_manager
            .start_source(SOURCE_ID.to_string())
            .await
            .expect("start lifecycle source");

        Self {
            manager,
            source_manager,
            graph,
            source,
        }
    }

    async fn add_query(&self, query_id: &str) -> Arc<dyn Query> {
        self.add_query_config(query_config(query_id)).await
    }

    async fn add_query_config(&self, config: QueryConfig) -> Arc<dyn Query> {
        let query_id = config.id.clone();
        let source_ids: Vec<String> = config
            .sources
            .iter()
            .map(|source| source.source_id.clone())
            .collect();
        self.graph
            .write()
            .await
            .register_query(&query_id, HashMap::new(), &source_ids)
            .expect("register lifecycle query");
        self.manager
            .provision_query(config)
            .await
            .expect("provision lifecycle query");
        self.manager
            .get_query_instance(&query_id)
            .await
            .expect("get lifecycle query")
    }

    async fn start_query(&self, query_id: &str) {
        self.manager
            .start_query(query_id.to_string())
            .await
            .expect("start lifecycle query");
        wait_for_status(&self.manager, query_id, ComponentStatus::Running).await;
    }
}

fn query_config(query_id: &str) -> QueryConfig {
    query_config_with_sources(query_id, &[SOURCE_ID])
}

fn query_config_with_policy(
    query_id: &str,
    recovery_policy: crate::recovery::RecoveryPolicy,
) -> QueryConfig {
    let mut config = query_config(query_id);
    config.recovery_policy = Some(recovery_policy);
    config
}

fn query_config_with_sources(query_id: &str, source_ids: &[&str]) -> QueryConfig {
    QueryConfig {
        id: query_id.to_string(),
        query: "MATCH (n:Person) RETURN n.name AS name".to_string(),
        query_language: QueryLanguage::Cypher,
        middleware: vec![],
        sources: source_ids
            .iter()
            .map(|source_id| SourceSubscriptionConfig {
                source_id: (*source_id).to_string(),
                nodes: vec![],
                relations: vec![],
                pipeline: vec![],
            })
            .collect(),
        auto_start: false,
        joins: None,
        enable_bootstrap: false,
        bootstrap_buffer_size: 8,
        priority_queue_capacity: Some(1),
        dispatch_buffer_capacity: Some(8),
        dispatch_mode: Some(DispatchMode::Channel),
        storage_backend: Some(StorageBackendRef::Named(BACKEND_NAME.to_string())),
        recovery_policy: None,
        outbox_capacity: 8,
        bootstrap_timeout_secs: 2,
    }
}

fn concrete_query(query: &Arc<dyn Query>) -> &DrasiQuery {
    query
        .as_any()
        .downcast_ref::<DrasiQuery>()
        .expect("DrasiQuery runtime")
}

async fn wait_for_status(manager: &QueryManager, query_id: &str, expected: ComponentStatus) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if manager
                .get_query_status(query_id.to_string())
                .await
                .expect("read query status")
                == expected
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("query status transition timed out");
}

async fn receive_result(receiver: &mut Box<dyn ChangeReceiver<QueryResult>>) -> Arc<QueryResult> {
    tokio::time::timeout(Duration::from_secs(5), receiver.recv())
        .await
        .expect("query result timed out")
        .expect("query result channel closed")
}

#[tokio::test]
async fn atomic_fence_unblocks_shared_source_and_error_stop_allows_replay() {
    let temp_dir = tempfile::TempDir::new().expect("create temp directory");
    let harness = LifecycleHarness::new(temp_dir.path()).await;
    let failed = harness.add_query("atomic-failed").await;
    let healthy = harness.add_query("atomic-healthy").await;
    concrete_query(&failed)
        .set_processing_observer(Some(Arc::new(FailBeforeCommit)))
        .await;
    let mut failed_results = failed
        .subscribe("failed-test".to_string())
        .await
        .expect("subscribe failed query output")
        .receiver;
    let mut healthy_results = healthy
        .subscribe("healthy-test".to_string())
        .await
        .expect("subscribe healthy query output")
        .receiver;

    harness.start_query("atomic-failed").await;
    harness.start_query("atomic-healthy").await;
    harness
        .source
        .inject(1, "first")
        .await
        .expect("inject failing event");
    wait_for_status(&harness.manager, "atomic-failed", ComponentStatus::Error).await;
    let healthy_first = receive_result(&mut healthy_results).await;
    assert_eq!(healthy_first.sequence, 1);
    assert_eq!(failed.subscription_count().await, 0);
    assert!(!concrete_query(&failed).publication_recovery_required());

    for sequence in 2..=6 {
        tokio::time::timeout(
            Duration::from_secs(2),
            harness
                .source
                .inject(sequence, &format!("healthy-{sequence}")),
        )
        .await
        .expect("failed query must not backpressure the shared source")
        .expect("inject event after atomic fence");
        let healthy_result = receive_result(&mut healthy_results).await;
        assert_eq!(healthy_result.sequence, sequence);
    }

    harness
        .manager
        .stop_query("atomic-failed".to_string())
        .await
        .expect("public stop from Error");
    wait_for_status(&harness.manager, "atomic-failed", ComponentStatus::Stopped).await;
    assert_eq!(failed.subscription_count().await, 0);
    assert!(harness.source.removed_position_handles() >= 1);

    concrete_query(&failed).set_processing_observer(None).await;
    let restarted_subscription = failed
        .subscribe("failed-restart-test".to_string())
        .await
        .expect("subscribe restarted failed query")
        .receiver;
    failed_results = restarted_subscription;
    harness.start_query("atomic-failed").await;
    assert_eq!(failed.subscription_count().await, 2);

    harness
        .source
        .inject(1, "replayed")
        .await
        .expect("replay pre-commit failure");
    let replayed = receive_result(&mut failed_results).await;
    assert_eq!(replayed.sequence, 1);
    assert_eq!(replayed.results.len(), 1);

    harness
        .manager
        .stop_query("atomic-failed".to_string())
        .await
        .expect("stop restarted failed query");
    harness
        .manager
        .stop_query("atomic-healthy".to_string())
        .await
        .expect("stop healthy query");
    harness
        .source_manager
        .stop_source(SOURCE_ID.to_string())
        .await
        .expect("stop lifecycle source");
}

#[tokio::test]
async fn start_reaps_existing_forwarders_before_installing_replacements() {
    let temp_dir = tempfile::TempDir::new().expect("create temp directory");
    let harness = LifecycleHarness::new(temp_dir.path()).await;
    let query = harness.add_query("atomic-restart").await;
    let mut results = query
        .subscribe("restart-test".to_string())
        .await
        .expect("subscribe restart query")
        .receiver;
    harness.start_query("atomic-restart").await;
    assert_eq!(query.subscription_count().await, 2);
    query
        .start()
        .await
        .expect("same-state runtime start should be idempotent");
    assert_eq!(
        query.subscription_count().await,
        2,
        "same-state start must not replace active forwarders"
    );

    concrete_query(&query)
        .set_local_status_for_test(ComponentStatus::Error)
        .await;
    wait_for_status(&harness.manager, "atomic-restart", ComponentStatus::Error).await;
    harness
        .manager
        .start_query("atomic-restart".to_string())
        .await
        .expect("retry query from Error");
    wait_for_status(&harness.manager, "atomic-restart", ComponentStatus::Running).await;
    assert_eq!(
        query.subscription_count().await,
        2,
        "start must reap the old source/future forwarders before replacement"
    );
    assert!(harness.source.removed_position_handles() >= 1);

    harness
        .source
        .inject(1, "after-restart")
        .await
        .expect("inject after restart");
    assert_eq!(receive_result(&mut results).await.sequence, 1);

    harness
        .manager
        .stop_query("atomic-restart".to_string())
        .await
        .expect("stop restart query");
    harness
        .source_manager
        .stop_source(SOURCE_ID.to_string())
        .await
        .expect("stop lifecycle source");
}

#[tokio::test]
async fn post_commit_fence_reconciles_and_replays_without_reusing_sequence() {
    let temp_dir = tempfile::TempDir::new().expect("create temp directory");
    let harness = LifecycleHarness::new(temp_dir.path()).await;
    let query = harness.add_query("atomic-post-commit").await;
    let mut results = query
        .subscribe("post-commit-recovery".to_string())
        .await
        .expect("subscribe before post-commit failure")
        .receiver;
    concrete_query(&query)
        .set_processing_observer(Some(Arc::new(FailAfterCommit)))
        .await;
    harness.start_query("atomic-post-commit").await;

    harness
        .source
        .inject(1, "committed")
        .await
        .expect("inject post-commit failure");
    wait_for_status(
        &harness.manager,
        "atomic-post-commit",
        ComponentStatus::Error,
    )
    .await;
    let concrete = concrete_query(&query);
    assert!(concrete.publication_recovery_required());
    assert_eq!(concrete.output_sequence_for_test().await, 0);
    assert_eq!(query.subscription_count().await, 0);
    let outbox = concrete
        .get_outbox_writer()
        .await
        .expect("atomic outbox writer");
    let durable = outbox
        .read_from("atomic-post-commit", 0)
        .await
        .expect("read committed outbox");
    assert_eq!(durable.len(), 1);
    assert_eq!(durable[0].0, 1);
    drop(outbox);

    harness
        .manager
        .stop_query("atomic-post-commit".to_string())
        .await
        .expect("stop post-commit query from Error");
    wait_for_status(
        &harness.manager,
        "atomic-post-commit",
        ComponentStatus::Stopped,
    )
    .await;
    concrete.set_processing_observer(None).await;

    harness
        .manager
        .start_query("atomic-post-commit".to_string())
        .await
        .expect("post-commit restart should reconcile durable output");
    wait_for_status(
        &harness.manager,
        "atomic-post-commit",
        ComponentStatus::Running,
    )
    .await;
    let recovered = receive_result(&mut results).await;
    assert_eq!(recovered.sequence, 1);
    assert_eq!(
        rmp_serde::to_vec(recovered.as_ref()).expect("serialize recovered result"),
        durable[0].1,
        "recovered and persisted QueryResult bytes must remain identical"
    );
    assert!(!concrete.publication_recovery_required());
    assert_eq!(concrete.output_sequence_for_test().await, 1);

    harness
        .source
        .inject(2, "after-recovery")
        .await
        .expect("inject after post-commit recovery");
    assert_eq!(receive_result(&mut results).await.sequence, 2);
    assert_eq!(
        concrete
            .get_outbox_writer()
            .await
            .expect("reopened atomic outbox writer")
            .read_from("atomic-post-commit", 0)
            .await
            .expect("reread committed outbox")
            .into_iter()
            .map(|(sequence, _)| sequence)
            .collect::<Vec<_>>(),
        vec![1, 2],
        "restart must continue after the durable sequence"
    );

    let first_stop = harness.manager.stop_query("atomic-post-commit".to_string());
    let second_stop = harness.manager.stop_query("atomic-post-commit".to_string());
    let (first_stop, second_stop) = tokio::join!(first_stop, second_stop);
    first_stop.expect("first repeated stop");
    second_stop.expect("second repeated stop");
    wait_for_status(
        &harness.manager,
        "atomic-post-commit",
        ComponentStatus::Stopped,
    )
    .await;
    assert_eq!(query.subscription_count().await, 0);

    harness
        .source_manager
        .stop_source(SOURCE_ID.to_string())
        .await
        .expect("stop lifecycle source");
}

#[tokio::test]
async fn strict_recovery_rejects_missing_durable_outbox() {
    let temp_dir = tempfile::TempDir::new().expect("create temp directory");
    let harness = LifecycleHarness::new(temp_dir.path()).await;
    let query = harness
        .add_query_config(query_config_with_policy(
            "atomic-strict-output",
            crate::recovery::RecoveryPolicy::Strict,
        ))
        .await;
    let mut results = query
        .subscribe("strict-output".to_string())
        .await
        .expect("subscribe strict output")
        .receiver;
    harness.start_query("atomic-strict-output").await;
    harness
        .source
        .inject(1, "durable")
        .await
        .expect("inject durable result");
    assert_eq!(receive_result(&mut results).await.sequence, 1);
    harness
        .manager
        .stop_query("atomic-strict-output".to_string())
        .await
        .expect("stop strict query");

    let outbox = concrete_query(&query)
        .get_outbox_writer()
        .await
        .expect("strict outbox");
    outbox
        .clear("atomic-strict-output")
        .await
        .expect("corrupt durable outbox");
    drop(outbox);

    let error = harness
        .manager
        .start_query("atomic-strict-output".to_string())
        .await
        .expect_err("Strict recovery must reject an incomplete durable bundle");
    assert!(
        format!("{error:#}").contains("outbox"),
        "unexpected reconciliation error: {error:#}"
    );
    wait_for_status(
        &harness.manager,
        "atomic-strict-output",
        ComponentStatus::Error,
    )
    .await;
    assert_eq!(query.subscription_count().await, 0);

    harness
        .manager
        .stop_query("atomic-strict-output".to_string())
        .await
        .expect("stop strict query from Error");
    harness
        .source_manager
        .stop_source(SOURCE_ID.to_string())
        .await
        .expect("stop lifecycle source");
}

#[tokio::test]
async fn auto_reset_clears_inconsistent_durable_output_before_live_processing() {
    let temp_dir = tempfile::TempDir::new().expect("create temp directory");
    let harness = LifecycleHarness::new(temp_dir.path()).await;
    let query = harness
        .add_query_config(query_config_with_policy(
            "atomic-reset-output",
            crate::recovery::RecoveryPolicy::AutoReset,
        ))
        .await;
    let mut results = query
        .subscribe("reset-output".to_string())
        .await
        .expect("subscribe reset output")
        .receiver;
    harness.start_query("atomic-reset-output").await;
    harness
        .source
        .inject(1, "before-reset")
        .await
        .expect("inject result before reset");
    assert_eq!(receive_result(&mut results).await.sequence, 1);
    harness
        .manager
        .stop_query("atomic-reset-output".to_string())
        .await
        .expect("stop reset query");

    let outbox = concrete_query(&query)
        .get_outbox_writer()
        .await
        .expect("reset outbox");
    outbox
        .clear("atomic-reset-output")
        .await
        .expect("corrupt durable outbox");
    drop(outbox);

    harness
        .manager
        .start_query("atomic-reset-output".to_string())
        .await
        .expect("AutoReset should recover an incomplete durable bundle");
    wait_for_status(
        &harness.manager,
        "atomic-reset-output",
        ComponentStatus::Running,
    )
    .await;
    assert_eq!(concrete_query(&query).output_sequence_for_test().await, 1);

    harness
        .source
        .inject(2, "after-reset")
        .await
        .expect("inject after reset");
    assert_eq!(receive_result(&mut results).await.sequence, 2);

    harness
        .manager
        .stop_query("atomic-reset-output".to_string())
        .await
        .expect("stop reset query");
    harness
        .source_manager
        .stop_source(SOURCE_ID.to_string())
        .await
        .expect("stop lifecycle source");
}

#[tokio::test]
async fn persistent_legacy_write_failures_make_strict_restart_fail_closed() {
    for stage in [
        LegacyFailureStage::Outbox,
        LegacyFailureStage::LiveResults,
        LegacyFailureStage::ResultSequence,
    ] {
        let temp_dir = tempfile::TempDir::new().expect("create temp directory");
        let provider: Arc<dyn IndexBackendPlugin> = Arc::new(LegacyFailureProvider {
            inner: RocksDbIndexProvider::new(temp_dir.path(), true, false),
            stage,
            failed: Arc::new(AtomicBool::new(false)),
        });
        let harness = LifecycleHarness::new_with_provider(provider).await;
        let query_id = format!("legacy-strict-{stage:?}");
        let query = harness
            .add_query_config(query_config_with_policy(
                &query_id,
                crate::recovery::RecoveryPolicy::Strict,
            ))
            .await;
        let mut results = query
            .subscribe(format!("{query_id}-results"))
            .await
            .expect("subscribe Legacy output")
            .receiver;
        harness.start_query(&query_id).await;

        harness
            .source
            .inject(1, "failed-output")
            .await
            .expect("inject Legacy output failure");
        wait_for_status(&harness.manager, &query_id, ComponentStatus::Error).await;
        assert!(
            tokio::time::timeout(Duration::from_millis(100), results.recv())
                .await
                .is_err(),
            "{stage:?}: failed persistent output was delivered"
        );
        let concrete = concrete_query(&query);
        assert!(concrete.publication_recovery_required());
        assert_eq!(
            concrete
                .get_checkpoint_store()
                .await
                .expect("Legacy checkpoint store")
                .read_checkpoint(SOURCE_ID)
                .await
                .expect("read causative Legacy checkpoint")
                .expect("causative Legacy checkpoint")
                .sequence,
            1
        );

        harness
            .manager
            .stop_query(query_id.clone())
            .await
            .expect("stop failed Legacy query");
        wait_for_status(&harness.manager, &query_id, ComponentStatus::Stopped).await;
        let error = harness
            .manager
            .start_query(query_id.clone())
            .await
            .expect_err("Strict restart must reject partial Legacy output");
        assert!(
            format!("{error:#}").contains("unfinished persistent Legacy publication"),
            "{stage:?}: unexpected Strict restart error: {error:#}"
        );
        assert!(concrete.publication_recovery_required());

        harness
            .manager
            .stop_query(query_id)
            .await
            .expect("stop Strict query from Error");
        harness
            .source_manager
            .stop_source(SOURCE_ID.to_string())
            .await
            .expect("stop lifecycle source");
    }
}

#[tokio::test]
async fn persistent_legacy_write_failures_auto_reset_and_resume() {
    for stage in [
        LegacyFailureStage::Outbox,
        LegacyFailureStage::LiveResults,
        LegacyFailureStage::ResultSequence,
    ] {
        let temp_dir = tempfile::TempDir::new().expect("create temp directory");
        let provider: Arc<dyn IndexBackendPlugin> = Arc::new(LegacyFailureProvider {
            inner: RocksDbIndexProvider::new(temp_dir.path(), true, false),
            stage,
            failed: Arc::new(AtomicBool::new(false)),
        });
        let harness = LifecycleHarness::new_with_provider(provider).await;
        let query_id = format!("legacy-reset-{stage:?}");
        let query = harness
            .add_query_config(query_config_with_policy(
                &query_id,
                crate::recovery::RecoveryPolicy::AutoReset,
            ))
            .await;
        let mut results = query
            .subscribe(format!("{query_id}-results"))
            .await
            .expect("subscribe Legacy reset output")
            .receiver;
        harness.start_query(&query_id).await;
        harness
            .source
            .inject(1, "failed-output")
            .await
            .expect("inject Legacy output failure");
        wait_for_status(&harness.manager, &query_id, ComponentStatus::Error).await;
        let concrete = concrete_query(&query);
        assert!(concrete.publication_recovery_required());

        harness
            .manager
            .stop_query(query_id.clone())
            .await
            .expect("stop failed Legacy query");
        wait_for_status(&harness.manager, &query_id, ComponentStatus::Stopped).await;
        harness
            .manager
            .start_query(query_id.clone())
            .await
            .expect("AutoReset restart should clear partial Legacy output");
        wait_for_status(&harness.manager, &query_id, ComponentStatus::Running).await;
        assert!(!concrete.publication_recovery_required());
        assert_eq!(concrete.output_sequence_for_test().await, 1);
        let checkpoint_store = concrete
            .get_checkpoint_store()
            .await
            .expect("reopened Legacy checkpoint store");
        assert!(checkpoint_store
            .read_checkpoint(QUERY_BOOTSTRAP_MARKER_V1)
            .await
            .expect("read cleared bootstrap marker")
            .is_none());

        harness
            .source
            .inject(2, "after-reset")
            .await
            .expect("inject after Legacy AutoReset");
        let delivered = receive_result(&mut results).await;
        assert_eq!(delivered.sequence, 2);
        assert_eq!(
            checkpoint_store
                .read_checkpoint(SOURCE_ID)
                .await
                .expect("read resumed source checkpoint")
                .expect("resumed source checkpoint")
                .sequence,
            2
        );
        let outbox = concrete
            .get_outbox_writer()
            .await
            .expect("Legacy outbox after reset")
            .read_from(&query_id, 0)
            .await
            .expect("read Legacy outbox after reset");
        assert_eq!(
            outbox
                .iter()
                .map(|(sequence, _)| *sequence)
                .collect::<Vec<_>>(),
            vec![2],
            "{stage:?}: stale or reused outbox entries survived AutoReset"
        );

        harness
            .manager
            .stop_query(query_id)
            .await
            .expect("stop recovered Legacy query");
        harness
            .source_manager
            .stop_source(SOURCE_ID.to_string())
            .await
            .expect("stop lifecycle source");
    }
}

#[tokio::test]
async fn fresh_process_auto_reset_recovers_legacy_pending_marker_high_water() {
    let temp_dir = tempfile::TempDir::new().expect("create temp directory");
    let failed = Arc::new(AtomicBool::new(false));
    let provider1: Arc<dyn IndexBackendPlugin> = Arc::new(LegacyFailureProvider {
        inner: RocksDbIndexProvider::new(temp_dir.path(), true, false),
        stage: LegacyFailureStage::Outbox,
        failed: failed.clone(),
    });
    let harness1 = LifecycleHarness::new_with_provider(provider1).await;
    let query_id = "legacy-fresh-process";
    let query1 = harness1
        .add_query_config(query_config_with_policy(
            query_id,
            crate::recovery::RecoveryPolicy::AutoReset,
        ))
        .await;
    harness1.start_query(query_id).await;
    harness1
        .source
        .inject(1, "lost-before-process-restart")
        .await
        .expect("inject Legacy failure");
    wait_for_status(&harness1.manager, query_id, ComponentStatus::Error).await;
    assert_eq!(
        read_legacy_pending_state(
            concrete_query(&query1)
                .get_checkpoint_store()
                .await
                .expect("first-process checkpoint store")
                .as_ref()
        )
        .await
        .expect("read first-process pending marker"),
        LegacyPendingState::Pending {
            output_sequence: Some(1)
        }
    );
    harness1
        .manager
        .stop_query(query_id.to_string())
        .await
        .expect("stop first-process query");
    wait_for_status(&harness1.manager, query_id, ComponentStatus::Stopped).await;
    query1.release_persistent_handles().await;
    harness1
        .source_manager
        .stop_source(SOURCE_ID.to_string())
        .await
        .expect("stop first-process source");
    drop(query1);
    drop(harness1);

    let provider2: Arc<dyn IndexBackendPlugin> = Arc::new(LegacyFailureProvider {
        inner: RocksDbIndexProvider::new(temp_dir.path(), true, false),
        stage: LegacyFailureStage::Outbox,
        failed,
    });
    let harness2 = LifecycleHarness::new_with_provider(provider2).await;
    let query2 = harness2
        .add_query_config(query_config_with_policy(
            query_id,
            crate::recovery::RecoveryPolicy::AutoReset,
        ))
        .await;
    let mut results = query2
        .subscribe("fresh-process-results".to_string())
        .await
        .expect("subscribe fresh-process output")
        .receiver;
    harness2.start_query(query_id).await;
    assert_eq!(
        concrete_query(&query2).output_sequence_for_test().await,
        1,
        "fresh process did not preserve the pending output high-water"
    );
    harness2
        .source
        .inject(2, "after-process-reset")
        .await
        .expect("inject after fresh-process AutoReset");
    assert_eq!(receive_result(&mut results).await.sequence, 2);

    harness2
        .manager
        .stop_query(query_id.to_string())
        .await
        .expect("stop recovered fresh-process query");
    harness2
        .source_manager
        .stop_source(SOURCE_ID.to_string())
        .await
        .expect("stop second-process source");
}

#[tokio::test]
async fn legacy_pending_marker_stage_failure_rolls_back_source_progress() {
    let temp_dir = tempfile::TempDir::new().expect("create temp directory");
    let provider: Arc<dyn IndexBackendPlugin> = Arc::new(LegacyFailureProvider {
        inner: RocksDbIndexProvider::new(temp_dir.path(), true, false),
        stage: LegacyFailureStage::MarkerStage,
        failed: Arc::new(AtomicBool::new(false)),
    });
    let harness = LifecycleHarness::new_with_provider(provider).await;
    let query_id = "legacy-marker-stage";
    let query = harness.add_query(query_id).await;
    let mut results = query
        .subscribe("legacy-marker-stage-results".to_string())
        .await
        .expect("subscribe marker-stage output")
        .receiver;
    harness.start_query(query_id).await;

    harness
        .source
        .inject(1, "first")
        .await
        .expect("inject marker-stage failure");
    wait_for_status(&harness.manager, query_id, ComponentStatus::Error).await;
    let concrete = concrete_query(&query);
    let checkpoint_store = concrete
        .get_checkpoint_store()
        .await
        .expect("marker-stage checkpoint store");
    assert!(checkpoint_store
        .read_checkpoint(SOURCE_ID)
        .await
        .expect("read rolled-back source checkpoint")
        .is_none());
    assert!(checkpoint_store
        .read_checkpoint(LEGACY_OUTPUT_PENDING_MARKER_V1)
        .await
        .expect("read failed marker")
        .is_none());
    assert_eq!(concrete.output_sequence_for_test().await, 0);
    assert!(!concrete.publication_recovery_required());
    assert_eq!(harness.source.base.compute_confirmed_position().await, None);
    drop(checkpoint_store);

    harness
        .manager
        .stop_query(query_id.to_string())
        .await
        .expect("stop marker-stage query");
    wait_for_status(&harness.manager, query_id, ComponentStatus::Stopped).await;
    harness
        .manager
        .start_query(query_id.to_string())
        .await
        .expect("restart after marker-stage rollback");
    wait_for_status(&harness.manager, query_id, ComponentStatus::Running).await;
    harness
        .source
        .inject(1, "retry")
        .await
        .expect("retry rolled-back input");
    let retried = receive_result(&mut results).await;
    assert_eq!(retried.sequence, 1);
    assert!(matches!(retried.results[0], ResultDiff::Add { .. }));

    harness
        .manager
        .stop_query(query_id.to_string())
        .await
        .expect("stop recovered marker-stage query");
    harness
        .source_manager
        .stop_source(SOURCE_ID.to_string())
        .await
        .expect("stop lifecycle source");
}

#[tokio::test]
async fn legacy_marker_clear_failure_stays_detectable_after_output_success() {
    let temp_dir = tempfile::TempDir::new().expect("create temp directory");
    let provider: Arc<dyn IndexBackendPlugin> = Arc::new(LegacyFailureProvider {
        inner: RocksDbIndexProvider::new(temp_dir.path(), true, false),
        stage: LegacyFailureStage::MarkerClear,
        failed: Arc::new(AtomicBool::new(false)),
    });
    let harness = LifecycleHarness::new_with_provider(provider).await;
    let query_id = "legacy-marker-clear";
    let query = harness.add_query(query_id).await;
    let mut results = query
        .subscribe("legacy-marker-clear-results".to_string())
        .await
        .expect("subscribe marker-clear output")
        .receiver;
    harness.start_query(query_id).await;
    harness
        .source
        .inject(1, "committed")
        .await
        .expect("inject marker-clear failure");
    let delivered = receive_result(&mut results).await;
    assert_eq!(delivered.sequence, 1);
    wait_for_status(&harness.manager, query_id, ComponentStatus::Error).await;

    let concrete = concrete_query(&query);
    let checkpoint_store = concrete
        .get_checkpoint_store()
        .await
        .expect("marker-clear checkpoint store");
    assert_eq!(
        checkpoint_store
            .read_checkpoint(LEGACY_OUTPUT_PENDING_MARKER_V1)
            .await
            .expect("read retained marker")
            .expect("pending marker retained")
            .sequence,
        1
    );
    assert_eq!(
        checkpoint_store
            .read_result_sequence(query_id)
            .await
            .expect("read committed output sequence"),
        Some(1)
    );
    assert_eq!(
        harness.source.base.compute_confirmed_position().await,
        None,
        "marker-clear failure must prevent source acknowledgement"
    );
    drop(checkpoint_store);

    harness
        .manager
        .stop_query(query_id.to_string())
        .await
        .expect("stop marker-clear query");
    wait_for_status(&harness.manager, query_id, ComponentStatus::Stopped).await;
    let error = harness
        .manager
        .start_query(query_id.to_string())
        .await
        .expect_err("Strict restart must detect retained pending marker");
    assert!(format!("{error:#}").contains("unfinished persistent Legacy publication"));

    harness
        .manager
        .stop_query(query_id.to_string())
        .await
        .expect("stop marker-clear query from Error");
    harness
        .source_manager
        .stop_source(SOURCE_ID.to_string())
        .await
        .expect("stop lifecycle source");
}

#[tokio::test]
async fn persistent_legacy_noop_clears_marker_without_allocating_sequence() {
    let temp_dir = tempfile::TempDir::new().expect("create temp directory");
    let provider: Arc<dyn IndexBackendPlugin> = Arc::new(LegacyFailureProvider {
        inner: RocksDbIndexProvider::new(temp_dir.path(), true, false),
        stage: LegacyFailureStage::ResultSequence,
        failed: Arc::new(AtomicBool::new(false)),
    });
    let harness = LifecycleHarness::new_with_provider(provider).await;
    let query_id = "legacy-noop";
    let mut config = query_config(query_id);
    config.query = "MATCH (n:Person) WHERE n.name = 'visible' RETURN n.name AS name".to_string();
    let query = harness.add_query_config(config).await;
    harness.start_query(query_id).await;
    harness
        .source
        .inject(1, "hidden")
        .await
        .expect("inject no-output input");
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if harness.source.base.compute_confirmed_position().await == Some(1) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("Noop input should be acknowledged after marker clear");

    let concrete = concrete_query(&query);
    let checkpoint_store = concrete
        .get_checkpoint_store()
        .await
        .expect("Noop checkpoint store");
    assert_eq!(
        checkpoint_store
            .read_checkpoint(LEGACY_OUTPUT_PENDING_MARKER_V1)
            .await
            .expect("read cleared Noop marker")
            .expect("logical clear marker")
            .sequence,
        0
    );
    assert_eq!(
        read_legacy_pending_state(checkpoint_store.as_ref())
            .await
            .expect("read logical Noop marker state"),
        LegacyPendingState::Absent
    );
    assert_eq!(concrete.output_sequence_for_test().await, 0);
    assert_eq!(
        checkpoint_store
            .read_result_sequence(query_id)
            .await
            .expect("read Noop result sequence"),
        None
    );
    assert_eq!(query.status().await, ComponentStatus::Running);

    harness
        .manager
        .stop_query(query_id.to_string())
        .await
        .expect("stop Noop query");
    harness
        .source_manager
        .stop_source(SOURCE_ID.to_string())
        .await
        .expect("stop lifecycle source");
}

#[tokio::test]
async fn reconfigure_after_post_commit_fence_hydrates_replacement_runtime() {
    let temp_dir = tempfile::TempDir::new().expect("create temp directory");
    let harness = LifecycleHarness::new(temp_dir.path()).await;
    let query = harness.add_query("atomic-reconfigure").await;
    concrete_query(&query)
        .set_processing_observer(Some(Arc::new(FailAfterCommit)))
        .await;
    harness.start_query("atomic-reconfigure").await;
    harness
        .source
        .inject(1, "committed")
        .await
        .expect("inject post-commit failure");
    wait_for_status(
        &harness.manager,
        "atomic-reconfigure",
        ComponentStatus::Error,
    )
    .await;

    harness
        .manager
        .update_query(
            "atomic-reconfigure".to_string(),
            query_config("atomic-reconfigure"),
        )
        .await
        .expect("reconfigure should replace the fenced runtime");
    let replacement = harness
        .manager
        .get_query_instance("atomic-reconfigure")
        .await
        .expect("replacement query");
    harness.start_query("atomic-reconfigure").await;
    let recovered = replacement
        .fetch_outbox(0)
        .await
        .expect("replacement should expose durable outbox");
    assert_eq!(recovered.latest_sequence, 1);
    assert_eq!(
        recovered
            .results
            .iter()
            .map(|result| result.sequence)
            .collect::<Vec<_>>(),
        vec![1]
    );
    assert_eq!(
        concrete_query(&replacement)
            .output_sequence_for_test()
            .await,
        1
    );

    harness
        .manager
        .stop_query("atomic-reconfigure".to_string())
        .await
        .expect("stop reconfigured query");
    harness
        .source_manager
        .stop_source(SOURCE_ID.to_string())
        .await
        .expect("stop lifecycle source");
}

#[tokio::test]
async fn incomplete_atomic_bootstrap_marker_clears_partial_state_before_restart() {
    let temp_dir = tempfile::TempDir::new().expect("create temp directory");
    let query_id = "atomic-bootstrap-recovery";
    let config = query_config(query_id);
    let provider = RocksDbIndexProvider::new(temp_dir.path(), true, false);
    let created = provider
        .create_indexes(query_id)
        .await
        .expect("create bootstrap recovery indexes");
    let checkpoint_store = created.checkpoint_store.clone().expect("checkpoint store");
    let live_results = created
        .live_results_writer
        .clone()
        .expect("live-results writer");
    checkpoint_store
        .write_config_hash(super::compute_config_hash(&config))
        .await
        .expect("seed config hash");
    created
        .set
        .session_control
        .begin()
        .await
        .expect("begin marker transaction");
    checkpoint_store
        .stage_checkpoint(QUERY_BOOTSTRAP_MARKER_V1, 0, None)
        .await
        .expect("seed in-progress marker");
    created
        .set
        .session_control
        .commit()
        .await
        .expect("commit marker transaction");
    let partial_row =
        rmp_serde::to_vec(&json!({ "name": "partial" })).expect("serialize partial bootstrap row");
    live_results
        .apply_mutations(
            query_id,
            &[drasi_core::interface::RowMutation {
                row_signature: 99,
                data: Some(&partial_row),
            }],
        )
        .await
        .expect("seed partial live row");
    drop(live_results);
    drop(checkpoint_store);
    drop(created);

    let harness = LifecycleHarness::new(temp_dir.path()).await;
    let query = harness.add_query_config(config).await;
    harness.start_query(query_id).await;
    let concrete = concrete_query(&query);
    assert_eq!(concrete.output_sequence_for_test().await, 0);
    let checkpoint_store = concrete
        .get_checkpoint_store()
        .await
        .expect("reopened checkpoint store");
    assert!(checkpoint_store
        .read_checkpoint(QUERY_BOOTSTRAP_MARKER_V1)
        .await
        .expect("read cleared bootstrap marker")
        .is_none());

    harness
        .manager
        .stop_query(query_id.to_string())
        .await
        .expect("stop recovered query");
    harness
        .source_manager
        .stop_source(SOURCE_ID.to_string())
        .await
        .expect("stop lifecycle source");
}

#[tokio::test]
async fn public_stop_cancels_blocked_start_and_aborts_pending_forwarders() {
    let temp_dir = tempfile::TempDir::new().expect("create temp directory");
    let harness = LifecycleHarness::new(temp_dir.path()).await;
    let (blocking_source, subscribe_entered) = BlockingSubscribeSource::new();
    harness
        .graph
        .write()
        .await
        .register_source(BLOCKING_SOURCE_ID, HashMap::new())
        .expect("register blocking source");
    harness
        .source_manager
        .provision_source(blocking_source.clone())
        .await
        .expect("provision blocking source");
    harness
        .source_manager
        .start_source(BLOCKING_SOURCE_ID.to_string())
        .await
        .expect("start blocking source");

    let query = harness
        .add_query_config(query_config_with_sources(
            "atomic-cancelled-start",
            &[SOURCE_ID, BLOCKING_SOURCE_ID],
        ))
        .await;
    let manager = harness.manager.clone();
    let start_task = tokio::spawn(async move {
        manager
            .start_query("atomic-cancelled-start".to_string())
            .await
    });
    subscribe_entered
        .await
        .expect("second source subscription should block");
    tokio::time::timeout(
        Duration::from_secs(2),
        harness
            .manager
            .stop_query("atomic-cancelled-start".to_string()),
    )
    .await
    .expect("public stop must cancel a blocked subscription")
    .expect("stop query during blocked start");
    assert!(start_task
        .await
        .expect("start task should complete after cancellation")
        .expect_err("cancelled start should return an error")
        .to_string()
        .contains("cancelled by stop"));
    wait_for_status(
        &harness.manager,
        "atomic-cancelled-start",
        ComponentStatus::Stopped,
    )
    .await;
    assert_eq!(
        query.subscription_count().await,
        0,
        "partially built forwarders must never be installed"
    );
    assert!(
        harness.source.removed_position_handles() >= 1,
        "public stop must release position state from partial subscriptions"
    );

    for sequence in 1..=4 {
        tokio::time::timeout(
            Duration::from_secs(2),
            harness.source.inject(sequence, "after-cancelled-start"),
        )
        .await
        .expect("cancelled startup forwarder must release its bounded receiver")
        .expect("dispatch after cancelled start");
    }

    harness
        .source_manager
        .stop_source(BLOCKING_SOURCE_ID.to_string())
        .await
        .expect("stop blocking source");
    harness
        .source_manager
        .stop_source(SOURCE_ID.to_string())
        .await
        .expect("stop lifecycle source");
}

#[tokio::test]
async fn ingress_fence_close_is_idempotent_under_race() {
    let queue = super::PriorityQueue::new(1);
    let fence = QueryIngressFence::new(queue.clone());
    let first = tokio::spawn(async {
        std::future::pending::<()>().await;
    });
    let second = tokio::spawn(async {
        std::future::pending::<()>().await;
    });
    fence.install(vec![first, second]).await;
    assert_eq!(fence.task_count().await, 2);

    let ((), ()) = tokio::join!(fence.close(), fence.close());
    assert_eq!(fence.task_count().await, 0);
    assert!(queue.is_empty().await);
}

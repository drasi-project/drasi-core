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
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

use anyhow::Result;
use async_trait::async_trait;
use bytes::Bytes;
use drasi_core::{
    interface::{IndexBackendPlugin, IndexError},
    middleware::MiddlewareTypeRegistry,
    models::{Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange},
};
use drasi_index_rocksdb::RocksDbIndexProvider;
use serde_json::json;
use tokio::sync::{oneshot, Mutex, Notify, RwLock};

use super::{
    manager::{DrasiQuery, Query, QueryManager},
    query_composite_host::{QueryIngressFence, QueryProcessingObserver},
};
use crate::{
    channels::{
        ChangeReceiver, ComponentStatus, DispatchMode, QueryResult, SourceEvent,
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

struct LifecycleHarness {
    manager: Arc<QueryManager>,
    source_manager: Arc<SourceManager>,
    graph: Arc<RwLock<ComponentGraph>>,
    source: BoundedLifecycleSource,
}

impl LifecycleHarness {
    async fn new(path: &std::path::Path) -> Self {
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
        let provider: Arc<dyn IndexBackendPlugin> =
            Arc::new(RocksDbIndexProvider::new(path, true, false));
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
async fn post_commit_fence_rejects_restart_without_reusing_durable_sequence() {
    let temp_dir = tempfile::TempDir::new().expect("create temp directory");
    let harness = LifecycleHarness::new(temp_dir.path()).await;
    let query = harness.add_query("atomic-post-commit").await;
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
    let reconfigure = harness
        .manager
        .update_query(
            "atomic-post-commit".to_string(),
            query_config("atomic-post-commit"),
        )
        .await
        .expect_err("reconfiguration must preserve the recovery fence");
    assert!(
        reconfigure.to_string().contains("A7 output reconciliation"),
        "unexpected reconfiguration error: {reconfigure:#}"
    );

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

    let restart = harness
        .manager
        .start_query("atomic-post-commit".to_string())
        .await
        .expect_err("unsafe in-process restart must be rejected");
    assert!(
        restart.to_string().contains("A7 output reconciliation"),
        "unexpected restart error: {restart:#}"
    );
    wait_for_status(
        &harness.manager,
        "atomic-post-commit",
        ComponentStatus::Error,
    )
    .await;
    assert_eq!(query.subscription_count().await, 0);
    assert_eq!(concrete.output_sequence_for_test().await, 0);
    assert_eq!(
        outbox
            .read_from("atomic-post-commit", 0)
            .await
            .expect("reread committed outbox")
            .into_iter()
            .map(|(sequence, _)| sequence)
            .collect::<Vec<_>>(),
        vec![1],
        "rejected restart must not reuse or overwrite the durable sequence"
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

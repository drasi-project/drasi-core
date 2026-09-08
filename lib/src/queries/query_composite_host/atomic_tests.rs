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
    collections::{HashMap, HashSet},
    path::Path,
    sync::{
        atomic::{AtomicU64, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use async_trait::async_trait;
use bytes::Bytes;
use drasi_core::{
    evaluation::{
        context::{QueryPartEvaluationContext, QueryVariables},
        functions::FunctionRegistry,
        variable_value::VariableValue,
    },
    interface::{
        CheckpointStore, CreatedIndexes, ElementIndex, FutureQueue, IndexBackendPlugin, IndexError,
        LiveResultsWriter, OutboxWriter, PushType, RowMutation, SessionControl, SourceCheckpoint,
        TransactionDomain,
    },
    models::{Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange},
    query::{ContinuousQuery, QueryBuilder},
};
use drasi_functions_cypher::CypherFunctionSet;
use drasi_index_rocksdb::RocksDbIndexProvider;
use drasi_query_ast::api::QueryConfiguration;
use drasi_query_cypher::CypherParser;
use serde_json::json;
use tokio::sync::{mpsc, oneshot, Mutex as AsyncMutex, RwLock};

use super::*;
use crate::{
    bootstrap::BootstrapResult,
    channels::{BootstrapEvent, ChangeReceiver},
    config::{QueryConfig, QueryLanguage},
};

const QUERY_ID: &str = "atomic-host-query";
const SOURCE_ID: &str = "atomic-host-source";

struct AtomicTestQueryConfig;

impl QueryConfiguration for AtomicTestQueryConfig {
    fn get_aggregating_function_names(&self) -> HashSet<String> {
        ["count", "sum", "min", "max", "avg", "collect", "stdev", "stdevp"]
            .into_iter()
            .map(str::to_string)
            .collect()
    }
}

#[derive(Clone)]
enum BackendFault {
    None,
    Commit,
    CommitThenBlock(Arc<CommitBlock>),
    CheckpointStageAfterOne,
    OutboxStage,
    LiveResultsStage,
    ResultSequenceWrite,
}

struct FailingCommitSessionControl {
    inner: Arc<dyn SessionControl>,
    transaction_domain: TransactionDomain,
}

#[async_trait]
impl SessionControl for FailingCommitSessionControl {
    fn transaction_domain(&self) -> Option<TransactionDomain> {
        Some(self.transaction_domain.clone())
    }

    async fn begin(&self) -> std::result::Result<(), IndexError> {
        self.inner.begin().await
    }

    async fn commit(&self) -> std::result::Result<(), IndexError> {
        Err(IndexError::CorruptedData)
    }

    fn rollback(&self) -> std::result::Result<(), IndexError> {
        self.inner.rollback()
    }
}

struct CommitBlock {
    committed_tx: Mutex<Option<oneshot::Sender<()>>>,
    commits_before_block: AtomicUsize,
}

struct CommitThenBlockSessionControl {
    inner: Arc<dyn SessionControl>,
    transaction_domain: TransactionDomain,
    block: Arc<CommitBlock>,
}

#[async_trait]
impl SessionControl for CommitThenBlockSessionControl {
    fn transaction_domain(&self) -> Option<TransactionDomain> {
        Some(self.transaction_domain.clone())
    }

    async fn begin(&self) -> std::result::Result<(), IndexError> {
        self.inner.begin().await
    }

    async fn commit(&self) -> std::result::Result<(), IndexError> {
        if self
            .block
            .commits_before_block
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |remaining| {
                remaining.checked_sub(1)
            })
            .is_ok()
        {
            return self.inner.commit().await;
        }

        self.inner.commit().await?;
        if let Some(committed_tx) = self
            .block
            .committed_tx
            .lock()
            .expect("commit-block signal lock poisoned")
            .take()
        {
            let _ = committed_tx.send(());
        }
        std::future::pending().await
    }

    fn rollback(&self) -> std::result::Result<(), IndexError> {
        self.inner.rollback()
    }
}

struct FailingOutboxWriter {
    inner: Arc<dyn OutboxWriter>,
}

struct FailingLiveResultsWriter {
    inner: Arc<dyn LiveResultsWriter>,
}

struct FailingSecondCheckpointStore {
    inner: Arc<dyn CheckpointStore>,
    stage_calls: AtomicUsize,
}

struct FailingResultSequenceCheckpointStore {
    inner: Arc<dyn CheckpointStore>,
}

#[async_trait]
impl CheckpointStore for FailingSecondCheckpointStore {
    fn transaction_domain(&self) -> Option<TransactionDomain> {
        self.inner.transaction_domain()
    }

    fn is_persistent(&self) -> bool {
        self.inner.is_persistent()
    }

    async fn stage_checkpoint(
        &self,
        source_id: &str,
        sequence: u64,
        source_position: Option<&Bytes>,
    ) -> std::result::Result<(), IndexError> {
        if self.stage_calls.fetch_add(1, Ordering::AcqRel) == 1 {
            return Err(IndexError::CorruptedData);
        }
        self.inner
            .stage_checkpoint(source_id, sequence, source_position)
            .await
    }

    async fn read_checkpoint(
        &self,
        source_id: &str,
    ) -> std::result::Result<Option<SourceCheckpoint>, IndexError> {
        self.inner.read_checkpoint(source_id).await
    }

    async fn read_all_checkpoints(
        &self,
    ) -> std::result::Result<HashMap<String, SourceCheckpoint>, IndexError> {
        self.inner.read_all_checkpoints().await
    }

    async fn clear_checkpoints(&self) -> std::result::Result<(), IndexError> {
        self.inner.clear_checkpoints().await
    }

    async fn write_config_hash(&self, hash: u64) -> std::result::Result<(), IndexError> {
        self.inner.write_config_hash(hash).await
    }

    async fn read_config_hash(&self) -> std::result::Result<Option<u64>, IndexError> {
        self.inner.read_config_hash().await
    }

    async fn write_result_sequence(
        &self,
        query_id: &str,
        sequence: u64,
    ) -> std::result::Result<(), IndexError> {
        self.inner.write_result_sequence(query_id, sequence).await
    }

    async fn read_result_sequence(
        &self,
        query_id: &str,
    ) -> std::result::Result<Option<u64>, IndexError> {
        self.inner.read_result_sequence(query_id).await
    }
}

#[async_trait]
impl CheckpointStore for FailingResultSequenceCheckpointStore {
    fn transaction_domain(&self) -> Option<TransactionDomain> {
        self.inner.transaction_domain()
    }

    fn is_persistent(&self) -> bool {
        self.inner.is_persistent()
    }

    async fn stage_checkpoint(
        &self,
        source_id: &str,
        sequence: u64,
        source_position: Option<&Bytes>,
    ) -> std::result::Result<(), IndexError> {
        self.inner
            .stage_checkpoint(source_id, sequence, source_position)
            .await
    }

    async fn read_checkpoint(
        &self,
        source_id: &str,
    ) -> std::result::Result<Option<SourceCheckpoint>, IndexError> {
        self.inner.read_checkpoint(source_id).await
    }

    async fn read_all_checkpoints(
        &self,
    ) -> std::result::Result<HashMap<String, SourceCheckpoint>, IndexError> {
        self.inner.read_all_checkpoints().await
    }

    async fn clear_checkpoints(&self) -> std::result::Result<(), IndexError> {
        self.inner.clear_checkpoints().await
    }

    async fn write_config_hash(&self, hash: u64) -> std::result::Result<(), IndexError> {
        self.inner.write_config_hash(hash).await
    }

    async fn read_config_hash(&self) -> std::result::Result<Option<u64>, IndexError> {
        self.inner.read_config_hash().await
    }

    async fn write_result_sequence(
        &self,
        _query_id: &str,
        _sequence: u64,
    ) -> std::result::Result<(), IndexError> {
        Err(IndexError::other(std::io::Error::other(
            "injected Legacy result-sequence write failure",
        )))
    }

    async fn read_result_sequence(
        &self,
        query_id: &str,
    ) -> std::result::Result<Option<u64>, IndexError> {
        self.inner.read_result_sequence(query_id).await
    }
}

#[async_trait]
impl LiveResultsWriter for FailingLiveResultsWriter {
    fn transaction_domain(&self) -> Option<TransactionDomain> {
        self.inner.transaction_domain()
    }

    async fn apply_mutations(
        &self,
        _query_id: &str,
        _mutations: &[RowMutation<'_>],
    ) -> std::result::Result<(), IndexError> {
        Err(IndexError::other(std::io::Error::other(
            "injected atomic live-results stage failure",
        )))
    }

    async fn read_snapshot(
        &self,
        query_id: &str,
    ) -> std::result::Result<Vec<(u64, Vec<u8>)>, IndexError> {
        self.inner.read_snapshot(query_id).await
    }

    async fn clear(&self, query_id: &str) -> std::result::Result<(), IndexError> {
        self.inner.clear(query_id).await
    }

    async fn row_count(&self, query_id: &str) -> std::result::Result<usize, IndexError> {
        self.inner.row_count(query_id).await
    }
}

#[async_trait]
impl OutboxWriter for FailingOutboxWriter {
    fn transaction_domain(&self) -> Option<TransactionDomain> {
        self.inner.transaction_domain()
    }

    async fn append(
        &self,
        _query_id: &str,
        _sequence: u64,
        _data: &[u8],
    ) -> std::result::Result<(), IndexError> {
        Err(IndexError::other(std::io::Error::other(
            "injected atomic outbox stage failure",
        )))
    }

    async fn read_from(
        &self,
        query_id: &str,
        after_sequence: u64,
    ) -> std::result::Result<Vec<(u64, Vec<u8>)>, IndexError> {
        self.inner.read_from(query_id, after_sequence).await
    }

    async fn read_latest_sequence(
        &self,
        query_id: &str,
    ) -> std::result::Result<Option<u64>, IndexError> {
        self.inner.read_latest_sequence(query_id).await
    }

    async fn clear(&self, query_id: &str) -> std::result::Result<(), IndexError> {
        self.inner.clear(query_id).await
    }

    async fn trim_to_capacity(
        &self,
        query_id: &str,
        capacity: usize,
    ) -> std::result::Result<usize, IndexError> {
        self.inner.trim_to_capacity(query_id, capacity).await
    }
}

struct RecordingDispatcher {
    count: Arc<AtomicUsize>,
    tx: mpsc::UnboundedSender<Arc<QueryResult>>,
}

#[async_trait]
impl ChangeDispatcher<QueryResult> for RecordingDispatcher {
    async fn dispatch_change(&self, change: Arc<QueryResult>) -> anyhow::Result<()> {
        self.count.fetch_add(1, Ordering::AcqRel);
        self.tx
            .send(change)
            .map_err(|_| anyhow::anyhow!("atomic test result receiver dropped"))
    }

    async fn create_receiver(&self) -> anyhow::Result<Box<dyn ChangeReceiver<QueryResult>>> {
        Err(anyhow::anyhow!(
            "atomic test dispatcher does not create receivers"
        ))
    }
}

struct AtomicHostFixture {
    base: QueryBase,
    priority_queue: PriorityQueue,
    output_state: Arc<RwLock<QueryOutputState>>,
    dispatchers: Arc<RwLock<Vec<Box<dyn ChangeDispatcher<QueryResult> + Send + Sync>>>>,
    output_rx: mpsc::UnboundedReceiver<Arc<QueryResult>>,
    dispatch_count: Arc<AtomicUsize>,
    future_queue_source: Arc<FutureQueueSource>,
    checkpoint_store: Arc<dyn CheckpointStore>,
    outbox_writer: Arc<dyn OutboxWriter>,
    live_results_writer: Arc<dyn LiveResultsWriter>,
    element_index: Arc<dyn ElementIndex>,
    future_queue: Arc<dyn FutureQueue>,
    session_control: Arc<dyn SessionControl>,
    continuous_query: Arc<ContinuousQuery>,
    position_handle: Arc<AtomicU64>,
    publication_recovery: AtomicPublicationRecovery,
    mode: QueryProcessingMode,
}

async fn build_host(
    path: &Path,
    query_text: &str,
    fault: BackendFault,
    force_legacy: bool,
    observer: Option<Arc<dyn QueryProcessingObserver>>,
) -> (QueryCompositeHost, AtomicHostFixture) {
    build_host_with_bootstrap(path, query_text, fault, force_legacy, observer, Vec::new()).await
}

async fn build_host_with_bootstrap(
    path: &Path,
    query_text: &str,
    fault: BackendFault,
    force_legacy: bool,
    observer: Option<Arc<dyn QueryProcessingObserver>>,
    bootstrap_inputs: Vec<QueryBootstrapInput>,
) -> (QueryCompositeHost, AtomicHostFixture) {
    let provider = RocksDbIndexProvider::new(path, true, false);
    let mut created = provider
        .create_indexes(QUERY_ID)
        .await
        .expect("create RocksDB indexes");

    match fault {
        BackendFault::None => {}
        BackendFault::Commit => {
            let inner = created.set.session_control.clone();
            let transaction_domain = inner
                .transaction_domain()
                .expect("RocksDB session transaction domain");
            created.set.session_control = Arc::new(FailingCommitSessionControl {
                inner,
                transaction_domain,
            });
        }
        BackendFault::CommitThenBlock(block) => {
            let inner = created.set.session_control.clone();
            let transaction_domain = inner
                .transaction_domain()
                .expect("RocksDB session transaction domain");
            created.set.session_control = Arc::new(CommitThenBlockSessionControl {
                inner,
                transaction_domain,
                block,
            });
        }
        BackendFault::CheckpointStageAfterOne => {
            let inner = created
                .checkpoint_store
                .as_ref()
                .expect("RocksDB checkpoint store")
                .clone();
            created.checkpoint_store = Some(Arc::new(FailingSecondCheckpointStore {
                inner,
                stage_calls: AtomicUsize::new(0),
            }));
        }
        BackendFault::OutboxStage => {
            let inner = created
                .outbox_writer
                .as_ref()
                .expect("RocksDB outbox writer")
                .clone();
            created.outbox_writer = Some(Arc::new(FailingOutboxWriter { inner }));
        }
        BackendFault::LiveResultsStage => {
            let inner = created
                .live_results_writer
                .as_ref()
                .expect("RocksDB live-results writer")
                .clone();
            created.live_results_writer = Some(Arc::new(FailingLiveResultsWriter { inner }));
        }
        BackendFault::ResultSequenceWrite => {
            let inner = created
                .checkpoint_store
                .as_ref()
                .expect("RocksDB checkpoint store")
                .clone();
            created.checkpoint_store =
                Some(Arc::new(FailingResultSequenceCheckpointStore { inner }));
        }
    }

    let mode = if force_legacy {
        QueryProcessingMode::legacy()
    } else {
        QueryProcessingMode::from_created_indexes(&created)
    };
    if !force_legacy {
        assert_eq!(mode.diagnostic_name(), "atomic");
    }

    let checkpoint_store = created
        .checkpoint_store
        .as_ref()
        .expect("RocksDB checkpoint store")
        .clone();
    let outbox_writer = created
        .outbox_writer
        .as_ref()
        .expect("RocksDB outbox writer")
        .clone();
    let live_results_writer = created
        .live_results_writer
        .as_ref()
        .expect("RocksDB live-results writer")
        .clone();
    let indexes = created.set;
    let element_index = indexes.element_index.clone();
    let future_queue = indexes.future_queue.clone();
    let session_control = indexes.session_control.clone();

    let functions = Arc::new(FunctionRegistry::new()).with_cypher_function_set();
    let parser = Arc::new(CypherParser::new(Arc::new(AtomicTestQueryConfig)));
    let continuous_query = Arc::new(
        QueryBuilder::new(query_text, parser)
            .with_function_registry(functions)
            .with_element_index(indexes.element_index)
            .with_archive_index(indexes.archive_index)
            .with_result_index(indexes.result_index)
            .with_future_queue(indexes.future_queue)
            .with_session_control(indexes.session_control)
            .build()
            .await,
    );

    let base = QueryBase::new(QueryConfig {
        id: QUERY_ID.to_string(),
        query: query_text.to_string(),
        query_language: QueryLanguage::Cypher,
        middleware: vec![],
        sources: vec![],
        auto_start: false,
        joins: None,
        enable_bootstrap: false,
        bootstrap_buffer_size: 10,
        priority_queue_capacity: Some(16),
        dispatch_buffer_capacity: Some(16),
        dispatch_mode: None,
        storage_backend: None,
        recovery_policy: None,
        outbox_capacity: 16,
        bootstrap_timeout_secs: 1,
    })
    .expect("create query base");
    base.set_status(ComponentStatus::Starting, None).await;

    let (result_tx, output_rx) = mpsc::unbounded_channel();
    let dispatch_count = Arc::new(AtomicUsize::new(0));
    let dispatchers: Arc<RwLock<Vec<Box<dyn ChangeDispatcher<QueryResult> + Send + Sync>>>> =
        Arc::new(RwLock::new(vec![Box::new(RecordingDispatcher {
            count: dispatch_count.clone(),
            tx: result_tx,
        })]));
    let output_state = Arc::new(RwLock::new(QueryOutputState::new(16)));
    let priority_queue = PriorityQueue::new(16);
    let future_queue_source = Arc::new(FutureQueueSource::new(
        continuous_query.future_queue(),
        QUERY_ID.to_string(),
    ));
    let position_handle = Arc::new(AtomicU64::new(u64::MAX));
    let position_handles = HashMap::from([(SOURCE_ID.to_string(), position_handle.clone())]);
    let ingress_fence = QueryIngressFence::new(priority_queue.clone());
    let publication_recovery = AtomicPublicationRecovery::default();

    let output_dependencies = QueryOutputDependencies::new(
        QUERY_ID.to_string(),
        output_state.clone(),
        dispatchers.clone(),
        Some(outbox_writer.clone()),
        Some(live_results_writer.clone()),
        Some(checkpoint_store.clone()),
        16,
        Arc::new(QueryOutputMetrics::new()),
    )
    .with_publication_recovery(publication_recovery.clone());
    let output_dependencies = match observer {
        Some(observer) => output_dependencies.with_observer(observer),
        None => output_dependencies,
    };

    let host = QueryCompositeHost::new(
        QueryHostRuntime::new(
            "atomic-host-test".to_string(),
            QUERY_ID.to_string(),
            priority_queue.clone(),
            base.status_handle(),
            future_queue_source.clone(),
            ingress_fence,
        ),
        QueryBootstrapScope::new(
            bootstrap_inputs,
            checkpoint_store.clone(),
            Some(session_control.clone()),
        ),
        mode.clone(),
        QueryLiveDependencies::new(
            continuous_query.clone(),
            checkpoint_store.clone(),
            HashMap::new(),
            position_handles,
        ),
        output_dependencies,
    );

    (
        host,
        AtomicHostFixture {
            base,
            priority_queue,
            output_state,
            dispatchers,
            output_rx,
            dispatch_count,
            future_queue_source,
            checkpoint_store,
            outbox_writer,
            live_results_writer,
            element_index,
            future_queue,
            session_control,
            continuous_query,
            position_handle,
            publication_recovery,
            mode,
        },
    )
}

fn person_change(id: &str, name: &str, effective_from: u64) -> SourceChange {
    SourceChange::Insert {
        element: Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new(SOURCE_ID, id),
                labels: vec![Arc::from("Person")].into(),
                effective_from,
            },
            properties: ElementPropertyMap::from(json!({
                "name": name,
                "age": 30,
            })),
        },
    }
}

fn sequenced_event(
    change: SourceChange,
    sequence: u64,
    source_position: Bytes,
) -> Arc<SourceEventWrapper> {
    let timestamp = chrono::DateTime::from_timestamp_millis(change.get_realtime() as i64)
        .expect("valid event timestamp");
    let mut event = SourceEventWrapper::with_sequence(
        SOURCE_ID.to_string(),
        SourceEvent::Change(change),
        timestamp,
        sequence,
        Some(ProfilingMetadata {
            source_ns: Some(101),
            source_receive_ns: Some(202),
            source_send_ns: Some(303),
            ..Default::default()
        }),
    );
    event.set_source_position(source_position);
    Arc::new(event)
}

async fn start_host(host: QueryCompositeHost, fixture: &AtomicHostFixture) {
    host.start(&fixture.base).await;
    wait_for_status(&fixture.base, ComponentStatus::Running).await;
}

async fn wait_for_status(base: &QueryBase, expected: ComponentStatus) {
    if base.get_status().await == expected {
        return;
    }
    let mut status_rx = base.status_handle().subscribe_status();
    tokio::time::timeout(
        Duration::from_secs(5),
        status_rx.wait_for(|status| *status == expected),
    )
    .await
    .expect("status transition timed out")
    .expect("status channel closed");
}

async fn stop_host(fixture: &AtomicHostFixture) {
    fixture
        .base
        .set_status(ComponentStatus::Stopping, None)
        .await;
    QueryCompositeHost::stop(&fixture.base)
        .await
        .expect("stop query host");
}

async fn enqueue_source_change(
    fixture: &AtomicHostFixture,
    id: &str,
    name: &str,
    sequence: u64,
    source_position: Bytes,
) {
    assert!(
        fixture
            .priority_queue
            .enqueue(sequenced_event(
                person_change(id, name, sequence * 1_000),
                sequence,
                source_position,
            ))
            .await
    );
}

async fn receive_result(
    receiver: &mut mpsc::UnboundedReceiver<Arc<QueryResult>>,
) -> Arc<QueryResult> {
    tokio::time::timeout(Duration::from_secs(5), receiver.recv())
        .await
        .expect("query result timed out")
        .expect("query result channel closed")
}

async fn element_exists(fixture: &AtomicHostFixture, id: &str) -> bool {
    fixture
        .session_control
        .begin()
        .await
        .expect("begin verification session");
    let element = fixture
        .element_index
        .get_element(&ElementReference::new(SOURCE_ID, id))
        .await
        .expect("read element");
    fixture
        .session_control
        .rollback()
        .expect("end verification session");
    element.is_some()
}

async fn assert_no_durable_output(fixture: &AtomicHostFixture) {
    assert!(fixture
        .checkpoint_store
        .read_checkpoint(SOURCE_ID)
        .await
        .expect("read checkpoint")
        .is_none());
    assert!(fixture
        .outbox_writer
        .read_from(QUERY_ID, 0)
        .await
        .expect("read outbox")
        .is_empty());
    assert!(fixture
        .live_results_writer
        .read_snapshot(QUERY_ID)
        .await
        .expect("read live results")
        .is_empty());
    assert_eq!(
        fixture
            .checkpoint_store
            .read_result_sequence(QUERY_ID)
            .await
            .expect("read result sequence"),
        None
    );
}

#[tokio::test]
async fn rocksdb_multi_source_bootstrap_commits_live_rows_and_handoff_before_running() {
    let temp_dir = tempfile::TempDir::new().expect("create temp directory");
    let (source_a_tx, source_a_rx) = mpsc::channel(4);
    let (source_b_tx, source_b_rx) = mpsc::channel(4);
    let (result_a_tx, result_a_rx) = oneshot::channel();
    let (result_b_tx, result_b_rx) = oneshot::channel();
    let (host, mut fixture) = build_host_with_bootstrap(
        temp_dir.path(),
        "MATCH (n:Person) RETURN n.name AS name",
        BackendFault::None,
        false,
        None,
        vec![
            QueryBootstrapInput::new(SOURCE_ID.to_string(), source_a_rx, Some(result_a_rx)),
            QueryBootstrapInput::new(
                "atomic-host-source-b".to_string(),
                source_b_rx,
                Some(result_b_rx),
            ),
        ],
    )
    .await;
    host.start(&fixture.base).await;
    let started = receive_result(&mut fixture.output_rx).await;
    assert_eq!(started.metadata["control_signal"], "bootstrapStarted");
    assert_eq!(
        QueryBootstrapScope::read_recovery_state(fixture.checkpoint_store.as_ref())
            .await
            .expect("read in-progress marker"),
        QueryBootstrapRecoveryState::InProgress
    );

    source_a_tx
        .send(BootstrapEvent {
            source_id: SOURCE_ID.to_string(),
            change: person_change("bootstrap-a", "Ada", 1_000),
            timestamp: chrono::Utc::now(),
            sequence: 0,
        })
        .await
        .expect("send source A bootstrap event");
    source_b_tx
        .send(BootstrapEvent {
            source_id: "atomic-host-source-b".to_string(),
            change: person_change("bootstrap-b", "Grace", 2_000),
            timestamp: chrono::Utc::now(),
            sequence: 0,
        })
        .await
        .expect("send source B bootstrap event");
    result_a_tx
        .send(Ok(BootstrapResult {
            event_count: 1,
            source_position: Some(Bytes::from_static(b"boundary-a")),
        }))
        .expect("send source A bootstrap result");
    result_b_tx
        .send(Ok(BootstrapResult {
            event_count: 1,
            source_position: Some(Bytes::from_static(b"boundary-b")),
        }))
        .expect("send source B bootstrap result");
    drop(source_a_tx);
    drop(source_b_tx);

    let completed = receive_result(&mut fixture.output_rx).await;
    assert_eq!(completed.metadata["control_signal"], "bootstrapCompleted");
    wait_for_status(&fixture.base, ComponentStatus::Running).await;
    assert_eq!(
        QueryBootstrapScope::read_recovery_state(fixture.checkpoint_store.as_ref())
            .await
            .expect("read completed marker"),
        QueryBootstrapRecoveryState::Complete
    );
    assert_eq!(
        fixture
            .checkpoint_store
            .read_checkpoint(SOURCE_ID)
            .await
            .expect("read source A checkpoint"),
        Some(SourceCheckpoint::new(
            0,
            Some(Bytes::from_static(b"boundary-a"))
        ))
    );
    assert_eq!(
        fixture
            .checkpoint_store
            .read_checkpoint("atomic-host-source-b")
            .await
            .expect("read source B checkpoint"),
        Some(SourceCheckpoint::new(
            0,
            Some(Bytes::from_static(b"boundary-b"))
        ))
    );
    assert_eq!(
        fixture
            .live_results_writer
            .read_snapshot(QUERY_ID)
            .await
            .expect("read bootstrap live rows")
            .len(),
        2
    );
    assert!(fixture
        .outbox_writer
        .read_from(QUERY_ID, 0)
        .await
        .expect("read bootstrap outbox")
        .is_empty());
    assert_eq!(
        fixture
            .checkpoint_store
            .read_result_sequence(QUERY_ID)
            .await
            .expect("read bootstrap output sequence"),
        None
    );
    assert_eq!(fixture.output_state.read().await.results_len(), 2);
    assert_eq!(fixture.output_state.read().await.as_of_sequence(), 0);
    let fresh_state = Arc::new(RwLock::new(QueryOutputState::new(16)));
    let dependencies = QueryOutputDependencies::new(
        QUERY_ID.to_string(),
        fresh_state.clone(),
        Arc::new(RwLock::new(Vec::new())),
        Some(fixture.outbox_writer.clone()),
        Some(fixture.live_results_writer.clone()),
        Some(fixture.checkpoint_store.clone()),
        16,
        Arc::new(QueryOutputMetrics::new()),
    );
    QueryCompositeHost::reconcile_output_state(&fixture.mode, &dependencies)
        .await
        .expect("hydrate the sequence-0 bootstrap snapshot");
    assert_eq!(fresh_state.read().await.as_of_sequence(), 0);
    assert_eq!(fresh_state.read().await.results_len(), 2);

    stop_host(&fixture).await;
}

#[tokio::test]
async fn atomic_bootstrap_persistence_failure_rolls_back_and_keeps_handoff_closed() {
    let temp_dir = tempfile::TempDir::new().expect("create temp directory");
    let (bootstrap_tx, bootstrap_rx) = mpsc::channel(2);
    let (result_tx, result_rx) = oneshot::channel();
    let (host, mut fixture) = build_host_with_bootstrap(
        temp_dir.path(),
        "MATCH (n:Person) RETURN n.name AS name",
        BackendFault::LiveResultsStage,
        false,
        None,
        vec![QueryBootstrapInput::new(SOURCE_ID.to_string(), bootstrap_rx, Some(result_rx))],
    )
    .await;
    host.start(&fixture.base).await;
    let started = receive_result(&mut fixture.output_rx).await;
    assert_eq!(started.metadata["control_signal"], "bootstrapStarted");
    bootstrap_tx
        .send(BootstrapEvent {
            source_id: SOURCE_ID.to_string(),
            change: person_change("bootstrap-rollback", "Rollback", 1_000),
            timestamp: chrono::Utc::now(),
            sequence: 0,
        })
        .await
        .expect("send failing bootstrap event");
    result_tx
        .send(Ok(BootstrapResult::default()))
        .expect("send bootstrap result");
    drop(bootstrap_tx);

    wait_for_status(&fixture.base, ComponentStatus::Error).await;
    assert!(!element_exists(&fixture, "bootstrap-rollback").await);
    assert_no_durable_output(&fixture).await;
    assert_eq!(fixture.output_state.read().await.results_len(), 0);
    assert_eq!(
        QueryBootstrapScope::read_recovery_state(fixture.checkpoint_store.as_ref())
            .await
            .expect("read failed bootstrap marker"),
        QueryBootstrapRecoveryState::InProgress
    );
    if let Ok(Some(result)) =
        tokio::time::timeout(Duration::from_millis(50), fixture.output_rx.recv()).await
    {
        assert_ne!(result.metadata["control_signal"], "bootstrapCompleted");
    }

    stop_host(&fixture).await;
}

#[tokio::test]
async fn atomic_bootstrap_checkpoint_failure_does_not_complete_handoff() {
    let temp_dir = tempfile::TempDir::new().expect("create temp directory");
    let (bootstrap_tx, bootstrap_rx) = mpsc::channel(1);
    let (result_tx, result_rx) = oneshot::channel();
    let (host, mut fixture) = build_host_with_bootstrap(
        temp_dir.path(),
        "MATCH (n:Person) RETURN n.name AS name",
        BackendFault::CheckpointStageAfterOne,
        false,
        None,
        vec![QueryBootstrapInput::new(SOURCE_ID.to_string(), bootstrap_rx, Some(result_rx))],
    )
    .await;
    host.start(&fixture.base).await;
    let started = receive_result(&mut fixture.output_rx).await;
    assert_eq!(started.metadata["control_signal"], "bootstrapStarted");
    result_tx
        .send(Ok(BootstrapResult {
            event_count: 0,
            source_position: Some(Bytes::from_static(b"handoff")),
        }))
        .expect("send bootstrap result");
    drop(bootstrap_tx);

    wait_for_status(&fixture.base, ComponentStatus::Error).await;
    assert!(fixture
        .checkpoint_store
        .read_checkpoint(SOURCE_ID)
        .await
        .expect("read failed handoff checkpoint")
        .is_none());
    assert_eq!(
        QueryBootstrapScope::read_recovery_state(fixture.checkpoint_store.as_ref())
            .await
            .expect("read handoff marker"),
        QueryBootstrapRecoveryState::InProgress
    );
    if let Ok(Some(result)) =
        tokio::time::timeout(Duration::from_millis(50), fixture.output_rx.recv()).await
    {
        assert_ne!(result.metadata["control_signal"], "bootstrapCompleted");
    }

    stop_host(&fixture).await;
}

struct FailAfterStagingObserver;

#[async_trait]
impl QueryProcessingObserver for FailAfterStagingObserver {
    async fn after_output_staged(&self) -> std::result::Result<(), IndexError> {
        Err(IndexError::other(std::io::Error::other(
            "injected failure after all output staging",
        )))
    }
}

struct BlockAfterStagingObserver {
    staged_tx: Mutex<Option<oneshot::Sender<()>>>,
    release_rx: AsyncMutex<Option<oneshot::Receiver<()>>>,
}

#[async_trait]
impl QueryProcessingObserver for BlockAfterStagingObserver {
    async fn after_output_staged(&self) -> std::result::Result<(), IndexError> {
        if let Some(staged_tx) = self
            .staged_tx
            .lock()
            .expect("staged signal lock poisoned")
            .take()
        {
            let _ = staged_tx.send(());
        }
        let release_rx = self
            .release_rx
            .lock()
            .await
            .take()
            .expect("release receiver already consumed");
        release_rx.await.map_err(|_| {
            IndexError::other(std::io::Error::other(
                "atomic staging release signal dropped",
            ))
        })
    }
}

struct FailAfterCommitObserver;

#[async_trait]
impl QueryProcessingObserver for FailAfterCommitObserver {
    async fn after_commit_before_publish(&self) -> anyhow::Result<()> {
        Err(anyhow::anyhow!(
            "injected failure after commit before in-memory publication"
        ))
    }
}

struct BlockAfterCommitObserver {
    committed_tx: Mutex<Option<oneshot::Sender<()>>>,
    release_rx: AsyncMutex<Option<oneshot::Receiver<()>>>,
}

#[async_trait]
impl QueryProcessingObserver for BlockAfterCommitObserver {
    async fn after_commit_before_publish(&self) -> anyhow::Result<()> {
        if let Some(committed_tx) = self
            .committed_tx
            .lock()
            .expect("commit signal lock poisoned")
            .take()
        {
            let _ = committed_tx.send(());
        }
        self.release_rx
            .lock()
            .await
            .take()
            .expect("commit release receiver already consumed")
            .await
            .map_err(|_| anyhow::anyhow!("commit release signal dropped"))
    }
}

struct StaleSequenceAfterCommitObserver {
    output_state: Mutex<Option<Arc<RwLock<QueryOutputState>>>>,
}

#[async_trait]
impl QueryProcessingObserver for StaleSequenceAfterCommitObserver {
    async fn after_commit_before_publish(&self) -> anyhow::Result<()> {
        let output_state = self
            .output_state
            .lock()
            .expect("stale-sequence state lock poisoned")
            .as_ref()
            .expect("stale-sequence state not initialized")
            .clone();
        let mut state = output_state.write().await;
        state.advance_sequence_and_push(QueryResult::new(
            QUERY_ID.to_string(),
            0,
            chrono::Utc::now(),
            vec![],
            HashMap::new(),
        ));
        Ok(())
    }
}

struct AckSignalObserver {
    ack_tx: Mutex<Option<oneshot::Sender<()>>>,
}

#[async_trait]
impl QueryProcessingObserver for AckSignalObserver {
    fn after_source_acknowledged(&self) {
        if let Some(ack_tx) = self.ack_tx.lock().expect("ack signal lock poisoned").take() {
            let _ = ack_tx.send(());
        }
    }
}

#[derive(Default)]
struct CountingObserver {
    staged: AtomicUsize,
    committed: AtomicUsize,
}

#[async_trait]
impl QueryProcessingObserver for CountingObserver {
    async fn after_output_staged(&self) -> std::result::Result<(), IndexError> {
        self.staged.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }

    async fn after_commit_before_publish(&self) -> anyhow::Result<()> {
        self.committed.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }
}

#[tokio::test]
async fn rocksdb_atomic_source_commits_every_resource_before_ack_and_dispatch() {
    let temp_dir = tempfile::TempDir::new().expect("create temp directory");
    let (host, mut fixture) = build_host(
        temp_dir.path(),
        "MATCH (n:Person) RETURN n.name AS name",
        BackendFault::None,
        false,
        None,
    )
    .await;
    start_host(host, &fixture).await;

    let source_position = Bytes::from_static(b"atomic-position-1");
    enqueue_source_change(
        &fixture,
        "person-success",
        "Ada",
        1,
        source_position.clone(),
    )
    .await;
    let dispatched = receive_result(&mut fixture.output_rx).await;

    assert_eq!(dispatched.sequence, 1);
    assert_eq!(dispatched.metadata["source_id"], SOURCE_ID);
    let profiling = dispatched
        .profiling
        .as_ref()
        .expect("atomic result profiling");
    assert!(profiling.query_core_return_ns.is_some());
    assert!(profiling.query_send_ns.is_some());
    assert!(
        profiling.query_core_return_ns <= profiling.query_send_ns,
        "output staging begins after evaluation completes"
    );
    assert_eq!(
        fixture
            .checkpoint_store
            .read_checkpoint(SOURCE_ID)
            .await
            .expect("read checkpoint"),
        Some(SourceCheckpoint::new(1, Some(source_position)))
    );
    let outbox = fixture
        .outbox_writer
        .read_from(QUERY_ID, 0)
        .await
        .expect("read outbox");
    assert_eq!(outbox.len(), 1);
    assert_eq!(
        outbox[0].1,
        rmp_serde::to_vec(dispatched.as_ref()).expect("serialize dispatched result"),
        "the persisted and live-dispatched QueryResult must be byte-identical"
    );
    let row_signature = match &dispatched.results[0] {
        ResultDiff::Add {
            data,
            row_signature,
        } => {
            assert_eq!(data, &json!({ "name": "Ada" }));
            *row_signature
        }
        other => panic!("expected Add result, got {other:?}"),
    };
    assert_eq!(
        fixture
            .live_results_writer
            .read_snapshot(QUERY_ID)
            .await
            .expect("read live results"),
        vec![(
            row_signature,
            rmp_serde::to_vec(&json!({ "name": "Ada" })).expect("serialize expected live row"),
        )]
    );
    assert_eq!(
        fixture
            .checkpoint_store
            .read_result_sequence(QUERY_ID)
            .await
            .expect("read result sequence"),
        Some(1)
    );
    assert_eq!(fixture.output_state.read().await.as_of_sequence(), 1);
    assert_eq!(fixture.position_handle.load(Ordering::Acquire), 1);
    assert_eq!(fixture.dispatch_count.load(Ordering::Acquire), 1);
    assert!(element_exists(&fixture, "person-success").await);
    assert!(!fixture.publication_recovery.is_required());

    stop_host(&fixture).await;
}

#[tokio::test]
async fn rocksdb_atomic_source_fault_matrix_rolls_back_and_fences() {
    let cases: Vec<(
        &'static str,
        BackendFault,
        Option<Arc<dyn QueryProcessingObserver>>,
    )> = vec![
        (
            "hook-after-staging",
            BackendFault::None,
            Some(Arc::new(FailAfterStagingObserver)),
        ),
        ("commit", BackendFault::Commit, None),
        ("writer-stage", BackendFault::LiveResultsStage, None),
    ];

    for (name, fault, observer) in cases {
        let temp_dir = tempfile::TempDir::new().expect("create temp directory");
        let (host, mut fixture) = build_host(
            temp_dir.path(),
            "MATCH (n:Person) RETURN n.name AS name",
            fault,
            false,
            observer,
        )
        .await;
        start_host(host, &fixture).await;

        enqueue_source_change(
            &fixture,
            &format!("person-{name}"),
            name,
            7,
            Bytes::from_static(b"failed-position"),
        )
        .await;
        wait_for_status(&fixture.base, ComponentStatus::Error).await;

        assert_no_durable_output(&fixture).await;
        assert_eq!(
            fixture.output_state.read().await.as_of_sequence(),
            0,
            "{name}: in-memory sequence changed"
        );
        assert_eq!(
            fixture.position_handle.load(Ordering::Acquire),
            u64::MAX,
            "{name}: source position was acknowledged"
        );
        assert_eq!(
            fixture.dispatch_count.load(Ordering::Acquire),
            0,
            "{name}: result was dispatched"
        );
        assert!(
            !fixture.publication_recovery.is_required(),
            "{name}: pre-commit failure incorrectly requires output reconciliation"
        );
        assert!(
            !element_exists(&fixture, &format!("person-{name}")).await,
            "{name}: core element state committed"
        );

        stop_host(&fixture).await;
    }
}

#[tokio::test]
async fn cancellation_during_atomic_hook_rolls_back_without_ack_or_dispatch() {
    let temp_dir = tempfile::TempDir::new().expect("create temp directory");
    let (staged_tx, staged_rx) = oneshot::channel();
    let (_release_tx, release_rx) = oneshot::channel();
    let observer = Arc::new(BlockAfterStagingObserver {
        staged_tx: Mutex::new(Some(staged_tx)),
        release_rx: AsyncMutex::new(Some(release_rx)),
    });
    let (host, mut fixture) = build_host(
        temp_dir.path(),
        "MATCH (n:Person) RETURN n.name AS name",
        BackendFault::None,
        false,
        Some(observer),
    )
    .await;
    start_host(host, &fixture).await;

    enqueue_source_change(
        &fixture,
        "person-cancelled",
        "Cancelled",
        9,
        Bytes::from_static(b"cancelled-position"),
    )
    .await;
    staged_rx
        .await
        .expect("observer should report fully staged output");
    assert!(fixture
        .outbox_writer
        .read_from(QUERY_ID, 0)
        .await
        .expect("read in-flight outbox")
        .is_empty());
    assert!(fixture
        .live_results_writer
        .read_snapshot(QUERY_ID)
        .await
        .expect("read in-flight live results")
        .is_empty());
    assert_eq!(
        fixture
            .checkpoint_store
            .read_result_sequence(QUERY_ID)
            .await
            .expect("read in-flight result sequence"),
        None,
        "snapshot readers must not observe a staged sequence before commit"
    );

    let task = fixture
        .base
        .task_handle
        .write()
        .await
        .take()
        .expect("host task handle");
    task.abort();
    assert!(task
        .await
        .expect_err("aborted host task should not complete")
        .is_cancelled());

    assert_no_durable_output(&fixture).await;
    assert!(!element_exists(&fixture, "person-cancelled").await);
    assert_eq!(fixture.output_state.read().await.as_of_sequence(), 0);
    assert_eq!(fixture.position_handle.load(Ordering::Acquire), u64::MAX);
    assert_eq!(fixture.dispatch_count.load(Ordering::Acquire), 0);
    fixture.future_queue_source.stop().await;
}

#[tokio::test]
async fn cancellation_after_commit_marks_output_recovery_required() {
    let temp_dir = tempfile::TempDir::new().expect("create temp directory");
    let (committed_tx, committed_rx) = oneshot::channel();
    let (_release_tx, release_rx) = oneshot::channel();
    let observer = Arc::new(BlockAfterCommitObserver {
        committed_tx: Mutex::new(Some(committed_tx)),
        release_rx: AsyncMutex::new(Some(release_rx)),
    });
    let (host, mut fixture) = build_host(
        temp_dir.path(),
        "MATCH (n:Person) RETURN n.name AS name",
        BackendFault::None,
        false,
        Some(observer),
    )
    .await;
    start_host(host, &fixture).await;

    enqueue_source_change(
        &fixture,
        "person-post-commit-cancelled",
        "Committed",
        10,
        Bytes::from_static(b"post-commit-cancelled-position"),
    )
    .await;
    committed_rx
        .await
        .expect("observer should report committed output");
    assert!(fixture.publication_recovery.is_required());

    let task = fixture
        .base
        .task_handle
        .write()
        .await
        .take()
        .expect("host task handle");
    task.abort();
    assert!(task
        .await
        .expect_err("aborted host task should not complete")
        .is_cancelled());

    assert!(fixture.publication_recovery.is_required());
    assert_eq!(fixture.output_state.read().await.as_of_sequence(), 0);
    assert_eq!(fixture.position_handle.load(Ordering::Acquire), u64::MAX);
    assert_eq!(fixture.dispatch_count.load(Ordering::Acquire), 0);
    assert_eq!(
        fixture
            .checkpoint_store
            .read_result_sequence(QUERY_ID)
            .await
            .expect("read committed result sequence"),
        Some(1)
    );
    let durable = fixture
        .outbox_writer
        .read_from(QUERY_ID, 0)
        .await
        .expect("read committed source outbox");
    let dependencies = QueryOutputDependencies::new(
        QUERY_ID.to_string(),
        fixture.output_state.clone(),
        fixture.dispatchers.clone(),
        Some(fixture.outbox_writer.clone()),
        Some(fixture.live_results_writer.clone()),
        Some(fixture.checkpoint_store.clone()),
        16,
        Arc::new(QueryOutputMetrics::new()),
    )
    .with_publication_recovery(fixture.publication_recovery.clone());
    QueryCompositeHost::reconcile_output_state(&fixture.mode, &dependencies)
        .await
        .expect("reconcile committed source output");
    let recovered = receive_result(&mut fixture.output_rx).await;
    assert_eq!(recovered.sequence, 1);
    assert_eq!(
        rmp_serde::to_vec(recovered.as_ref()).expect("serialize recovered source output"),
        durable[0].1
    );
    assert_eq!(fixture.output_state.read().await.as_of_sequence(), 1);
    assert!(!fixture.publication_recovery.is_required());
    fixture.future_queue_source.stop().await;
}

#[tokio::test]
async fn cancellation_while_source_commit_returns_late_keeps_recovery_fence_armed() {
    let temp_dir = tempfile::TempDir::new().expect("create temp directory");
    let (committed_tx, committed_rx) = oneshot::channel();
    let commit_block = Arc::new(CommitBlock {
        committed_tx: Mutex::new(Some(committed_tx)),
        commits_before_block: AtomicUsize::new(0),
    });
    let (host, fixture) = build_host(
        temp_dir.path(),
        "MATCH (n:Person) RETURN n.name AS name",
        BackendFault::CommitThenBlock(commit_block),
        false,
        None,
    )
    .await;
    start_host(host, &fixture).await;
    enqueue_source_change(
        &fixture,
        "person-commit-cancelled",
        "Committed",
        12,
        Bytes::from_static(b"commit-cancelled-position"),
    )
    .await;
    committed_rx.await.expect("source commit should complete");
    assert!(fixture.publication_recovery.is_required());

    let task = fixture
        .base
        .task_handle
        .write()
        .await
        .take()
        .expect("host task handle");
    task.abort();
    assert!(task
        .await
        .expect_err("aborted host task should not complete")
        .is_cancelled());

    assert!(fixture.publication_recovery.is_required());
    assert_eq!(fixture.output_state.read().await.as_of_sequence(), 0);
    assert_eq!(fixture.position_handle.load(Ordering::Acquire), u64::MAX);
    assert_eq!(fixture.dispatch_count.load(Ordering::Acquire), 0);
    assert_eq!(
        fixture
            .checkpoint_store
            .read_result_sequence(QUERY_ID)
            .await
            .expect("read committed result sequence"),
        Some(1)
    );
    assert_eq!(
        fixture
            .outbox_writer
            .read_from(QUERY_ID, 0)
            .await
            .expect("read committed outbox")
            .len(),
        1
    );
    fixture.future_queue_source.stop().await;
}

#[tokio::test]
async fn cancellation_while_future_commit_returns_late_keeps_recovery_fence_armed() {
    let temp_dir = tempfile::TempDir::new().expect("create temp directory");
    let (committed_tx, committed_rx) = oneshot::channel();
    let commit_block = Arc::new(CommitBlock {
        committed_tx: Mutex::new(Some(committed_tx)),
        commits_before_block: AtomicUsize::new(1),
    });
    let (host, mut fixture) = build_host(
        temp_dir.path(),
        "MATCH (n:Person) WHERE drasi.trueLater(true, 2000) RETURN n.name AS name",
        BackendFault::CommitThenBlock(commit_block),
        false,
        None,
    )
    .await;
    seed_due_future(&fixture).await;
    start_host(host, &fixture).await;
    enqueue_futures_due(&fixture).await;
    committed_rx
        .await
        .expect("due-future commit should complete");
    assert!(fixture.publication_recovery.is_required());

    let task = fixture
        .base
        .task_handle
        .write()
        .await
        .take()
        .expect("host task handle");
    task.abort();
    assert!(task
        .await
        .expect_err("aborted host task should not complete")
        .is_cancelled());

    assert!(fixture.publication_recovery.is_required());
    assert_eq!(fixture.output_state.read().await.as_of_sequence(), 0);
    assert_eq!(fixture.dispatch_count.load(Ordering::Acquire), 0);
    assert_eq!(
        fixture
            .future_queue
            .peek_due_time()
            .await
            .expect("peek committed future"),
        None
    );
    assert_eq!(
        fixture
            .checkpoint_store
            .read_result_sequence(QUERY_ID)
            .await
            .expect("read future result sequence"),
        Some(1)
    );
    let durable = fixture
        .outbox_writer
        .read_from(QUERY_ID, 0)
        .await
        .expect("read committed future outbox");
    let dependencies = QueryOutputDependencies::new(
        QUERY_ID.to_string(),
        fixture.output_state.clone(),
        fixture.dispatchers.clone(),
        Some(fixture.outbox_writer.clone()),
        Some(fixture.live_results_writer.clone()),
        Some(fixture.checkpoint_store.clone()),
        16,
        Arc::new(QueryOutputMetrics::new()),
    )
    .with_publication_recovery(fixture.publication_recovery.clone());
    QueryCompositeHost::reconcile_output_state(&fixture.mode, &dependencies)
        .await
        .expect("reconcile committed future");
    let recovered = receive_result(&mut fixture.output_rx).await;
    assert_eq!(recovered.sequence, 1);
    assert_eq!(
        rmp_serde::to_vec(recovered.as_ref()).expect("serialize recovered future"),
        durable[0].1
    );
    assert_eq!(fixture.output_state.read().await.as_of_sequence(), 1);
    assert!(!fixture.publication_recovery.is_required());
    fixture.future_queue_source.stop().await;
}

#[tokio::test]
async fn failure_after_commit_leaves_authoritative_output_without_publication() {
    let temp_dir = tempfile::TempDir::new().expect("create temp directory");
    let (host, fixture) = build_host(
        temp_dir.path(),
        "MATCH (n:Person) RETURN n.name AS name",
        BackendFault::None,
        false,
        Some(Arc::new(FailAfterCommitObserver)),
    )
    .await;
    start_host(host, &fixture).await;

    let source_position = Bytes::from_static(b"committed-before-publish");
    enqueue_source_change(
        &fixture,
        "person-committed",
        "Committed",
        11,
        source_position.clone(),
    )
    .await;
    wait_for_status(&fixture.base, ComponentStatus::Error).await;

    assert_eq!(
        fixture
            .checkpoint_store
            .read_checkpoint(SOURCE_ID)
            .await
            .expect("read committed checkpoint"),
        Some(SourceCheckpoint::new(11, Some(source_position)))
    );
    assert_eq!(
        fixture
            .outbox_writer
            .read_from(QUERY_ID, 0)
            .await
            .expect("read committed outbox")
            .len(),
        1
    );
    assert_eq!(
        fixture
            .live_results_writer
            .read_snapshot(QUERY_ID)
            .await
            .expect("read committed live results")
            .len(),
        1
    );
    assert_eq!(
        fixture
            .checkpoint_store
            .read_result_sequence(QUERY_ID)
            .await
            .expect("read committed result sequence"),
        Some(1)
    );
    assert!(element_exists(&fixture, "person-committed").await);
    assert_eq!(fixture.output_state.read().await.as_of_sequence(), 0);
    assert_eq!(fixture.position_handle.load(Ordering::Acquire), u64::MAX);
    assert_eq!(fixture.dispatch_count.load(Ordering::Acquire), 0);
    assert!(fixture.publication_recovery.is_required());

    stop_host(&fixture).await;
}

#[tokio::test]
async fn stale_prepared_sequence_after_commit_is_fatal_and_not_acknowledged() {
    let temp_dir = tempfile::TempDir::new().expect("create temp directory");
    let observer = Arc::new(StaleSequenceAfterCommitObserver {
        output_state: Mutex::new(None),
    });
    let (host, mut fixture) = build_host(
        temp_dir.path(),
        "MATCH (n:Person) RETURN n.name AS name",
        BackendFault::None,
        false,
        Some(observer.clone()),
    )
    .await;
    *observer
        .output_state
        .lock()
        .expect("stale-sequence state lock poisoned") = Some(fixture.output_state.clone());
    start_host(host, &fixture).await;

    enqueue_source_change(
        &fixture,
        "person-stale",
        "Stale",
        13,
        Bytes::from_static(b"stale-position"),
    )
    .await;
    wait_for_status(&fixture.base, ComponentStatus::Error).await;

    assert_eq!(
        fixture
            .checkpoint_store
            .read_result_sequence(QUERY_ID)
            .await
            .expect("read committed result sequence"),
        Some(1)
    );
    assert_eq!(fixture.position_handle.load(Ordering::Acquire), u64::MAX);
    assert_eq!(fixture.dispatch_count.load(Ordering::Acquire), 0);
    assert_eq!(fixture.output_state.read().await.as_of_sequence(), 1);
    assert!(fixture.publication_recovery.is_required());
    let dependencies = QueryOutputDependencies::new(
        QUERY_ID.to_string(),
        fixture.output_state.clone(),
        fixture.dispatchers.clone(),
        Some(fixture.outbox_writer.clone()),
        Some(fixture.live_results_writer.clone()),
        Some(fixture.checkpoint_store.clone()),
        16,
        Arc::new(QueryOutputMetrics::new()),
    )
    .with_publication_recovery(fixture.publication_recovery.clone());
    QueryCompositeHost::reconcile_output_state(&fixture.mode, &dependencies)
        .await
        .expect("reconcile stale process-local output");
    let recovered = receive_result(&mut fixture.output_rx).await;
    assert_eq!(recovered.sequence, 1);
    assert_eq!(recovered.results.len(), 1);
    assert!(!fixture.publication_recovery.is_required());

    stop_host(&fixture).await;
}

#[tokio::test]
async fn no_result_commits_checkpoint_and_ack_without_allocating_output_sequence() {
    let temp_dir = tempfile::TempDir::new().expect("create temp directory");
    let (ack_tx, ack_rx) = oneshot::channel();
    let observer = Arc::new(AckSignalObserver {
        ack_tx: Mutex::new(Some(ack_tx)),
    });
    let (host, fixture) = build_host(
        temp_dir.path(),
        "MATCH (n:Person) WHERE n.age > 100 RETURN n.name AS name",
        BackendFault::None,
        false,
        Some(observer),
    )
    .await;
    start_host(host, &fixture).await;

    let source_position = Bytes::from_static(b"no-result-position");
    enqueue_source_change(
        &fixture,
        "person-no-result",
        "Invisible",
        17,
        source_position.clone(),
    )
    .await;
    ack_rx.await.expect("source acknowledgement signal");

    assert_eq!(
        fixture
            .checkpoint_store
            .read_checkpoint(SOURCE_ID)
            .await
            .expect("read no-result checkpoint"),
        Some(SourceCheckpoint::new(17, Some(source_position)))
    );
    assert!(element_exists(&fixture, "person-no-result").await);
    assert_eq!(fixture.output_state.read().await.as_of_sequence(), 0);
    assert_eq!(fixture.position_handle.load(Ordering::Acquire), 17);
    assert_eq!(fixture.dispatch_count.load(Ordering::Acquire), 0);
    assert!(fixture
        .outbox_writer
        .read_from(QUERY_ID, 0)
        .await
        .expect("read no-result outbox")
        .is_empty());
    assert!(fixture
        .live_results_writer
        .read_snapshot(QUERY_ID)
        .await
        .expect("read no-result live results")
        .is_empty());
    assert_eq!(
        fixture
            .checkpoint_store
            .read_result_sequence(QUERY_ID)
            .await
            .expect("read no-result result sequence"),
        None
    );

    stop_host(&fixture).await;
}

async fn seed_due_future(fixture: &AtomicHostFixture) {
    let initial = fixture
        .continuous_query
        .process_source_change(person_change("person-future", "Future", 1_000))
        .await
        .expect("seed due future");
    assert!(initial.is_empty());
    assert_eq!(
        fixture
            .future_queue
            .peek_due_time()
            .await
            .expect("peek seeded future"),
        Some(2_000)
    );
}

async fn seed_later_future(fixture: &AtomicHostFixture) -> u64 {
    let due_time = current_time_millis().saturating_add(60_000);
    fixture
        .session_control
        .begin()
        .await
        .expect("begin later-future session");
    fixture
        .future_queue
        .push(
            PushType::Always,
            999,
            u64::MAX,
            &ElementReference::new(SOURCE_ID, "person-future"),
            due_time.saturating_sub(1),
            due_time,
        )
        .await
        .expect("stage later future");
    fixture
        .session_control
        .commit()
        .await
        .expect("commit later future");
    due_time
}

async fn enqueue_futures_due(fixture: &AtomicHostFixture) {
    assert!(
        fixture
            .priority_queue
            .enqueue(Arc::new(SourceEventWrapper::new(
                "__future_queue__".to_string(),
                SourceEvent::Control(SourceControl::FuturesDue),
                chrono::Utc::now(),
            )))
            .await
    );
}

#[tokio::test]
async fn rocksdb_due_future_output_commits_with_pop_before_dispatch() {
    let temp_dir = tempfile::TempDir::new().expect("create temp directory");
    let (host, mut fixture) = build_host(
        temp_dir.path(),
        "MATCH (n:Person) WHERE drasi.trueLater(true, 2000) RETURN n.name AS name",
        BackendFault::None,
        false,
        None,
    )
    .await;
    seed_due_future(&fixture).await;
    let later_due_time = seed_later_future(&fixture).await;
    start_host(host, &fixture).await;
    enqueue_futures_due(&fixture).await;

    let dispatched = receive_result(&mut fixture.output_rx).await;
    assert_eq!(dispatched.sequence, 1);
    assert_eq!(dispatched.metadata["source_id"], SOURCE_ID);
    assert_eq!(
        fixture
            .future_queue
            .peek_due_time()
            .await
            .expect("peek retained later future"),
        Some(later_due_time),
        "one due signal must not consume a future scheduled for later"
    );
    assert_eq!(
        fixture
            .outbox_writer
            .read_from(QUERY_ID, 0)
            .await
            .expect("read future outbox")[0]
            .1,
        rmp_serde::to_vec(dispatched.as_ref()).expect("serialize future result")
    );
    assert_eq!(
        fixture
            .live_results_writer
            .read_snapshot(QUERY_ID)
            .await
            .expect("read future live results")
            .len(),
        1
    );
    assert_eq!(
        fixture
            .checkpoint_store
            .read_result_sequence(QUERY_ID)
            .await
            .expect("read future result sequence"),
        Some(1)
    );
    assert!(fixture
        .checkpoint_store
        .read_checkpoint(SOURCE_ID)
        .await
        .expect("read future source checkpoint")
        .is_none());
    assert_eq!(fixture.dispatch_count.load(Ordering::Acquire), 1);

    stop_host(&fixture).await;
}

#[tokio::test]
async fn due_future_stage_failure_retains_future_and_publishes_nothing() {
    let temp_dir = tempfile::TempDir::new().expect("create temp directory");
    let (host, fixture) = build_host(
        temp_dir.path(),
        "MATCH (n:Person) WHERE drasi.trueLater(true, 2000) RETURN n.name AS name",
        BackendFault::None,
        false,
        Some(Arc::new(FailAfterStagingObserver)),
    )
    .await;
    seed_due_future(&fixture).await;
    start_host(host, &fixture).await;
    enqueue_futures_due(&fixture).await;
    wait_for_status(&fixture.base, ComponentStatus::Error).await;

    assert_eq!(
        fixture
            .future_queue
            .peek_due_time()
            .await
            .expect("peek retained future"),
        Some(2_000)
    );
    assert_no_durable_output(&fixture).await;
    assert_eq!(fixture.output_state.read().await.as_of_sequence(), 0);
    assert_eq!(fixture.dispatch_count.load(Ordering::Acquire), 0);

    stop_host(&fixture).await;
}

#[tokio::test]
async fn due_future_post_commit_failure_requires_output_reconciliation() {
    let temp_dir = tempfile::TempDir::new().expect("create temp directory");
    let (host, fixture) = build_host(
        temp_dir.path(),
        "MATCH (n:Person) WHERE drasi.trueLater(true, 2000) RETURN n.name AS name",
        BackendFault::None,
        false,
        Some(Arc::new(FailAfterCommitObserver)),
    )
    .await;
    seed_due_future(&fixture).await;
    start_host(host, &fixture).await;
    enqueue_futures_due(&fixture).await;
    wait_for_status(&fixture.base, ComponentStatus::Error).await;

    assert_eq!(
        fixture
            .future_queue
            .peek_due_time()
            .await
            .expect("peek committed future"),
        None,
        "the future pop committed before publication failed"
    );
    assert_eq!(
        fixture
            .checkpoint_store
            .read_result_sequence(QUERY_ID)
            .await
            .expect("read future result sequence"),
        Some(1)
    );
    assert_eq!(
        fixture
            .outbox_writer
            .read_from(QUERY_ID, 0)
            .await
            .expect("read future outbox")
            .len(),
        1
    );
    assert_eq!(fixture.output_state.read().await.as_of_sequence(), 0);
    assert_eq!(fixture.dispatch_count.load(Ordering::Acquire), 0);
    assert!(fixture.publication_recovery.is_required());

    stop_host(&fixture).await;
}

#[tokio::test]
async fn explicit_legacy_mode_preserves_a1_ordering_and_skips_atomic_observers() {
    let temp_dir = tempfile::TempDir::new().expect("create temp directory");
    let observer = Arc::new(CountingObserver::default());
    let (host, mut fixture) = build_host(
        temp_dir.path(),
        "MATCH (n:Person) RETURN n.name AS name",
        BackendFault::None,
        true,
        Some(observer.clone()),
    )
    .await;
    start_host(host, &fixture).await;

    enqueue_source_change(
        &fixture,
        "person-legacy",
        "Legacy",
        19,
        Bytes::from_static(b"legacy-position"),
    )
    .await;
    let dispatched = receive_result(&mut fixture.output_rx).await;

    assert_eq!(dispatched.sequence, 1);
    assert_eq!(fixture.base.get_status().await, ComponentStatus::Running);
    assert_eq!(fixture.position_handle.load(Ordering::Acquire), 19);
    assert!(element_exists(&fixture, "person-legacy").await);
    assert_eq!(
        fixture
            .checkpoint_store
            .read_checkpoint(SOURCE_ID)
            .await
            .expect("read legacy checkpoint")
            .expect("legacy checkpoint committed")
            .sequence,
        19
    );
    assert_eq!(
        fixture
            .outbox_writer
            .read_from(QUERY_ID, 0)
            .await
            .expect("read legacy outbox")
            .len(),
        1
    );
    assert_eq!(
        fixture
            .checkpoint_store
            .read_result_sequence(QUERY_ID)
            .await
            .expect("read legacy result sequence"),
        Some(1)
    );
    assert_eq!(observer.staged.load(Ordering::Acquire), 0);
    assert_eq!(observer.committed.load(Ordering::Acquire), 0);
    assert_eq!(fixture.dispatch_count.load(Ordering::Acquire), 1);

    stop_host(&fixture).await;
}

#[tokio::test]
async fn persistent_legacy_output_failures_fence_immediately_and_strict_restart_rejects() {
    for (name, fault, expected_stage) in [
        ("outbox", BackendFault::OutboxStage, "outbox"),
        (
            "live-results",
            BackendFault::LiveResultsStage,
            "live-result",
        ),
        (
            "result-sequence",
            BackendFault::ResultSequenceWrite,
            "result sequence",
        ),
    ] {
        let temp_dir = tempfile::TempDir::new().expect("create temp directory");
        let (host, fixture) = build_host(
            temp_dir.path(),
            "MATCH (n:Person) RETURN n.name AS name",
            fault,
            true,
            None,
        )
        .await;
        start_host(host, &fixture).await;

        enqueue_source_change(
            &fixture,
            &format!("legacy-{name}-first"),
            "First",
            1,
            Bytes::from(format!("legacy-{name}-position-1")),
        )
        .await;
        enqueue_source_change(
            &fixture,
            &format!("legacy-{name}-later"),
            "Later",
            2,
            Bytes::from(format!("legacy-{name}-position-2")),
        )
        .await;
        wait_for_status(&fixture.base, ComponentStatus::Error).await;

        assert_eq!(
            fixture
                .checkpoint_store
                .read_checkpoint(SOURCE_ID)
                .await
                .expect("read Legacy checkpoint")
                .expect("causative Legacy checkpoint")
                .sequence,
            1,
            "{name}: later source sequence advanced after the persistence failure"
        );
        assert_eq!(
            fixture.position_handle.load(Ordering::Acquire),
            u64::MAX,
            "{name}: failed Legacy output was acknowledged"
        );
        assert_eq!(
            fixture.dispatch_count.load(Ordering::Acquire),
            0,
            "{name}: failed Legacy output was dispatched"
        );
        assert!(
            fixture.publication_recovery.is_required(),
            "{name}: persistent Legacy failure did not arm recovery"
        );

        let fresh_state = Arc::new(RwLock::new(QueryOutputState::new(16)));
        let dependencies = QueryOutputDependencies::new(
            QUERY_ID.to_string(),
            fresh_state,
            Arc::new(RwLock::new(Vec::new())),
            Some(fixture.outbox_writer.clone()),
            Some(fixture.live_results_writer.clone()),
            Some(fixture.checkpoint_store.clone()),
            16,
            Arc::new(QueryOutputMetrics::new()),
        );
        let error = QueryCompositeHost::reconcile_output_state(
            &QueryProcessingMode::legacy(),
            &dependencies,
        )
        .await
        .expect_err("Strict restart must reject partial Legacy output");
        assert!(
            format!("{error:#}").contains(expected_stage)
                || format!("{error:#}").contains("durable output is inconsistent"),
            "{name}: unexpected reconciliation error: {error:#}"
        );

        stop_host(&fixture).await;
    }
}

#[tokio::test]
async fn capability_selection_requires_the_complete_created_indexes_bundle() {
    let temp_dir = tempfile::TempDir::new().expect("create temp directory");
    let provider = RocksDbIndexProvider::new(temp_dir.path(), true, false);
    let mut created: CreatedIndexes = provider
        .create_indexes("capability-selection")
        .await
        .expect("create RocksDB indexes");
    assert_eq!(
        QueryProcessingMode::from_created_indexes(&created).diagnostic_name(),
        "atomic"
    );

    created.outbox_writer = None;
    assert_eq!(
        QueryProcessingMode::from_created_indexes(&created).diagnostic_name(),
        "legacy",
        "a Garnet/custom/missing-writer-shaped bundle must not enter atomic mode"
    );
}

#[tokio::test]
async fn atomic_startup_hydrates_durable_output_and_continues_sequence() {
    let temp_dir = tempfile::TempDir::new().expect("create temp directory");
    let provider = RocksDbIndexProvider::new(temp_dir.path(), true, false);
    let created = provider
        .create_indexes("hydrated-sequence")
        .await
        .expect("create RocksDB indexes");
    let mode = QueryProcessingMode::from_created_indexes(&created);
    let checkpoint_store = created
        .checkpoint_store
        .as_ref()
        .expect("checkpoint store")
        .clone();
    let outbox_writer = created
        .outbox_writer
        .as_ref()
        .expect("outbox writer")
        .clone();
    let live_results_writer = created
        .live_results_writer
        .as_ref()
        .expect("live-results writer")
        .clone();
    let durable_result = QueryResult::new(
        "hydrated-sequence".to_string(),
        41,
        chrono::Utc::now(),
        vec![ResultDiff::Add {
            data: json!({ "name": "Durable" }),
            row_signature: 7,
        }],
        HashMap::new(),
    );
    let durable_bytes = rmp_serde::to_vec(&durable_result).expect("serialize durable result");
    created
        .set
        .session_control
        .begin()
        .await
        .expect("begin durable seed transaction");
    outbox_writer
        .append("hydrated-sequence", 41, &durable_bytes)
        .await
        .expect("seed durable outbox");
    let row_bytes =
        rmp_serde::to_vec(&json!({ "name": "Durable" })).expect("serialize durable live row");
    live_results_writer
        .apply_mutations(
            "hydrated-sequence",
            &[RowMutation {
                row_signature: 7,
                data: Some(&row_bytes),
            }],
        )
        .await
        .expect("seed durable live row");
    checkpoint_store
        .write_result_sequence("hydrated-sequence", 41)
        .await
        .expect("seed durable result sequence");
    created
        .set
        .session_control
        .commit()
        .await
        .expect("commit durable seed transaction");

    let output_state = Arc::new(RwLock::new(QueryOutputState::new(16)));
    let dependencies = QueryOutputDependencies::new(
        "hydrated-sequence".to_string(),
        output_state.clone(),
        Arc::new(RwLock::new(Vec::new())),
        Some(outbox_writer),
        Some(live_results_writer),
        Some(checkpoint_store),
        16,
        Arc::new(QueryOutputMetrics::new()),
    );
    QueryCompositeHost::reconcile_output_state(&mode, &dependencies)
        .await
        .expect("hydrate durable output");
    let output = OutputPublicationStage::new(dependencies);
    let contexts = [QueryPartEvaluationContext::Adding {
        after: QueryVariables::from([(
            Box::<str>::from("name"),
            VariableValue::String("A7".to_string()),
        )]),
        row_signature: 7,
    }];

    let prepared = output
        .prepare_atomic(&contexts, SOURCE_ID, ProfilingMetadata::default())
        .await
        .expect("prepare hydrated output");
    assert_eq!(prepared.result.sequence, 42);
    let state = output_state.read().await;
    assert_eq!(state.as_of_sequence(), 41);
    assert_eq!(state.get_result(&7), Some(&json!({ "name": "Durable" })));
    assert_eq!(
        rmp_serde::to_vec(
            state
                .fetch_outbox_after(40)
                .expect("hydrated outbox")
                .first()
                .expect("durable result")
                .as_ref()
        )
        .expect("serialize hydrated result"),
        durable_bytes
    );
}

#[tokio::test]
async fn atomic_startup_rejects_a_gapped_durable_outbox() {
    let temp_dir = tempfile::TempDir::new().expect("create temp directory");
    let provider = RocksDbIndexProvider::new(temp_dir.path(), true, false);
    let created = provider
        .create_indexes("gapped-output")
        .await
        .expect("create RocksDB indexes");
    let mode = QueryProcessingMode::from_created_indexes(&created);
    let checkpoint_store = created.checkpoint_store.clone().expect("checkpoint store");
    let outbox_writer = created.outbox_writer.clone().expect("outbox writer");
    let live_results_writer = created
        .live_results_writer
        .clone()
        .expect("live-results writer");
    for sequence in [40, 42] {
        let result = QueryResult::new(
            "gapped-output".to_string(),
            sequence,
            chrono::Utc::now(),
            vec![ResultDiff::Add {
                data: json!({ "sequence": sequence }),
                row_signature: sequence,
            }],
            HashMap::new(),
        );
        outbox_writer
            .append(
                "gapped-output",
                sequence,
                &rmp_serde::to_vec(&result).expect("serialize gapped result"),
            )
            .await
            .expect("seed gapped outbox");
    }
    let row_bytes =
        rmp_serde::to_vec(&json!({ "sequence": 42 })).expect("serialize latest live row");
    live_results_writer
        .apply_mutations(
            "gapped-output",
            &[RowMutation {
                row_signature: 42,
                data: Some(&row_bytes),
            }],
        )
        .await
        .expect("seed latest live row");
    checkpoint_store
        .write_result_sequence("gapped-output", 42)
        .await
        .expect("seed durable result sequence");

    let output_state = Arc::new(RwLock::new(QueryOutputState::new(16)));
    let dependencies = QueryOutputDependencies::new(
        "gapped-output".to_string(),
        output_state.clone(),
        Arc::new(RwLock::new(Vec::new())),
        Some(outbox_writer),
        Some(live_results_writer),
        Some(checkpoint_store),
        16,
        Arc::new(QueryOutputMetrics::new()),
    );
    let error = QueryCompositeHost::reconcile_output_state(&mode, &dependencies)
        .await
        .expect_err("gapped durable outbox must be rejected");
    assert!(format!("{error:#}").contains("gap"));
    assert_eq!(output_state.read().await.as_of_sequence(), 0);
}

#[tokio::test]
async fn atomic_startup_rejects_missing_sequence_and_live_snapshot_artifacts() {
    let sequence_dir = tempfile::TempDir::new().expect("create sequence temp directory");
    let sequence_provider = RocksDbIndexProvider::new(sequence_dir.path(), true, false);
    let sequence_created = sequence_provider
        .create_indexes("missing-sequence")
        .await
        .expect("create missing-sequence indexes");
    let sequence_mode = QueryProcessingMode::from_created_indexes(&sequence_created);
    let sequence_outbox = sequence_created
        .outbox_writer
        .clone()
        .expect("sequence outbox");
    let sequence_live = sequence_created
        .live_results_writer
        .clone()
        .expect("sequence live results");
    let result = QueryResult::new(
        "missing-sequence".to_string(),
        1,
        chrono::Utc::now(),
        vec![ResultDiff::Add {
            data: json!({ "name": "missing-sequence" }),
            row_signature: 1,
        }],
        HashMap::new(),
    );
    sequence_outbox
        .append(
            "missing-sequence",
            1,
            &rmp_serde::to_vec(&result).expect("serialize missing-sequence result"),
        )
        .await
        .expect("seed outbox without result sequence");
    let row = rmp_serde::to_vec(&json!({ "name": "missing-sequence" }))
        .expect("serialize missing-sequence row");
    sequence_live
        .apply_mutations(
            "missing-sequence",
            &[RowMutation {
                row_signature: 1,
                data: Some(&row),
            }],
        )
        .await
        .expect("seed live row without result sequence");
    let sequence_state = Arc::new(RwLock::new(QueryOutputState::new(16)));
    let sequence_dependencies = QueryOutputDependencies::new(
        "missing-sequence".to_string(),
        sequence_state,
        Arc::new(RwLock::new(Vec::new())),
        Some(sequence_outbox),
        Some(sequence_live),
        sequence_created.checkpoint_store.clone(),
        16,
        Arc::new(QueryOutputMetrics::new()),
    );
    let error = QueryCompositeHost::reconcile_output_state(&sequence_mode, &sequence_dependencies)
        .await
        .expect_err("outbox without a result sequence must be rejected");
    assert!(format!("{error:#}").contains("result sequence is 0"));

    let live_dir = tempfile::TempDir::new().expect("create live-result temp directory");
    let live_provider = RocksDbIndexProvider::new(live_dir.path(), true, false);
    let live_created = live_provider
        .create_indexes("missing-live")
        .await
        .expect("create missing-live indexes");
    let live_mode = QueryProcessingMode::from_created_indexes(&live_created);
    let live_checkpoint = live_created
        .checkpoint_store
        .clone()
        .expect("live checkpoint store");
    let live_outbox = live_created.outbox_writer.clone().expect("live outbox");
    let live_writer = live_created
        .live_results_writer
        .clone()
        .expect("live results");
    let result = QueryResult::new(
        "missing-live".to_string(),
        1,
        chrono::Utc::now(),
        vec![ResultDiff::Add {
            data: json!({ "name": "missing-live" }),
            row_signature: 1,
        }],
        HashMap::new(),
    );
    live_outbox
        .append(
            "missing-live",
            1,
            &rmp_serde::to_vec(&result).expect("serialize missing-live result"),
        )
        .await
        .expect("seed outbox without live row");
    live_checkpoint
        .write_result_sequence("missing-live", 1)
        .await
        .expect("seed result sequence without live row");
    let live_dependencies = QueryOutputDependencies::new(
        "missing-live".to_string(),
        Arc::new(RwLock::new(QueryOutputState::new(16))),
        Arc::new(RwLock::new(Vec::new())),
        Some(live_outbox),
        Some(live_writer),
        Some(live_checkpoint),
        16,
        Arc::new(QueryOutputMetrics::new()),
    );
    let error = QueryCompositeHost::reconcile_output_state(&live_mode, &live_dependencies)
        .await
        .expect_err("outbox mutation without a live row must be rejected");
    assert!(format!("{error:#}").contains("live-result row"));
}

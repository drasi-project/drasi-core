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

//! Characterization tests for the fixed Source -> Continuous Query -> Reaction pipeline.
//!
//! These tests intentionally pin the current transaction and output boundaries. In
//! particular, they document the known durability gap where source progress commits
//! before query output is persisted. A later stack layer will intentionally invert
//! that behavior by staging output inside the core transaction.

use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, Mutex},
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
    in_memory_index::{
        in_memory_element_index::InMemoryElementIndex, in_memory_future_queue::InMemoryFutureQueue,
        in_memory_result_index::InMemoryResultIndex,
    },
    interface::{
        CheckpointStore, CreatedIndexes, ElementIndex, FutureQueue, IndexBackendPlugin, IndexError,
        IndexSet, LiveResultsWriter, OutboxWriter, RowMutation, SessionControl, SourceCheckpoint,
    },
    middleware::MiddlewareTypeRegistry,
    models::{Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange},
    query::QueryBuilder,
};
use drasi_functions_cypher::CypherFunctionSet;
use drasi_query_cypher::CypherParser;
use serde_json::json;
use tokio::sync::{mpsc, RwLock};

use super::*;
use crate::{
    channels::{
        ChangeDispatcher, ChangeReceiver, ComponentStatus, QueryResult, ResultDiff, SourceEvent,
        SourceEventWrapper, SubscriptionResponse,
    },
    config::{QueryLanguage, SourceSubscriptionConfig},
    indexes::config::StorageBackendRef,
    profiling::ProfilingMetadata,
    sources::{SourceBase, SourceBaseParams},
};

const SOURCE_ID: &str = "pipeline-source";
const QUERY_ID: &str = "pipeline-query";

#[derive(Debug, Clone, PartialEq, Eq)]
enum TraceEvent {
    SessionBegin,
    CheckpointStaged {
        source_id: String,
        sequence: u64,
        source_position: Option<Bytes>,
    },
    SessionCommit,
    SessionRollback,
    OutboxAppendAttempt {
        query_id: String,
        sequence: u64,
    },
    LiveResultsApplyAttempt {
        query_id: String,
        mutation_count: usize,
    },
    ResultSequenceWritten {
        query_id: String,
        sequence: u64,
    },
    ReactionDispatch {
        sequence: u64,
    },
}

#[derive(Clone, Default)]
struct EventTrace {
    events: Arc<Mutex<Vec<TraceEvent>>>,
}

impl EventTrace {
    fn record(&self, event: TraceEvent) {
        self.events.lock().unwrap().push(event);
    }

    fn clear(&self) {
        self.events.lock().unwrap().clear();
    }

    fn snapshot(&self) -> Vec<TraceEvent> {
        self.events.lock().unwrap().clone()
    }
}

#[derive(Default)]
struct TransactionState {
    active: bool,
    pending_checkpoints: HashMap<String, SourceCheckpoint>,
    committed_checkpoints: HashMap<String, SourceCheckpoint>,
    config_hash: Option<u64>,
    result_sequences: HashMap<String, u64>,
}

struct RecordingSessionControl {
    state: Arc<Mutex<TransactionState>>,
    trace: EventTrace,
}

#[async_trait]
impl SessionControl for RecordingSessionControl {
    async fn begin(&self) -> Result<(), IndexError> {
        let mut state = self.state.lock().unwrap();
        if state.active {
            return Err(IndexError::other(std::io::Error::other(
                "test session already active",
            )));
        }
        state.active = true;
        self.trace.record(TraceEvent::SessionBegin);
        Ok(())
    }

    async fn commit(&self) -> Result<(), IndexError> {
        let mut state = self.state.lock().unwrap();
        if !state.active {
            return Err(IndexError::other(std::io::Error::other(
                "test session is not active",
            )));
        }
        let pending = std::mem::take(&mut state.pending_checkpoints);
        state.committed_checkpoints.extend(pending);
        state.active = false;
        self.trace.record(TraceEvent::SessionCommit);
        Ok(())
    }

    fn rollback(&self) -> Result<(), IndexError> {
        let mut state = self.state.lock().unwrap();
        state.pending_checkpoints.clear();
        state.active = false;
        self.trace.record(TraceEvent::SessionRollback);
        Ok(())
    }
}

struct TransactionalCheckpointStore {
    state: Arc<Mutex<TransactionState>>,
    trace: EventTrace,
}

#[async_trait]
impl CheckpointStore for TransactionalCheckpointStore {
    fn is_persistent(&self) -> bool {
        true
    }

    async fn stage_checkpoint(
        &self,
        source_id: &str,
        sequence: u64,
        source_position: Option<&Bytes>,
    ) -> Result<(), IndexError> {
        let checkpoint = SourceCheckpoint::new(sequence, source_position.cloned());
        let mut state = self.state.lock().unwrap();
        if !state.active {
            return Err(IndexError::other(std::io::Error::other(
                "checkpoint staged outside a test session",
            )));
        }
        state
            .pending_checkpoints
            .insert(source_id.to_string(), checkpoint.clone());
        self.trace.record(TraceEvent::CheckpointStaged {
            source_id: source_id.to_string(),
            sequence,
            source_position: checkpoint.source_position,
        });
        Ok(())
    }

    async fn read_checkpoint(
        &self,
        source_id: &str,
    ) -> Result<Option<SourceCheckpoint>, IndexError> {
        Ok(self
            .state
            .lock()
            .unwrap()
            .committed_checkpoints
            .get(source_id)
            .cloned())
    }

    async fn read_all_checkpoints(&self) -> Result<HashMap<String, SourceCheckpoint>, IndexError> {
        Ok(self.state.lock().unwrap().committed_checkpoints.clone())
    }

    async fn clear_checkpoints(&self) -> Result<(), IndexError> {
        let mut state = self.state.lock().unwrap();
        state.pending_checkpoints.clear();
        state.committed_checkpoints.clear();
        state.config_hash = None;
        state.result_sequences.clear();
        Ok(())
    }

    async fn write_config_hash(&self, hash: u64) -> Result<(), IndexError> {
        self.state.lock().unwrap().config_hash = Some(hash);
        Ok(())
    }

    async fn read_config_hash(&self) -> Result<Option<u64>, IndexError> {
        Ok(self.state.lock().unwrap().config_hash)
    }

    async fn write_result_sequence(&self, query_id: &str, sequence: u64) -> Result<(), IndexError> {
        self.state
            .lock()
            .unwrap()
            .result_sequences
            .insert(query_id.to_string(), sequence);
        self.trace.record(TraceEvent::ResultSequenceWritten {
            query_id: query_id.to_string(),
            sequence,
        });
        Ok(())
    }

    async fn read_result_sequence(&self, query_id: &str) -> Result<Option<u64>, IndexError> {
        Ok(self
            .state
            .lock()
            .unwrap()
            .result_sequences
            .get(query_id)
            .copied())
    }
}

struct RecordingOutputStore {
    fail_writes: bool,
    trace: EventTrace,
    outbox: RwLock<HashMap<String, BTreeMap<u64, Vec<u8>>>>,
    live_results: RwLock<HashMap<String, BTreeMap<u64, Vec<u8>>>>,
}

impl RecordingOutputStore {
    fn new(fail_writes: bool, trace: EventTrace) -> Self {
        Self {
            fail_writes,
            trace,
            outbox: RwLock::new(HashMap::new()),
            live_results: RwLock::new(HashMap::new()),
        }
    }

    fn injected_failure() -> IndexError {
        IndexError::other(std::io::Error::other("injected output persistence failure"))
    }
}

#[async_trait]
impl OutboxWriter for RecordingOutputStore {
    async fn append(&self, query_id: &str, sequence: u64, data: &[u8]) -> Result<(), IndexError> {
        self.trace.record(TraceEvent::OutboxAppendAttempt {
            query_id: query_id.to_string(),
            sequence,
        });
        if self.fail_writes {
            return Err(Self::injected_failure());
        }
        self.outbox
            .write()
            .await
            .entry(query_id.to_string())
            .or_default()
            .insert(sequence, data.to_vec());
        Ok(())
    }

    async fn read_from(
        &self,
        query_id: &str,
        after_sequence: u64,
    ) -> Result<Vec<(u64, Vec<u8>)>, IndexError> {
        Ok(self
            .outbox
            .read()
            .await
            .get(query_id)
            .map(|entries| {
                entries
                    .range((after_sequence.saturating_add(1))..)
                    .map(|(sequence, data)| (*sequence, data.clone()))
                    .collect()
            })
            .unwrap_or_default())
    }

    async fn read_latest_sequence(&self, query_id: &str) -> Result<Option<u64>, IndexError> {
        Ok(self
            .outbox
            .read()
            .await
            .get(query_id)
            .and_then(|entries| entries.last_key_value().map(|(sequence, _)| *sequence)))
    }

    async fn clear(&self, query_id: &str) -> Result<(), IndexError> {
        self.outbox.write().await.remove(query_id);
        Ok(())
    }

    async fn trim_to_capacity(&self, query_id: &str, capacity: usize) -> Result<usize, IndexError> {
        let mut outbox = self.outbox.write().await;
        let Some(entries) = outbox.get_mut(query_id) else {
            return Ok(0);
        };
        let mut removed = 0;
        while entries.len() > capacity {
            let sequence = *entries.first_key_value().unwrap().0;
            entries.remove(&sequence);
            removed += 1;
        }
        Ok(removed)
    }
}

#[async_trait]
impl LiveResultsWriter for RecordingOutputStore {
    async fn apply_mutations(
        &self,
        query_id: &str,
        mutations: &[RowMutation<'_>],
    ) -> Result<(), IndexError> {
        self.trace.record(TraceEvent::LiveResultsApplyAttempt {
            query_id: query_id.to_string(),
            mutation_count: mutations.len(),
        });
        if self.fail_writes {
            return Err(Self::injected_failure());
        }
        let mut store = self.live_results.write().await;
        let rows = store.entry(query_id.to_string()).or_default();
        for mutation in mutations {
            match mutation.data {
                Some(data) => {
                    rows.insert(mutation.row_signature, data.to_vec());
                }
                None => {
                    rows.remove(&mutation.row_signature);
                }
            }
        }
        Ok(())
    }

    async fn read_snapshot(&self, query_id: &str) -> Result<Vec<(u64, Vec<u8>)>, IndexError> {
        Ok(self
            .live_results
            .read()
            .await
            .get(query_id)
            .map(|rows| {
                rows.iter()
                    .map(|(signature, data)| (*signature, data.clone()))
                    .collect()
            })
            .unwrap_or_default())
    }

    async fn clear(&self, query_id: &str) -> Result<(), IndexError> {
        self.live_results.write().await.remove(query_id);
        Ok(())
    }

    async fn row_count(&self, query_id: &str) -> Result<usize, IndexError> {
        Ok(self
            .live_results
            .read()
            .await
            .get(query_id)
            .map_or(0, BTreeMap::len))
    }
}

struct RecordingDispatcher {
    trace: EventTrace,
    tx: mpsc::UnboundedSender<Arc<QueryResult>>,
}

#[async_trait]
impl ChangeDispatcher<QueryResult> for RecordingDispatcher {
    async fn dispatch_change(&self, change: Arc<QueryResult>) -> anyhow::Result<()> {
        self.trace.record(TraceEvent::ReactionDispatch {
            sequence: change.sequence,
        });
        self.tx
            .send(change)
            .map_err(|_| anyhow::anyhow!("test result receiver dropped"))
    }

    async fn create_receiver(&self) -> anyhow::Result<Box<dyn ChangeReceiver<QueryResult>>> {
        Err(anyhow::anyhow!(
            "recording dispatcher does not expose a ChangeReceiver"
        ))
    }
}

struct CharacterizationBackend {
    trace: EventTrace,
    element_index: Arc<InMemoryElementIndex>,
    result_index: Arc<InMemoryResultIndex>,
    future_queue: Arc<InMemoryFutureQueue>,
    session_control: Arc<RecordingSessionControl>,
    checkpoint_store: Arc<TransactionalCheckpointStore>,
    output_store: Arc<RecordingOutputStore>,
}

impl CharacterizationBackend {
    fn new(fail_output_writes: bool) -> Self {
        let trace = EventTrace::default();
        let state = Arc::new(Mutex::new(TransactionState::default()));
        Self {
            trace: trace.clone(),
            element_index: Arc::new(InMemoryElementIndex::new()),
            result_index: Arc::new(InMemoryResultIndex::new()),
            future_queue: Arc::new(InMemoryFutureQueue::new()),
            session_control: Arc::new(RecordingSessionControl {
                state: state.clone(),
                trace: trace.clone(),
            }),
            checkpoint_store: Arc::new(TransactionalCheckpointStore {
                state,
                trace: trace.clone(),
            }),
            output_store: Arc::new(RecordingOutputStore::new(fail_output_writes, trace)),
        }
    }
}

#[async_trait]
impl IndexBackendPlugin for CharacterizationBackend {
    async fn create_indexes(&self, _query_id: &str) -> Result<CreatedIndexes, IndexError> {
        Ok(CreatedIndexes {
            set: IndexSet {
                element_index: self.element_index.clone(),
                archive_index: self.element_index.clone(),
                result_index: self.result_index.clone(),
                future_queue: self.future_queue.clone(),
                session_control: self.session_control.clone(),
            },
            checkpoint_store: Some(self.checkpoint_store.clone()),
            outbox_writer: Some(self.output_store.clone()),
            live_results_writer: Some(self.output_store.clone()),
        })
    }

    fn is_volatile(&self) -> bool {
        false
    }
}

struct CharacterizationSource {
    base: SourceBase,
    subscription_tx: mpsc::UnboundedSender<crate::config::SourceSubscriptionSettings>,
}

impl CharacterizationSource {
    fn new(
        id: &str,
        subscription_tx: mpsc::UnboundedSender<crate::config::SourceSubscriptionSettings>,
    ) -> Self {
        Self {
            base: SourceBase::new(SourceBaseParams::new(id)).unwrap(),
            subscription_tx,
        }
    }

    async fn inject_with_profiling(
        &self,
        change: SourceChange,
        sequence: u64,
        source_position: Bytes,
        profiling: ProfilingMetadata,
    ) -> anyhow::Result<()> {
        let timestamp =
            chrono::DateTime::from_timestamp_millis(change.get_realtime() as i64).unwrap();
        let mut event = SourceEventWrapper::with_sequence(
            self.id().to_string(),
            SourceEvent::Change(change),
            timestamp,
            sequence,
            Some(profiling),
        );
        event.set_source_position(source_position);
        self.base.dispatch_event(event).await
    }
}

#[async_trait]
impl Source for CharacterizationSource {
    fn id(&self) -> &str {
        self.base.get_id()
    }

    fn type_name(&self) -> &str {
        "pipeline-characterization"
    }

    fn properties(&self) -> HashMap<String, serde_json::Value> {
        HashMap::new()
    }

    async fn start(&self) -> anyhow::Result<()> {
        self.base
            .set_status(ComponentStatus::Starting, Some("Starting".to_string()))
            .await;
        self.base
            .set_status(ComponentStatus::Running, Some("Running".to_string()))
            .await;
        Ok(())
    }

    async fn stop(&self) -> anyhow::Result<()> {
        self.base
            .set_status(ComponentStatus::Stopping, Some("Stopping".to_string()))
            .await;
        self.base
            .set_status(ComponentStatus::Stopped, Some("Stopped".to_string()))
            .await;
        Ok(())
    }

    async fn status(&self) -> ComponentStatus {
        self.base.status_handle().get_status().await
    }

    async fn subscribe(
        &self,
        settings: crate::config::SourceSubscriptionSettings,
    ) -> anyhow::Result<SubscriptionResponse> {
        self.subscription_tx
            .send(settings.clone())
            .map_err(|_| anyhow::anyhow!("test subscription receiver dropped"))?;
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

struct PipelineHarness {
    graph: Arc<RwLock<ComponentGraph>>,
    update_tx: ComponentUpdateSender,
    source_manager: Arc<SourceManager>,
    index_factory: Arc<crate::indexes::IndexFactory>,
    middleware_registry: Arc<MiddlewareTypeRegistry>,
    subscription_rx: mpsc::UnboundedReceiver<crate::config::SourceSubscriptionSettings>,
}

impl PipelineHarness {
    async fn new(backend: Arc<CharacterizationBackend>) -> Self {
        let log_registry = crate::managers::get_or_init_global_registry();
        let (graph, mut update_rx) = ComponentGraph::new("pipeline-characterization");
        let update_tx = graph.update_sender();
        let graph = Arc::new(RwLock::new(graph));
        let graph_for_updates = graph.clone();
        tokio::spawn(async move {
            while let Some(update) = update_rx.recv().await {
                graph_for_updates.write().await.apply_update(update);
            }
        });

        let source_manager = Arc::new(SourceManager::new(
            "pipeline-characterization",
            log_registry,
            graph.clone(),
            update_tx.clone(),
        ));
        let (subscription_tx, subscription_rx) = mpsc::unbounded_channel();
        let source = CharacterizationSource::new(SOURCE_ID, subscription_tx);
        graph
            .write()
            .await
            .register_source(SOURCE_ID, HashMap::new())
            .unwrap();
        source_manager.provision_source(source).await.unwrap();
        source_manager
            .start_source(SOURCE_ID.to_string())
            .await
            .unwrap();

        graph
            .write()
            .await
            .register_query(QUERY_ID, HashMap::new(), &[SOURCE_ID.to_string()])
            .unwrap();

        let providers: HashMap<String, Arc<dyn IndexBackendPlugin>> = HashMap::from([(
            "characterization".to_string(),
            backend as Arc<dyn IndexBackendPlugin>,
        )]);

        Self {
            graph,
            update_tx,
            source_manager,
            index_factory: Arc::new(crate::indexes::IndexFactory::new(vec![], providers)),
            middleware_registry: Arc::new(MiddlewareTypeRegistry::new()),
            subscription_rx,
        }
    }

    async fn start_query(
        &self,
        trace: EventTrace,
    ) -> (Arc<DrasiQuery>, mpsc::UnboundedReceiver<Arc<QueryResult>>) {
        let query = Arc::new(
            DrasiQuery::new(
                "pipeline-characterization",
                query_config(),
                self.source_manager.clone(),
                self.index_factory.clone(),
                self.middleware_registry.clone(),
                None,
            )
            .unwrap(),
        );
        query
            .initialize(crate::context::QueryRuntimeContext::new(
                "pipeline-characterization",
                QUERY_ID,
                self.update_tx.clone(),
            ))
            .await;

        let (result_tx, result_rx) = mpsc::unbounded_channel();
        query
            .base
            .dispatchers
            .write()
            .await
            .push(Box::new(RecordingDispatcher {
                trace,
                tx: result_tx,
            }));

        let mut status_rx = query.base.status_handle().subscribe_status();
        query.start().await.unwrap();
        tokio::time::timeout(
            Duration::from_secs(5),
            status_rx.wait_for(|status| *status == ComponentStatus::Running),
        )
        .await
        .expect("query should reach Running")
        .expect("query status channel should remain open");

        (query, result_rx)
    }

    async fn next_subscription(&mut self) -> crate::config::SourceSubscriptionSettings {
        tokio::time::timeout(Duration::from_secs(5), self.subscription_rx.recv())
            .await
            .expect("source subscription should arrive")
            .expect("source subscription channel should remain open")
    }

    async fn inject(&self, change: SourceChange, sequence: u64, source_position: Bytes) {
        self.inject_with_profiling(
            change,
            sequence,
            source_position,
            ProfilingMetadata::default(),
        )
        .await;
    }

    async fn inject_with_profiling(
        &self,
        change: SourceChange,
        sequence: u64,
        source_position: Bytes,
        profiling: ProfilingMetadata,
    ) {
        let source = self
            .source_manager
            .get_source_instance(SOURCE_ID)
            .await
            .unwrap();
        source
            .as_any()
            .downcast_ref::<CharacterizationSource>()
            .unwrap()
            .inject_with_profiling(change, sequence, source_position, profiling)
            .await
            .unwrap();
    }
}

fn query_config() -> QueryConfig {
    QueryConfig {
        id: QUERY_ID.to_string(),
        query: "MATCH (n:Person) RETURN n.name AS name".to_string(),
        query_language: QueryLanguage::Cypher,
        middleware: vec![],
        sources: vec![SourceSubscriptionConfig {
            source_id: SOURCE_ID.to_string(),
            nodes: vec![],
            relations: vec![],
            pipeline: vec![],
        }],
        auto_start: false,
        joins: None,
        enable_bootstrap: false,
        bootstrap_buffer_size: 100,
        priority_queue_capacity: None,
        dispatch_buffer_capacity: None,
        dispatch_mode: None,
        storage_backend: Some(StorageBackendRef::Named("characterization".to_string())),
        recovery_policy: None,
        outbox_capacity: 16,
        bootstrap_timeout_secs: 5,
    }
}

fn person_insert(node_id: &str, name: &str, effective_from: u64) -> SourceChange {
    SourceChange::Insert {
        element: Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new(SOURCE_ID, node_id),
                labels: vec!["Person".into()].into(),
                effective_from,
            },
            properties: ElementPropertyMap::from(json!({ "name": name })),
        },
    }
}

fn variables(entries: &[(&str, VariableValue)]) -> QueryVariables {
    entries
        .iter()
        .map(|(key, value)| ((*key).into(), value.clone()))
        .collect()
}

fn legacy_result_diffs(results: &[QueryPartEvaluationContext]) -> Vec<ResultDiff> {
    results
        .iter()
        .filter_map(|context| match context {
            QueryPartEvaluationContext::Adding {
                after,
                row_signature,
            } => Some(ResultDiff::Add {
                data: convert_query_variables_to_json(after),
                row_signature: *row_signature,
            }),
            QueryPartEvaluationContext::Updating {
                before,
                after,
                row_signature,
            } => {
                let after = convert_query_variables_to_json(after);
                Some(ResultDiff::Update {
                    data: after.clone(),
                    before: convert_query_variables_to_json(before),
                    after,
                    grouping_keys: None,
                    row_signature: *row_signature,
                })
            }
            QueryPartEvaluationContext::Removing {
                before,
                row_signature,
            } => Some(ResultDiff::Delete {
                data: convert_query_variables_to_json(before),
                row_signature: *row_signature,
            }),
            QueryPartEvaluationContext::Aggregation {
                before,
                after,
                row_signature,
                ..
            } => Some(ResultDiff::Aggregation {
                before: before.as_ref().map(convert_query_variables_to_json),
                after: convert_query_variables_to_json(after),
                row_signature: *row_signature,
            }),
            QueryPartEvaluationContext::Noop => None,
        })
        .collect()
}

async fn next_result(receiver: &mut mpsc::UnboundedReceiver<Arc<QueryResult>>) -> Arc<QueryResult> {
    tokio::time::timeout(Duration::from_secs(5), receiver.recv())
        .await
        .expect("query result should arrive")
        .expect("query result channel should remain open")
}

#[tokio::test]
async fn evaluation_results_map_to_ordered_query_result_and_sequence() {
    let trace = EventTrace::default();
    let output_store = Arc::new(RecordingOutputStore::new(false, trace.clone()));
    let checkpoint_state = Arc::new(Mutex::new(TransactionState::default()));
    let checkpoint_store = Arc::new(TransactionalCheckpointStore {
        state: checkpoint_state,
        trace: trace.clone(),
    });
    let output_state = RwLock::new(QueryOutputState::new(16));
    let output_metrics = Arc::new(QueryOutputMetrics::new());
    let (result_tx, mut result_rx) = mpsc::unbounded_channel();
    let dispatchers: RwLock<Vec<Box<dyn ChangeDispatcher<QueryResult> + Send + Sync>>> =
        RwLock::new(vec![Box::new(RecordingDispatcher {
            trace,
            tx: result_tx,
        })]);
    let outbox_writer: Option<Arc<dyn OutboxWriter>> = Some(output_store.clone());
    let live_results_writer: Option<Arc<dyn LiveResultsWriter>> = Some(output_store.clone());
    let checkpoint_writer: Option<Arc<dyn CheckpointStore>> = Some(checkpoint_store.clone());

    let contexts = vec![
        QueryPartEvaluationContext::Adding {
            after: variables(&[("kind", VariableValue::String("added".to_string()))]),
            row_signature: 11,
        },
        QueryPartEvaluationContext::Noop,
        QueryPartEvaluationContext::Updating {
            before: variables(&[("kind", VariableValue::String("before".to_string()))]),
            after: variables(&[("kind", VariableValue::String("after".to_string()))]),
            row_signature: 22,
        },
        QueryPartEvaluationContext::Removing {
            before: variables(&[("kind", VariableValue::String("removed".to_string()))]),
            row_signature: 33,
        },
        QueryPartEvaluationContext::Aggregation {
            before: Some(variables(&[("total", VariableValue::Integer(1.into()))])),
            after: variables(&[("total", VariableValue::Integer(2.into()))]),
            grouping_keys: vec!["group".to_string()],
            default_before: false,
            default_after: false,
            row_signature: 44,
        },
    ];

    dispatch_query_results(
        &contexts,
        "source-a",
        "query-a",
        &output_state,
        &dispatchers,
        &outbox_writer,
        &live_results_writer,
        &checkpoint_writer,
        16,
        ProfilingMetadata::default(),
        &output_metrics,
    )
    .await
    .unwrap();

    let result = next_result(&mut result_rx).await;
    assert_eq!(result.query_id, "query-a");
    assert_eq!(result.sequence, 1);
    assert_eq!(
        result.results,
        vec![
            ResultDiff::Add {
                data: json!({ "kind": "added" }),
                row_signature: 11,
            },
            ResultDiff::Update {
                data: json!({ "kind": "after" }),
                before: json!({ "kind": "before" }),
                after: json!({ "kind": "after" }),
                grouping_keys: None,
                row_signature: 22,
            },
            ResultDiff::Delete {
                data: json!({ "kind": "removed" }),
                row_signature: 33,
            },
            ResultDiff::Aggregation {
                before: Some(json!({ "total": 1 })),
                after: json!({ "total": 2 }),
                row_signature: 44,
            },
        ]
    );
    assert_eq!(result.metadata["source_id"], json!("source-a"));
    assert_eq!(result.metadata["processed_by"], json!("drasi-core"));
    assert_eq!(result.metadata["result_count"], json!(4));
    assert_eq!(result.profiling, Some(ProfilingMetadata::default()));

    let envelope = crate::change::query_evaluation_to_envelope(
        &contexts,
        crate::change::QueryEnvelopeMetadata::new(
            result.query_id.clone(),
            Some(Arc::from("source-a")),
            result.sequence,
            result.timestamp,
            result.metadata.clone(),
            result.profiling.clone(),
        ),
    )
    .unwrap()
    .expect("non-Noop evaluation results should produce an envelope");
    let adapted = crate::change::query_result_from_envelope(&envelope).unwrap();
    assert_eq!(adapted.query_id, result.query_id);
    assert_eq!(adapted.sequence, result.sequence);
    assert_eq!(adapted.timestamp, result.timestamp);
    assert_eq!(adapted.results, result.results);
    assert_eq!(adapted.metadata, result.metadata);
    assert_eq!(adapted.profiling, result.profiling);

    let state = output_state.read().await;
    assert_eq!(state.as_of_sequence(), 1);
    assert_eq!(state.outbox_len(), 1);
    assert_eq!(state.results_len(), 3);
    assert_eq!(state.get_result(&11), Some(&json!({ "kind": "added" })));
    assert_eq!(state.get_result(&22), Some(&json!({ "kind": "after" })));
    assert_eq!(state.get_result(&33), None);
    assert_eq!(state.get_result(&44), Some(&json!({ "total": 2 })));
    drop(state);

    let persisted = output_store.read_from("query-a", 0).await.unwrap();
    assert_eq!(persisted.len(), 1);
    assert_eq!(persisted[0].0, 1);
    assert_eq!(
        persisted[0].1,
        rmp_serde::to_vec(result.as_ref()).unwrap(),
        "the durable outbox payload must match the dispatched QueryResult"
    );
    assert_eq!(
        checkpoint_store
            .read_result_sequence("query-a")
            .await
            .unwrap(),
        Some(1)
    );

    dispatch_query_results(
        &[QueryPartEvaluationContext::Adding {
            after: variables(&[("kind", VariableValue::String("second".to_string()))]),
            row_signature: 55,
        }],
        "source-b",
        "query-a",
        &output_state,
        &dispatchers,
        &outbox_writer,
        &live_results_writer,
        &checkpoint_writer,
        16,
        ProfilingMetadata::default(),
        &output_metrics,
    )
    .await
    .unwrap();

    let second = next_result(&mut result_rx).await;
    assert_eq!(second.sequence, 2);
    assert_eq!(second.metadata["source_id"], json!("source-b"));
    assert_eq!(
        output_store
            .read_from("query-a", 0)
            .await
            .unwrap()
            .into_iter()
            .map(|(sequence, _)| sequence)
            .collect::<Vec<_>>(),
        vec![1, 2]
    );
}

#[tokio::test]
async fn live_source_change_envelope_path_preserves_legacy_result_metadata() {
    let backend = Arc::new(CharacterizationBackend::new(false));
    let mut harness = PipelineHarness::new(backend.clone()).await;
    let (query, mut result_rx) = harness.start_query(backend.trace.clone()).await;
    let subscription = harness.next_subscription().await;
    assert_eq!(subscription.resume_sequence, None);
    assert_eq!(subscription.resume_from, None);
    backend.trace.clear();

    let source_position = Bytes::from_static(b"envelope-position-7");
    let source_profiling = ProfilingMetadata {
        source_ns: Some(10),
        reactivator_start_ns: Some(20),
        reactivator_end_ns: Some(30),
        source_receive_ns: Some(40),
        source_send_ns: Some(50),
        ..Default::default()
    };
    harness
        .inject_with_profiling(
            person_insert("person-envelope", "Envelope", 1_000),
            7,
            source_position.clone(),
            source_profiling.clone(),
        )
        .await;

    let delivered = next_result(&mut result_rx).await;
    assert_eq!(delivered.query_id, QUERY_ID);
    assert_eq!(delivered.sequence, 1);
    assert_eq!(delivered.results.len(), 1);
    assert!(matches!(
        &delivered.results[0],
        ResultDiff::Add { data, .. } if data == &json!({ "name": "Envelope" })
    ));
    assert_eq!(
        delivered.metadata,
        HashMap::from([
            ("source_id".to_string(), json!(SOURCE_ID)),
            ("processed_by".to_string(), json!("drasi-core")),
            ("result_count".to_string(), json!(1)),
        ])
    );

    let delivered_profiling = delivered
        .profiling
        .as_ref()
        .expect("live results retain source profiling");
    assert_eq!(delivered_profiling.source_ns, source_profiling.source_ns);
    assert_eq!(
        delivered_profiling.reactivator_start_ns,
        source_profiling.reactivator_start_ns
    );
    assert_eq!(
        delivered_profiling.reactivator_end_ns,
        source_profiling.reactivator_end_ns
    );
    assert_eq!(
        delivered_profiling.source_receive_ns,
        source_profiling.source_receive_ns
    );
    assert_eq!(
        delivered_profiling.source_send_ns,
        source_profiling.source_send_ns
    );
    assert!(delivered_profiling.query_receive_ns.is_some());
    assert!(delivered_profiling.query_core_call_ns.is_some());
    assert!(delivered_profiling.query_core_return_ns.is_some());
    assert!(delivered_profiling.query_send_ns.is_some());

    assert_eq!(
        backend.trace.snapshot(),
        vec![
            TraceEvent::SessionBegin,
            TraceEvent::CheckpointStaged {
                source_id: SOURCE_ID.to_string(),
                sequence: 7,
                source_position: Some(source_position),
            },
            TraceEvent::SessionCommit,
            TraceEvent::OutboxAppendAttempt {
                query_id: QUERY_ID.to_string(),
                sequence: 1,
            },
            TraceEvent::LiveResultsApplyAttempt {
                query_id: QUERY_ID.to_string(),
                mutation_count: 1,
            },
            TraceEvent::ResultSequenceWritten {
                query_id: QUERY_ID.to_string(),
                sequence: 1,
            },
            TraceEvent::ReactionDispatch { sequence: 1 },
        ]
    );
    assert_eq!(
        backend.output_store.read_from(QUERY_ID, 0).await.unwrap()[0].1,
        rmp_serde::to_vec(delivered.as_ref()).unwrap()
    );

    query.stop().await.unwrap();
    harness
        .source_manager
        .stop_source(SOURCE_ID.to_string())
        .await
        .unwrap();
    drop(harness.graph);
}

#[tokio::test]
async fn committed_input_has_no_recoverable_output_when_persistence_fails() {
    let backend = Arc::new(CharacterizationBackend::new(true));
    let mut harness = PipelineHarness::new(backend.clone()).await;

    let (query, mut result_rx) = harness.start_query(backend.trace.clone()).await;
    let initial_subscription = harness.next_subscription().await;
    assert_eq!(initial_subscription.resume_sequence, None);
    assert_eq!(initial_subscription.resume_from, None);
    backend.trace.clear();

    let first_position = Bytes::from_static(b"position-1");
    harness
        .inject(
            person_insert("person-1", "Alice", 1_000),
            1,
            first_position.clone(),
        )
        .await;
    let delivered = next_result(&mut result_rx).await;
    assert_eq!(delivered.sequence, 1);
    assert_eq!(delivered.results.len(), 1);
    assert!(
        matches!(
            &delivered.results[0],
            ResultDiff::Add { data, .. } if data == &json!({ "name": "Alice" })
        ),
        "the committed input should produce Alice's Add result"
    );

    assert_eq!(
        backend.trace.snapshot(),
        vec![
            TraceEvent::SessionBegin,
            TraceEvent::CheckpointStaged {
                source_id: SOURCE_ID.to_string(),
                sequence: 1,
                source_position: Some(first_position.clone()),
            },
            TraceEvent::SessionCommit,
            TraceEvent::OutboxAppendAttempt {
                query_id: QUERY_ID.to_string(),
                sequence: 1,
            },
            TraceEvent::LiveResultsApplyAttempt {
                query_id: QUERY_ID.to_string(),
                mutation_count: 1,
            },
            TraceEvent::ReactionDispatch { sequence: 1 },
        ],
        "current DrasiQuery commits input progress before best-effort output persistence and dispatch"
    );

    assert!(
        backend
            .element_index
            .get_element(&ElementReference::new(SOURCE_ID, "person-1"))
            .await
            .unwrap()
            .is_some(),
        "core element state committed"
    );
    assert_eq!(
        backend
            .checkpoint_store
            .read_checkpoint(SOURCE_ID)
            .await
            .unwrap(),
        Some(SourceCheckpoint::new(1, Some(first_position.clone()))),
        "source progress committed with core state"
    );
    assert!(
        backend
            .output_store
            .read_from(QUERY_ID, 0)
            .await
            .unwrap()
            .is_empty(),
        "failed outbox persistence leaves no replayable output"
    );
    assert!(
        backend
            .output_store
            .read_snapshot(QUERY_ID)
            .await
            .unwrap()
            .is_empty(),
        "failed live-result persistence leaves no recoverable snapshot"
    );
    assert_eq!(
        backend
            .checkpoint_store
            .read_result_sequence(QUERY_ID)
            .await
            .unwrap(),
        None,
        "failed output writes must not claim a durable result sequence"
    );

    query.stop().await.unwrap();

    let (restarted_query, mut restarted_result_rx) =
        harness.start_query(backend.trace.clone()).await;
    let resumed_subscription = harness.next_subscription().await;
    assert_eq!(resumed_subscription.resume_sequence, Some(1));
    assert_eq!(resumed_subscription.resume_from, Some(first_position));

    let recovered_snapshot = restarted_query.fetch_snapshot().await.unwrap();
    assert_eq!(recovered_snapshot.as_of_sequence, 0);
    assert!(
        recovered_snapshot.is_empty(),
        "after a fresh host starts from committed input position 1, output 1 cannot be recovered"
    );

    backend.trace.clear();
    harness
        .inject(
            person_insert("person-1", "Alice", 1_000),
            1,
            Bytes::from_static(b"position-2"),
        )
        .await;
    harness
        .inject(
            person_insert("person-2", "Bob", 2_000),
            2,
            Bytes::from_static(b"position-3"),
        )
        .await;

    let after_restart = next_result(&mut restarted_result_rx).await;
    assert_eq!(after_restart.sequence, 1);
    assert_eq!(after_restart.results.len(), 1);
    assert!(
        matches!(
            &after_restart.results[0],
            ResultDiff::Add { data, .. } if data == &json!({ "name": "Bob" })
        ),
        "checkpoint-seeded dedup must suppress replayed source sequence 1",
    );
    assert_eq!(
        backend.trace.snapshot(),
        vec![
            TraceEvent::SessionBegin,
            TraceEvent::CheckpointStaged {
                source_id: SOURCE_ID.to_string(),
                sequence: 2,
                source_position: Some(Bytes::from_static(b"position-3")),
            },
            TraceEvent::SessionCommit,
            TraceEvent::OutboxAppendAttempt {
                query_id: QUERY_ID.to_string(),
                sequence: 1,
            },
            TraceEvent::LiveResultsApplyAttempt {
                query_id: QUERY_ID.to_string(),
                mutation_count: 1,
            },
            TraceEvent::ReactionDispatch { sequence: 1 },
        ],
        "the replayed sequence is skipped before a core session begins"
    );

    restarted_query.stop().await.unwrap();
    harness
        .source_manager
        .stop_source(SOURCE_ID.to_string())
        .await
        .unwrap();
    drop(harness.graph);
}

#[tokio::test]
async fn due_future_output_is_dispatched_after_core_commit() {
    let trace = EventTrace::default();
    let state = Arc::new(Mutex::new(TransactionState::default()));
    let session_control = Arc::new(RecordingSessionControl {
        state: state.clone(),
        trace: trace.clone(),
    });
    let checkpoint_store = Arc::new(TransactionalCheckpointStore {
        state,
        trace: trace.clone(),
    });
    let output_store = Arc::new(RecordingOutputStore::new(false, trace.clone()));
    let element_index = Arc::new(InMemoryElementIndex::new());
    let result_index = Arc::new(InMemoryResultIndex::new());
    let future_queue = Arc::new(InMemoryFutureQueue::new());

    let parser = Arc::new(CypherParser::new(Arc::new(DefaultQueryConfig)));
    let function_registry = Arc::new(FunctionRegistry::new()).with_cypher_function_set();
    let query = QueryBuilder::new(
        "
        MATCH (o:Order)
        WHERE o.status = 'ready'
          AND drasi.trueFor(sum(o.value) > 1, duration({ seconds: 5 }))
        RETURN sum(o.value) AS value
        ",
        parser,
    )
    .with_function_registry(function_registry)
    .with_element_index(element_index.clone())
    .with_archive_index(element_index)
    .with_result_index(result_index)
    .with_future_queue(future_queue.clone())
    .with_session_control(session_control)
    .build()
    .await;

    let initial = query
        .process_source_change(SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("future-source", "order-1"),
                    labels: vec!["Order".into()].into(),
                    effective_from: 1_577_836_800_000,
                },
                properties: ElementPropertyMap::from(json!({
                    "status": "ready",
                    "value": 2,
                })),
            },
        })
        .await
        .unwrap();
    assert!(initial.is_empty());
    assert!(future_queue.peek_due_time().await.unwrap().is_some());

    trace.clear();
    let due = query
        .process_due_futures()
        .await
        .unwrap()
        .expect("the trueFor evaluation should have queued one future");
    assert!(!due.results.is_empty());
    assert_eq!(
        trace.snapshot(),
        vec![TraceEvent::SessionBegin, TraceEvent::SessionCommit],
        "the future pop and core evaluation commit before output handling starts"
    );

    let output_state = RwLock::new(QueryOutputState::new(16));
    let output_metrics = Arc::new(QueryOutputMetrics::new());
    let (result_tx, mut result_rx) = mpsc::unbounded_channel();
    let dispatchers: RwLock<Vec<Box<dyn ChangeDispatcher<QueryResult> + Send + Sync>>> =
        RwLock::new(vec![Box::new(RecordingDispatcher {
            trace: trace.clone(),
            tx: result_tx,
        })]);
    let outbox_writer: Option<Arc<dyn OutboxWriter>> = Some(output_store.clone());
    let live_results_writer: Option<Arc<dyn LiveResultsWriter>> = Some(output_store);
    let checkpoint_writer: Option<Arc<dyn CheckpointStore>> = Some(checkpoint_store);

    let profiling = ProfilingMetadata {
        source_ns: Some(101),
        source_receive_ns: Some(202),
        source_send_ns: Some(303),
        ..Default::default()
    };
    let expected_results = legacy_result_diffs(&due.results);
    dispatch_query_results(
        &due.results,
        &due.source_id,
        "future-query",
        &output_state,
        &dispatchers,
        &outbox_writer,
        &live_results_writer,
        &checkpoint_writer,
        16,
        profiling.clone(),
        &output_metrics,
    )
    .await
    .unwrap();
    let dispatched = next_result(&mut result_rx).await;
    assert_eq!(dispatched.query_id, "future-query");
    assert_eq!(dispatched.sequence, 1);
    assert_eq!(dispatched.results, expected_results);
    assert_eq!(
        dispatched.metadata,
        HashMap::from([
            ("source_id".to_string(), json!("future-source")),
            ("processed_by".to_string(), json!("drasi-core")),
            ("result_count".to_string(), json!(dispatched.results.len()),),
        ])
    );
    assert_eq!(dispatched.profiling, Some(profiling));
    assert_eq!(
        trace.snapshot(),
        vec![
            TraceEvent::SessionBegin,
            TraceEvent::SessionCommit,
            TraceEvent::OutboxAppendAttempt {
                query_id: "future-query".to_string(),
                sequence: 1,
            },
            TraceEvent::LiveResultsApplyAttempt {
                query_id: "future-query".to_string(),
                mutation_count: due.results.len(),
            },
            TraceEvent::ResultSequenceWritten {
                query_id: "future-query".to_string(),
                sequence: 1,
            },
            TraceEvent::ReactionDispatch { sequence: 1 },
        ],
        "due-future output persistence and dispatch currently happen after the core commit"
    );
}

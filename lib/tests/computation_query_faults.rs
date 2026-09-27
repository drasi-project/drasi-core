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

#![cfg(test)]
#![cfg(feature = "computation-rocksdb-tests")]

use async_trait::async_trait;
use bytes::Bytes;
use drasi_core::{
    computation::{
        ComputationIndexProvider, ComputationIndexes, ComputationResource, TransactionDomain,
    },
    interface::{
        CheckpointStore, IndexError, IndexSet, LiveResultsWriter, RowMutation, SessionControl,
        SourceCheckpoint,
    },
    models::{Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange},
};
use drasi_index_rocksdb::{
    computation::RocksDbComputationProvider, RocksDbMemoryBudget, RocksIndexOptions,
};
use drasi_lib::computation::v1::*;
use std::{
    collections::HashMap,
    num::NonZeroUsize,
    result::Result,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

const BOOTSTRAP: &str = "\0computation:query-bootstrap:v1";
const PENDING: &str = "\0computation:pending-output:v1";
const DELIVERED: &str = "\0computation:query-delivered:v1";
const DELIVERY_SEQUENCE: &str = "\0computation:query-delivery-sequence:v1";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fault {
    InputCheckpoint,
    ResultSequence,
    PendingStart,
    PendingStage,
    PendingClear,
    BootstrapComplete,
    LiveRows,
    ReadLiveRows,
    Commit,
    CommittedThenWait,
    DeliverySequence,
    DeliveryConfirmed,
    InputTransport,
}

struct Injection {
    point: Fault,
    armed: AtomicBool,
    committed: tokio::sync::Notify,
}
impl Injection {
    fn new(point: Fault) -> Arc<Self> {
        Arc::new(Self {
            point,
            armed: AtomicBool::new(false),
            committed: tokio::sync::Notify::new(),
        })
    }
    fn take(&self, point: Fault) -> bool {
        self.point == point && self.armed.swap(false, Ordering::AcqRel)
    }
}
struct Session {
    inner: Arc<dyn SessionControl>,
    fault: Arc<Injection>,
}
#[async_trait]
impl SessionControl for Session {
    async fn begin(&self) -> Result<(), IndexError> {
        self.inner.begin().await
    }
    async fn commit(&self) -> Result<(), IndexError> {
        if self.fault.take(Fault::Commit) {
            return Err(IndexError::IOError);
        }
        self.inner.commit().await?;
        if self.fault.take(Fault::CommittedThenWait) {
            self.fault.committed.notify_one();
            std::future::pending().await
        } else {
            Ok(())
        }
    }
    fn rollback(&self) -> Result<(), IndexError> {
        self.inner.rollback()
    }
}

struct Checkpoints {
    inner: Arc<dyn CheckpointStore>,
    fault: Arc<Injection>,
}
#[async_trait]
impl CheckpointStore for Checkpoints {
    fn is_persistent(&self) -> bool {
        true
    }
    async fn stage_checkpoint(
        &self,
        key: &str,
        sequence: u64,
        position: Option<&Bytes>,
    ) -> Result<(), IndexError> {
        let fault = if key.starts_with("\0computation:input-transport:v1:") {
            Some(Fault::InputTransport)
        } else if key == DELIVERY_SEQUENCE {
            Some(Fault::DeliverySequence)
        } else if key == DELIVERED {
            Some(Fault::DeliveryConfirmed)
        } else if key.starts_with("computation:input:") {
            Some(Fault::InputCheckpoint)
        } else if key == PENDING && position.is_some() {
            Some(Fault::PendingStart)
        } else if key == PENDING && sequence > 0 {
            Some(Fault::PendingStage)
        } else if key == PENDING {
            Some(Fault::PendingClear)
        } else if key == BOOTSTRAP && sequence == 1 {
            Some(Fault::BootstrapComplete)
        } else {
            None
        };
        if fault.is_some_and(|point| self.fault.take(point)) {
            return Err(IndexError::IOError);
        }
        self.inner.stage_checkpoint(key, sequence, position).await
    }
    async fn read_checkpoint(&self, key: &str) -> Result<Option<SourceCheckpoint>, IndexError> {
        self.inner.read_checkpoint(key).await
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
    async fn stage_result_sequence(&self, id: &str, sequence: u64) -> Result<(), IndexError> {
        if self.fault.take(Fault::ResultSequence) {
            return Err(IndexError::IOError);
        }
        self.inner.stage_result_sequence(id, sequence).await
    }
    async fn write_result_sequence(&self, id: &str, sequence: u64) -> Result<(), IndexError> {
        self.inner.write_result_sequence(id, sequence).await
    }
    async fn read_result_sequence(&self, id: &str) -> Result<Option<u64>, IndexError> {
        self.inner.read_result_sequence(id).await
    }
    async fn write_output_generation(&self, id: &str, generation: u64) -> Result<(), IndexError> {
        self.inner.write_output_generation(id, generation).await
    }
    async fn read_output_generation(&self, id: &str) -> Result<Option<u64>, IndexError> {
        self.inner.read_output_generation(id).await
    }
}

struct Live {
    inner: Arc<dyn LiveResultsWriter>,
    fault: Arc<Injection>,
}
#[async_trait]
impl LiveResultsWriter for Live {
    async fn apply_mutations(&self, id: &str, rows: &[RowMutation<'_>]) -> Result<(), IndexError> {
        if self.fault.take(Fault::LiveRows) {
            return Err(IndexError::IOError);
        }
        self.inner.apply_mutations(id, rows).await
    }
    async fn read_snapshot(&self, id: &str) -> Result<Vec<(u64, Vec<u8>)>, IndexError> {
        if self.fault.take(Fault::ReadLiveRows) {
            return Err(IndexError::IOError);
        }
        self.inner.read_snapshot(id).await
    }
    async fn clear(&self, id: &str) -> Result<(), IndexError> {
        self.inner.clear(id).await
    }
    async fn row_count(&self, id: &str) -> Result<usize, IndexError> {
        self.inner.row_count(id).await
    }
}

struct Provider {
    inner: Arc<dyn ComputationIndexProvider>,
    fault: Arc<Injection>,
}
#[async_trait]
impl ComputationIndexProvider for Provider {
    async fn create_indexes(
        &self,
        graph: &str,
        id: &str,
    ) -> Result<ComputationIndexes, IndexError> {
        let original = self.inner.create_indexes(graph, id).await?;
        let set = original.indexes();
        let control: Arc<dyn SessionControl> = Arc::new(Session {
            inner: set.session_control.clone(),
            fault: self.fault.clone(),
        });
        let domain = TransactionDomain::new(control.clone());
        let result = ComputationIndexes::try_new(
            IndexSet {
                element_index: set.element_index.clone(),
                archive_index: set.archive_index.clone(),
                result_index: set.result_index.clone(),
                future_queue: set.future_queue.clone(),
                session_control: control,
            },
            Some(domain.clone()),
            Some(ComputationResource::participating(
                Arc::new(Checkpoints {
                    inner: original.checkpoint_store().expect("checkpoint").clone(),
                    fault: self.fault.clone(),
                }),
                &domain,
            )),
            Some(ComputationResource::participating(
                original.outbox_writer().expect("outbox").clone(),
                &domain,
            )),
            Some(ComputationResource::participating(
                Arc::new(Live {
                    inner: original.live_results_writer().expect("live").clone(),
                    fault: self.fault.clone(),
                }),
                &domain,
            )),
        )
        .map_err(IndexError::other)?;
        Ok(result.with_cleanup(original.cleanup().expect("owner").clone()))
    }
    fn is_volatile(&self) -> bool {
        false
    }
}
fn provider(path: &std::path::Path) -> Arc<dyn ComputationIndexProvider> {
    provider_for(path, Backend::Computation)
}

#[derive(Clone, Copy, Debug)]
enum Backend {
    Computation,
    OrdinaryPlugin,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EntryPoint {
    Direct,
    TransactionBody,
}

struct QueryUnderTest {
    processor: Box<dyn Transformer>,
    results: QueryResults,
}

impl QueryUnderTest {
    fn new(query: ContinuousQueryTransformer, entry: EntryPoint) -> Self {
        let results = query.results();
        let processor: Box<dyn Transformer> = match entry {
            EntryPoint::Direct => Box::new(query),
            EntryPoint::TransactionBody => Box::new(TransactionTransformer::from_query(query)),
        };
        Self { processor, results }
    }
    fn results(&self) -> QueryResults {
        self.results.clone()
    }
}

impl std::ops::Deref for QueryUnderTest {
    type Target = dyn Transformer;
    fn deref(&self) -> &Self::Target {
        self.processor.as_ref()
    }
}

impl std::ops::DerefMut for QueryUnderTest {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.processor.as_mut()
    }
}

fn provider_for(path: &std::path::Path, backend: Backend) -> Arc<dyn ComputationIndexProvider> {
    match backend {
        Backend::Computation => Arc::new(RocksDbComputationProvider::new(
            path,
            RocksIndexOptions::new(
                false,
                false,
                RocksDbMemoryBudget::from_total_budget_bytes(32 << 20).expect("budget"),
            ),
        )),
        Backend::OrdinaryPlugin => LegacyIndexProviderAdapter::new(Arc::new(
            drasi_index_rocksdb::RocksDbIndexProvider::new(path, false, false),
        )),
    }
}
fn definition(temporal: bool) -> ContinuousQueryDefinition {
    ContinuousQueryDefinition {
        graph_id: "faults".into(),
        id: ComponentId::try_new("query").expect("id"),
        query: if temporal {
            "MATCH (n:Person) WHERE drasi.trueLater(true, 2000) RETURN n.name AS name"
        } else {
            "MATCH (n:Person) RETURN n.name AS name"
        }
        .into(),
        language: ComputationQueryLanguage::Cypher,
        output_stream: StreamId::try_new("query/out").expect("stream"),
        outbox_capacity: NonZeroUsize::new(16).expect("capacity"),
    }
}
fn input(sequence: u64) -> InputEnvelope {
    let element = Element::Node {
        metadata: ElementMetadata {
            reference: ElementReference::new("people", "one"),
            labels: Arc::from([Arc::from("Person")]),
            effective_from: 999 + sequence,
        },
        properties: ElementPropertyMap::from(
            serde_json::json!({"name": format!("name-{sequence}")}),
        ),
    };
    InputEnvelope {
        port: PortId::try_new("in").expect("port"),
        envelope: GraphChangeCodec::encode_change(
            if sequence == 1 {
                SourceChange::Insert { element }
            } else {
                SourceChange::Update { element }
            },
            StreamId::try_new("source/out").expect("stream"),
            sequence,
            None,
        )
        .expect("input"),
    }
}

#[tokio::test]
async fn atomic_fault_matrix_rolls_back_all_output_and_source_progress_before_fencing() {
    for backend in [Backend::Computation, Backend::OrdinaryPlugin] {
        atomic_fault_matrix(backend, EntryPoint::Direct).await;
    }
}

#[tokio::test]
async fn transaction_query_body_atomic_fault_matrix_preserves_all_participants() {
    for backend in [Backend::Computation, Backend::OrdinaryPlugin] {
        atomic_fault_matrix(backend, EntryPoint::TransactionBody).await;
    }
}

async fn atomic_fault_matrix(backend: Backend, entry: EntryPoint) {
    for point in [
        Fault::InputCheckpoint,
        Fault::LiveRows,
        Fault::ResultSequence,
        Fault::DeliverySequence,
        Fault::Commit,
    ] {
        if point == Fault::DeliverySequence && entry == EntryPoint::Direct {
            continue;
        }
        let temp = tempfile::tempdir().expect("temp");
        let base = provider_for(temp.path(), backend);
        let fault = Injection::new(point);
        let progress = Arc::new(
            QuerySourceProgress::new("faults", ComponentId::try_new("query").expect("id"))
                .expect("scope"),
        );
        {
            let query = ContinuousQueryTransformer::new(
                definition(false),
                Arc::new(Provider {
                    inner: base.clone(),
                    fault: fault.clone(),
                }),
            )
            .await
            .expect("construct")
            .with_source_progress(progress.clone())
            .expect("source progress");
            let mut query = QueryUnderTest::new(query, entry);
            query.start().await.expect("start");
            fault.armed.store(true, Ordering::Release);
            assert!(query.transform(input(1)).await.is_err(), "{point:?}");
            assert!(
                query.transform(input(2)).await.is_err(),
                "same-instance failure fence"
            );
            assert_eq!(
                query.results().snapshot().expect("snapshot").as_of_sequence,
                0
            );
            assert!(
                progress.snapshot().checkpoints.is_empty(),
                "failed writes cannot acknowledge source progress"
            );
            assert!(
                progress.wait_ready().await.is_err(),
                "a failed owner wakes a source waiting for confirmed progress"
            );
            query.stop().await.expect("cleanup");
        }
        {
            let stored = base
                .create_indexes("faults", "query")
                .await
                .expect("independent reopen");
            let checkpoints = stored.checkpoint_store().unwrap();
            assert!(
                checkpoints
                    .read_all_checkpoints()
                    .await
                    .unwrap()
                    .keys()
                    .all(|key| !key.starts_with("computation:input:")),
                "{backend:?}/{entry:?}/{point:?}"
            );
            assert_eq!(
                checkpoints
                    .read_result_sequence("query")
                    .await
                    .unwrap()
                    .unwrap_or(0),
                0
            );
            assert!(stored
                .outbox_writer()
                .unwrap()
                .read_from("query", 0)
                .await
                .unwrap()
                .is_empty());
            assert!(stored
                .live_results_writer()
                .unwrap()
                .read_snapshot("query")
                .await
                .unwrap()
                .is_empty());
            stored
                .cleanup()
                .unwrap()
                .shutdown()
                .await
                .expect("release independent reader");
        }
        let reopened = ContinuousQueryTransformer::new(definition(false), base)
            .await
            .expect("reopen")
            .with_source_progress(progress.clone())
            .expect("source progress");
        let mut reopened = QueryUnderTest::new(reopened, entry);
        reopened.start().await.expect("consistent rollback");
        assert!(reopened
            .results()
            .snapshot()
            .expect("snapshot")
            .rows
            .is_empty());
        assert!(reopened.results().replay(0).expect("retained").is_empty());
        assert_eq!(
            reopened
                .transform(input(1))
                .await
                .expect("checkpoint rolled back")[0]
                .envelope
                .system()
                .sequence(),
            1
        );
        assert_eq!(
            progress.snapshot().checkpoints
                [&SourceProgressKey::Stream(StreamId::try_new("source/out").expect("stream"))]
                .sequence,
            1
        );
        reopened.stop().await.expect("stop");
    }
}

#[tokio::test]
async fn outbox_overflow_eviction_rolls_back_with_failed_output_and_stays_bounded_after_restart() {
    for backend in [Backend::Computation, Backend::OrdinaryPlugin] {
        let temp = tempfile::tempdir().expect("temp");
        let base = provider_for(temp.path(), backend);
        let fault = Injection::new(Fault::LiveRows);
        let definition = || {
            let mut query = definition(false);
            query.outbox_capacity = NonZeroUsize::new(2).expect("capacity");
            query
        };
        {
            let mut query = ContinuousQueryTransformer::new(
                definition(),
                Arc::new(Provider {
                    inner: base.clone(),
                    fault: fault.clone(),
                }),
            )
            .await
            .expect("construct");
            query.start().await.expect("start");
            query.transform(input(1)).await.expect("first output");
            query.transform(input(2)).await.expect("full outbox");
            fault.armed.store(true, Ordering::Release);
            assert!(query.transform(input(3)).await.is_err(), "{backend:?}");
            query.stop().await.expect("stop failed query");
        }
        {
            let mut query = ContinuousQueryTransformer::new(definition(), base.clone())
                .await
                .expect("reopen");
            query.start().await.expect("recover rolled-back eviction");
            assert_eq!(
                query
                    .results()
                    .replay(0)
                    .expect("original retained prefix")
                    .iter()
                    .map(|output| output.system().sequence())
                    .collect::<Vec<_>>(),
                vec![1, 2],
                "{backend:?}"
            );
            let output = query
                .transform(input(3))
                .await
                .expect("replay failed input");
            assert_eq!(output.len(), 1);
            let result = QueryChangeCodec::to_legacy_result(&output[0].envelope).expect("result");
            assert!(matches!(
                result.results.as_slice(),
                [drasi_lib::channels::ResultDiff::Update { before, after, .. }]
                    if before == &serde_json::json!({"name": "name-2"})
                        && after == &serde_json::json!({"name": "name-3"})
            ));
            query.stop().await.expect("stop successful replay");
        }
        let mut query = ContinuousQueryTransformer::new(definition(), base)
            .await
            .expect("second reopen");
        query.start().await.expect("recover committed retention");
        assert!(query.results().replay(0).is_err());
        assert_eq!(
            query
                .results()
                .replay(1)
                .expect("retained suffix")
                .iter()
                .map(|output| output.system().sequence())
                .collect::<Vec<_>>(),
            vec![2, 3],
            "{backend:?}"
        );
        query.stop().await.expect("cleanup");
    }
}

#[tokio::test]
async fn non_atomic_pending_marker_stage_and_clear_failures_have_distinct_recovery_boundaries() {
    for point in [Fault::PendingStart, Fault::PendingStage, Fault::PendingClear] {
        let temp = tempfile::tempdir().expect("temp");
        let base = provider(temp.path());
        let fault = Injection::new(point);
        let options = QueryOptions {
            recovery: QueryRecoveryPolicy::Strict,
            publication: QueryPublicationMode::NonAtomic,
        };
        {
            let mut query = ContinuousQueryTransformer::new_with_options(
                definition(false),
                Arc::new(Provider {
                    inner: base.clone(),
                    fault: fault.clone(),
                }),
                options,
            )
            .await
            .expect("construct");
            query.start().await.expect("start");
            fault.armed.store(true, Ordering::Release);
            assert!(query.transform(input(1)).await.is_err());
            query.stop().await.expect("cleanup");
        }
        let mut reopened =
            ContinuousQueryTransformer::new_with_options(definition(false), base, options)
                .await
                .expect("reopen");
        if point == Fault::PendingStart {
            reopened
                .start()
                .await
                .expect("initial fence failed before evaluation began");
            assert_eq!(
                reopened.transform(input(1)).await.expect("replay input")[0]
                    .envelope
                    .system()
                    .sequence(),
                1
            );
        } else {
            let error = reopened
                .start()
                .await
                .expect_err("successful core commit, incomplete publication");
            assert!(matches!(
                error.downcast_ref::<QueryRecoveryError>(),
                Some(QueryRecoveryError::PendingPublication)
            ));
        }
        reopened.stop().await.expect("cleanup");
    }
}

struct Bootstrap;
#[async_trait]
impl ComputationBootstrapProvider for Bootstrap {
    async fn snapshot(&self) -> anyhow::Result<ComputationBootstrapSnapshot> {
        Ok(ComputationBootstrapSnapshot {
            changes: Box::pin(futures::stream::iter([Ok(input(1).envelope)])),
            watermarks: vec![BootstrapWatermark {
                stream: StreamId::try_new("source/out").expect("stream"),
                source_id: None,
                sequence: 1,
                position: None,
            }],
        })
    }
}

#[tokio::test]
async fn recovery_io_failure_does_not_authorize_auto_reset_of_committed_state() {
    struct NoBootstrap;
    #[async_trait]
    impl ComputationBootstrapProvider for NoBootstrap {
        async fn snapshot(&self) -> anyhow::Result<ComputationBootstrapSnapshot> {
            panic!("a storage read outage must not trigger destructive bootstrap")
        }
    }

    let temp = tempfile::tempdir().expect("temp");
    let base = provider(temp.path());
    {
        let mut first = ContinuousQueryTransformer::new(definition(false), base.clone())
            .await
            .expect("query");
        first.start().await.expect("start");
        first.transform(input(1)).await.expect("committed input");
        first.stop().await.expect("stop");
    }
    let fault = Injection::new(Fault::ReadLiveRows);
    {
        let mut unavailable = ContinuousQueryTransformer::new_with_options(
            definition(false),
            Arc::new(Provider {
                inner: base.clone(),
                fault: fault.clone(),
            }),
            QueryOptions {
                recovery: QueryRecoveryPolicy::AutoReset,
                publication: QueryPublicationMode::Atomic,
            },
        )
        .await
        .expect("reopen")
        .with_bootstrap(Arc::new(NoBootstrap));
        fault.armed.store(true, Ordering::Release);
        let error = unavailable.start().await.expect_err("storage outage");
        assert!(matches!(
            error.downcast_ref::<IndexError>(),
            Some(IndexError::IOError)
        ));
        assert!(error.downcast_ref::<QueryRecoveryError>().is_none());
        unavailable.stop().await.expect("cleanup");
    }
    let mut recovered = ContinuousQueryTransformer::new(definition(false), base)
        .await
        .expect("reopen after outage");
    recovered.start().await.expect("committed state retained");
    let snapshot = recovered.results().snapshot().expect("snapshot");
    assert_eq!(snapshot.as_of_sequence, 1);
    assert_eq!(snapshot.rows.len(), 1);
    assert_eq!(snapshot.generation, 0);
    assert_eq!(recovered.results().replay(0).expect("outbox").len(), 1);
    recovered.stop().await.expect("stop");
}

#[tokio::test]
async fn bootstrap_projection_and_completion_checkpoint_failures_keep_handoff_closed_until_reset() {
    for point in [Fault::LiveRows, Fault::BootstrapComplete] {
        let temp = tempfile::tempdir().expect("temp");
        let base = provider(temp.path());
        let fault = Injection::new(point);
        {
            let mut query = ContinuousQueryTransformer::new(
                definition(false),
                Arc::new(Provider {
                    inner: base.clone(),
                    fault: fault.clone(),
                }),
            )
            .await
            .expect("construct")
            .with_bootstrap(Arc::new(Bootstrap));
            fault.armed.store(true, Ordering::Release);
            assert!(query.start().await.is_err());
            assert!(
                query.transform(input(2)).await.is_err(),
                "failed bootstrap must close live ingress"
            );
            query.stop().await.expect("cleanup");
        }
        {
            let mut strict = ContinuousQueryTransformer::new(definition(false), base.clone())
                .await
                .expect("reopen")
                .with_bootstrap(Arc::new(Bootstrap));
            let error = strict.start().await.expect_err("incomplete durable marker");
            assert!(matches!(
                error.downcast_ref::<QueryRecoveryError>(),
                Some(QueryRecoveryError::IncompleteBootstrap)
            ));
            strict.stop().await.expect("cleanup");
        }
        let mut reset = ContinuousQueryTransformer::new_with_options(
            definition(false),
            base,
            QueryOptions {
                recovery: QueryRecoveryPolicy::AutoReset,
                publication: QueryPublicationMode::Atomic,
            },
        )
        .await
        .expect("reset")
        .with_bootstrap(Arc::new(Bootstrap));
        reset.start().await.expect("complete new snapshot");
        assert_eq!(reset.results().snapshot().expect("snapshot").rows.len(), 1);
        assert!(reset
            .transform(input(1))
            .await
            .expect("covered watermark")
            .is_empty());
        reset.stop().await.expect("stop");
    }
}

#[tokio::test]
async fn cancelled_committed_source_and_future_outputs_recover_without_reusing_sequences() {
    for backend in [Backend::Computation, Backend::OrdinaryPlugin] {
        cancelled_committed_outputs(backend, EntryPoint::Direct).await;
    }
}

#[tokio::test]
async fn transaction_query_body_replays_cancelled_commits_before_accepting_more_input() {
    for backend in [Backend::Computation, Backend::OrdinaryPlugin] {
        cancelled_committed_outputs(backend, EntryPoint::TransactionBody).await;
    }
}

async fn cancelled_committed_outputs(backend: Backend, entry: EntryPoint) {
    for temporal in [false, true] {
        let temp = tempfile::tempdir().expect("temp");
        let base = provider_for(temp.path(), backend);
        let fault = Injection::new(Fault::CommittedThenWait);
        {
            let query = ContinuousQueryTransformer::new(
                definition(temporal),
                Arc::new(Provider {
                    inner: base.clone(),
                    fault: fault.clone(),
                }),
            )
            .await
            .expect("construct");
            let mut query = QueryUnderTest::new(query, entry);
            query.start().await.expect("start");
            if temporal {
                assert!(query
                    .transform(input(1))
                    .await
                    .expect("schedule")
                    .is_empty());
            }
            fault.armed.store(true, Ordering::Release);
            let mut operation = Box::pin(async {
                if temporal {
                    query.on_wakeup().await
                } else {
                    query.transform(input(1)).await
                }
            });
            tokio::select! {
                result = &mut operation => panic!("commit result deliberately withheld: {result:?}"),
                _ = fault.committed.notified() => {}
            }
            drop(operation);
            assert_eq!(
                query
                    .results()
                    .snapshot()
                    .expect("not yet published")
                    .as_of_sequence,
                0
            );
            assert!(
                query.transform(input(2)).await.is_err(),
                "late commit cannot open failed ingress"
            );
            query.stop().await.expect("join owned I/O and retire query");
        }
        let recovered = ContinuousQueryTransformer::new(definition(temporal), base)
            .await
            .expect("reopen");
        let mut recovered = QueryUnderTest::new(recovered, entry);
        recovered.start().await.expect("committed output recovery");
        let retained = recovered
            .results()
            .replay(0)
            .expect("unpublished retained output");
        assert_eq!(retained.len(), 1);
        assert_eq!(retained[0].system().sequence(), 1);
        if entry == EntryPoint::TransactionBody {
            assert!(recovered.has_pending_emissions());
            let replay = recovered
                .continue_transform()
                .await
                .expect("pending committed output");
            assert_eq!(replay.len(), 1);
            assert_ne!(replay[0].envelope.id(), retained[0].id());
            assert_eq!(
                QueryRecoveryIdentity::from_envelope(&replay[0].envelope).unwrap(),
                QueryRecoveryIdentity::from_envelope(&retained[0]).unwrap()
            );
            assert_eq!(
                QueryChangeCodec::query_generation(&replay[0].envelope).unwrap(),
                QueryChangeCodec::query_generation(&retained[0]).unwrap()
            );
            assert_eq!(
                QueryChangeCodec::query_sequence(&replay[0].envelope).unwrap(),
                1
            );
            assert_eq!(replay[0].envelope.system().sequence(), 2);
            recovered
                .delivery_completed(&replay)
                .await
                .expect("durable handoff");
            assert!(!recovered.has_pending_emissions());
        }
        assert!(recovered
            .transform(input(1))
            .await
            .expect("raw input dedup")
            .is_empty());
        if temporal {
            assert!(recovered
                .on_wakeup()
                .await
                .expect("committed pop")
                .is_empty());
        } else {
            assert_eq!(
                recovered.transform(input(2)).await.expect("next output")[0]
                    .envelope
                    .system()
                    .sequence(),
                if entry == EntryPoint::TransactionBody {
                    3
                } else {
                    2
                }
            );
        }

        recovered.stop().await.expect("stop");
    }
}

#[tokio::test]
async fn transaction_query_body_failed_confirmation_replays_identical_results_without_recalculation(
) {
    for backend in [Backend::Computation, Backend::OrdinaryPlugin] {
        let temp = tempfile::tempdir().expect("temp");
        let base = provider_for(temp.path(), backend);
        let fault = Injection::new(Fault::DeliveryConfirmed);
        let output;
        {
            let query = ContinuousQueryTransformer::new(
                definition(false),
                Arc::new(Provider {
                    inner: base.clone(),
                    fault: fault.clone(),
                }),
            )
            .await
            .expect("construct");
            let mut query = TransactionTransformer::from_query(query);
            query.start().await.expect("start");
            output = query.transform(input(1)).await.expect("commit result");
            fault.armed.store(true, Ordering::Release);
            assert!(query.delivery_completed(&output).await.is_err());
            query.stop().await.expect("cleanup");
        }
        {
            let query = ContinuousQueryTransformer::new(definition(false), base.clone())
                .await
                .expect("reopen");
            let mut query = TransactionTransformer::from_query(query);
            query.start().await.expect("start");
            assert!(query.has_pending_emissions());
            let replay = query.continue_transform().await.expect("replay");
            assert_eq!(replay.len(), 1);
            assert_ne!(replay[0].envelope.id(), output[0].envelope.id());
            assert_eq!(
                QueryRecoveryIdentity::from_envelope(&replay[0].envelope).unwrap(),
                QueryRecoveryIdentity::from_envelope(&output[0].envelope).unwrap()
            );
            assert_eq!(
                QueryChangeCodec::query_sequence(&replay[0].envelope).unwrap(),
                1
            );
            assert_eq!(
                QueryChangeCodec::query_generation(&replay[0].envelope).unwrap(),
                QueryChangeCodec::query_generation(&output[0].envelope).unwrap()
            );
            assert_eq!(replay[0].envelope.system().sequence(), 2);
            let result = QueryChangeCodec::to_legacy_result(&replay[0].envelope).expect("result");
            assert!(matches!(result.results.as_slice(),
                        [drasi_lib::channels::ResultDiff::Add { data, .. }] if data == &serde_json::json!({"name":"name-1"})));
            let snapshot = query.query_results().unwrap().snapshot().unwrap();
            assert_eq!(snapshot.as_of_sequence, 1);
            assert_eq!(snapshot.rows.len(), 1);
            query
                .delivery_completed(&replay)
                .await
                .expect("confirm replay");
            assert!(query
                .transform(input(1))
                .await
                .expect("deduplicated input")
                .is_empty());
            query.stop().await.expect("stop");
        }
        let query = ContinuousQueryTransformer::new(definition(false), base)
            .await
            .expect("reopen confirmed");
        let mut query = TransactionTransformer::from_query(query);
        query.start().await.expect("start");
        assert!(!query.has_pending_emissions());
        assert_eq!(query.query_results().unwrap().replay(0).unwrap().len(), 1);
        query.stop().await.expect("cleanup");
    }
}
#[tokio::test]
async fn strict_recovery_rejects_missing_or_gapped_committed_output() {
    for backend in [Backend::Computation, Backend::OrdinaryPlugin] {
        strict_recovery_output_validation(backend).await;
    }
}

async fn strict_recovery_output_validation(backend: Backend) {
    for gap in [false, true] {
        let temp = tempfile::tempdir().expect("temp");
        let base = provider_for(temp.path(), backend);
        {
            let mut query = ContinuousQueryTransformer::new(definition(false), base.clone())
                .await
                .expect("construct");
            query.start().await.expect("start");
            for sequence in 1..=3 {
                query.transform(input(sequence)).await.expect("output");
            }
            query.stop().await.expect("stop");
        }
        {
            let resources = base
                .create_indexes("faults", "query")
                .await
                .expect("resources");
            let outbox = resources.outbox_writer().expect("outbox");
            let rows = outbox.read_from("query", 0).await.expect("retained");
            resources
                .indexes()
                .session_control
                .begin()
                .await
                .expect("begin");
            outbox.clear("query").await.expect("clear");
            if gap {
                for (sequence, bytes) in rows.iter().filter(|(sequence, _)| *sequence != 2) {
                    outbox
                        .append("query", *sequence, bytes)
                        .await
                        .expect("interior gap");
                }
            }
            resources
                .indexes()
                .session_control
                .commit()
                .await
                .expect("commit corruption");
            resources
                .cleanup()
                .expect("owner")
                .shutdown()
                .await
                .expect("cleanup");
        }
        let mut query = ContinuousQueryTransformer::new(definition(false), base)
            .await
            .expect("reopen");
        let error = query.start().await.expect_err("cannot infer lost output");
        assert!(matches!(
            error.downcast_ref::<QueryRecoveryError>(),
            Some(QueryRecoveryError::Inconsistent(_))
        ));
        query.stop().await.expect("cleanup");
    }
}

fn replayed_source_input(transport: u64) -> InputEnvelope {
    let original = input(1);
    let change = GraphChangeCodec::decode_changes(&original.envelope)
        .expect("original source change")
        .remove(0);
    let source = drasi_lib::channels::SourceEventWrapper::new(
        "people".into(),
        drasi_lib::channels::SourceEvent::Change(change),
        chrono::DateTime::from_timestamp_millis(1000).expect("timestamp"),
        1,
    );
    InputEnvelope {
        port: PortId::try_new("in").expect("port"),
        envelope: GraphChangeCodec::encode_source_event(
            Arc::new(source),
            &ComponentId::try_new("source").expect("source"),
            StreamId::try_new("source/out").expect("stream"),
            transport,
            None,
        )
        .expect("source envelope"),
    }
}

#[tokio::test]
async fn transport_checkpoint_failure_cannot_commit_query_state_or_advance_duplicate_input() {
    for backend in [Backend::Computation, Backend::OrdinaryPlugin] {
        let directory = tempfile::tempdir().expect("storage");
        let base = provider_for(directory.path(), backend);
        let fault = Injection::new(Fault::InputTransport);
        let progress = Arc::new(
            QuerySourceProgress::new("faults", ComponentId::try_new("query").expect("query"))
                .expect("progress"),
        );
        {
            let query = ContinuousQueryTransformer::new(
                definition(false),
                Arc::new(Provider {
                    inner: base.clone(),
                    fault: fault.clone(),
                }),
            )
            .await
            .expect("query")
            .with_source_progress(progress.clone())
            .expect("source progress");
            let mut query = TransactionTransformer::from_query(query);
            query.start().await.expect("start");
            fault.armed.store(true, Ordering::Release);
            assert!(query.transform(replayed_source_input(11)).await.is_err());
            assert_eq!(
                query
                    .query_results()
                    .expect("query results")
                    .snapshot()
                    .expect("snapshot")
                    .as_of_sequence,
                0
            );
            assert!(progress.snapshot().checkpoints.is_empty());
            assert!(progress.snapshot().transport_sequences.is_empty());
            query.stop().await.expect("cleanup");
        }
        {
            let stored = base
                .create_indexes("faults", "query")
                .await
                .expect("reopen failed transaction");
            let checkpoints = stored
                .checkpoint_store()
                .expect("checkpoints")
                .read_all_checkpoints()
                .await
                .expect("durable checkpoints");
            assert!(checkpoints
                .keys()
                .all(|key| !key.starts_with("computation:input:")
                    && !key.starts_with("\0computation:input-transport:v1:")));
            assert!(stored
                .outbox_writer()
                .expect("outbox")
                .read_from("query", 0)
                .await
                .expect("durable output")
                .is_empty());
            assert!(stored
                .live_results_writer()
                .expect("rows")
                .read_snapshot("query")
                .await
                .expect("durable rows")
                .is_empty());
            stored
                .cleanup()
                .expect("owner")
                .shutdown()
                .await
                .expect("release observer");
        }
        {
            let query = ContinuousQueryTransformer::new(
                definition(false),
                Arc::new(Provider {
                    inner: base.clone(),
                    fault: fault.clone(),
                }),
            )
            .await
            .expect("reopen")
            .with_source_progress(progress.clone())
            .expect("progress");
            let mut query = TransactionTransformer::from_query(query);
            query.start().await.expect("start after rollback");
            let output = query
                .transform(replayed_source_input(11))
                .await
                .expect("replay rolled-back input");
            assert_eq!(output.len(), 1);
            query.delivery_completed(&output).await.expect("confirm");
            assert_eq!(
                progress.snapshot().checkpoints[&SourceProgressKey::Source("people".into())]
                    .sequence,
                1
            );
            assert_eq!(
                progress.snapshot().transport_sequences
                    [&StreamId::try_new("source/out").expect("stream")],
                11
            );
            fault.armed.store(true, Ordering::Release);
            assert!(
                query.transform(replayed_source_input(12)).await.is_err(),
                "duplicate progress must not hide persistence failure"
            );
            query.stop().await.expect("cleanup");
        }
        {
            let stored = base
                .create_indexes("faults", "query")
                .await
                .expect("independent durable inspection");
            let all = stored
                .checkpoint_store()
                .expect("checkpoints")
                .read_all_checkpoints()
                .await
                .expect("committed checkpoints");
            let transport: Vec<_> = all
                .iter()
                .filter(|(key, _)| key.starts_with("\0computation:input-transport:v1:"))
                .collect();
            assert_eq!(transport.len(), 1);
            assert_eq!(
                transport[0].1.sequence, 11,
                "failed duplicate receipt cannot advance transport progress"
            );
            assert_eq!(
                stored
                    .outbox_writer()
                    .expect("outbox")
                    .read_from("query", 0)
                    .await
                    .expect("committed outputs")
                    .len(),
                1
            );
            stored
                .cleanup()
                .expect("owner")
                .shutdown()
                .await
                .expect("close observer");
        }
        let query = ContinuousQueryTransformer::new(definition(false), base)
            .await
            .expect("reopen")
            .with_source_progress(progress.clone())
            .expect("progress");
        let mut query = TransactionTransformer::from_query(query);
        query.start().await.expect("start");
        assert!(query
            .transform(replayed_source_input(12))
            .await
            .expect("duplicate retry")
            .is_empty());
        assert_eq!(
            query
                .query_results()
                .expect("query results")
                .snapshot()
                .expect("snapshot")
                .as_of_sequence,
            1
        );
        assert_eq!(
            progress.snapshot().transport_sequences
                [&StreamId::try_new("source/out").expect("stream")],
            12
        );
        query.stop().await.expect("stop");
    }
}

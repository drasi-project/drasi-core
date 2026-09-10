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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fault {
    InputCheckpoint,
    ResultSequence,
    PendingStage,
    PendingClear,
    BootstrapComplete,
    LiveRows,
    Commit,
    CommittedThenWait,
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
        let fault = if key.starts_with("computation:input:") {
            Some(Fault::InputCheckpoint)
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
    async fn write_result_sequence(&self, id: &str, sequence: u64) -> Result<(), IndexError> {
        if self.fault.take(Fault::ResultSequence) {
            return Err(IndexError::IOError);
        }
        self.inner.write_result_sequence(id, sequence).await
    }
    async fn read_result_sequence(&self, id: &str) -> Result<Option<u64>, IndexError> {
        self.inner.read_result_sequence(id).await
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
    Arc::new(RocksDbComputationProvider::new(
        path,
        RocksIndexOptions::new(
            false,
            false,
            RocksDbMemoryBudget::from_total_budget_bytes(32 << 20).expect("budget"),
        ),
    ))
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
    for point in [Fault::InputCheckpoint, Fault::LiveRows, Fault::ResultSequence, Fault::Commit] {
        let temp = tempfile::tempdir().expect("temp");
        let base = provider(temp.path());
        let fault = Injection::new(point);
        let progress = Arc::new(
            QuerySourceProgress::new("faults", ComponentId::try_new("query").expect("id"))
                .expect("scope"),
        );
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
            .with_source_progress(progress.clone())
            .expect("source progress");
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
        let mut reopened = ContinuousQueryTransformer::new(definition(false), base)
            .await
            .expect("reopen")
            .with_source_progress(progress.clone())
            .expect("source progress");
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
async fn non_atomic_pending_marker_stage_and_clear_failures_have_distinct_recovery_boundaries() {
    for point in [Fault::PendingStage, Fault::PendingClear] {
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
        if point == Fault::PendingStage {
            reopened
                .start()
                .await
                .expect("marker stage failed before core commit");
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
    for temporal in [false, true] {
        let temp = tempfile::tempdir().expect("temp");
        let base = provider(temp.path());
        let fault = Injection::new(Fault::CommittedThenWait);
        {
            let mut query = ContinuousQueryTransformer::new(
                definition(temporal),
                Arc::new(Provider {
                    inner: base.clone(),
                    fault: fault.clone(),
                }),
            )
            .await
            .expect("construct");
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
        let mut recovered = ContinuousQueryTransformer::new(definition(temporal), base)
            .await
            .expect("reopen");
        recovered.start().await.expect("committed output recovery");
        let retained = recovered
            .results()
            .replay(0)
            .expect("unpublished retained output");
        assert_eq!(retained.len(), 1);
        assert_eq!(retained[0].system().sequence(), 1);
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
                2
            );
        }
        recovered.stop().await.expect("stop");
    }
}

#[tokio::test]
async fn strict_recovery_rejects_missing_or_gapped_committed_output() {
    for gap in [false, true] {
        let temp = tempfile::tempdir().expect("temp");
        let base = provider(temp.path());
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

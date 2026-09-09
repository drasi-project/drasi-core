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

#![cfg(feature = "computation")]

use std::{path::Path, sync::Arc};

use async_trait::async_trait;
use bytes::Bytes;
use drasi_core::{
    computation::{
        AtomicResultTransaction, ComputationIndexProvider, ComputationIndexes, ComputationQuery,
        ComputationQueryError, ComputationResource, TransactionDomain,
    },
    evaluation::{
        context::QueryPartEvaluationContext,
        functions::{aggregation::RegisterAggregationFunctions, FunctionRegistry},
        variable_value::VariableValue,
    },
    interface::{
        CheckpointStore, IndexBackendPlugin, IndexError, IndexSet, LiveResultsWriter, OutboxWriter,
        RowMutation, SessionControl, SessionGuard,
    },
    models::{Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange},
    query::QueryBuilder,
};
use drasi_index_rocksdb::{
    computation::RocksDbComputationProvider, RocksDbIndexProvider, RocksDbMemoryBudget,
    RocksIndexOptions,
};
use drasi_query_cypher::CypherParser;
use serde_json::json;

const QUERY: &str = "people";

struct Fixture {
    query: ComputationQuery,
    transaction: AtomicResultTransaction,
    checkpoint: Arc<dyn CheckpointStore>,
    outbox: Arc<dyn OutboxWriter>,
    live: Arc<dyn LiveResultsWriter>,
}

fn provider(path: &Path) -> RocksDbComputationProvider {
    RocksDbComputationProvider::new(
        path,
        RocksIndexOptions::new(
            false,
            false,
            RocksDbMemoryBudget::from_total_budget_bytes(32 << 20).expect("memory budget"),
        ),
    )
}

async fn resources(path: &Path, graph: &str) -> ComputationIndexes {
    provider(path)
        .create_indexes(graph, QUERY)
        .await
        .expect("graph-owned indexes")
}

async fn fixture(path: &Path, graph: &str, query: &str) -> Fixture {
    let resources = resources(path, graph).await;
    from_resources(resources, query).await
}

async fn from_resources(resources: ComputationIndexes, query: &str) -> Fixture {
    let transaction = resources
        .atomic_result_transaction()
        .expect("complete computation transaction");
    let checkpoint = resources.checkpoint_store().expect("checkpoint").clone();
    let outbox = resources.outbox_writer().expect("outbox").clone();
    let live = resources
        .live_results_writer()
        .expect("live projection")
        .clone();
    let functions = Arc::new(FunctionRegistry::new());
    functions.register_aggregation_functions();
    let query = ComputationQuery::try_build(
        QueryBuilder::new(query, Arc::new(CypherParser::new(functions.clone())))
            .with_function_registry(functions),
        resources,
    )
    .await
    .expect("computation query");
    Fixture {
        query,
        transaction,
        checkpoint,
        outbox,
        live,
    }
}

struct FailingCommit(Arc<dyn SessionControl>);

#[async_trait]
impl SessionControl for FailingCommit {
    async fn begin(&self) -> Result<(), IndexError> {
        self.0.begin().await
    }

    async fn commit(&self) -> Result<(), IndexError> {
        Err(IndexError::CorruptedData)
    }

    fn rollback(&self) -> Result<(), IndexError> {
        self.0.rollback()
    }
}

fn rebind_resources(
    resources: &ComputationIndexes,
    control: Arc<dyn SessionControl>,
    output_participates: bool,
) -> ComputationIndexes {
    let domain = TransactionDomain::new(control.clone());
    let set = resources.indexes();
    let outbox = resources.outbox_writer().expect("outbox").clone();
    ComputationIndexes::try_new(
        IndexSet {
            element_index: set.element_index.clone(),
            archive_index: set.archive_index.clone(),
            result_index: set.result_index.clone(),
            future_queue: set.future_queue.clone(),
            session_control: control,
        },
        Some(domain.clone()),
        Some(ComputationResource::participating(
            resources.checkpoint_store().expect("checkpoint").clone(),
            &domain,
        )),
        Some(if output_participates {
            ComputationResource::participating(outbox, &domain)
        } else {
            ComputationResource::independent(outbox)
        }),
        Some(ComputationResource::participating(
            resources.live_results_writer().expect("live").clone(),
            &domain,
        )),
    )
    .expect("explicit provider participation")
    .with_cleanup(resources.cleanup().expect("graph cleanup owner").clone())
}

fn person(id: &str) -> SourceChange {
    SourceChange::Insert {
        element: Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new("source", id),
                labels: Arc::from([Arc::from("Person")]),
                effective_from: 1000,
            },
            properties: ElementPropertyMap::from(json!({ "name": id })),
        },
    }
}

async fn stage(fixture: &Fixture, sequence: u64, signature: u64) -> Result<(), IndexError> {
    fixture
        .checkpoint
        .stage_checkpoint("source", sequence, Some(&Bytes::from_static(b"position")))
        .await?;
    fixture
        .checkpoint
        .write_result_sequence(QUERY, sequence)
        .await?;
    fixture.outbox.append(QUERY, sequence, b"output").await?;
    fixture.outbox.trim_to_capacity(QUERY, 1).await?;
    fixture
        .live
        .apply_mutations(
            QUERY,
            &[RowMutation {
                row_signature: signature,
                data: Some(b"live-row"),
            }],
        )
        .await
}

async fn assert_empty(fixture: &Fixture) {
    assert!(fixture
        .checkpoint
        .read_checkpoint("source")
        .await
        .expect("read checkpoint")
        .is_none());
    assert!(fixture
        .checkpoint
        .read_result_sequence(QUERY)
        .await
        .expect("read sequence")
        .is_none());
    assert!(fixture
        .outbox
        .read_from(QUERY, 0)
        .await
        .expect("read output")
        .is_empty());
    assert!(fixture
        .live
        .read_snapshot(QUERY)
        .await
        .expect("read live")
        .is_empty());
}

#[tokio::test]
async fn graph_query_commits_indexes_checkpoint_sequence_outbox_and_live_rows_together() {
    let temp = tempfile::tempdir().expect("temp dir");
    {
        let fixture = fixture(
            temp.path(),
            "graph",
            "MATCH (n:Person) RETURN n.name AS name",
        )
        .await;
        let mut observed = None;
        let results = fixture
            .query
            .process_source_change_with_result_hook(
                person("alice"),
                &fixture.transaction,
                |result| {
                    let result = result.clone();
                    let observed = &mut observed;
                    let fixture = &fixture;
                    async move {
                        assert!(matches!(
                            result.as_ref(),
                            [QueryPartEvaluationContext::Adding { .. }]
                        ));
                        stage(fixture, 1, result[0].row_signature()).await?;
                        assert_empty_committed_output(fixture).await;
                        *observed = Some(result);
                        Ok(())
                    }
                },
            )
            .await
            .expect("commit");
        assert!(Arc::ptr_eq(&observed.expect("hook result"), &results));
        assert_eq!(
            fixture
                .checkpoint
                .read_result_sequence(QUERY)
                .await
                .expect("sequence"),
            Some(1)
        );
        assert_eq!(
            fixture.outbox.read_from(QUERY, 0).await.expect("outbox"),
            [(1, b"output".to_vec())]
        );
        assert_eq!(
            fixture.live.read_snapshot(QUERY).await.expect("live"),
            [(results[0].row_signature(), b"live-row".to_vec())]
        );
    }
    let reopened = resources(temp.path(), "graph").await;
    assert_eq!(
        reopened
            .checkpoint_store()
            .expect("checkpoint")
            .read_result_sequence(QUERY)
            .await
            .expect("reopened sequence"),
        Some(1)
    );
    assert_eq!(
        reopened
            .outbox_writer()
            .expect("outbox")
            .read_from(QUERY, 0)
            .await
            .expect("reopened outbox"),
        [(1, b"output".to_vec())]
    );
}

async fn assert_empty_committed_output(fixture: &Fixture) {
    assert_eq!(
        fixture
            .checkpoint
            .read_result_sequence(QUERY)
            .await
            .expect("sequence"),
        None
    );
    assert!(fixture
        .outbox
        .read_from(QUERY, 0)
        .await
        .expect("output")
        .is_empty());
    assert!(fixture
        .live
        .read_snapshot(QUERY)
        .await
        .expect("live")
        .is_empty());
}

#[tokio::test]
async fn hook_failure_rolls_back_aggregate_indexes_and_every_output_resource() {
    let temp = tempfile::tempdir().expect("temp dir");
    let fixture = fixture(
        temp.path(),
        "rollback",
        "MATCH (n:Person) RETURN count(n) AS total",
    )
    .await;
    let failed = fixture
        .query
        .process_source_change_with_result_hook(person("alice"), &fixture.transaction, |results| {
            let fixture = &fixture;
            async move {
                stage(fixture, 1, results[0].row_signature()).await?;
                Err(IndexError::CorruptedData)
            }
        })
        .await;
    assert!(matches!(
        failed,
        Err(ComputationQueryError::Index(IndexError::CorruptedData))
    ));
    assert_empty(&fixture).await;
    let results = fixture
        .query
        .process_source_change_with_result_hook(person("alice"), &fixture.transaction, |results| {
            let fixture = &fixture;
            async move { stage(fixture, 1, results[0].row_signature()).await }
        })
        .await
        .expect("retry after completed rollback");
    let [QueryPartEvaluationContext::Aggregation { before, after, .. }] = results.as_ref() else {
        panic!("expected aggregation after rollback");
    };
    assert_eq!(
        before.as_ref().expect("initial aggregate").get("total"),
        Some(&VariableValue::from(0))
    );
    assert_eq!(after.get("total"), Some(&VariableValue::from(1)));
}

#[tokio::test]
async fn failed_commit_rolls_back_every_graph_resource() {
    let temp = tempfile::tempdir().expect("temp dir");
    let original = resources(temp.path(), "failed-commit").await;
    let control = Arc::new(FailingCommit(original.indexes().session_control.clone()));
    let fixture = from_resources(
        rebind_resources(&original, control, true),
        "MATCH (n:Person) RETURN n.name AS name",
    )
    .await;
    let failed = fixture
        .query
        .process_source_change_with_result_hook(person("alice"), &fixture.transaction, |result| {
            let fixture = &fixture;
            async move { stage(fixture, 1, result[0].row_signature()).await }
        })
        .await;
    assert!(matches!(
        failed,
        Err(ComputationQueryError::Index(IndexError::CorruptedData))
    ));
    assert_empty(&fixture).await;
    assert!(
        fixture.query.recovery_required(),
        "a failed commit requires recovery rather than an assumed safe retry"
    );
    let _inspection = SessionGuard::begin(original.indexes().session_control.clone())
        .await
        .expect("inspect rolled-back core state");
    assert!(original
        .indexes()
        .element_index
        .get_element(&ElementReference::new("source", "alice"))
        .await
        .expect("read rolled-back element")
        .is_none());
}

#[tokio::test]
async fn incomplete_writer_participation_cannot_produce_a_transaction_capability() {
    let temp = tempfile::tempdir().expect("temp dir");
    let original = resources(temp.path(), "incomplete").await;
    let incomplete = rebind_resources(&original, original.indexes().session_control.clone(), false);
    assert!(matches!(
        incomplete.atomic_result_transaction(),
        Err(ComputationQueryError::AtomicOutputUnsupported)
    ));
}

#[tokio::test]
async fn relationship_query_executes_through_scoped_lazy_index_streams() {
    let temp = tempfile::tempdir().expect("temp dir");
    let fixture = fixture(
        temp.path(),
        "relationship",
        "MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN 1 AS matched",
    )
    .await;
    fixture
        .query
        .process_source_change(person("alice"))
        .await
        .expect("alice");
    fixture
        .query
        .process_source_change(person("bob"))
        .await
        .expect("bob");
    let results = fixture
        .query
        .process_source_change(SourceChange::Insert {
            element: Element::Relation {
                metadata: ElementMetadata {
                    reference: ElementReference::new("source", "knows"),
                    labels: Arc::from([Arc::from("KNOWS")]),
                    effective_from: 2000,
                },
                in_node: ElementReference::new("source", "alice"),
                out_node: ElementReference::new("source", "bob"),
                properties: ElementPropertyMap::new(),
            },
        })
        .await
        .expect("relationship evaluation");
    let [QueryPartEvaluationContext::Adding { after, .. }] = results.as_slice() else {
        panic!("expected the relationship to match");
    };
    assert_eq!(after.get("matched"), Some(&VariableValue::from(1)));
    fixture
        .query
        .shutdown()
        .await
        .expect("join query resources");
}

#[tokio::test]
async fn cancelled_hook_rolls_back_output_and_rejects_unsafe_same_instance_retry() {
    let temp = tempfile::tempdir().expect("temp dir");
    let fixture = fixture(
        temp.path(),
        "cancelled",
        "MATCH (n:Person) RETURN n.name AS name",
    )
    .await;
    let entered = tokio::sync::Notify::new();
    let mut operation = Box::pin(fixture.query.process_source_change_with_result_hook(
        person("alice"),
        &fixture.transaction,
        |result| {
            let fixture = &fixture;
            let entered = &entered;
            async move {
                stage(fixture, 1, result[0].row_signature()).await?;
                entered.notify_one();
                std::future::pending().await
            }
        },
    ));
    tokio::select! {
        result = &mut operation => panic!("hook must remain pending: {result:?}"),
        _ = entered.notified() => {}
    }
    drop(operation);
    assert!(fixture.query.recovery_required());
    assert!(matches!(
        fixture.query.process_source_change(person("alice")).await,
        Err(ComputationQueryError::RecoveryRequired)
    ));
    assert!(fixture.outbox.read_from(QUERY, 0).await.is_err());
    fixture
        .query
        .shutdown()
        .await
        .expect("await registered resource cleanup");
    drop(fixture);
    let reopened = self::fixture(
        temp.path(),
        "cancelled",
        "MATCH (n:Person) RETURN n.name AS name",
    )
    .await;
    assert_empty(&reopened).await;
}

#[tokio::test]
async fn foreign_capability_is_rejected_before_evaluation_or_hook() {
    let temp = tempfile::tempdir().expect("temp dir");
    let first = fixture(
        temp.path(),
        "first",
        "MATCH (n:Person) RETURN n.name AS name",
    )
    .await;
    let second = fixture(
        temp.path(),
        "second",
        "MATCH (n:Person) RETURN n.name AS name",
    )
    .await;
    let mut called = false;
    let rejected = first
        .query
        .process_source_change_with_result_hook(person("alice"), &second.transaction, |_| async {
            called = true;
            Ok(())
        })
        .await;
    assert!(matches!(
        rejected,
        Err(ComputationQueryError::TransactionMismatch)
    ));
    assert!(!called);
    let result = first
        .query
        .process_source_change(person("alice"))
        .await
        .expect("first add");
    assert!(matches!(
        result.as_slice(),
        [QueryPartEvaluationContext::Adding { .. }]
    ));
    assert_empty(&first).await;
    assert_empty(&second).await;
}

#[tokio::test]
async fn due_future_rollback_retains_source_provenance_and_replayable_work() {
    let temp = tempfile::tempdir().expect("temp dir");
    let fixture = fixture(
        temp.path(),
        "future",
        "MATCH (n:Person) WHERE drasi.trueLater(true, 2000) RETURN n.name AS name",
    )
    .await;
    assert!(fixture
        .query
        .process_source_change(person("alice"))
        .await
        .expect("schedule")
        .is_empty());
    let failed = fixture
        .query
        .process_due_futures_with_result_hook(&fixture.transaction, |result| {
            let fixture = &fixture;
            async move {
                assert_eq!(result.source_id.as_ref(), "source");
                stage(fixture, 1, result.results[0].row_signature()).await?;
                Err(IndexError::CorruptedData)
            }
        })
        .await;
    assert!(matches!(
        failed,
        Err(ComputationQueryError::Index(IndexError::CorruptedData))
    ));
    assert_empty(&fixture).await;
    assert_eq!(
        fixture
            .query
            .resources()
            .indexes()
            .future_queue
            .peek_due_time()
            .await
            .expect("raw persistent future"),
        Some(2000)
    );
    let result = fixture
        .query
        .process_due_futures_with_result_hook(&fixture.transaction, |result| {
            let fixture = &fixture;
            async move { stage(fixture, 1, result.results[0].row_signature()).await }
        })
        .await
        .expect("replay")
        .expect("due work retained");
    assert_eq!(result.source_id.as_ref(), "source");
    assert!(fixture
        .query
        .process_due_futures_with_result_hook(&fixture.transaction, |_| async {
            panic!("empty future queue cannot invoke hook")
        })
        .await
        .expect("empty queue")
        .is_none());
}

#[tokio::test]
async fn graph_writers_require_a_session_and_transactional_trim_rolls_back() {
    let temp = tempfile::tempdir().expect("temp dir");
    let resources = resources(temp.path(), "writers").await;
    let outbox = resources.outbox_writer().expect("outbox");
    let live = resources.live_results_writer().expect("live");
    let checkpoint = resources.checkpoint_store().expect("checkpoint");
    assert!(outbox.append(QUERY, 1, b"no-session").await.is_err());
    assert!(checkpoint.write_result_sequence(QUERY, 1).await.is_err());
    assert!(live.apply_mutations(QUERY, &[]).await.is_err());
    {
        let guard = SessionGuard::begin(resources.indexes().session_control.clone())
            .await
            .expect("session");
        outbox.append(QUERY, 1, b"one").await.expect("one");
        outbox.append(QUERY, 2, b"two").await.expect("two");
        guard.commit().await.expect("commit");
    }
    {
        let _guard = SessionGuard::begin(resources.indexes().session_control.clone())
            .await
            .expect("session");
        outbox.append(QUERY, 3, b"three").await.expect("three");
        assert_eq!(outbox.trim_to_capacity(QUERY, 1).await.expect("trim"), 2);
    }
    assert_eq!(
        outbox.read_from(QUERY, 0).await.expect("rolled-back trim"),
        [(1, b"one".to_vec()), (2, b"two".to_vec())]
    );
    assert!(outbox
        .read_from(QUERY, u64::MAX)
        .await
        .expect("after end")
        .is_empty());
}

#[tokio::test]
async fn computation_and_legacy_queries_with_same_id_use_separate_persistent_storage() {
    let temp = tempfile::tempdir().expect("temp dir");
    let legacy = RocksDbIndexProvider::new(temp.path(), false, false)
        .create_indexes(QUERY)
        .await
        .expect("legacy provider");
    let fixture = fixture(
        temp.path(),
        "graph",
        "MATCH (n:Person) RETURN n.name AS name",
    )
    .await;
    fixture
        .query
        .process_source_change_with_result_hook(person("alice"), &fixture.transaction, |results| {
            let fixture = &fixture;
            async move { stage(fixture, 1, results[0].row_signature()).await }
        })
        .await
        .expect("graph commit");
    assert!(legacy
        .checkpoint_store
        .expect("legacy checkpoint")
        .read_checkpoint("source")
        .await
        .expect("legacy read")
        .is_none());
    assert!(legacy
        .outbox_writer
        .expect("legacy outbox")
        .read_from(QUERY, 0)
        .await
        .expect("legacy output")
        .is_empty());
    assert!(temp.path().join(QUERY).is_dir());
    assert!(temp.path().join("computation-v1").is_dir());
}

struct CommitThenWait {
    inner: Arc<dyn SessionControl>,
    committed: Arc<tokio::sync::Notify>,
}

#[async_trait]
impl SessionControl for CommitThenWait {
    async fn begin(&self) -> Result<(), IndexError> {
        self.inner.begin().await
    }

    async fn commit(&self) -> Result<(), IndexError> {
        self.inner.commit().await?;
        self.committed.notify_one();
        std::future::pending().await
    }

    fn rollback(&self) -> Result<(), IndexError> {
        self.inner.rollback()
    }
}

#[tokio::test]
async fn cancelled_commit_requires_recovery_without_falsely_claiming_output_rollback() {
    let temp = tempfile::tempdir().expect("temp dir");
    {
        let original = resources(temp.path(), "commit-cancelled").await;
        let committed = Arc::new(tokio::sync::Notify::new());
        let control = Arc::new(CommitThenWait {
            inner: original.indexes().session_control.clone(),
            committed: committed.clone(),
        });
        let fixture = from_resources(
            rebind_resources(&original, control, true),
            "MATCH (n:Person) RETURN n.name AS name",
        )
        .await;
        let mut operation = Box::pin(fixture.query.process_source_change_with_result_hook(
            person("alice"),
            &fixture.transaction,
            |result| {
                let fixture = &fixture;
                async move { stage(fixture, 1, result[0].row_signature()).await }
            },
        ));
        tokio::select! {
            result = &mut operation => panic!("commit result is deliberately withheld: {result:?}"),
            _ = committed.notified() => {}
        }
        drop(operation);
        assert!(fixture.query.recovery_required());
        assert!(matches!(
            fixture.query.process_source_change(person("bob")).await,
            Err(ComputationQueryError::RecoveryRequired)
        ));
        fixture
            .query
            .shutdown()
            .await
            .expect("await cancelled commit work");
    }
    let reopened = resources(temp.path(), "commit-cancelled").await;
    assert_eq!(
        reopened
            .checkpoint_store()
            .expect("checkpoint")
            .read_result_sequence(QUERY)
            .await
            .expect("recover committed sequence"),
        Some(1)
    );
    assert_eq!(
        reopened
            .outbox_writer()
            .expect("outbox")
            .read_from(QUERY, 0)
            .await
            .expect("recover committed output"),
        [(1, b"output".to_vec())]
    );
}

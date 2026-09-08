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
    future,
    path::Path,
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        Arc,
    },
};

use async_trait::async_trait;
use drasi_core::{
    evaluation::{
        context::{QueryPartEvaluationContext, QueryVariables},
        functions::{aggregation::RegisterAggregationFunctions, FunctionRegistry},
        variable_value::VariableValue,
        EvaluationError,
    },
    interface::{
        AtomicResultTransaction, CheckpointStore, ElementIndex, FutureQueue, IndexBackendPlugin,
        IndexError, LiveResultsWriter, OutboxWriter, RowMutation, SessionControl,
        TransactionDomain,
    },
    models::{Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange},
    query::{ContinuousQuery, QueryBuilder},
};
use drasi_index_rocksdb::RocksDbIndexProvider;
use drasi_query_cypher::CypherParser;
use serde_json::json;

struct QueryFixture {
    query_id: String,
    query: Arc<ContinuousQuery>,
    transaction: AtomicResultTransaction,
    checkpoint_store: Arc<dyn CheckpointStore>,
    outbox_writer: Arc<dyn OutboxWriter>,
    live_results_writer: Arc<dyn LiveResultsWriter>,
    element_index: Arc<dyn ElementIndex>,
    future_queue: Arc<dyn FutureQueue>,
    session_control: Arc<dyn SessionControl>,
}

async fn build_query(
    path: &Path,
    query_id: &str,
    query: &str,
    functions: Arc<FunctionRegistry>,
) -> QueryFixture {
    build_query_internal(path, query_id, query, functions, false).await
}

async fn build_query_with_failing_commit(
    path: &Path,
    query_id: &str,
    query: &str,
    functions: Arc<FunctionRegistry>,
) -> QueryFixture {
    build_query_internal(path, query_id, query, functions, true).await
}

async fn build_query_internal(
    path: &Path,
    query_id: &str,
    query: &str,
    functions: Arc<FunctionRegistry>,
    fail_commit: bool,
) -> QueryFixture {
    let provider = RocksDbIndexProvider::new(path, true, false);
    let created = provider
        .create_indexes(query_id)
        .await
        .expect("create RocksDB indexes");
    let transaction_domain = created
        .set
        .session_control
        .transaction_domain()
        .expect("RocksDB core session domain");
    let transaction = created
        .atomic_result_transaction()
        .expect("RocksDB bundle should share one transaction domain");
    let checkpoint_store = created
        .checkpoint_store
        .clone()
        .expect("RocksDB provides a checkpoint store");
    let outbox_writer = created
        .outbox_writer
        .clone()
        .expect("RocksDB provides an outbox writer");
    let live_results_writer = created
        .live_results_writer
        .clone()
        .expect("RocksDB provides a live-results writer");
    let indexes = created.set;
    let session_control: Arc<dyn SessionControl> = if fail_commit {
        Arc::new(FailingCommitSessionControl {
            inner: indexes.session_control.clone(),
            transaction_domain: transaction_domain.clone(),
        })
    } else {
        indexes.session_control.clone()
    };
    let parser = Arc::new(CypherParser::new(functions.clone()));
    let continuous_query = Arc::new(
        QueryBuilder::new(query, parser)
            .with_function_registry(functions)
            .with_element_index(indexes.element_index.clone())
            .with_archive_index(indexes.archive_index)
            .with_result_index(indexes.result_index)
            .with_future_queue(indexes.future_queue.clone())
            .with_session_control(session_control.clone())
            .build()
            .await,
    );

    QueryFixture {
        query_id: query_id.to_string(),
        query: continuous_query,
        transaction,
        checkpoint_store,
        outbox_writer,
        live_results_writer,
        element_index: indexes.element_index,
        future_queue: indexes.future_queue,
        session_control,
    }
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

    async fn begin(&self) -> Result<(), IndexError> {
        self.inner.begin().await
    }

    async fn commit(&self) -> Result<(), IndexError> {
        Err(IndexError::CorruptedData)
    }

    fn rollback(&self) -> Result<(), IndexError> {
        self.inner.rollback()
    }
}

async fn stage_all_persistent_output(
    checkpoint_store: &dyn CheckpointStore,
    outbox_writer: &dyn OutboxWriter,
    live_results_writer: &dyn LiveResultsWriter,
    query_id: &str,
    sequence: u64,
    row_signature: u64,
    outbox_capacity: usize,
) -> Result<(), IndexError> {
    let outbox_data = format!("result-{sequence}").into_bytes();
    let live_result_data = format!("row-{sequence}").into_bytes();
    checkpoint_store
        .stage_checkpoint("people", sequence, None)
        .await?;
    outbox_writer
        .append(query_id, sequence, &outbox_data)
        .await?;
    outbox_writer
        .trim_to_capacity(query_id, outbox_capacity)
        .await?;
    live_results_writer
        .apply_mutations(
            query_id,
            &[RowMutation {
                row_signature,
                data: Some(&live_result_data),
            }],
        )
        .await?;
    checkpoint_store
        .write_result_sequence(query_id, sequence)
        .await
}

async fn assert_no_persistent_output(fixture: &QueryFixture) {
    assert!(fixture
        .checkpoint_store
        .read_checkpoint("people")
        .await
        .expect("read checkpoint")
        .is_none());
    assert!(fixture
        .outbox_writer
        .read_from(&fixture.query_id, 0)
        .await
        .expect("read outbox")
        .is_empty());
    assert!(fixture
        .live_results_writer
        .read_snapshot(&fixture.query_id)
        .await
        .expect("read live results")
        .is_empty());
    assert_eq!(
        fixture
            .checkpoint_store
            .read_result_sequence(&fixture.query_id)
            .await
            .expect("read result sequence"),
        None
    );
}

fn person_change(id: &str, name: &str, effective_from: u64) -> SourceChange {
    SourceChange::Insert {
        element: Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new("people", id),
                labels: Arc::new([Arc::from("Person")]),
                effective_from,
            },
            properties: ElementPropertyMap::from(json!({ "name": name })),
        },
    }
}

fn value<'a>(variables: &'a QueryVariables, key: &str) -> &'a VariableValue {
    variables
        .get(key)
        .unwrap_or_else(|| panic!("missing result variable {key}"))
}

fn assert_count_transition(
    results: &[QueryPartEvaluationContext],
    expected_before: i64,
    expected_after: i64,
) {
    let [QueryPartEvaluationContext::Aggregation {
        before: Some(before),
        after,
        ..
    }] = results
    else {
        panic!("expected one aggregation result, got {results:?}");
    };
    assert_eq!(
        value(before, "total"),
        &VariableValue::Integer(expected_before.into())
    );
    assert_eq!(
        value(after, "total"),
        &VariableValue::Integer(expected_after.into())
    );
}

fn assert_added_name(results: &[QueryPartEvaluationContext], expected: &str) -> u64 {
    let [QueryPartEvaluationContext::Adding {
        after,
        row_signature,
    }] = results
    else {
        panic!("expected one adding result, got {results:?}");
    };
    assert_eq!(value(after, "name"), &VariableValue::from(expected));
    *row_signature
}

#[tokio::test]
async fn provider_writers_preserve_outside_session_behavior() {
    let temp_dir = tempfile::TempDir::new().expect("create temp directory");
    let fixture = build_query(
        temp_dir.path(),
        "outside-session-writers",
        "MATCH (n:Person) RETURN n.name AS name",
        Arc::new(FunctionRegistry::new()),
    )
    .await;

    fixture
        .outbox_writer
        .append(&fixture.query_id, 1, b"first")
        .await
        .expect("outside-session outbox append");
    fixture
        .outbox_writer
        .append(&fixture.query_id, 2, b"second")
        .await
        .expect("outside-session outbox append");
    assert_eq!(
        fixture
            .outbox_writer
            .trim_to_capacity(&fixture.query_id, 1)
            .await
            .expect("outside-session outbox trim"),
        1
    );
    fixture
        .live_results_writer
        .apply_mutations(
            &fixture.query_id,
            &[RowMutation {
                row_signature: 42,
                data: Some(b"live"),
            }],
        )
        .await
        .expect("outside-session live-result mutation");
    fixture
        .checkpoint_store
        .write_result_sequence(&fixture.query_id, 2)
        .await
        .expect("outside-session result-sequence write");

    assert_eq!(
        fixture
            .outbox_writer
            .read_from(&fixture.query_id, 0)
            .await
            .expect("read outside-session outbox"),
        vec![(2, b"second".to_vec())]
    );
    assert_eq!(
        fixture
            .live_results_writer
            .read_snapshot(&fixture.query_id)
            .await
            .expect("read outside-session live results"),
        vec![(42, b"live".to_vec())]
    );
    assert_eq!(
        fixture
            .checkpoint_store
            .read_result_sequence(&fixture.query_id)
            .await
            .expect("read outside-session result sequence"),
        Some(2)
    );
}

#[tokio::test]
async fn source_result_hook_commits_or_rolls_back_core_and_staged_writes_together() {
    let temp_dir = tempfile::TempDir::new().expect("create temp directory");
    let functions = Arc::new(FunctionRegistry::new());
    functions.register_aggregation_functions();
    let fixture = build_query(
        temp_dir.path(),
        "source-result-hook",
        "MATCH (n:Person) RETURN count(n) AS total",
        functions,
    )
    .await;

    let mismatched_fixture = build_query(
        temp_dir.path(),
        "mismatched-source-domain",
        "MATCH (n:Person) RETURN n.name AS name",
        Arc::new(FunctionRegistry::new()),
    )
    .await;
    let mismatched_hook_called = Arc::new(AtomicBool::new(false));
    let hook_called = mismatched_hook_called.clone();
    let rejected = fixture
        .query
        .process_source_change_with_result_hook(
            person_change("alice", "Alice", 1_000),
            &mismatched_fixture.transaction,
            move |_| async move {
                hook_called.store(true, Ordering::Relaxed);
                Ok(())
            },
        )
        .await;
    assert!(matches!(
        rejected,
        Err(EvaluationError::IndexError(
            IndexError::TransactionDomainMismatch
        ))
    ));
    assert!(!mismatched_hook_called.load(Ordering::Relaxed));

    fixture
        .session_control
        .begin()
        .await
        .expect("begin mismatch verification session");
    let alice = fixture
        .element_index
        .get_element(&ElementReference::new("people", "alice"))
        .await
        .expect("read rejected element");
    fixture
        .session_control
        .rollback()
        .expect("end mismatch verification session");
    assert!(alice.is_none(), "domain rejection must precede core writes");

    fixture
        .outbox_writer
        .append(&fixture.query_id, 1, b"legacy-outbox")
        .await
        .expect("outside-session append should commit directly");

    let checkpoint_store = fixture.checkpoint_store.clone();
    let outbox_writer = fixture.outbox_writer.clone();
    let live_results_writer = fixture.live_results_writer.clone();
    let query_id = fixture.query_id.clone();
    let committed = fixture
        .query
        .process_source_change_with_result_hook(
            person_change("alice", "Alice", 1_000),
            &fixture.transaction,
            move |results| async move {
                assert_count_transition(&results, 0, 1);
                stage_all_persistent_output(
                    checkpoint_store.as_ref(),
                    outbox_writer.as_ref(),
                    live_results_writer.as_ref(),
                    &query_id,
                    2,
                    results[0].row_signature(),
                    1,
                )
                .await
            },
        )
        .await
        .expect("successful hook should commit");
    assert_count_transition(&committed, 0, 1);
    assert_eq!(
        fixture
            .checkpoint_store
            .read_checkpoint("people")
            .await
            .expect("read checkpoint")
            .expect("checkpoint should commit")
            .sequence,
        2
    );
    assert_eq!(
        fixture
            .outbox_writer
            .read_from(&fixture.query_id, 0)
            .await
            .expect("read committed outbox"),
        vec![(2, b"result-2".to_vec())],
        "transactional trim should retain only the latest entry"
    );
    assert_eq!(
        fixture
            .live_results_writer
            .read_snapshot(&fixture.query_id)
            .await
            .expect("read committed live results"),
        vec![(committed[0].row_signature(), b"row-2".to_vec())]
    );
    assert_eq!(
        fixture
            .checkpoint_store
            .read_result_sequence(&fixture.query_id)
            .await
            .expect("read committed result sequence"),
        Some(2)
    );

    let checkpoint_store = fixture.checkpoint_store.clone();
    let outbox_writer = fixture.outbox_writer.clone();
    let live_results_writer = fixture.live_results_writer.clone();
    let query_id = fixture.query_id.clone();
    let failed = fixture
        .query
        .process_source_change_with_result_hook(
            person_change("bob", "Bob", 2_000),
            &fixture.transaction,
            move |results| async move {
                assert_count_transition(&results, 1, 2);
                stage_all_persistent_output(
                    checkpoint_store.as_ref(),
                    outbox_writer.as_ref(),
                    live_results_writer.as_ref(),
                    &query_id,
                    3,
                    results[0].row_signature(),
                    1,
                )
                .await?;
                Err(IndexError::CorruptedData)
            },
        )
        .await;
    assert!(matches!(
        failed,
        Err(EvaluationError::IndexError(IndexError::CorruptedData))
    ));
    assert_eq!(
        fixture
            .checkpoint_store
            .read_checkpoint("people")
            .await
            .expect("read checkpoint")
            .expect("previous checkpoint should remain")
            .sequence,
        2,
        "the staged checkpoint must roll back"
    );
    assert_eq!(
        fixture
            .outbox_writer
            .read_from(&fixture.query_id, 0)
            .await
            .expect("read rolled-back outbox"),
        vec![(2, b"result-2".to_vec())]
    );
    assert_eq!(
        fixture
            .live_results_writer
            .read_snapshot(&fixture.query_id)
            .await
            .expect("read rolled-back live results"),
        vec![(committed[0].row_signature(), b"row-2".to_vec())]
    );
    assert_eq!(
        fixture
            .checkpoint_store
            .read_result_sequence(&fixture.query_id)
            .await
            .expect("read rolled-back result sequence"),
        Some(2)
    );

    fixture
        .session_control
        .begin()
        .await
        .expect("begin verification session");
    let bob = fixture
        .element_index
        .get_element(&ElementReference::new("people", "bob"))
        .await
        .expect("read rolled-back element");
    fixture
        .session_control
        .rollback()
        .expect("end verification session");
    assert!(bob.is_none(), "the core element write must roll back");

    let after_rollback = fixture
        .query
        .process_source_change(person_change("carol", "Carol", 3_000))
        .await
        .expect("processing should continue after rollback");
    assert_count_transition(&after_rollback, 1, 2);
}

#[tokio::test]
async fn cancelled_hook_rolls_back_all_persistent_output_and_core_state() {
    let temp_dir = tempfile::TempDir::new().expect("create temp directory");
    let fixture = build_query(
        temp_dir.path(),
        "cancelled-result-hook",
        "MATCH (n:Person) RETURN n.name AS name",
        Arc::new(FunctionRegistry::new()),
    )
    .await;
    let query = fixture.query.clone();
    let transaction = fixture.transaction.clone();
    let checkpoint_store = fixture.checkpoint_store.clone();
    let outbox_writer = fixture.outbox_writer.clone();
    let live_results_writer = fixture.live_results_writer.clone();
    let query_id = fixture.query_id.clone();
    let (staged_tx, staged_rx) = tokio::sync::oneshot::channel();

    let task = tokio::spawn(async move {
        query
            .process_source_change_with_result_hook(
                person_change("cancelled", "Cancelled", 1_000),
                &transaction,
                move |results| async move {
                    stage_all_persistent_output(
                        checkpoint_store.as_ref(),
                        outbox_writer.as_ref(),
                        live_results_writer.as_ref(),
                        &query_id,
                        1,
                        results[0].row_signature(),
                        16,
                    )
                    .await?;
                    let _ = staged_tx.send(());
                    future::pending::<Result<(), IndexError>>().await
                },
            )
            .await
    });

    staged_rx.await.expect("hook should stage every resource");
    task.abort();
    assert!(task
        .await
        .expect_err("task should be cancelled")
        .is_cancelled());

    assert_no_persistent_output(&fixture).await;
    fixture
        .session_control
        .begin()
        .await
        .expect("begin cancellation verification session");
    let element = fixture
        .element_index
        .get_element(&ElementReference::new("people", "cancelled"))
        .await
        .expect("read cancelled element");
    fixture
        .session_control
        .rollback()
        .expect("end cancellation verification session");
    assert!(element.is_none());
}

#[tokio::test]
async fn commit_failure_rolls_back_all_persistent_output_and_core_state() {
    let temp_dir = tempfile::TempDir::new().expect("create temp directory");
    let fixture = build_query_with_failing_commit(
        temp_dir.path(),
        "failed-commit-result-hook",
        "MATCH (n:Person) RETURN n.name AS name",
        Arc::new(FunctionRegistry::new()),
    )
    .await;
    let checkpoint_store = fixture.checkpoint_store.clone();
    let outbox_writer = fixture.outbox_writer.clone();
    let live_results_writer = fixture.live_results_writer.clone();
    let query_id = fixture.query_id.clone();

    let result = fixture
        .query
        .process_source_change_with_result_hook(
            person_change("failed-commit", "Failed", 1_000),
            &fixture.transaction,
            move |results| async move {
                stage_all_persistent_output(
                    checkpoint_store.as_ref(),
                    outbox_writer.as_ref(),
                    live_results_writer.as_ref(),
                    &query_id,
                    1,
                    results[0].row_signature(),
                    16,
                )
                .await
            },
        )
        .await;
    assert!(matches!(
        result,
        Err(EvaluationError::IndexError(IndexError::CorruptedData))
    ));

    assert_no_persistent_output(&fixture).await;
    fixture
        .session_control
        .begin()
        .await
        .expect("begin commit-failure verification session");
    let element = fixture
        .element_index
        .get_element(&ElementReference::new("people", "failed-commit"))
        .await
        .expect("read failed-commit element");
    fixture
        .session_control
        .rollback()
        .expect("end commit-failure verification session");
    assert!(element.is_none());
}

#[tokio::test]
async fn due_future_result_hook_observes_replays_and_skips_empty_queue() {
    let temp_dir = tempfile::TempDir::new().expect("create temp directory");
    let fixture = build_query(
        temp_dir.path(),
        "future-result-hook",
        "MATCH (n:Person) WHERE drasi.trueLater(true, 2000) RETURN n.name AS name",
        Arc::new(FunctionRegistry::new()),
    )
    .await;

    let initial = fixture
        .query
        .process_source_change(person_change("alice", "Alice", 1_000))
        .await
        .expect("initial evaluation should succeed");
    assert!(initial.is_empty());
    assert_eq!(
        fixture
            .future_queue
            .peek_due_time()
            .await
            .expect("peek future"),
        Some(2_000)
    );

    let mismatched_fixture = build_query(
        temp_dir.path(),
        "mismatched-future-domain",
        "MATCH (n:Person) RETURN n.name AS name",
        Arc::new(FunctionRegistry::new()),
    )
    .await;
    let mismatched_hook_called = Arc::new(AtomicBool::new(false));
    let hook_called = mismatched_hook_called.clone();
    let mismatched = fixture
        .query
        .process_due_futures_with_result_hook(
            &mismatched_fixture.transaction,
            move |_| async move {
                hook_called.store(true, Ordering::Relaxed);
                Ok(())
            },
        )
        .await;
    assert!(matches!(
        mismatched,
        Err(EvaluationError::IndexError(
            IndexError::TransactionDomainMismatch
        ))
    ));
    assert!(!mismatched_hook_called.load(Ordering::Relaxed));
    assert_eq!(
        fixture
            .future_queue
            .peek_due_time()
            .await
            .expect("peek future after domain rejection"),
        Some(2_000)
    );

    let failed_signature = Arc::new(AtomicU64::new(0));
    let hook_signature = failed_signature.clone();
    let checkpoint_store = fixture.checkpoint_store.clone();
    let outbox_writer = fixture.outbox_writer.clone();
    let live_results_writer = fixture.live_results_writer.clone();
    let query_id = fixture.query_id.clone();
    let failed = fixture
        .query
        .process_due_futures_with_result_hook(&fixture.transaction, move |due_result| async move {
            assert_eq!(due_result.source_id.as_ref(), "people");
            let row_signature = assert_added_name(&due_result.results, "Alice");
            hook_signature.store(row_signature, Ordering::Relaxed);
            stage_all_persistent_output(
                checkpoint_store.as_ref(),
                outbox_writer.as_ref(),
                live_results_writer.as_ref(),
                &query_id,
                1,
                row_signature,
                16,
            )
            .await?;
            Err(IndexError::CorruptedData)
        })
        .await;
    assert!(matches!(
        failed,
        Err(EvaluationError::IndexError(IndexError::CorruptedData))
    ));
    assert!(
        fixture
            .checkpoint_store
            .read_checkpoint("people")
            .await
            .expect("read checkpoint")
            .is_none(),
        "the staged future output write must roll back"
    );
    assert!(fixture
        .outbox_writer
        .read_from(&fixture.query_id, 0)
        .await
        .expect("read rolled-back future outbox")
        .is_empty());
    assert!(fixture
        .live_results_writer
        .read_snapshot(&fixture.query_id)
        .await
        .expect("read rolled-back future live results")
        .is_empty());
    assert_eq!(
        fixture
            .checkpoint_store
            .read_result_sequence(&fixture.query_id)
            .await
            .expect("read rolled-back future result sequence"),
        None
    );
    assert_eq!(
        fixture
            .future_queue
            .peek_due_time()
            .await
            .expect("peek replayable future"),
        Some(2_000),
        "a hook failure must retain the popped future"
    );

    let observed_pointer = Arc::new(AtomicUsize::new(0));
    let hook_pointer = observed_pointer.clone();
    let checkpoint_store = fixture.checkpoint_store.clone();
    let outbox_writer = fixture.outbox_writer.clone();
    let live_results_writer = fixture.live_results_writer.clone();
    let query_id = fixture.query_id.clone();
    let replayed = fixture
        .query
        .process_due_futures_with_result_hook(&fixture.transaction, move |due_result| async move {
            hook_pointer.store(due_result.results.as_ptr() as usize, Ordering::Relaxed);
            let row_signature = assert_added_name(&due_result.results, "Alice");
            assert_eq!(row_signature, failed_signature.load(Ordering::Relaxed));
            stage_all_persistent_output(
                checkpoint_store.as_ref(),
                outbox_writer.as_ref(),
                live_results_writer.as_ref(),
                &query_id,
                2,
                row_signature,
                16,
            )
            .await
        })
        .await
        .expect("replayed future should commit")
        .expect("the future should be replayed");
    assert_eq!(
        observed_pointer.load(Ordering::Relaxed),
        replayed.results.as_ptr() as usize,
        "the hook and caller must share the due-future result allocation"
    );
    assert_eq!(
        fixture
            .checkpoint_store
            .read_checkpoint("people")
            .await
            .expect("read checkpoint")
            .expect("future output checkpoint should commit")
            .sequence,
        2
    );
    assert_eq!(
        fixture
            .outbox_writer
            .read_from(&fixture.query_id, 0)
            .await
            .expect("read committed future outbox"),
        vec![(2, b"result-2".to_vec())]
    );
    assert_eq!(
        fixture
            .live_results_writer
            .read_snapshot(&fixture.query_id)
            .await
            .expect("read committed future live results"),
        vec![(replayed.results[0].row_signature(), b"row-2".to_vec())]
    );
    assert_eq!(
        fixture
            .checkpoint_store
            .read_result_sequence(&fixture.query_id)
            .await
            .expect("read committed future result sequence"),
        Some(2)
    );
    assert_eq!(
        fixture
            .future_queue
            .peek_due_time()
            .await
            .expect("peek drained queue"),
        None
    );

    let no_due_hook_called = Arc::new(AtomicBool::new(false));
    let hook_called = no_due_hook_called.clone();
    let no_due = fixture
        .query
        .process_due_futures_with_result_hook(&fixture.transaction, move |_| async move {
            hook_called.store(true, Ordering::Relaxed);
            Ok(())
        })
        .await
        .expect("empty queue should succeed");
    assert!(no_due.is_none());
    assert!(
        !no_due_hook_called.load(Ordering::Relaxed),
        "the hook must not run when no future is due"
    );
}

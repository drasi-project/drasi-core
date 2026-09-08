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
    path::Path,
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        Arc,
    },
};

use drasi_core::{
    evaluation::{
        context::{QueryPartEvaluationContext, QueryVariables},
        functions::{aggregation::RegisterAggregationFunctions, FunctionRegistry},
        variable_value::VariableValue,
        EvaluationError,
    },
    interface::{
        CheckpointStore, ElementIndex, FutureQueue, IndexBackendPlugin, IndexError, SessionControl,
    },
    models::{Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange},
    query::{ContinuousQuery, QueryBuilder},
};
use drasi_index_rocksdb::RocksDbIndexProvider;
use drasi_query_cypher::CypherParser;
use serde_json::json;

struct QueryFixture {
    query: ContinuousQuery,
    checkpoint_store: Arc<dyn CheckpointStore>,
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
    let provider = RocksDbIndexProvider::new(path, true, false);
    let created = provider
        .create_indexes(query_id)
        .await
        .expect("create RocksDB indexes");
    let checkpoint_store = created
        .checkpoint_store
        .expect("RocksDB provides a checkpoint store");
    let indexes = created.set;
    let parser = Arc::new(CypherParser::new(functions.clone()));
    let continuous_query = QueryBuilder::new(query, parser)
        .with_function_registry(functions)
        .with_element_index(indexes.element_index.clone())
        .with_archive_index(indexes.archive_index)
        .with_result_index(indexes.result_index)
        .with_future_queue(indexes.future_queue.clone())
        .with_session_control(indexes.session_control.clone())
        .build()
        .await;

    QueryFixture {
        query: continuous_query,
        checkpoint_store,
        element_index: indexes.element_index,
        future_queue: indexes.future_queue,
        session_control: indexes.session_control,
    }
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
    assert!(fixture.query.supports_atomic_result_hooks());

    let checkpoint_store = fixture.checkpoint_store.clone();
    let committed = fixture
        .query
        .process_source_change_with_result_hook(
            person_change("alice", "Alice", 1_000),
            move |results| async move {
                checkpoint_store.stage_checkpoint("people", 1, None).await?;
                assert_count_transition(&results, 0, 1);
                Ok(())
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
        1
    );

    let checkpoint_store = fixture.checkpoint_store.clone();
    let failed = fixture
        .query
        .process_source_change_with_result_hook(
            person_change("bob", "Bob", 2_000),
            move |results| async move {
                checkpoint_store.stage_checkpoint("people", 2, None).await?;
                assert_count_transition(&results, 1, 2);
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
        1,
        "the staged checkpoint must roll back"
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
async fn due_future_result_hook_observes_replays_and_skips_empty_queue() {
    let temp_dir = tempfile::TempDir::new().expect("create temp directory");
    let fixture = build_query(
        temp_dir.path(),
        "future-result-hook",
        "MATCH (n:Person) WHERE drasi.trueLater(true, 2000) RETURN n.name AS name",
        Arc::new(FunctionRegistry::new()),
    )
    .await;
    assert!(fixture.query.supports_atomic_result_hooks());

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

    let failed_signature = Arc::new(AtomicU64::new(0));
    let hook_signature = failed_signature.clone();
    let checkpoint_store = fixture.checkpoint_store.clone();
    let failed = fixture
        .query
        .process_due_futures_with_result_hook(move |due_result| async move {
            checkpoint_store
                .stage_checkpoint("future-output", 1, None)
                .await?;
            assert_eq!(due_result.source_id.as_ref(), "people");
            hook_signature.store(
                assert_added_name(&due_result.results, "Alice"),
                Ordering::Relaxed,
            );
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
            .read_checkpoint("future-output")
            .await
            .expect("read checkpoint")
            .is_none(),
        "the staged future output write must roll back"
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
    let replayed = fixture
        .query
        .process_due_futures_with_result_hook(move |due_result| async move {
            checkpoint_store
                .stage_checkpoint("future-output", 2, None)
                .await?;
            hook_pointer.store(due_result.results.as_ptr() as usize, Ordering::Relaxed);
            assert_eq!(
                assert_added_name(&due_result.results, "Alice"),
                failed_signature.load(Ordering::Relaxed)
            );
            Ok(())
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
            .read_checkpoint("future-output")
            .await
            .expect("read checkpoint")
            .expect("future output checkpoint should commit")
            .sequence,
        2
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
        .process_due_futures_with_result_hook(move |_| async move {
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

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

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use drasi_query_cypher::CypherParser;
use serde_json::json;

use crate::{
    evaluation::{context::QueryPartEvaluationContext, functions::FunctionRegistry},
    in_memory_index::{
        in_memory_element_index::InMemoryElementIndex, in_memory_future_queue::InMemoryFutureQueue,
        in_memory_result_index::InMemoryResultIndex,
    },
    interface::{IndexError, IndexSet, NoOpSessionControl, SessionControl},
    models::{Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange},
    query::QueryBuilder,
};

use super::*;

fn memory_indexes(session: Arc<dyn SessionControl>) -> IndexSet {
    let elements = Arc::new(InMemoryElementIndex::new());
    IndexSet {
        element_index: elements.clone(),
        archive_index: elements,
        result_index: Arc::new(InMemoryResultIndex::new()),
        future_queue: Arc::new(InMemoryFutureQueue::new()),
        session_control: session,
    }
}

fn builder() -> QueryBuilder {
    let functions = Arc::new(FunctionRegistry::new());
    QueryBuilder::new(
        "MATCH (n:Person) RETURN n.name AS name",
        Arc::new(CypherParser::new(functions.clone())),
    )
    .with_function_registry(functions)
}

fn change() -> SourceChange {
    SourceChange::Insert {
        element: Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new("people", "alice"),
                labels: Arc::from([Arc::from("Person")]),
                effective_from: 100,
            },
            properties: ElementPropertyMap::from(json!({ "name": "Alice" })),
        },
    }
}

#[derive(Default)]
struct RecordingSession(Mutex<Vec<&'static str>>);

#[async_trait]
impl SessionControl for RecordingSession {
    async fn begin(&self) -> std::result::Result<(), IndexError> {
        self.0.lock().unwrap().push("begin");
        Ok(())
    }

    async fn commit(&self) -> std::result::Result<(), IndexError> {
        self.0.lock().unwrap().push("commit");
        Ok(())
    }

    fn rollback(&self) -> std::result::Result<(), IndexError> {
        self.0.lock().unwrap().push("rollback");
        Ok(())
    }
}

#[tokio::test]
async fn result_hook_reuses_typed_results_before_commit_without_claiming_memory_atomicity() {
    let session = Arc::new(RecordingSession::default());
    let resources =
        ComputationIndexes::try_new(memory_indexes(session.clone()), None, None, None, None)
            .unwrap();
    assert!(matches!(
        resources.atomic_result_transaction(),
        Err(ComputationQueryError::AtomicOutputUnsupported)
    ));
    let query = ComputationQuery::try_build(builder(), resources)
        .await
        .unwrap();
    let mut observed = None;
    let results = query
        .process_source_change_with_non_atomic_result_hook(change(), |results| async {
            assert_eq!(*session.0.lock().unwrap(), ["begin"]);
            assert!(matches!(
                results.as_ref(),
                [QueryPartEvaluationContext::Adding { .. }]
            ));
            observed = Some(results);
            Ok(())
        })
        .await
        .unwrap();
    assert!(Arc::ptr_eq(&observed.unwrap(), &results));
    assert_eq!(*session.0.lock().unwrap(), ["begin", "commit"]);
}

#[tokio::test]
async fn non_atomic_hook_failure_is_visible_and_rolls_back_the_session() {
    let session = Arc::new(RecordingSession::default());
    let resources =
        ComputationIndexes::try_new(memory_indexes(session.clone()), None, None, None, None)
            .unwrap();
    let query = ComputationQuery::try_build(builder(), resources)
        .await
        .unwrap();
    let result = query
        .process_source_change_with_non_atomic_result_hook(change(), |_| async {
            Err(IndexError::CorruptedData)
        })
        .await;
    assert!(matches!(
        result,
        Err(ComputationQueryError::Index(IndexError::CorruptedData))
    ));
    // Memory writes are not rolled back; the graph must not infer atomicity.
    assert_eq!(*session.0.lock().unwrap(), ["begin", "rollback"]);
}

#[tokio::test]
async fn empty_future_queue_does_not_call_result_hook() {
    let resources = ComputationIndexes::try_new(
        memory_indexes(Arc::new(NoOpSessionControl)),
        None,
        None,
        None,
        None,
    )
    .unwrap();
    let query = ComputationQuery::try_build(builder(), resources)
        .await
        .unwrap();
    let result = query
        .process_due_futures_with_non_atomic_result_hook(|_| async {
            panic!("an empty queue must not invoke the hook")
        })
        .await
        .unwrap();
    assert!(result.is_none());
}

#[test]
fn domain_cannot_be_attached_to_an_unrelated_actual_session() {
    let claimed = Arc::new(RecordingSession::default());
    let actual = Arc::new(RecordingSession::default());
    let domain = TransactionDomain::new(claimed);
    assert!(matches!(
        ComputationIndexes::try_new(memory_indexes(actual), Some(domain), None, None, None),
        Err(ComputationQueryError::TransactionMismatch)
    ));
}

#[tokio::test]
async fn dropping_an_evaluation_fences_the_constructed_query_until_replacement() {
    let session = Arc::new(RecordingSession::default());
    let resources =
        ComputationIndexes::try_new(memory_indexes(session.clone()), None, None, None, None)
            .unwrap();
    let query = ComputationQuery::try_build(builder(), resources)
        .await
        .unwrap();
    let entered = tokio::sync::Notify::new();
    let mut operation = Box::pin(query.process_source_change_with_non_atomic_result_hook(
        change(),
        |_| async {
            entered.notify_one();
            std::future::pending().await
        },
    ));
    tokio::select! {
        result = &mut operation => panic!("hook should remain pending: {result:?}"),
        _ = entered.notified() => {}
    }
    assert!(matches!(
        query.shutdown().await,
        Err(ComputationQueryError::OperationInProgress)
    ));
    drop(operation);
    assert!(query.recovery_required());
    assert_eq!(*session.0.lock().unwrap(), ["begin", "rollback"]);
    assert!(matches!(
        query.process_source_change(change()).await,
        Err(ComputationQueryError::RecoveryRequired)
    ));
    assert_eq!(*session.0.lock().unwrap(), ["begin", "rollback"]);
}

#[tokio::test]
async fn rollback_failure_retains_both_errors_and_fences_the_query() {
    struct FailingRollback;

    #[async_trait]
    impl SessionControl for FailingRollback {
        async fn begin(&self) -> std::result::Result<(), IndexError> {
            Ok(())
        }

        async fn commit(&self) -> std::result::Result<(), IndexError> {
            panic!("failed evaluation cannot commit")
        }

        fn rollback(&self) -> std::result::Result<(), IndexError> {
            Err(IndexError::IOError)
        }
    }

    let resources = ComputationIndexes::try_new(
        memory_indexes(Arc::new(FailingRollback)),
        None,
        None,
        None,
        None,
    )
    .unwrap();
    let query = ComputationQuery::try_build(builder(), resources)
        .await
        .unwrap();
    let error = query
        .process_source_change_with_non_atomic_result_hook(change(), |_| async {
            Err(IndexError::CorruptedData)
        })
        .await
        .expect_err("both failures must be returned");
    let ComputationQueryError::Rollback { failure, rollback } = error else {
        panic!("expected evaluation and rollback failures");
    };
    assert!(matches!(
        *failure,
        ComputationQueryError::Index(IndexError::CorruptedData)
    ));
    assert_eq!(rollback, IndexError::IOError);
    assert!(query.recovery_required());
}

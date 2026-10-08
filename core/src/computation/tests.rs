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
async fn fallback_checkpoint_is_independent_and_never_replaces_a_provider_store() {
    use crate::{
        in_memory_index::in_memory_checkpoint_store::InMemoryCheckpointStore,
        interface::CheckpointStore,
    };
    let fallback: Arc<dyn CheckpointStore> = Arc::new(InMemoryCheckpointStore::new());
    let declared = ComputationIndexes::try_new(
        memory_indexes(Arc::new(NoOpSessionControl)),
        None,
        None,
        None,
        None,
    )
    .expect("unannotated bundle");
    assert_eq!(declared.durability(), StorageDurability::UNKNOWN);
    let declared = declared.with_durability(StorageDurability::LOCAL_PROCESS_RESTART);
    assert_eq!(
        declared.durability(),
        StorageDurability::LOCAL_PROCESS_RESTART
    );
    assert_eq!(
        declared
            .with_fallback_checkpoint(fallback.clone())
            .durability()
            .process_restart,
        FailureSurvival::Unknown
    );
    let resources = InMemoryComputationProvider
        .create_indexes("graph", "query")
        .await
        .unwrap()
        .with_fallback_checkpoint(fallback.clone());
    assert_eq!(resources.durability(), StorageDurability::VOLATILE);
    assert!(Arc::ptr_eq(
        resources.checkpoint_store().unwrap(),
        &fallback
    ));
    assert!(matches!(
        resources.atomic_result_transaction(),
        Err(ComputationQueryError::AtomicOutputUnsupported)
    ));

    let configured: Arc<dyn CheckpointStore> = Arc::new(InMemoryCheckpointStore::new());
    configured
        .stage_checkpoint("source", 7, None)
        .await
        .unwrap();
    let resources = ComputationIndexes::try_new(
        memory_indexes(Arc::new(NoOpSessionControl)),
        None,
        Some(ComputationResource::independent(configured.clone())),
        None,
        None,
    )
    .unwrap()
    .with_fallback_checkpoint(fallback);
    assert!(Arc::ptr_eq(
        resources.checkpoint_store().unwrap(),
        &configured
    ));
    assert_eq!(
        resources
            .checkpoint_store()
            .unwrap()
            .read_checkpoint("source")
            .await
            .unwrap()
            .unwrap()
            .sequence,
        7
    );
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

fn transaction_resources(session: Arc<dyn SessionControl>) -> ComputationIndexes {
    use crate::in_memory_index::{
        in_memory_checkpoint_store::InMemoryCheckpointStore,
        in_memory_live_results_writer::InMemoryLiveResultsWriter,
        in_memory_outbox_writer::InMemoryOutboxWriter,
    };
    let domain = TransactionDomain::new(session.clone());
    // No data writes: these fixtures exercise operation/cleanup ownership only.
    ComputationIndexes::try_new(
        memory_indexes(session),
        Some(domain.clone()),
        Some(ComputationResource::participating(
            Arc::new(InMemoryCheckpointStore::new()),
            &domain,
        )),
        Some(ComputationResource::participating(
            Arc::new(InMemoryOutboxWriter::new()),
            &domain,
        )),
        Some(ComputationResource::participating(
            Arc::new(InMemoryLiveResultsWriter::new()),
            &domain,
        )),
    )
    .unwrap()
}

#[tokio::test(flavor = "current_thread")]
async fn transaction_group_retirement_blocks_writes_until_confirmed_resume() {
    let session = Arc::new(RecordingSession::default());
    let resources = transaction_resources(session.clone());
    let future_queue = resources.indexes().future_queue.clone();
    let group = ComputationTransactionGroup::try_new(resources).unwrap();
    let journal = group.journal_transaction("journal").unwrap();
    let frozen = group.freeze_for_retirement().unwrap();
    assert!(frozen.contains(&journal));
    assert_eq!(frozen.journal_count().unwrap(), 1);
    assert!(!frozen.has_processor().unwrap());
    assert!(!frozen.has_scheduled_work().await.unwrap());
    assert!(group.freeze_for_retirement().is_err());
    assert!(!group.recovery_required());
    let mut waiting = Box::pin(journal.run(async { Ok(()) }));
    assert!(futures::poll!(&mut waiting).is_pending());
    assert!(session.0.lock().unwrap().is_empty());
    frozen.resume();
    waiting.await.unwrap();
    assert_eq!(*session.0.lock().unwrap(), ["begin", "commit"]);
    future_queue
        .push(
            crate::interface::PushType::Always,
            0,
            1,
            &ElementReference::new("source", "timer"),
            100,
            200,
        )
        .await
        .unwrap();
    let frozen = group.freeze_for_retirement().unwrap();
    assert!(frozen.has_scheduled_work().await.unwrap());
    frozen.resume();
    future_queue.clear().await.unwrap();
    let frozen = group.freeze_for_retirement().unwrap();
    assert!(!frozen.has_scheduled_work().await.unwrap());
    frozen.retire();
    assert!(group.recovery_required());
    journal.shutdown().await.unwrap();
    group.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn query_retirement_uses_actual_operation_owner_and_checks_scheduled_work() {
    let session = Arc::new(RecordingSession::default());
    let resources = transaction_resources(session);
    let queue = resources.indexes().future_queue.clone();
    let query = ComputationQuery::try_build(builder(), resources)
        .await
        .unwrap();
    let owner = query.retirement_owner().upgrade().unwrap();
    let frozen = owner.freeze_for_retirement().unwrap();
    assert!(!frozen.has_scheduled_work().await.unwrap());
    let mut write = Box::pin(query.resource_transaction(|| async { Ok(()) }));
    assert!(futures::poll!(&mut write).is_pending());
    frozen.resume();
    write.await.unwrap();
    queue
        .push(
            crate::interface::PushType::Always,
            0,
            1,
            &ElementReference::new("source", "timer"),
            100,
            200,
        )
        .await
        .unwrap();
    let frozen = owner.freeze_for_retirement().unwrap();
    assert!(frozen.has_scheduled_work().await.unwrap());
    query.shutdown().await.unwrap();
    assert!(frozen.has_scheduled_work().await.is_err());
    frozen.resume();
    assert!(query.recovery_required());
    assert!(query
        .resource_transaction(|| async { Ok(()) })
        .await
        .is_err());
}

#[tokio::test(flavor = "current_thread")]
async fn transaction_group_unresolved_retirement_fences_waiters_and_retains_cleanup() {
    for retire in [false, true] {
        let session = Arc::new(RecordingSession::default());
        let group =
            ComputationTransactionGroup::try_new(transaction_resources(session.clone())).unwrap();
        let journal = group.journal_transaction("journal").unwrap();
        let frozen = group.freeze_for_retirement().unwrap();
        let mut waiting =
            Box::pin(journal.run::<()>(async { panic!("retired operation must not execute") }));
        assert!(futures::poll!(&mut waiting).is_pending());
        if retire {
            frozen.retire();
        } else {
            drop(frozen);
        }
        assert!(waiting.await.is_err());
        assert!(group.recovery_required());
        assert!(session.0.lock().unwrap().is_empty());
        assert!(group.freeze_for_retirement().is_err());
        journal.shutdown().await.unwrap();
        group.shutdown().await.unwrap();
    }
}

#[tokio::test(flavor = "current_thread")]
async fn transaction_group_retirement_rejects_inflight_work_without_fencing_it() {
    let session = Arc::new(RecordingSession::default());
    let group =
        ComputationTransactionGroup::try_new(transaction_resources(session.clone())).unwrap();
    let journal = group.journal_transaction("journal").unwrap();
    let (release, wait) = tokio::sync::oneshot::channel();
    let mut operation = Box::pin(journal.run(async {
        wait.await.unwrap();
        Ok(())
    }));
    assert!(futures::poll!(&mut operation).is_pending());
    assert!(group.freeze_for_retirement().is_err());
    assert!(!group.recovery_required());
    release.send(()).unwrap();
    operation.await.unwrap();
    group.freeze_for_retirement().unwrap().resume();
    journal.shutdown().await.unwrap();
    group.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn transaction_group_shutdown_revokes_retirement_without_reopening_members() {
    let session = Arc::new(RecordingSession::default());
    let group = ComputationTransactionGroup::try_new(transaction_resources(session)).unwrap();
    let journal = group.journal_transaction("journal").unwrap();
    let frozen = group.freeze_for_retirement().unwrap();
    group.shutdown().await.unwrap();
    assert!(frozen.has_scheduled_work().await.is_err());
    frozen.resume();
    assert!(journal
        .run::<()>(async { panic!("shutdown must remain terminal") })
        .await
        .is_err());
    journal.shutdown().await.unwrap();
    group.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn resource_transaction_cancellation_joins_provider_io_before_rollback() {
    let session = Arc::new(RecordingSession::default());
    let work = Arc::new(ComputationIoScope::default());
    let transaction = ComputationTransaction::try_new(
        transaction_resources(session.clone()).with_cleanup(work.clone()),
    )
    .unwrap();
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let mut operation = Box::pin(transaction.run(async {
        work.run(move || {
            entered_tx.send(()).unwrap();
            release_rx
                .recv_timeout(std::time::Duration::from_secs(5))
                .map_err(IndexError::other)?;
            Ok(())
        })
        .await?;
        Ok(())
    }));
    tokio::select! {
        result = &mut operation => panic!("I/O should still be pending: {result:?}"),
        result = entered_rx => result.unwrap(),
    }
    assert!(matches!(
        transaction.shutdown().await,
        Err(ComputationQueryError::OperationInProgress)
    ));
    drop(operation);
    assert!(transaction.recovery_required());
    assert_eq!(*session.0.lock().unwrap(), ["begin"]);
    let mut shutdown = Box::pin(transaction.shutdown());
    assert!(futures::poll!(&mut shutdown).is_pending());
    drop(shutdown);
    assert_eq!(*session.0.lock().unwrap(), ["begin"]);
    release_tx.send(()).unwrap();
    transaction.shutdown().await.unwrap();
    assert_eq!(*session.0.lock().unwrap(), ["begin", "rollback"]);
    assert!(matches!(
        transaction.run(async { Ok(()) }).await,
        Err(ComputationQueryError::RecoveryRequired)
    ));
}

#[tokio::test(flavor = "current_thread")]
async fn resource_transaction_retirement_holds_operations_until_resolution() {
    for resolution in ["resume", "retire", "abandon"] {
        let session = Arc::new(RecordingSession::default());
        let transaction = Arc::new(
            ComputationTransaction::try_new(transaction_resources(session.clone())).unwrap(),
        );
        let frozen = transaction.freeze_for_retirement().unwrap();
        let mut waiting = Box::pin(transaction.run(async { Ok(42) }));
        assert!(futures::poll!(&mut waiting).is_pending());
        assert!(session.0.lock().unwrap().is_empty());
        assert!(transaction.freeze_for_retirement().is_err());
        match resolution {
            "resume" => frozen.resume(),
            "retire" => frozen.retire(),
            _ => drop(frozen),
        }
        if resolution == "resume" {
            assert_eq!(waiting.await.unwrap(), 42);
            assert_eq!(*session.0.lock().unwrap(), ["begin", "commit"]);
        } else {
            assert!(matches!(
                waiting.await,
                Err(ComputationQueryError::RecoveryRequired)
            ));
            assert!(session.0.lock().unwrap().is_empty());
            assert!(transaction.freeze_for_retirement().is_err());
        }
        transaction.shutdown().await.unwrap();
        assert!(transaction.freeze_for_retirement().is_err());
    }
}

#[tokio::test(flavor = "current_thread")]
async fn resource_transaction_retirement_rejects_active_and_shared_owners() {
    let session = Arc::new(RecordingSession::default());
    let transaction =
        Arc::new(ComputationTransaction::try_new(transaction_resources(session.clone())).unwrap());
    let (release, wait) = tokio::sync::oneshot::channel();
    let mut running = Box::pin(transaction.run(async {
        wait.await.unwrap();
        Ok(())
    }));
    assert!(futures::poll!(&mut running).is_pending());
    assert!(matches!(
        transaction.freeze_for_retirement(),
        Err(ComputationQueryError::OperationInProgress)
    ));
    assert!(!transaction.recovery_required());
    release.send(()).unwrap();
    running.await.unwrap();
    transaction.freeze_for_retirement().unwrap().resume();
    transaction.shutdown().await.unwrap();

    let group = ComputationTransactionGroup::try_new(transaction_resources(session)).unwrap();
    let member = Arc::new(group.journal_transaction("journal").unwrap());
    assert!(matches!(
        member.freeze_for_retirement(),
        Err(ComputationQueryError::TransactionMismatch)
    ));
    assert!(!group.recovery_required());
    member.shutdown().await.unwrap();
    group.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn resource_transaction_shutdown_revokes_retirement_without_reopening_owner() {
    let session = Arc::new(RecordingSession::default());
    let transaction =
        Arc::new(ComputationTransaction::try_new(transaction_resources(session)).unwrap());
    let frozen = transaction.freeze_for_retirement().unwrap();
    transaction.shutdown().await.unwrap();
    frozen.resume();
    assert!(matches!(
        transaction.run(async { Ok(()) }).await,
        Err(ComputationQueryError::RecoveryRequired)
    ));
    assert!(transaction.freeze_for_retirement().is_err());
    transaction.shutdown().await.unwrap();
}

#[tokio::test]
async fn resource_transaction_rolls_back_before_an_atomic_retry_without_an_evaluator() {
    let session = Arc::new(RecordingSession::default());
    let transaction =
        ComputationTransaction::try_new(transaction_resources(session.clone())).unwrap();
    let result: Result<()> = transaction
        .run(async { Err(IndexError::IOError.into()) })
        .await;
    assert!(matches!(
        result,
        Err(ComputationQueryError::Index(IndexError::IOError))
    ));
    assert!(!transaction.recovery_required());
    assert_eq!(transaction.run(async { Ok(42) }).await.unwrap(), 42);
    assert_eq!(
        *session.0.lock().unwrap(),
        ["begin", "rollback", "begin", "commit"]
    );
}

#[tokio::test]
async fn transaction_group_members_share_one_session_but_not_their_lifetimes() {
    let session = Arc::new(RecordingSession::default());
    let work = Arc::new(ComputationIoScope::default());
    let group = ComputationTransactionGroup::try_new(
        transaction_resources(session.clone()).with_cleanup(work.clone()),
    )
    .unwrap();
    let processor = ComputationTransaction::try_new(group.processor_indexes().unwrap()).unwrap();
    assert!(group.processor_indexes().is_err());
    let journal = group.journal_transaction("output").unwrap();
    assert!(group.journal_transaction("output").is_err());
    let entered = tokio::sync::Notify::new();
    let resume = tokio::sync::Notify::new();
    let mut processing = Box::pin(processor.run(async {
        entered.notify_one();
        resume.notified().await;
        Ok(())
    }));
    tokio::select! {
        result = &mut processing => panic!("processor should pause: {result:?}"),
        _ = entered.notified() => {},
    }
    let mut waiting = Box::pin(journal.run::<()>(async { panic!("waiting journal cannot enter") }));
    assert!(futures::poll!(&mut waiting).is_pending());
    drop(waiting);
    assert!(journal.recovery_required());
    assert!(!group.recovery_required());
    journal.shutdown().await.unwrap();
    assert_eq!(*session.0.lock().unwrap(), ["begin"]);
    resume.notify_one();
    processing.await.unwrap();
    drop(journal);
    let journal = group.journal_transaction("output").unwrap();
    processor.shutdown().await.unwrap();
    drop(processor);
    journal.run(async { Ok(()) }).await.unwrap();
    let replacement = ComputationTransaction::try_new(group.processor_indexes().unwrap()).unwrap();
    replacement.run(async { Ok(()) }).await.unwrap();
    replacement.shutdown().await.unwrap();
    journal.shutdown().await.unwrap();
    assert_eq!(work.run(|| Ok(9)).await.unwrap(), 9);
    assert_eq!(
        *session.0.lock().unwrap(),
        ["begin", "commit", "begin", "commit", "begin", "commit"]
    );
    group.shutdown().await.unwrap();
    assert!(work.run(|| Ok(9)).await.is_err());
}

#[tokio::test]
async fn transaction_group_interruption_fences_siblings_and_retains_cleanup_ownership() {
    let session = Arc::new(RecordingSession::default());
    let work = Arc::new(ComputationIoScope::default());
    let group = ComputationTransactionGroup::try_new(
        transaction_resources(session.clone()).with_cleanup(work.clone()),
    )
    .unwrap();
    let processor = ComputationTransaction::try_new(group.processor_indexes().unwrap()).unwrap();
    let journal = group.journal_transaction("output").unwrap();
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let mut operation = Box::pin(processor.run(async {
        work.run(move || {
            entered_tx.send(()).unwrap();
            release_rx
                .recv_timeout(std::time::Duration::from_secs(5))
                .map_err(IndexError::other)?;
            Ok(())
        })
        .await?;
        Ok(())
    }));
    tokio::select! {
        result = &mut operation => panic!("I/O should remain pending: {result:?}"),
        result = entered_rx => result.unwrap(),
    }
    drop(operation);
    assert!(group.recovery_required());
    assert!(journal.recovery_required());
    assert!(matches!(
        journal.run(async { Ok(()) }).await,
        Err(ComputationQueryError::RecoveryRequired)
    ));
    assert!(group.processor_indexes().is_err());
    assert!(group.shutdown().await.is_err());
    let mut shutdown = Box::pin(processor.shutdown());
    assert!(futures::poll!(&mut shutdown).is_pending());
    drop(shutdown);
    assert_eq!(*session.0.lock().unwrap(), ["begin"]);
    release_tx.send(()).unwrap();
    processor.shutdown().await.unwrap();
    assert_eq!(*session.0.lock().unwrap(), ["begin", "rollback"]);
    journal.shutdown().await.unwrap();
    group.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn transaction_group_failure_wakes_storage_and_member_gate_waiters() {
    let session = Arc::new(RecordingSession::default());
    let group =
        ComputationTransactionGroup::try_new(transaction_resources(session.clone())).unwrap();
    let processor = ComputationTransaction::try_new(group.processor_indexes().unwrap()).unwrap();
    let journal = group.journal_transaction("output").unwrap();
    let entered = tokio::sync::Notify::new();
    let resume = tokio::sync::Notify::new();
    let mut processing = Box::pin(processor.run(async {
        entered.notify_one();
        resume.notified().await;
        Ok(())
    }));
    tokio::select! {
        result = &mut processing => panic!("processor should retain the gate: {result:?}"),
        _ = entered.notified() => {},
    }
    let mut storage_waiter =
        Box::pin(journal.run::<()>(async { panic!("fenced journal must not enter") }));
    let mut member_waiter =
        Box::pin(processor.run::<()>(async { panic!("fenced processor must not reenter") }));
    let mut inspection =
        Box::pin(processor.inspect::<()>(async { panic!("fenced inspection must not enter") }));
    assert!(futures::poll!(&mut storage_waiter).is_pending());
    assert!(futures::poll!(&mut member_waiter).is_pending());
    assert!(futures::poll!(&mut inspection).is_pending());
    drop(group.journal_transaction("unrelated").unwrap());
    assert!(futures::poll!(&mut storage_waiter).is_pending());
    assert!(futures::poll!(&mut member_waiter).is_pending());
    assert!(futures::poll!(&mut inspection).is_pending());
    group.cancel();
    let waiting = tokio::time::timeout(std::time::Duration::from_secs(1), async {
        tokio::join!(storage_waiter, member_waiter, inspection)
    })
    .await;
    assert_eq!(*session.0.lock().unwrap(), ["begin"]);
    resume.notify_one();
    assert!(processing.await.is_err());
    assert_eq!(*session.0.lock().unwrap(), ["begin", "rollback"]);
    processor.shutdown().await.unwrap();
    journal.shutdown().await.unwrap();
    group.shutdown().await.unwrap();
    let (storage, member, inspection) =
        waiting.expect("group failure must wake every waiter before active processing finishes");
    assert!(storage.is_err());
    assert!(matches!(
        member,
        Err(ComputationQueryError::RecoveryRequired)
    ));
    assert!(matches!(
        inspection,
        Err(ComputationQueryError::RecoveryRequired)
    ));
}

#[tokio::test]
async fn transaction_group_dropped_owner_preserves_member_cleanup() {
    let session = Arc::new(RecordingSession::default());
    let work = Arc::new(ComputationIoScope::default());
    let group = ComputationTransactionGroup::try_new(
        transaction_resources(session.clone()).with_cleanup(work.clone()),
    )
    .unwrap();
    let processor = ComputationTransaction::try_new(group.processor_indexes().unwrap()).unwrap();
    let journal = group.journal_transaction("output").unwrap();
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let mut operation = Box::pin(processor.run(async {
        work.run(move || {
            entered_tx.send(()).unwrap();
            release_rx
                .recv_timeout(std::time::Duration::from_secs(5))
                .map_err(IndexError::other)?;
            Ok(())
        })
        .await?;
        Ok(())
    }));
    tokio::select! {
        result = &mut operation => panic!("I/O should remain pending: {result:?}"),
        result = entered_rx => result.unwrap(),
    }
    drop(group);
    assert!(processor.recovery_required());
    assert!(journal.recovery_required());
    drop(operation);
    let mut shutdown = Box::pin(processor.shutdown());
    assert!(futures::poll!(&mut shutdown).is_pending());
    drop(shutdown);
    assert_eq!(*session.0.lock().unwrap(), ["begin"]);
    release_tx.send(()).unwrap();
    processor.shutdown().await.unwrap();
    journal.shutdown().await.unwrap();
    assert_eq!(*session.0.lock().unwrap(), ["begin", "rollback"]);
    assert!(work.run(|| Ok(())).await.is_err());
}

#[tokio::test]
async fn transaction_group_atomic_rejection_does_not_fence_healthy_members() {
    let session = Arc::new(RecordingSession::default());
    let group =
        ComputationTransactionGroup::try_new(transaction_resources(session.clone())).unwrap();
    let processor = ComputationTransaction::try_new(group.processor_indexes().unwrap()).unwrap();
    let journal = group.journal_transaction("output").unwrap();
    let result: Result<()> = processor
        .run(async { Err(IndexError::IOError.into()) })
        .await;
    assert!(result.is_err());
    assert!(!group.recovery_required());
    assert!(!processor.recovery_required());
    journal.run(async { Ok(()) }).await.unwrap();
    processor.run(async { Ok(()) }).await.unwrap();
    assert_eq!(
        *session.0.lock().unwrap(),
        ["begin", "rollback", "begin", "commit", "begin", "commit"]
    );
    group.shutdown().await.unwrap();
}

#[tokio::test]
async fn transaction_group_registration_is_bounded_and_cannot_wrap_an_existing_group() {
    let resources = InMemoryComputationProvider
        .create_indexes("graph", "query")
        .await
        .unwrap();
    assert!(matches!(
        ComputationTransactionGroup::try_new(resources),
        Err(ComputationQueryError::AtomicOutputUnsupported)
    ));
    let group = ComputationTransactionGroup::try_new(transaction_resources(Arc::new(
        RecordingSession::default(),
    )))
    .unwrap();
    let processor = group.processor_indexes().unwrap();
    assert!(matches!(
        ComputationTransactionGroup::try_new(processor),
        Err(ComputationQueryError::TransactionMismatch)
    ));
    for invalid in ["", "a b", "a\nb", &"x".repeat(257)] {
        assert!(group.journal_transaction(invalid).is_err());
    }
    let maximum_name = group.journal_transaction(&"x".repeat(256)).unwrap();
    drop(maximum_name);
    let mut journals = Vec::new();
    for index in 0..256 {
        journals.push(
            group
                .journal_transaction(&format!("journal-{index}"))
                .unwrap(),
        );
    }
    assert!(group.journal_transaction("overflow").is_err());
    journals.pop();
    journals.push(group.journal_transaction("replacement").unwrap());
    drop(group);
    for journal in journals {
        assert!(journal.recovery_required());
        assert!(matches!(
            journal.run(async { Ok(()) }).await,
            Err(ComputationQueryError::RecoveryRequired)
        ));
        journal.shutdown().await.unwrap();
    }
}

#[derive(Clone, Copy)]
enum GroupMutationMode {
    Complete,
    Reject,
    PauseStaging,
    PauseCompletion,
    FailCompletion,
}

#[derive(Default)]
struct GroupMutationGate {
    entered: tokio::sync::Notify,
    resume: tokio::sync::Notify,
    dropped: std::sync::atomic::AtomicUsize,
}

struct GroupMutationProbe {
    target: std::sync::Weak<ComputationTransaction>,
    session: Arc<RecordingSession>,
    mode: GroupMutationMode,
    gate: Arc<GroupMutationGate>,
}

#[async_trait]
impl TransactionGroupMutation for GroupMutationProbe {
    async fn stage(
        &mut self,
        context: &TransactionGroupContext<'_>,
    ) -> std::result::Result<(), IndexError> {
        let target = self.target.upgrade().ok_or(IndexError::NotSupported)?;
        context.require_member(&target).map_err(IndexError::other)?;
        self.session.0.lock().unwrap().push("stage");
        if matches!(self.mode, GroupMutationMode::Reject) {
            return Err(IndexError::IOError);
        }
        if matches!(self.mode, GroupMutationMode::PauseStaging) {
            self.gate.entered.notify_one();
            self.gate.resume.notified().await;
        }
        Ok(())
    }

    async fn committed(&mut self) -> std::result::Result<(), IndexError> {
        if matches!(self.mode, GroupMutationMode::FailCompletion) {
            return Err(IndexError::IOError);
        }
        if matches!(self.mode, GroupMutationMode::PauseCompletion) {
            self.gate.entered.notify_one();
            self.gate.resume.notified().await;
        }
        self.session.0.lock().unwrap().push("visible");
        Ok(())
    }
}

impl Drop for GroupMutationProbe {
    fn drop(&mut self) {
        self.gate
            .dropped
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
}

#[tokio::test]
async fn transaction_group_staging_checks_actual_membership_and_exact_limits() {
    let session = Arc::new(RecordingSession::default());
    let group =
        ComputationTransactionGroup::try_new(transaction_resources(session.clone())).unwrap();
    let processor = ComputationTransaction::try_new(group.processor_indexes().unwrap()).unwrap();
    let journal = Arc::new(group.journal_transaction("output").unwrap());
    let foreign_group = ComputationTransactionGroup::try_new(transaction_resources(Arc::new(
        RecordingSession::default(),
    )))
    .unwrap();
    let foreign = Arc::new(foreign_group.journal_transaction("output").unwrap());
    let gate = Arc::new(GroupMutationGate::default());
    let mutation =
        |target: &Arc<ComputationTransaction>, mode| -> Box<dyn TransactionGroupMutation> {
            Box::new(GroupMutationProbe {
                target: Arc::downgrade(target),
                session: session.clone(),
                mode,
                gate: gate.clone(),
            })
        };
    assert!(processor.shares_transaction_group(&journal));
    assert!(!processor.shares_transaction_group(&foreign));
    let error = processor
        .run_with_mutations(async {
            Ok(((), vec![mutation(&foreign, GroupMutationMode::Complete)]))
        })
        .await
        .unwrap_err();
    let ComputationQueryError::Index(IndexError::Other(cause)) = error else {
        panic!("expected a retained membership error: {error}");
    };
    assert!(matches!(
        cause.downcast_ref::<ComputationQueryError>(),
        Some(ComputationQueryError::TransactionMismatch)
    ));
    assert_eq!(*session.0.lock().unwrap(), ["begin", "rollback"]);
    assert!(!group.recovery_required());
    processor
        .run_with_mutations(async {
            Ok((
                (),
                (0..256)
                    .map(|_| mutation(&journal, GroupMutationMode::Complete))
                    .collect(),
            ))
        })
        .await
        .unwrap();
    let log = session.0.lock().unwrap().clone();
    assert_eq!(log.iter().filter(|entry| **entry == "stage").count(), 256);
    assert_eq!(log.iter().filter(|entry| **entry == "visible").count(), 256);
    assert!(processor
        .run_with_mutations(async {
            Ok((
                (),
                (0..257)
                    .map(|_| mutation(&journal, GroupMutationMode::Complete))
                    .collect(),
            ))
        })
        .await
        .is_err());
    assert!(!group.recovery_required());
    processor.shutdown().await.unwrap();
    journal.shutdown().await.unwrap();
    foreign.shutdown().await.unwrap();
    group.shutdown().await.unwrap();
    foreign_group.shutdown().await.unwrap();
}

#[tokio::test]
async fn transaction_group_staging_and_completion_have_distinct_failure_boundaries() {
    use std::sync::atomic::Ordering;
    for mode in [
        GroupMutationMode::Reject,
        GroupMutationMode::PauseStaging,
        GroupMutationMode::PauseCompletion,
        GroupMutationMode::FailCompletion,
    ] {
        let session = Arc::new(RecordingSession::default());
        let group =
            ComputationTransactionGroup::try_new(transaction_resources(session.clone())).unwrap();
        let processor =
            ComputationTransaction::try_new(group.processor_indexes().unwrap()).unwrap();
        let journal = Arc::new(group.journal_transaction("output").unwrap());
        let gate = Arc::new(GroupMutationGate::default());
        let mutations: Vec<Box<dyn TransactionGroupMutation>> =
            vec![Box::new(GroupMutationProbe {
                target: Arc::downgrade(&journal),
                session: session.clone(),
                mode,
                gate: gate.clone(),
            })];
        let mut operation = Box::pin(processor.run_with_mutations(async { Ok(((), mutations)) }));
        if matches!(
            mode,
            GroupMutationMode::PauseStaging | GroupMutationMode::PauseCompletion
        ) {
            tokio::select! {
                result = &mut operation => panic!("mutation should pause: {result:?}"),
                _ = gate.entered.notified() => {},
            }
            let mut waiting =
                Box::pin(journal.run::<()>(async { panic!("uncommitted view cannot be entered") }));
            assert!(futures::poll!(&mut waiting).is_pending());
            drop(waiting);
            drop(operation);
            assert_eq!(gate.dropped.load(Ordering::SeqCst), 0);
            assert!(group.recovery_required());
            processor.shutdown().await.unwrap();
        } else {
            assert!(operation.await.is_err());
            assert_eq!(
                group.recovery_required(),
                matches!(mode, GroupMutationMode::FailCompletion)
            );
        }
        assert_eq!(gate.dropped.load(Ordering::SeqCst), 1);
        assert_eq!(
            *session.0.lock().unwrap(),
            if matches!(
                mode,
                GroupMutationMode::PauseCompletion | GroupMutationMode::FailCompletion
            ) {
                vec!["begin", "stage", "commit"]
            } else {
                vec!["begin", "stage", "rollback"]
            }
        );
        processor.shutdown().await.unwrap();
        journal.shutdown().await.unwrap();
        group.shutdown().await.unwrap();
    }
}

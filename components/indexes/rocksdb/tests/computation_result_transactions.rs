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
        ComputationQueryError, ComputationResource, ComputationTransaction,
        ComputationTransactionGroup, TransactionDomain, TransactionGroupContext,
        TransactionGroupMutation,
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

#[path = "computation_result_transactions/source_transactions.rs"]
mod source_transactions;

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

#[tokio::test]
async fn constructed_durability_does_not_upgrade_transaction_participation() {
    use drasi_core::interface::{FailureMode, StorageDurability};
    let directory = tempfile::tempdir().expect("directory");
    let provider = provider(directory.path());
    assert_eq!(
        provider.durability(),
        StorageDurability::LOCAL_PROCESS_RESTART
    );
    let resources = provider
        .create_indexes("durability", QUERY)
        .await
        .expect("resources");
    assert_eq!(resources.durability(), provider.durability());
    assert!(resources
        .durability()
        .require(FailureMode::ProcessRestart)
        .is_ok());
    assert!(resources
        .durability()
        .require(FailureMode::PowerLoss)
        .is_err());
    let independent = rebind_resources(
        &resources,
        resources.indexes().session_control.clone(),
        false,
    );
    assert_eq!(independent.durability(), StorageDurability::UNKNOWN);
    let independent = independent.with_durability(provider.durability());
    assert!(independent.atomic_result_transaction().is_err());
    resources
        .cleanup()
        .expect("owner")
        .shutdown()
        .await
        .expect("shutdown");
}

enum GroupAppendOutcome {
    Commit,
    Reject,
    ExitBeforeCommit,
    ExitAfterCommit,
}

struct GroupJournalAppend {
    journal: std::sync::Weak<ComputationTransaction>,
    visible: Arc<std::sync::atomic::AtomicUsize>,
    outcome: GroupAppendOutcome,
}

#[async_trait]
impl TransactionGroupMutation for GroupJournalAppend {
    async fn stage(&mut self, context: &TransactionGroupContext<'_>) -> Result<(), IndexError> {
        let journal = self.journal.upgrade().ok_or(IndexError::NotSupported)?;
        context
            .require_member(&journal)
            .map_err(IndexError::other)?;
        journal
            .resources()
            .outbox_writer()
            .expect("journal")
            .append("pipe-output", 1, b"pending")
            .await?;
        match self.outcome {
            GroupAppendOutcome::Reject => Err(IndexError::IOError),
            GroupAppendOutcome::ExitBeforeCommit => std::process::exit(76),
            _ => Ok(()),
        }
    }

    async fn committed(&mut self) -> Result<(), IndexError> {
        if matches!(self.outcome, GroupAppendOutcome::ExitAfterCommit) {
            std::process::exit(76);
        }
        self.visible
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
}

#[tokio::test]
async fn shared_group_preserves_atomic_output_and_independent_member_lifetimes() {
    let directory = tempfile::tempdir().expect("directory");
    {
        let group =
            ComputationTransactionGroup::try_new(resources(directory.path(), "shared").await)
                .expect("shared group");
        let journal = Arc::new(group.journal_transaction("output").expect("journal"));
        let visible = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mutation = |outcome| -> Box<dyn TransactionGroupMutation> {
            Box::new(GroupJournalAppend {
                journal: Arc::downgrade(&journal),
                visible: visible.clone(),
                outcome,
            })
        };
        let pipe = journal
            .resources()
            .outbox_writer()
            .expect("pipe outbox")
            .clone();
        let fixture = from_resources(
            group.processor_indexes().expect("processor"),
            "MATCH (n:Person) RETURN n.name AS name",
        )
        .await;
        let rejected = fixture
            .query
            .process_source_changes_with_transaction_group(
                vec![person("alice")],
                &fixture.transaction,
                |result| {
                    let fixture = &fixture;
                    let mutation = mutation(GroupAppendOutcome::Reject);
                    async move {
                        stage(fixture, 1, result[0].row_signature()).await?;
                        Ok(vec![mutation])
                    }
                },
            )
            .await;
        assert!(rejected.is_err());
        assert!(!group.recovery_required());
        assert_eq!(visible.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_empty_committed_output(&fixture).await;
        assert!(pipe
            .read_from("pipe-output", 0)
            .await
            .expect("rolled back pipe")
            .is_empty());
        let result = fixture
            .query
            .process_source_changes_with_transaction_group(
                vec![person("alice")],
                &fixture.transaction,
                |result| {
                    let fixture = &fixture;
                    let mutation = mutation(GroupAppendOutcome::Commit);
                    async move {
                        stage(fixture, 1, result[0].row_signature()).await?;
                        Ok(vec![mutation])
                    }
                },
            )
            .await
            .expect("shared commit");
        assert!(matches!(
            result.as_ref(),
            [QueryPartEvaluationContext::Adding { .. }]
        ));
        assert_eq!(visible.load(std::sync::atomic::Ordering::SeqCst), 1);
        fixture.query.shutdown().await.expect("stop producer only");
        drop(fixture);
        journal
            .run(async {
                pipe.append("pipe-handled", 1, b"handled").await?;
                Ok(())
            })
            .await
            .expect("journal drains after producer stop");
        let replacement = from_resources(
            group.processor_indexes().expect("replacement"),
            "MATCH (n:Person) RETURN n.name AS name",
        )
        .await;
        assert_eq!(
            replacement
                .checkpoint
                .read_result_sequence(QUERY)
                .await
                .unwrap(),
            Some(1)
        );
        assert_eq!(
            pipe.read_from("pipe-output", 0).await.unwrap(),
            [(1, b"pending".to_vec())]
        );
        replacement
            .query
            .shutdown()
            .await
            .expect("stop replacement");
        journal.shutdown().await.expect("stop journal only");
        group.shutdown().await.expect("stop storage owner");
    }
    let group =
        ComputationTransactionGroup::try_new(resources(directory.path(), "shared").await).unwrap();
    let reopened = ComputationTransaction::try_new(group.processor_indexes().unwrap()).unwrap();
    let journal = group.journal_transaction("output").unwrap();
    assert_eq!(
        reopened
            .resources()
            .checkpoint_store()
            .unwrap()
            .read_result_sequence(QUERY)
            .await
            .unwrap(),
        Some(1)
    );
    let outbox = reopened.resources().outbox_writer().unwrap();
    let pipe = journal.resources().outbox_writer().unwrap();
    for (writer, key, value) in [
        (outbox, QUERY, b"output".as_slice()),
        (pipe, "pipe-output", b"pending"),
        (pipe, "pipe-handled", b"handled"),
    ] {
        assert_eq!(
            writer.read_from(key, 0).await.unwrap(),
            [(1, value.to_vec())]
        );
    }
    reopened.shutdown().await.unwrap();
    journal.shutdown().await.unwrap();
    group.shutdown().await.unwrap();
}

const GROUP_CRASH_ROOT: &str = "DRASI_TRANSACTION_GROUP_CRASH_ROOT";
const GROUP_CRASH_AFTER: &str = "DRASI_TRANSACTION_GROUP_CRASH_AFTER";

#[tokio::test]
async fn shared_group_journals_survive_processor_clear_without_cross_member_eviction() {
    let directory = tempfile::tempdir().unwrap();
    {
        let group = ComputationTransactionGroup::try_new(
            resources(directory.path(), "group-namespaces").await,
        )
        .unwrap();
        let processor =
            ComputationTransaction::try_new(group.processor_indexes().unwrap()).unwrap();
        let first = group.journal_transaction("a").unwrap();
        let second = group.journal_transaction("a_b").unwrap();
        for (member, value) in [
            (&processor, b"producer".as_slice()),
            (&first, b"first"),
            (&second, b"second"),
        ] {
            member
                .run(async {
                    let outbox = member.resources().outbox_writer().unwrap();
                    outbox.append("same", 1, value).await?;
                    outbox.append("metadata", 1, b"metadata").await?;
                    Ok(())
                })
                .await
                .unwrap();
        }
        let producer_output = processor.resources().outbox_writer().unwrap();
        processor
            .run(async {
                producer_output.clear("same").await?;
                producer_output.clear("metadata").await?;
                Ok(())
            })
            .await
            .unwrap();
        processor
            .resources()
            .checkpoint_store()
            .unwrap()
            .clear_checkpoints()
            .await
            .unwrap();
        let output = first.resources().outbox_writer().unwrap();
        assert_eq!(output.read_latest_sequence("same").await.unwrap(), Some(1));
        for (member, value) in [(&first, b"first".as_slice()), (&second, b"second")] {
            assert_eq!(
                member
                    .resources()
                    .outbox_writer()
                    .unwrap()
                    .read_from("same", 0)
                    .await
                    .unwrap(),
                [(1, value.to_vec())]
            );
        }
        first
            .run(async {
                output.append("same", 2, b"two").await?;
                assert_eq!(output.append_and_trim("same", 3, b"three", 2).await?, 1);
                Ok(())
            })
            .await
            .unwrap();
        let failed: Result<(), ComputationQueryError> = first
            .run(async {
                assert_eq!(output.trim_before("same", 3).await?, 1);
                Err(IndexError::IOError.into())
            })
            .await;
        assert!(failed.is_err());
        assert_eq!(
            output.read_from("same", 0).await.unwrap(),
            [(2, b"two".to_vec()), (3, b"three".to_vec())]
        );
        first
            .run(async {
                assert_eq!(output.trim_to_capacity("same", 1).await?, 1);
                Ok(())
            })
            .await
            .unwrap();
        assert_eq!(
            output.read_from("same", 2).await.unwrap(),
            [(3, b"three".to_vec())]
        );
        assert_eq!(output.read_latest_sequence("same").await.unwrap(), Some(3));
        processor.shutdown().await.unwrap();
        first.shutdown().await.unwrap();
        second.shutdown().await.unwrap();
        group.shutdown().await.unwrap();
    }
    let group =
        ComputationTransactionGroup::try_new(resources(directory.path(), "group-namespaces").await)
            .unwrap();
    let processor = ComputationTransaction::try_new(group.processor_indexes().unwrap()).unwrap();
    assert!(processor
        .resources()
        .outbox_writer()
        .unwrap()
        .read_from("same", 0)
        .await
        .unwrap()
        .is_empty());
    for (name, position, value) in [("a", 3, b"three".as_slice()), ("a_b", 1, b"second")] {
        let journal = group.journal_transaction(name).unwrap();
        let output = journal.resources().outbox_writer().unwrap();
        assert_eq!(
            output.read_from("same", 0).await.unwrap(),
            [(position, value.to_vec())]
        );
        assert_eq!(
            output.read_from("metadata", 0).await.unwrap(),
            [(1, b"metadata".to_vec())]
        );
        journal.shutdown().await.unwrap();
    }
    processor.shutdown().await.unwrap();
    group.shutdown().await.unwrap();
}

#[tokio::test]
async fn shared_group_scheduled_output_rolls_back_and_preserves_source_provenance() {
    let directory = tempfile::tempdir().unwrap();
    let group =
        ComputationTransactionGroup::try_new(resources(directory.path(), "group-future").await)
            .unwrap();
    let journal = Arc::new(group.journal_transaction("output").unwrap());
    let visible = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let pipe = journal.resources().outbox_writer().unwrap();
    let fixture = from_resources(
        group.processor_indexes().unwrap(),
        "MATCH (n:Person) WHERE drasi.trueLater(true, 2000) RETURN n.name AS name",
    )
    .await;
    assert!(fixture
        .query
        .process_source_change(person("alice"))
        .await
        .unwrap()
        .is_empty());
    let foreign = journal.resources().atomic_result_transaction().unwrap();
    assert!(matches!(
        fixture
            .query
            .process_due_futures_with_transaction_group(&foreign, |_| async {
                panic!("another member's proof must not evaluate or call the hook")
            })
            .await,
        Err(ComputationQueryError::TransactionMismatch)
    ));
    for outcome in [GroupAppendOutcome::Reject, GroupAppendOutcome::Commit] {
        let reject = matches!(outcome, GroupAppendOutcome::Reject);
        let mutation: Box<dyn TransactionGroupMutation> = Box::new(GroupJournalAppend {
            journal: Arc::downgrade(&journal),
            visible: visible.clone(),
            outcome,
        });
        let result = fixture
            .query
            .process_due_futures_with_transaction_group(&fixture.transaction, |result| {
                let fixture = &fixture;
                async move {
                    assert_eq!(result.source_id.as_ref(), "source");
                    stage(fixture, 1, result.results[0].row_signature()).await?;
                    Ok(vec![mutation])
                }
            })
            .await;
        if reject {
            assert!(matches!(
                result,
                Err(ComputationQueryError::Index(IndexError::IOError))
            ));
            assert!(!group.recovery_required());
            assert_empty_committed_output(&fixture).await;
            assert!(pipe.read_from("pipe-output", 0).await.unwrap().is_empty());
            assert_eq!(
                fixture
                    .query
                    .resources()
                    .indexes()
                    .future_queue
                    .peek_due_time()
                    .await
                    .unwrap(),
                Some(2000)
            );
            assert_eq!(visible.load(std::sync::atomic::Ordering::SeqCst), 0);
        } else {
            assert_eq!(result.unwrap().unwrap().source_id.as_ref(), "source");
            assert_eq!(
                pipe.read_from("pipe-output", 0).await.unwrap(),
                [(1, b"pending".to_vec())]
            );
            assert_eq!(visible.load(std::sync::atomic::Ordering::SeqCst), 1);
        }
    }
    assert!(fixture
        .query
        .process_due_futures_with_transaction_group(&fixture.transaction, |_| async {
            panic!("an empty queue cannot invoke the hook")
        })
        .await
        .unwrap()
        .is_none());
    fixture.query.shutdown().await.unwrap();
    journal.shutdown().await.unwrap();
    group.shutdown().await.unwrap();
}

#[tokio::test]
#[ignore = "explicit child of shared_group_recovers_atomic_output_after_process_exit"]
async fn shared_group_crash_child() {
    let directory = std::path::PathBuf::from(std::env::var(GROUP_CRASH_ROOT).unwrap());
    let outcome = match std::env::var(GROUP_CRASH_AFTER).unwrap().as_str() {
        "true" => GroupAppendOutcome::ExitAfterCommit,
        "false" => GroupAppendOutcome::ExitBeforeCommit,
        value => panic!("invalid crash boundary: {value}"),
    };
    let group =
        ComputationTransactionGroup::try_new(resources(&directory, "group-crash").await).unwrap();
    let journal = Arc::new(group.journal_transaction("output").unwrap());
    let fixture = from_resources(
        group.processor_indexes().unwrap(),
        "MATCH (n:Person) RETURN n.name AS name",
    )
    .await;
    let mutation: Box<dyn TransactionGroupMutation> = Box::new(GroupJournalAppend {
        journal: Arc::downgrade(&journal),
        visible: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        outcome,
    });
    fixture
        .query
        .process_source_changes_with_transaction_group(
            vec![person("alice")],
            &fixture.transaction,
            |result| {
                let fixture = &fixture;
                async move {
                    stage(fixture, 1, result[0].row_signature()).await?;
                    Ok(vec![mutation])
                }
            },
        )
        .await
        .unwrap();
    panic!("the child must exit at the requested transaction boundary");
}

#[tokio::test]
async fn shared_group_recovers_atomic_output_after_process_exit() {
    for committed in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "shared_group_crash_child",
                "--ignored",
                "--nocapture",
            ])
            .env(GROUP_CRASH_ROOT, directory.path())
            .env(GROUP_CRASH_AFTER, committed.to_string())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while child.try_wait().unwrap().is_none() {
            if std::time::Instant::now() >= deadline {
                child.kill().unwrap();
                let output = child.wait_with_output().unwrap();
                panic!("shared transaction child timed out: {output:?}");
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let output = child.wait_with_output().unwrap();
        assert_eq!(output.status.code(), Some(76), "{output:?}");
        let group =
            ComputationTransactionGroup::try_new(resources(directory.path(), "group-crash").await)
                .unwrap();
        let journal = group.journal_transaction("output").unwrap();
        let fixture = from_resources(
            group.processor_indexes().unwrap(),
            "MATCH (n:Person) RETURN n.name AS name",
        )
        .await;
        assert_eq!(
            fixture
                .checkpoint
                .read_result_sequence(QUERY)
                .await
                .unwrap(),
            committed.then_some(1)
        );
        let progress = fixture.checkpoint.read_checkpoint("source").await.unwrap();
        assert_eq!(
            progress.as_ref().map(|value| value.sequence),
            committed.then_some(1)
        );
        if let Some(progress) = progress {
            assert_eq!(
                progress.source_position,
                Some(Bytes::from_static(b"position"))
            );
        }
        for (writer, key, value) in [
            (&fixture.outbox, QUERY, b"output".as_slice()),
            (
                journal.resources().outbox_writer().unwrap(),
                "pipe-output",
                b"pending",
            ),
        ] {
            assert_eq!(
                writer.read_from(key, 0).await.unwrap(),
                if committed {
                    vec![(1, value.to_vec())]
                } else {
                    Vec::new()
                }
            );
        }
        let live = fixture.live.read_snapshot(QUERY).await.unwrap();
        assert_eq!(live.len(), usize::from(committed));
        fixture
            .query
            .resource_transaction(|| async {
                assert_eq!(
                    fixture
                        .query
                        .resources()
                        .indexes()
                        .element_index
                        .get_element(&ElementReference::new("source", "alice"))
                        .await?
                        .is_some(),
                    committed
                );
                Ok(())
            })
            .await
            .unwrap();
        fixture.query.shutdown().await.unwrap();
        journal.shutdown().await.unwrap();
        group.shutdown().await.unwrap();
    }
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
        .stage_result_sequence(QUERY, sequence)
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
    assert!(checkpoint.stage_result_sequence(QUERY, 1).await.is_err());
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
async fn output_generation_and_committed_head_survive_checkpoint_clear_and_reopen() {
    let temp = tempfile::tempdir().expect("temp dir");
    {
        let resources = resources(temp.path(), "generation").await;
        let checkpoint = resources.checkpoint_store().expect("checkpoint");
        assert_eq!(
            checkpoint
                .read_output_generation(QUERY)
                .await
                .expect("new generation"),
            None
        );
        checkpoint
            .write_output_generation(QUERY, u64::MAX)
            .await
            .expect("persist lifetime");
        checkpoint
            .write_result_sequence(QUERY, 41)
            .await
            .expect("persist standalone head");
        checkpoint
            .write_output_generation("other-query", 3)
            .await
            .expect("other lifetime");
        checkpoint
            .write_result_sequence("other-query", 9)
            .await
            .expect("other head");
        checkpoint
            .write_config_hash(123)
            .await
            .expect("config hash");
        let session = SessionGuard::begin(resources.indexes().session_control.clone())
            .await
            .expect("begin source checkpoint");
        checkpoint
            .stage_checkpoint("source", 7, Some(&Bytes::from_static(b"position")))
            .await
            .expect("stage source");
        session.commit().await.expect("commit source");
        {
            let _session = SessionGuard::begin(resources.indexes().session_control.clone())
                .await
                .expect("begin discarded head");
            checkpoint
                .stage_result_sequence(QUERY, 42)
                .await
                .expect("stage head");
            assert_eq!(
                checkpoint
                    .read_result_sequence(QUERY)
                    .await
                    .expect("committed head"),
                Some(41)
            );
        }
        checkpoint
            .clear_checkpoints()
            .await
            .expect("clear bootstrap state");
        assert!(checkpoint
            .read_all_checkpoints()
            .await
            .expect("cleared sources")
            .is_empty());
        assert_eq!(
            checkpoint.read_config_hash().await.expect("cleared config"),
            None
        );
        assert_eq!(
            checkpoint
                .read_result_sequence(QUERY)
                .await
                .expect("retained head"),
            Some(41)
        );
        assert_eq!(
            checkpoint
                .read_output_generation(QUERY)
                .await
                .expect("retained lifetime"),
            Some(u64::MAX)
        );
        assert_eq!(
            checkpoint
                .read_output_generation("other-query")
                .await
                .expect("other retained lifetime"),
            Some(3)
        );
        assert_eq!(
            checkpoint
                .read_result_sequence("other-query")
                .await
                .expect("other retained head"),
            Some(9)
        );
    }
    let reopened = resources(temp.path(), "generation").await;
    let checkpoint = reopened.checkpoint_store().expect("checkpoint");
    assert!(checkpoint
        .read_all_checkpoints()
        .await
        .expect("sources after reopen")
        .is_empty());
    assert_eq!(
        checkpoint
            .read_config_hash()
            .await
            .expect("config after reopen"),
        None
    );
    assert_eq!(
        checkpoint
            .read_result_sequence(QUERY)
            .await
            .expect("head after reopen"),
        Some(41)
    );
    assert_eq!(
        checkpoint
            .read_output_generation(QUERY)
            .await
            .expect("lifetime after reopen"),
        Some(u64::MAX)
    );
    let other = resources(temp.path(), "other-generation").await;
    assert_eq!(
        other
            .checkpoint_store()
            .expect("checkpoint")
            .read_output_generation(QUERY)
            .await
            .expect("isolated lifetime"),
        None
    );
}

#[tokio::test]
async fn native_retention_includes_staged_entries_and_respects_full_width_sequences() {
    let temp = tempfile::tempdir().expect("temp dir");
    let resources = resources(temp.path(), "retention").await;
    let outbox = resources.outbox_writer().expect("outbox");
    let session = SessionGuard::begin(resources.indexes().session_control.clone())
        .await
        .expect("begin seed");
    outbox.append(QUERY, 1, b"one").await.expect("one");
    outbox.append(QUERY, 2, b"two").await.expect("two");
    outbox
        .append("metadata", 1, b"marker")
        .await
        .expect("metadata");
    assert_eq!(outbox.trim_before(QUERY, 0).await.expect("retain all"), 0);
    session.commit().await.expect("commit seed");
    {
        let _session = SessionGuard::begin(resources.indexes().session_control.clone())
            .await
            .expect("begin rolled back retention");
        assert_eq!(
            outbox
                .append_and_trim(QUERY, 3, b"three", 3)
                .await
                .expect("staged trim"),
            2
        );
        assert_eq!(
            outbox
                .read_latest_sequence(QUERY)
                .await
                .expect("committed head"),
            Some(2)
        );
    }
    assert_eq!(
        outbox
            .read_from(QUERY, 0)
            .await
            .expect("rolled back retention"),
        [(1, b"one".to_vec()), (2, b"two".to_vec())]
    );
    let session = SessionGuard::begin(resources.indexes().session_control.clone())
        .await
        .expect("begin committed retention");
    assert_eq!(
        outbox
            .append_and_trim(QUERY, 3, b"three", 3)
            .await
            .expect("staged trim"),
        2
    );
    session.commit().await.expect("commit retention");
    assert_eq!(
        outbox
            .read_from(QUERY, 0)
            .await
            .expect("retained staged append"),
        [(3, b"three".to_vec())]
    );
    let session = SessionGuard::begin(resources.indexes().session_control.clone())
        .await
        .expect("begin boundary writes");
    outbox
        .append(QUERY, u64::MAX - 1, b"penultimate")
        .await
        .expect("penultimate");
    assert_eq!(
        outbox
            .append_and_trim(QUERY, u64::MAX, b"last", u64::MAX - 1)
            .await
            .expect("boundary retention"),
        1
    );
    assert_eq!(
        outbox
            .trim_to_capacity(QUERY, 1)
            .await
            .expect("retain maximum"),
        1
    );
    resources
        .checkpoint_store()
        .expect("checkpoint")
        .stage_result_sequence(QUERY, u64::MAX)
        .await
        .expect("stage maximum head");
    session.commit().await.expect("commit maximum");
    assert_eq!(
        outbox
            .read_latest_sequence(QUERY)
            .await
            .expect("maximum head"),
        Some(u64::MAX)
    );
    assert_eq!(
        outbox
            .read_from(QUERY, u64::MAX - 1)
            .await
            .expect("read maximum"),
        [(u64::MAX, b"last".to_vec())]
    );
    assert!(outbox
        .read_from(QUERY, u64::MAX)
        .await
        .expect("past maximum")
        .is_empty());
    assert_eq!(
        outbox
            .read_from("metadata", 0)
            .await
            .expect("metadata is isolated"),
        [(1, b"marker".to_vec())]
    );
    drop(resources);
    let reopened = self::resources(temp.path(), "retention").await;
    let outbox = reopened.outbox_writer().expect("outbox");
    assert_eq!(
        reopened
            .checkpoint_store()
            .expect("checkpoint")
            .read_result_sequence(QUERY)
            .await
            .expect("reopened maximum head"),
        Some(u64::MAX)
    );
    assert_eq!(
        outbox
            .read_latest_sequence(QUERY)
            .await
            .expect("reopened outbox head"),
        Some(u64::MAX)
    );
    assert_eq!(
        outbox
            .read_from(QUERY, 0)
            .await
            .expect("persisted retention"),
        [(u64::MAX, b"last".to_vec())]
    );
    assert!(outbox
        .read_from(QUERY, u64::MAX)
        .await
        .expect("reopened past maximum")
        .is_empty());
    assert_eq!(
        outbox
            .read_from("metadata", 0)
            .await
            .expect("reopened metadata"),
        [(1, b"marker".to_vec())]
    );
}

#[tokio::test]
async fn existing_native_outbox_namespaces_remain_readable_without_migration() {
    let temp = tempfile::tempdir().expect("temp dir");
    let graph = "persisted-format";
    let metadata_id = "recovery:people";
    let marker = br#"{"sequence":7,"in_progress":false,"generation":3}"#;
    let encode = |id: &str| -> String { id.bytes().map(|byte| format!("{byte:02x}")).collect() };
    let path = temp.path().join("computation-v1").join(encode(graph));
    {
        let options = RocksIndexOptions::new(
            false,
            false,
            RocksDbMemoryBudget::from_total_budget_bytes(32 << 20).expect("memory budget"),
        );
        let db = drasi_index_rocksdb::open_unified_db(
            path.to_str().expect("UTF-8 path"),
            &encode(QUERY),
            &options,
        )
        .expect("existing native database");
        let cf = db.cf_handle("outbox").expect("outbox column");
        for (id, sequence, data) in [
            (QUERY, 7u64, b"envelope".as_slice()),
            (metadata_id, 1u64, marker.as_slice()),
        ] {
            let mut key = id.as_bytes().to_vec();
            key.push(0);
            key.extend_from_slice(&sequence.to_be_bytes());
            db.put_cf(&cf, key, data)
                .expect("persist existing key format");
        }
    }
    let reopened = resources(temp.path(), graph).await;
    let outbox = reopened.outbox_writer().expect("outbox");
    assert_eq!(
        outbox.read_from(QUERY, 0).await.expect("existing envelope"),
        [(7, b"envelope".to_vec())]
    );
    assert_eq!(
        outbox
            .read_from(metadata_id, 0)
            .await
            .expect("existing recovery metadata"),
        [(1, marker.to_vec())]
    );
    let session = SessionGuard::begin(reopened.indexes().session_control.clone())
        .await
        .expect("begin primary clear");
    outbox.clear(QUERY).await.expect("clear primary");
    session.commit().await.expect("commit clear");
    assert_eq!(
        outbox
            .read_from(metadata_id, 0)
            .await
            .expect("recovery metadata survives"),
        [(1, marker.to_vec())]
    );
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

#[tokio::test]
async fn healthy_query_quiescence_retains_resources_and_batches_share_one_commit() {
    let temp = tempfile::tempdir().expect("temp dir");
    let fixture = fixture(
        temp.path(),
        "batch-quiescence",
        "MATCH (n:Person) RETURN n.name AS name",
    )
    .await;
    let results = fixture
        .query
        .process_source_changes_with_result_hook(
            vec![person("alice"), person("bob")],
            &fixture.transaction,
            |results| {
                let fixture = &fixture;
                async move {
                    assert_eq!(results.len(), 2);
                    stage(fixture, 1, results[0].row_signature()).await
                }
            },
        )
        .await
        .expect("single batch commit");
    assert_eq!(results.len(), 2);
    fixture
        .query
        .quiesce()
        .await
        .expect("await healthy provider work without sealing");
    assert!(!fixture.query.recovery_required());
    let next = fixture
        .query
        .process_source_change(person("charlie"))
        .await
        .expect("same healthy instance resumes");
    assert_eq!(next.len(), 1);
    fixture.query.shutdown().await.expect("final cleanup");
}

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

use super::*;
use drasi_core::evaluation::context::QueryVariables;

fn update(id: &str, properties: serde_json::Value, time: u64) -> SourceChange {
    SourceChange::Update {
        element: Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new("source", id),
                labels: Arc::from([Arc::from("Person")]),
                effective_from: time,
            },
            properties: ElementPropertyMap::from(properties),
        },
    }
}

fn remove(id: &str, label: &str, time: u64) -> SourceChange {
    SourceChange::Delete {
        metadata: ElementMetadata {
            reference: ElementReference::new("source", id),
            labels: Arc::from([Arc::from(label)]),
            effective_from: time,
        },
    }
}

fn repeated_row() -> Vec<SourceChange> {
    vec![
        person("alice"),
        update("alice", json!({"name": "intermediate"}), 1100),
        update("alice", json!({"name": "final"}), 1200),
        person("temporary"),
        remove("temporary", "Person", 1300),
    ]
}

fn added_name(results: &[QueryPartEvaluationContext], expected: &str) {
    let [QueryPartEvaluationContext::Adding { after, .. }] = results else {
        panic!("expected one final addition, got {results:?}");
    };
    assert_eq!(
        after.get("name"),
        Some(&VariableValue::String(expected.into()))
    );
}

async fn stage_results(
    fixture: &Fixture,
    sequence: u64,
    results: &[QueryPartEvaluationContext],
) -> Result<(), IndexError> {
    let mut rows = Vec::new();
    for result in results {
        let values = match result {
            QueryPartEvaluationContext::Adding { after, .. }
            | QueryPartEvaluationContext::Updating { after, .. }
            | QueryPartEvaluationContext::Aggregation { after, .. } => {
                Some(serde_json::to_vec(after).map_err(IndexError::other)?)
            }
            QueryPartEvaluationContext::Removing { .. } => None,
            QueryPartEvaluationContext::Noop => continue,
        };
        rows.push((result.row_signature(), values));
    }
    fixture
        .checkpoint
        .stage_checkpoint("source", sequence, None)
        .await?;
    fixture
        .checkpoint
        .stage_result_sequence(QUERY, sequence)
        .await?;
    fixture
        .outbox
        .append(
            QUERY,
            sequence,
            &serde_json::to_vec(&rows).map_err(IndexError::other)?,
        )
        .await?;
    fixture
        .live
        .apply_mutations(
            QUERY,
            &rows
                .iter()
                .map(|(signature, data)| RowMutation {
                    row_signature: *signature,
                    data: data.as_deref(),
                })
                .collect::<Vec<_>>(),
        )
        .await
}

async fn commit(
    fixture: &Fixture,
    sequence: u64,
    changes: Vec<SourceChange>,
) -> Arc<[QueryPartEvaluationContext]> {
    fixture
        .query
        .process_source_transaction_with_result_hook(
            changes,
            &fixture.transaction,
            |results| async move { stage_results(fixture, sequence, &results).await },
        )
        .await
        .expect("whole source transaction")
}

#[tokio::test]
async fn source_transaction_retains_final_rows_and_leaves_ordinary_batches_unchanged() {
    let directory = tempfile::tempdir().expect("directory");
    let grouped = fixture(
        directory.path(),
        "grouped",
        "MATCH (n:Person) RETURN n.name AS name",
    )
    .await;
    let results = commit(&grouped, 1, repeated_row()).await;
    added_name(&results, "final");
    let stored = grouped.live.read_snapshot(QUERY).await.expect("live state");
    assert_eq!(stored.len(), 1);
    let values: QueryVariables = serde_json::from_slice(&stored[0].1).expect("persisted final row");
    assert_eq!(values["name"], VariableValue::String("final".into()));
    assert_eq!(
        grouped
            .outbox
            .read_from(QUERY, 0)
            .await
            .expect("outbox")
            .len(),
        1
    );

    let ordinary = fixture(
        directory.path(),
        "ordinary",
        "MATCH (n:Person) RETURN n.name AS name",
    )
    .await;
    let original = ordinary
        .query
        .process_source_changes_with_result_hook(repeated_row(), &ordinary.transaction, |_| async {
            Ok(())
        })
        .await
        .expect("ordinary batch");
    assert_eq!(
        original.len(),
        5,
        "batching alone must not opt into final-state semantics"
    );

    let changed = commit(
        &grouped,
        2,
        vec![
            remove("alice", "Person", 1400),
            person("alice"),
            update("alice", json!({"name": "replacement"}), 1500),
        ],
    )
    .await;
    let [QueryPartEvaluationContext::Updating { before, after, .. }] = changed.as_ref() else {
        panic!("replacement must be one update: {changed:?}");
    };
    assert_eq!(before["name"], VariableValue::String("final".into()));
    assert_eq!(after["name"], VariableValue::String("replacement".into()));
    grouped.query.shutdown().await.expect("grouped cleanup");
    ordinary.query.shutdown().await.expect("ordinary cleanup");
}

#[tokio::test]
async fn source_transaction_aggregations_use_original_before_and_final_after() {
    let directory = tempfile::tempdir().expect("directory");
    let fixture = fixture(
        directory.path(),
        "aggregate",
        "MATCH (n:Person) RETURN sum(n.amount) AS total",
    )
    .await;
    let initial = commit(
        &fixture,
        1,
        vec![
            person("alice"),
            update("alice", json!({"amount": 10}), 1100),
            person("bob"),
            update("bob", json!({"amount": 20}), 1200),
        ],
    )
    .await;
    assert_eq!(initial.len(), 1);
    let changes = commit(
        &fixture,
        2,
        vec![
            update("alice", json!({"amount": 100}), 1300),
            update("bob", json!({"amount": 5}), 1400),
            update("alice", json!({"amount": 40}), 1500),
        ],
    )
    .await;
    let [QueryPartEvaluationContext::Aggregation {
        before: Some(before),
        after,
        ..
    }] = changes.as_ref()
    else {
        panic!("expected one final aggregate: {changes:?}");
    };
    assert_eq!(before["total"], VariableValue::Integer(30.into()));
    assert_eq!(after["total"], VariableValue::Integer(45.into()));
    let unchanged = commit(
        &fixture,
        3,
        vec![
            update("alice", json!({"amount": 100}), 1600),
            update("alice", json!({"amount": 40}), 1700),
        ],
    )
    .await;
    assert!(unchanged.is_empty());
    fixture.query.shutdown().await.expect("cleanup");
}

#[tokio::test]
async fn source_transaction_transient_relations_never_escape_the_group() {
    let directory = tempfile::tempdir().expect("directory");
    let fixture = fixture(
        directory.path(),
        "relations",
        "MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN a.name AS name",
    )
    .await;
    let relation = SourceChange::Insert {
        element: Element::Relation {
            metadata: ElementMetadata {
                reference: ElementReference::new("source", "knows"),
                labels: Arc::from([Arc::from("KNOWS")]),
                effective_from: 1100,
            },
            in_node: ElementReference::new("source", "alice"),
            out_node: ElementReference::new("source", "bob"),
            properties: ElementPropertyMap::new(),
        },
    };
    let transient = commit(
        &fixture,
        1,
        vec![
            person("alice"),
            person("bob"),
            relation.clone(),
            remove("knows", "KNOWS", 1200),
        ],
    )
    .await;
    assert!(transient.is_empty());
    assert!(fixture
        .live
        .read_snapshot(QUERY)
        .await
        .expect("live state")
        .is_empty());
    let final_row = commit(
        &fixture,
        2,
        vec![relation, update("alice", json!({"name": "final"}), 1300)],
    )
    .await;
    added_name(&final_row, "final");
    fixture.query.shutdown().await.expect("cleanup");
}

#[tokio::test]
async fn source_transaction_staging_failure_rolls_back_every_change_and_output() {
    let directory = tempfile::tempdir().expect("directory");
    let fixture = fixture(
        directory.path(),
        "rollback",
        "MATCH (n:Person) RETURN n.name AS name",
    )
    .await;
    let rejected = fixture
        .query
        .process_source_transaction_with_result_hook(
            repeated_row(),
            &fixture.transaction,
            |results| {
                let fixture = &fixture;
                async move {
                    added_name(&results, "final");
                    stage_results(fixture, 1, &results).await?;
                    Err(IndexError::CorruptedData)
                }
            },
        )
        .await;
    assert!(matches!(
        rejected,
        Err(ComputationQueryError::Index(IndexError::CorruptedData))
    ));
    assert_empty(&fixture).await;
    assert!(!fixture.query.recovery_required());
    added_name(&commit(&fixture, 1, repeated_row()).await, "final");
    fixture.query.shutdown().await.expect("cleanup");
}

#[tokio::test]
async fn source_transaction_cancelled_staging_fences_owner_and_restarts_as_one_group() {
    let directory = tempfile::tempdir().expect("directory");
    {
        let fixture = fixture(
            directory.path(),
            "cancelled",
            "MATCH (n:Person) RETURN n.name AS name",
        )
        .await;
        let staged = tokio::sync::Notify::new();
        let mut operation = Box::pin(fixture.query.process_source_transaction_with_result_hook(
            repeated_row(),
            &fixture.transaction,
            |results| {
                let fixture = &fixture;
                let staged = &staged;
                async move {
                    stage_results(fixture, 1, &results).await?;
                    staged.notify_one();
                    std::future::pending::<Result<(), IndexError>>().await
                }
            },
        ));
        tokio::select! {
            result = &mut operation => panic!("staging must remain pending: {result:?}"),
            _ = staged.notified() => {}
        }
        drop(operation);
        assert!(fixture.query.recovery_required());
        assert!(matches!(
            fixture
                .query
                .process_source_transaction_with_result_hook(
                    repeated_row(),
                    &fixture.transaction,
                    |_| async { Ok(()) },
                )
                .await,
            Err(ComputationQueryError::RecoveryRequired)
        ));
        fixture.query.shutdown().await.expect("join before reopen");
    }
    {
        let fixture = fixture(
            directory.path(),
            "cancelled",
            "MATCH (n:Person) RETURN n.name AS name",
        )
        .await;
        assert_empty(&fixture).await;
        added_name(&commit(&fixture, 1, repeated_row()).await, "final");
        fixture.query.shutdown().await.expect("cleanup");
    }
    let fixture = fixture(
        directory.path(),
        "cancelled",
        "MATCH (n:Person) RETURN n.name AS name",
    )
    .await;
    assert_eq!(
        fixture
            .outbox
            .read_from(QUERY, 0)
            .await
            .expect("reopened output")
            .len(),
        1
    );
    let stored = fixture
        .live
        .read_snapshot(QUERY)
        .await
        .expect("reopened state");
    assert_eq!(stored.len(), 1);
    let row: QueryVariables = serde_json::from_slice(&stored[0].1).expect("final row");
    assert_eq!(row["name"], VariableValue::String("final".into()));
    fixture.query.shutdown().await.expect("cleanup");
}

#[tokio::test]
async fn source_transaction_shared_journal_commits_only_final_results() {
    let directory = tempfile::tempdir().expect("directory");
    let group =
        ComputationTransactionGroup::try_new(resources(directory.path(), "shared-source").await)
            .expect("group");
    let journal = Arc::new(group.journal_transaction("output").expect("journal"));
    let visible = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let fixture = from_resources(
        group.processor_indexes().expect("processor"),
        "MATCH (n:Person) RETURN n.name AS name",
    )
    .await;
    for reject in [true, false] {
        let mutation: Box<dyn TransactionGroupMutation> = Box::new(GroupJournalAppend {
            journal: Arc::downgrade(&journal),
            visible: visible.clone(),
            outcome: if reject {
                GroupAppendOutcome::Reject
            } else {
                GroupAppendOutcome::Commit
            },
        });
        let result = fixture
            .query
            .process_source_transaction_with_transaction_group(
                repeated_row(),
                &fixture.transaction,
                |results| {
                    let fixture = &fixture;
                    async move {
                        added_name(&results, "final");
                        stage_results(fixture, 1, &results).await?;
                        Ok(vec![mutation])
                    }
                },
            )
            .await;
        assert_eq!(result.is_err(), reject);
        assert_eq!(
            visible.load(std::sync::atomic::Ordering::SeqCst),
            usize::from(!reject)
        );
        assert!(!group.recovery_required());
        assert_eq!(
            journal
                .resources()
                .outbox_writer()
                .expect("pipe")
                .read_from("pipe-output", 0)
                .await
                .expect("journal output")
                .len(),
            usize::from(!reject)
        );
        if reject {
            assert_empty_committed_output(&fixture).await;
        } else {
            added_name(&result.expect("committed group"), "final");
        }
    }
    fixture.query.shutdown().await.expect("processor cleanup");
    journal.shutdown().await.expect("journal cleanup");
    group.shutdown().await.expect("group cleanup");
}

#[tokio::test]
async fn source_transaction_rejects_unrelated_storage_before_evaluation() {
    let directory = tempfile::tempdir().expect("directory");
    let first = fixture(
        directory.path(),
        "first-source",
        "MATCH (n:Person) RETURN n.name AS name",
    )
    .await;
    let second = fixture(
        directory.path(),
        "second-source",
        "MATCH (n:Person) RETURN n.name AS name",
    )
    .await;
    assert!(matches!(
        first
            .query
            .process_source_transaction_with_result_hook(
                repeated_row(),
                &second.transaction,
                |_| async {
                    panic!("foreign hook");
                },
            )
            .await,
        Err(ComputationQueryError::TransactionMismatch)
    ));
    assert!(matches!(
        first
            .query
            .process_source_transaction_with_transaction_group(
                repeated_row(),
                &second.transaction,
                |_| async {
                    panic!("foreign journal hook");
                },
            )
            .await,
        Err(ComputationQueryError::TransactionMismatch)
    ));
    added_name(&commit(&first, 1, repeated_row()).await, "final");
    assert_empty(&second).await;
    first.query.shutdown().await.expect("first cleanup");
    second.query.shutdown().await.expect("second cleanup");
}

#[tokio::test]
async fn source_transaction_scheduled_work_reflects_only_committed_source_state() {
    let directory = tempfile::tempdir().expect("directory");
    let fixture = fixture(
        directory.path(),
        "source-timers",
        "MATCH (n:Person) WHERE drasi.trueLater(true, 2000) RETURN n.name AS name",
    )
    .await;
    assert!(commit(
        &fixture,
        1,
        vec![
            person("alice"),
            person("bob"),
            remove("alice", "Person", 1200),
        ]
    )
    .await
    .is_empty());
    let mut delivered = Vec::new();
    for _ in 0..3 {
        let Some(due) = fixture
            .query
            .process_due_futures_with_result_hook(&fixture.transaction, |_| async { Ok(()) })
            .await
            .expect("due work")
        else {
            break;
        };
        delivered.extend(due.results.clone());
    }
    added_name(&delivered, "bob");
    assert!(fixture
        .query
        .scheduling_queue()
        .peek_due_time()
        .await
        .expect("finished timers")
        .is_none());
    fixture.query.shutdown().await.expect("cleanup");
}

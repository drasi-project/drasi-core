// Copyright 2026 The Drasi Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::{collections::HashMap, sync::Arc};

use drasi_query_cypher::CypherParser;
use serde_json::json;

use crate::{
    evaluation::{
        context::{QueryPartEvaluationContext, QueryVariables},
        functions::{Count, Function, FunctionRegistry},
        variable_value::VariableValue,
    },
    in_memory_index::{
        in_memory_element_index::InMemoryElementIndex, in_memory_future_queue::InMemoryFutureQueue,
        in_memory_result_index::InMemoryResultIndex,
    },
    models::{Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange},
    query::{ContinuousQuery, QueryBuilder},
};

const CHAINED_OPTIONAL_QUERY: &str = "
MATCH (team:Team)
OPTIONAL MATCH (person:Person)-[:MEMBER_OF]->(team)
OPTIONAL MATCH (report:Report)-[:REPORT_FOR]->(person)
OPTIONAL MATCH (approval:Approval)-[:APPROVES]->(report)
RETURN team.id AS team,
  count(person) AS memberCount,
  count(report) AS reportCount,
  count(approval) AS approvalCount
";

const SINGLE_OPTIONAL_QUERY: &str = "
MATCH (team:Team)
OPTIONAL MATCH (person:Person)-[:MEMBER_OF]->(team)
RETURN team.id AS team, count(person) AS memberCount
";

const BRANCHED_OPTIONAL_QUERY: &str = "
MATCH (team:Team)
OPTIONAL MATCH (person:Person)-[:MEMBER_OF]->(team)
OPTIONAL MATCH (report:Report)-[:REPORT_FOR]->(person)
OPTIONAL MATCH (badge:Badge)-[:ASSIGNED_TO]->(person)
RETURN team.id AS team,
  count(person) AS memberCount,
  count(report) AS reportCount,
  count(badge) AS badgeCount
";

const DEFINITION_ROOTED_QUERY: &str = "
MATCH (slot:Slot)
OPTIONAL MATCH (child:RuntimeChild)-[:RUNS_IN]->(slot)
OPTIONAL MATCH (result:ChildResult)-[:RESULT_FOR]->(child)
OPTIONAL MATCH (evaluation:ChildEvaluation)-[:EVALUATES]->(result)
RETURN slot.id AS slot,
  count(child) AS childCount,
  count(result) AS resultCount,
  count(evaluation) AS evaluationCount
";

const NESTED_OPTIONAL_AGGREGATE_QUERY: &str = "
MATCH (team:Team)
OPTIONAL MATCH (person:Person)-[:MEMBER_OF]->(team)
OPTIONAL MATCH (report:Report)-[:REPORT_FOR]->(person)
WITH team, person, count(report) AS reportCount
WITH team,
  count(person) AS memberCount,
  count(CASE WHEN reportCount = 1 THEN 1 ELSE null END) AS completedMemberCount
WHERE memberCount > 0 AND completedMemberCount < memberCount
RETURN team.id AS team, memberCount, completedMemberCount
";

struct MaterializedQuery {
    query: ContinuousQuery,
    rows: HashMap<u64, QueryVariables>,
}

impl MaterializedQuery {
    async fn new(query_text: &str) -> Self {
        let functions = Arc::new(FunctionRegistry::new());
        functions.register_function("count", Function::Aggregating(Arc::new(Count {})));
        let parser = Arc::new(CypherParser::new(functions.clone()));
        let element_index = Arc::new(InMemoryElementIndex::new());
        let query = QueryBuilder::new(query_text, parser)
            .with_function_registry(functions)
            .with_element_index(element_index.clone())
            .with_archive_index(element_index)
            .with_result_index(Arc::new(InMemoryResultIndex::new()))
            .with_future_queue(Arc::new(InMemoryFutureQueue::new()))
            .build()
            .await;
        Self {
            query,
            rows: HashMap::new(),
        }
    }

    async fn process(&mut self, change: SourceChange) -> Vec<QueryPartEvaluationContext> {
        let changes = self.query.process_source_change(change).await.unwrap();
        for change in &changes {
            match change {
                QueryPartEvaluationContext::Adding {
                    after,
                    row_signature,
                }
                | QueryPartEvaluationContext::Updating {
                    after,
                    row_signature,
                    ..
                }
                | QueryPartEvaluationContext::Aggregation {
                    after,
                    row_signature,
                    ..
                } => {
                    self.rows.insert(*row_signature, after.clone());
                }
                QueryPartEvaluationContext::Removing { row_signature, .. } => {
                    self.rows.remove(row_signature);
                }
                QueryPartEvaluationContext::Noop => {}
            }
        }
        changes
    }

    fn only_row(&self) -> &QueryVariables {
        assert_eq!(self.rows.len(), 1);
        self.rows.values().next().unwrap()
    }
}

fn node(id: &str, label: &str, effective_from: u64) -> SourceChange {
    SourceChange::Insert {
        element: Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new("test", id),
                labels: Arc::new([Arc::from(label)]),
                effective_from,
            },
            properties: ElementPropertyMap::from(json!({"id": id})),
        },
    }
}

fn relation(id: &str, label: &str, effective_from: u64, from: &str, to: &str) -> SourceChange {
    SourceChange::Insert {
        element: Element::Relation {
            metadata: ElementMetadata {
                reference: ElementReference::new("test", id),
                labels: Arc::new([Arc::from(label)]),
                effective_from,
            },
            in_node: ElementReference::new("test", from),
            out_node: ElementReference::new("test", to),
            properties: ElementPropertyMap::from(json!({})),
        },
    }
}

fn delete(id: &str, effective_from: u64) -> SourceChange {
    SourceChange::Delete {
        metadata: ElementMetadata {
            reference: ElementReference::new("test", id),
            labels: Arc::new([]),
            effective_from,
        },
    }
}

fn assert_counts(row: &QueryVariables, key: &str, id: &str, counts: &[(&str, i64)]) {
    assert_eq!(row.get(key), Some(&VariableValue::from(json!(id))));
    for (name, expected) in counts {
        assert_eq!(
            row.get(*name),
            Some(&VariableValue::from(json!(*expected))),
            "{name}"
        );
    }
}

fn assert_aggregate_transition(
    changes: &[QueryPartEvaluationContext],
    key: &str,
    id: &str,
    before_counts: &[(&str, i64)],
    after_counts: &[(&str, i64)],
) {
    assert_eq!(changes.len(), 1);
    assert!(matches!(
        &changes[0],
        QueryPartEvaluationContext::Aggregation {
            before: Some(before),
            after,
            ..
        } if {
            assert_counts(before, key, id, before_counts);
            assert_counts(after, key, id, after_counts);
            true
        }
    ));
}

#[tokio::test]
async fn single_optional_child_transitions_between_default_and_real_match() {
    let mut subject = MaterializedQuery::new(SINGLE_OPTIONAL_QUERY).await;
    let counts = |count| [("memberCount", count)];

    subject.process(node("engineering", "Team", 1)).await;
    assert_counts(subject.only_row(), "team", "engineering", &counts(0));
    assert!(subject.process(node("alice", "Person", 2)).await.is_empty());

    let added = subject
        .process(relation(
            "alice-member",
            "MEMBER_OF",
            3,
            "alice",
            "engineering",
        ))
        .await;
    assert_aggregate_transition(&added, "team", "engineering", &counts(0), &counts(1));

    let removed = subject.process(delete("alice-member", 4)).await;
    assert_aggregate_transition(&removed, "team", "engineering", &counts(1), &counts(0));
}

#[tokio::test]
async fn chained_optional_aggregates_track_full_incremental_lifecycle() {
    let mut subject = MaterializedQuery::new(CHAINED_OPTIONAL_QUERY).await;
    let counts = |member, report, approval| {
        [
            ("memberCount", member),
            ("reportCount", report),
            ("approvalCount", approval),
        ]
    };

    subject.process(node("engineering", "Team", 1)).await;
    assert_counts(subject.only_row(), "team", "engineering", &counts(0, 0, 0));
    assert!(subject.process(node("alice", "Person", 2)).await.is_empty());

    let added = subject
        .process(relation(
            "alice-member",
            "MEMBER_OF",
            3,
            "alice",
            "engineering",
        ))
        .await;
    assert_aggregate_transition(
        &added,
        "team",
        "engineering",
        &counts(0, 0, 0),
        &counts(1, 0, 0),
    );
    assert_counts(subject.only_row(), "team", "engineering", &counts(1, 0, 0));

    let removed = subject.process(delete("alice-member", 4)).await;
    assert_aggregate_transition(
        &removed,
        "team",
        "engineering",
        &counts(1, 0, 0),
        &counts(0, 0, 0),
    );

    let readded = subject
        .process(relation(
            "alice-member",
            "MEMBER_OF",
            5,
            "alice",
            "engineering",
        ))
        .await;
    assert_aggregate_transition(
        &readded,
        "team",
        "engineering",
        &counts(0, 0, 0),
        &counts(1, 0, 0),
    );

    assert!(subject
        .process(node("alice-report", "Report", 6))
        .await
        .is_empty());
    let report_added = subject
        .process(relation(
            "alice-report-for",
            "REPORT_FOR",
            7,
            "alice-report",
            "alice",
        ))
        .await;
    assert_aggregate_transition(
        &report_added,
        "team",
        "engineering",
        &counts(1, 0, 0),
        &counts(1, 1, 0),
    );

    assert!(subject
        .process(node("alice-approval", "Approval", 8))
        .await
        .is_empty());
    let approval_added = subject
        .process(relation(
            "alice-approved",
            "APPROVES",
            9,
            "alice-approval",
            "alice-report",
        ))
        .await;
    assert_aggregate_transition(
        &approval_added,
        "team",
        "engineering",
        &counts(1, 1, 0),
        &counts(1, 1, 1),
    );

    assert!(subject.process(node("bob", "Person", 10)).await.is_empty());
    let bob_added = subject
        .process(relation(
            "bob-member",
            "MEMBER_OF",
            11,
            "bob",
            "engineering",
        ))
        .await;
    assert_aggregate_transition(
        &bob_added,
        "team",
        "engineering",
        &counts(1, 1, 1),
        &counts(2, 1, 1),
    );

    let approval_removed = subject.process(delete("alice-approved", 12)).await;
    assert_aggregate_transition(
        &approval_removed,
        "team",
        "engineering",
        &counts(2, 1, 1),
        &counts(2, 1, 0),
    );
    assert!(subject
        .process(delete("alice-approval", 13))
        .await
        .is_empty());

    let report_removed = subject.process(delete("alice-report-for", 14)).await;
    assert_aggregate_transition(
        &report_removed,
        "team",
        "engineering",
        &counts(2, 1, 0),
        &counts(2, 0, 0),
    );
    assert!(subject.process(delete("alice-report", 15)).await.is_empty());

    let alice_removed = subject.process(delete("alice-member", 16)).await;
    assert_aggregate_transition(
        &alice_removed,
        "team",
        "engineering",
        &counts(2, 0, 0),
        &counts(1, 0, 0),
    );
    assert!(subject.process(delete("alice", 17)).await.is_empty());

    let bob_removed = subject.process(delete("bob-member", 18)).await;
    assert_aggregate_transition(
        &bob_removed,
        "team",
        "engineering",
        &counts(1, 0, 0),
        &counts(0, 0, 0),
    );
    assert!(subject.process(delete("bob", 19)).await.is_empty());
    assert_counts(subject.only_row(), "team", "engineering", &counts(0, 0, 0));
}

#[tokio::test]
async fn chained_optional_aggregates_converge_across_event_order() {
    let mut nodes_first = MaterializedQuery::new(CHAINED_OPTIONAL_QUERY).await;
    let mut edges_first = MaterializedQuery::new(CHAINED_OPTIONAL_QUERY).await;

    for change in [
        node("engineering", "Team", 1),
        node("alice", "Person", 2),
        node("alice-report", "Report", 3),
        node("alice-approval", "Approval", 4),
        relation("alice-member", "MEMBER_OF", 5, "alice", "engineering"),
        relation("alice-report-for", "REPORT_FOR", 6, "alice-report", "alice"),
        relation(
            "alice-approved",
            "APPROVES",
            7,
            "alice-approval",
            "alice-report",
        ),
    ] {
        nodes_first.process(change).await;
    }

    for change in [
        node("engineering", "Team", 1),
        relation("alice-member", "MEMBER_OF", 2, "alice", "engineering"),
        relation("alice-report-for", "REPORT_FOR", 3, "alice-report", "alice"),
        relation(
            "alice-approved",
            "APPROVES",
            4,
            "alice-approval",
            "alice-report",
        ),
        node("alice-approval", "Approval", 5),
        node("alice-report", "Report", 6),
        node("alice", "Person", 7),
    ] {
        edges_first.process(change).await;
    }

    assert_eq!(nodes_first.rows, edges_first.rows);
    assert_counts(
        nodes_first.only_row(),
        "team",
        "engineering",
        &[("memberCount", 1), ("reportCount", 1), ("approvalCount", 1)],
    );
}

#[tokio::test]
async fn branched_optional_aggregates_default_all_dependent_paths_once() {
    let mut subject = MaterializedQuery::new(BRANCHED_OPTIONAL_QUERY).await;
    let counts = |member, report, badge| {
        [
            ("memberCount", member),
            ("reportCount", report),
            ("badgeCount", badge),
        ]
    };

    subject.process(node("engineering", "Team", 1)).await;
    subject.process(node("alice", "Person", 2)).await;
    subject.process(node("alice-report", "Report", 3)).await;
    subject.process(node("alice-badge", "Badge", 4)).await;
    subject
        .process(relation(
            "alice-report-for",
            "REPORT_FOR",
            5,
            "alice-report",
            "alice",
        ))
        .await;
    subject
        .process(relation(
            "alice-badge-assignment",
            "ASSIGNED_TO",
            6,
            "alice-badge",
            "alice",
        ))
        .await;
    assert_counts(subject.only_row(), "team", "engineering", &counts(0, 0, 0));

    let membership_added = subject
        .process(relation(
            "alice-member",
            "MEMBER_OF",
            7,
            "alice",
            "engineering",
        ))
        .await;
    assert_aggregate_transition(
        &membership_added,
        "team",
        "engineering",
        &counts(0, 0, 0),
        &counts(1, 1, 1),
    );

    let membership_removed = subject.process(delete("alice-member", 8)).await;
    assert_aggregate_transition(
        &membership_removed,
        "team",
        "engineering",
        &counts(1, 1, 1),
        &counts(0, 0, 0),
    );
    assert_counts(subject.only_row(), "team", "engineering", &counts(0, 0, 0));
}

#[tokio::test]
async fn definition_rooted_optional_chain_tracks_forward_and_reverse_transitions() {
    let mut subject = MaterializedQuery::new(DEFINITION_ROOTED_QUERY).await;
    let counts = |child, result, evaluation| {
        [
            ("childCount", child),
            ("resultCount", result),
            ("evaluationCount", evaluation),
        ]
    };

    subject.process(node("slot-a", "Slot", 1)).await;
    assert_counts(subject.only_row(), "slot", "slot-a", &counts(0, 0, 0));
    subject.process(node("runtime-a", "RuntimeChild", 2)).await;
    let child = subject
        .process(relation(
            "runtime-in-slot",
            "RUNS_IN",
            3,
            "runtime-a",
            "slot-a",
        ))
        .await;
    assert_aggregate_transition(&child, "slot", "slot-a", &counts(0, 0, 0), &counts(1, 0, 0));

    subject.process(node("result-a", "ChildResult", 4)).await;
    let result = subject
        .process(relation(
            "result-for-runtime",
            "RESULT_FOR",
            5,
            "result-a",
            "runtime-a",
        ))
        .await;
    assert_aggregate_transition(
        &result,
        "slot",
        "slot-a",
        &counts(1, 0, 0),
        &counts(1, 1, 0),
    );

    subject
        .process(node("evaluation-a", "ChildEvaluation", 6))
        .await;
    let evaluation = subject
        .process(relation(
            "evaluation-for-result",
            "EVALUATES",
            7,
            "evaluation-a",
            "result-a",
        ))
        .await;
    assert_aggregate_transition(
        &evaluation,
        "slot",
        "slot-a",
        &counts(1, 1, 0),
        &counts(1, 1, 1),
    );

    let evaluation_removed = subject.process(delete("evaluation-for-result", 8)).await;
    assert_aggregate_transition(
        &evaluation_removed,
        "slot",
        "slot-a",
        &counts(1, 1, 1),
        &counts(1, 1, 0),
    );
    let result_removed = subject.process(delete("result-for-runtime", 9)).await;
    assert_aggregate_transition(
        &result_removed,
        "slot",
        "slot-a",
        &counts(1, 1, 0),
        &counts(1, 0, 0),
    );
    let child_removed = subject.process(delete("runtime-in-slot", 10)).await;
    assert_aggregate_transition(
        &child_removed,
        "slot",
        "slot-a",
        &counts(1, 0, 0),
        &counts(0, 0, 0),
    );
    assert_counts(subject.only_row(), "slot", "slot-a", &counts(0, 0, 0));
}

#[tokio::test]
async fn nested_optional_aggregate_tracks_first_member_and_report_completion() {
    let mut subject = MaterializedQuery::new(NESTED_OPTIONAL_AGGREGATE_QUERY).await;
    let counts = |member, completed| [("memberCount", member), ("completedMemberCount", completed)];

    assert!(subject
        .process(node("engineering", "Team", 1))
        .await
        .is_empty());
    assert!(subject.process(node("alice", "Person", 2)).await.is_empty());

    let membership_added = subject
        .process(relation(
            "alice-member",
            "MEMBER_OF",
            3,
            "alice",
            "engineering",
        ))
        .await;
    assert_eq!(membership_added.len(), 1);
    assert!(matches!(
        &membership_added[0],
        QueryPartEvaluationContext::Adding { after, .. } if {
            assert_counts(after, "team", "engineering", &counts(1, 0));
            true
        }
    ));
    let team_signature = membership_added[0].row_signature();
    assert_counts(subject.only_row(), "team", "engineering", &counts(1, 0));

    let membership_removed = subject.process(delete("alice-member", 4)).await;
    assert_eq!(membership_removed.len(), 1);
    assert!(matches!(
        &membership_removed[0],
        QueryPartEvaluationContext::Removing {
            before,
            row_signature,
        } if {
            assert_counts(before, "team", "engineering", &counts(1, 0));
            *row_signature == team_signature
        }
    ));
    assert!(subject.rows.is_empty());

    let membership_readded = subject
        .process(relation(
            "alice-member",
            "MEMBER_OF",
            5,
            "alice",
            "engineering",
        ))
        .await;
    assert_eq!(membership_readded.len(), 1);
    assert!(matches!(
        &membership_readded[0],
        QueryPartEvaluationContext::Adding {
            after,
            row_signature,
        } if {
            assert_counts(after, "team", "engineering", &counts(1, 0));
            *row_signature == team_signature
        }
    ));

    assert!(subject
        .process(node("alice-report", "Report", 6))
        .await
        .is_empty());
    let report_added = subject
        .process(relation(
            "alice-report-for",
            "REPORT_FOR",
            7,
            "alice-report",
            "alice",
        ))
        .await;
    assert_eq!(report_added.len(), 1);
    assert!(matches!(
        &report_added[0],
        QueryPartEvaluationContext::Removing {
            before,
            row_signature,
        } if {
            assert_counts(before, "team", "engineering", &counts(1, 0));
            *row_signature == team_signature
        }
    ));
    assert!(subject.rows.is_empty());

    let report_removed = subject.process(delete("alice-report", 8)).await;
    assert_eq!(report_removed.len(), 1);
    assert!(matches!(
        &report_removed[0],
        QueryPartEvaluationContext::Adding {
            after,
            row_signature,
        } if {
            assert_counts(after, "team", "engineering", &counts(1, 0));
            *row_signature == team_signature
        }
    ));
    assert_counts(subject.only_row(), "team", "engineering", &counts(1, 0));
}

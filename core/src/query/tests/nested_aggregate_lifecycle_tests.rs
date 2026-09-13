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
        functions::{Count, Function, FunctionRegistry, ValueAccumulator},
        variable_value::VariableValue,
    },
    in_memory_index::{
        in_memory_element_index::InMemoryElementIndex, in_memory_future_queue::InMemoryFutureQueue,
        in_memory_result_index::InMemoryResultIndex,
    },
    interface::{AccumulatorIndex, ResultIndex, ResultKey, ResultOwner},
    models::{Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange},
    query::{ContinuousQuery, QueryBuilder},
};

const TEAM_SEAT_QUERY: &str = "
MATCH (team:Team)-[:HAS_SEAT]->(seat:Seat)
OPTIONAL MATCH (person:Person)-[:OCCUPIES]->(seat)
WITH team, seat, count(person) AS personCount
WITH team,
  count(CASE WHEN personCount = 1 THEN 1 ELSE null END) AS filledSeatCount
WHERE filledSeatCount <> team.declaredCount
RETURN team.id AS teamId, filledSeatCount
";

const LATE_REPORT_QUERY: &str = "
MATCH (team:Team)-[:HAS_SEAT]->(seat:Seat)
OPTIONAL MATCH (person:Person)-[:OCCUPIES]->(seat)
OPTIONAL MATCH (report:Report)-[:REPORT_FOR]->(person)
OPTIONAL MATCH (review:Review)-[:REVIEWS]->(report)
WITH team, seat, person, report, review,
  count(team) AS basePathCount,
  count(CASE WHEN person IS NOT NULL THEN 1 ELSE null END) AS structuralPathCount
WITH team, seat, person,
  count(CASE WHEN basePathCount <> 1 THEN 1 ELSE null END) AS invalidPathCount,
  count(CASE WHEN structuralPathCount = 1 AND report IS NOT NULL
    THEN 1 ELSE null END) AS reportCount,
  count(CASE WHEN structuralPathCount = 1 AND review IS NOT NULL
    THEN 1 ELSE null END) AS reviewCount,
  count(CASE WHEN structuralPathCount = 1
    AND NOT person.isActive
    AND review.reportId = report.id
    THEN 1 ELSE null END) AS validReportCount
WITH team, seat,
  count(CASE WHEN invalidPathCount = 0
    AND reportCount = 1
    AND reviewCount = 1
    AND validReportCount = 1
    THEN 1 ELSE null END) AS reviewedSeatCount
WITH team,
  count(CASE WHEN reviewedSeatCount = 1 THEN 1 ELSE null END) AS completedPersonCount
WHERE completedPersonCount = team.declaredCount
RETURN team.id AS teamId, completedPersonCount
";

struct MaterializedQuery {
    query: ContinuousQuery,
    rows: HashMap<u64, QueryVariables>,
}

impl MaterializedQuery {
    async fn new(query_text: &str) -> Self {
        let element_index = Arc::new(InMemoryElementIndex::new());
        let result_index = Arc::new(InMemoryResultIndex::new());
        Self::with_indexes(query_text, element_index, result_index).await
    }

    async fn with_indexes(
        query_text: &str,
        element_index: Arc<InMemoryElementIndex>,
        result_index: Arc<InMemoryResultIndex>,
    ) -> Self {
        let functions = Arc::new(FunctionRegistry::new());
        functions.register_function("count", Function::Aggregating(Arc::new(Count {})));
        let parser = Arc::new(CypherParser::new(functions.clone()));
        let query = QueryBuilder::new(query_text, parser)
            .with_function_registry(functions)
            .with_element_index(element_index.clone())
            .with_archive_index(element_index)
            .with_result_index(result_index)
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

    fn integer(&self, field: &str) -> i64 {
        assert_eq!(self.rows.len(), 1);
        match self.rows.values().next().unwrap().get(field) {
            Some(VariableValue::Integer(value)) => value.as_i64().unwrap(),
            other => panic!("expected integer {field}, got {other:?}"),
        }
    }
}

fn node(id: &str, label: &str, effective_from: u64, properties: serde_json::Value) -> SourceChange {
    SourceChange::Insert {
        element: Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new("test", id),
                labels: Arc::new([Arc::from(label)]),
                effective_from,
            },
            properties: ElementPropertyMap::from(properties),
        },
    }
}

fn update_node(
    id: &str,
    label: &str,
    effective_from: u64,
    properties: serde_json::Value,
) -> SourceChange {
    SourceChange::Update {
        element: Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new("test", id),
                labels: Arc::new([Arc::from(label)]),
                effective_from,
            },
            properties: ElementPropertyMap::from(properties),
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

async fn insert_team_and_seats(subject: &mut MaterializedQuery) -> Vec<QueryPartEvaluationContext> {
    let mut results = subject
        .process(node(
            "team",
            "Team",
            1,
            json!({"id": "team", "declaredCount": 2}),
        ))
        .await;
    results.extend(
        subject
            .process(node("seat-a", "Seat", 2, json!({"id": "a"})))
            .await,
    );
    results.extend(
        subject
            .process(relation("team-seat-a", "HAS_SEAT", 3, "team", "seat-a"))
            .await,
    );
    results.extend(
        subject
            .process(node("seat-b", "Seat", 4, json!({"id": "b"})))
            .await,
    );
    results.extend(
        subject
            .process(relation("team-seat-b", "HAS_SEAT", 5, "team", "seat-b"))
            .await,
    );
    results
}

async fn insert_person(
    subject: &mut MaterializedQuery,
    id: &str,
    seat: &str,
    effective_from: u64,
) -> Vec<QueryPartEvaluationContext> {
    subject
        .process(node(
            id,
            "Person",
            effective_from,
            json!({"id": id, "isActive": true}),
        ))
        .await;
    subject
        .process(relation(
            &format!("{id}-{seat}"),
            "OCCUPIES",
            effective_from + 1,
            id,
            seat,
        ))
        .await
}

#[tokio::test]
async fn zero_inner_groups_emit_add_then_update_and_track_full_lifecycle() {
    let mut subject = MaterializedQuery::new(TEAM_SEAT_QUERY).await;
    let initial = insert_team_and_seats(&mut subject).await;

    assert!(matches!(
        initial.as_slice(),
        [QueryPartEvaluationContext::Adding { .. }]
    ));
    assert_eq!(subject.integer("filledSeatCount"), 0);

    let first_person = insert_person(&mut subject, "person-a", "seat-a", 6).await;
    assert!(matches!(
        first_person.as_slice(),
        [QueryPartEvaluationContext::Updating { .. }]
    ));
    assert_eq!(subject.integer("filledSeatCount"), 1);

    let second_person = insert_person(&mut subject, "person-b", "seat-b", 8).await;
    assert!(matches!(
        second_person.as_slice(),
        [QueryPartEvaluationContext::Removing { .. }]
    ));
    assert!(subject.rows.is_empty());

    let remove_person = subject.process(delete("person-b-seat-b", 10)).await;
    assert!(matches!(
        remove_person.as_slice(),
        [QueryPartEvaluationContext::Adding { .. }]
    ));
    assert_eq!(subject.integer("filledSeatCount"), 1);

    subject.process(delete("person-a-seat-a", 11)).await;
    assert_eq!(subject.integer("filledSeatCount"), 0);

    subject.process(delete("team-seat-b", 12)).await;
    assert_eq!(
        subject.integer("filledSeatCount"),
        0,
        "removing a non-last zero contributor must retain the outer group"
    );

    let remove_last = subject.process(delete("team-seat-a", 13)).await;
    assert!(matches!(
        remove_last.as_slice(),
        [QueryPartEvaluationContext::Removing { .. }]
    ));
    assert!(subject.rows.is_empty());

    let readd = subject
        .process(relation("team-seat-a", "HAS_SEAT", 14, "team", "seat-a"))
        .await;
    assert!(matches!(
        readd.as_slice(),
        [QueryPartEvaluationContext::Adding { .. }]
    ));
    assert_eq!(subject.integer("filledSeatCount"), 0);
}

#[tokio::test]
async fn retained_cardinality_survives_query_restart() {
    let element_index = Arc::new(InMemoryElementIndex::new());
    let result_index = Arc::new(InMemoryResultIndex::new());
    let mut subject = MaterializedQuery::with_indexes(
        TEAM_SEAT_QUERY,
        element_index.clone(),
        result_index.clone(),
    )
    .await;
    insert_team_and_seats(&mut subject).await;
    assert_eq!(subject.integer("filledSeatCount"), 0);
    drop(subject);

    let mut restarted =
        MaterializedQuery::with_indexes(TEAM_SEAT_QUERY, element_index, result_index).await;
    let first_person = insert_person(&mut restarted, "person-a", "seat-a", 6).await;
    assert!(matches!(
        first_person.as_slice(),
        [QueryPartEvaluationContext::Updating { .. }]
    ));
    assert_eq!(restarted.integer("filledSeatCount"), 1);
}

async fn insert_team_and_person(subject: &mut MaterializedQuery) {
    for change in [
        node("team", "Team", 1, json!({"id": "team", "declaredCount": 1})),
        node("seat", "Seat", 2, json!({"id": "seat"})),
        relation("team-seat", "HAS_SEAT", 3, "team", "seat"),
        node(
            "person",
            "Person",
            4,
            json!({"id": "person", "isActive": true}),
        ),
        relation("person-seat", "OCCUPIES", 5, "person", "seat"),
    ] {
        subject.process(change).await;
    }
}

async fn insert_report_and_review(subject: &mut MaterializedQuery, effective_from: u64) {
    for change in [
        node("report", "Report", effective_from, json!({"id": "report"})),
        relation(
            "report-person",
            "REPORT_FOR",
            effective_from + 1,
            "report",
            "person",
        ),
        node(
            "review",
            "Review",
            effective_from + 2,
            json!({"id": "review", "reportId": "report"}),
        ),
        relation(
            "review-report",
            "REVIEWS",
            effective_from + 3,
            "review",
            "report",
        ),
    ] {
        subject.process(change).await;
    }
}

#[tokio::test]
async fn closed_person_converges_when_report_evidence_replays_later() {
    let mut evidence_late = MaterializedQuery::new(LATE_REPORT_QUERY).await;
    insert_team_and_person(&mut evidence_late).await;
    evidence_late
        .process(update_node(
            "person",
            "Person",
            6,
            json!({"id": "person", "isActive": false}),
        ))
        .await;
    insert_report_and_review(&mut evidence_late, 7).await;

    assert_eq!(evidence_late.integer("completedPersonCount"), 1);

    let mut close_last = MaterializedQuery::new(LATE_REPORT_QUERY).await;
    insert_team_and_person(&mut close_last).await;
    insert_report_and_review(&mut close_last, 6).await;
    close_last
        .process(update_node(
            "person",
            "Person",
            10,
            json!({"id": "person", "isActive": false}),
        ))
        .await;

    assert_eq!(close_last.rows, evidence_late.rows);
}

#[tokio::test]
async fn processing_rejects_markerless_retained_accumulators() {
    let element_index = Arc::new(InMemoryElementIndex::new());
    let result_index = Arc::new(InMemoryResultIndex::new());
    result_index
        .set(
            ResultKey::InputHash(42),
            ResultOwner::Function(1),
            Some(ValueAccumulator::Count { value: 1 }),
        )
        .await
        .unwrap();

    let functions = Arc::new(FunctionRegistry::new());
    functions.register_function("count", Function::Aggregating(Arc::new(Count {})));
    let parser = Arc::new(CypherParser::new(functions.clone()));
    let query = QueryBuilder::new(TEAM_SEAT_QUERY, parser)
        .with_function_registry(functions)
        .with_element_index(element_index.clone())
        .with_archive_index(element_index)
        .with_result_index(result_index)
        .with_future_queue(Arc::new(InMemoryFutureQueue::new()))
        .build()
        .await;

    let error = query
        .process_source_change(node(
            "team",
            "Team",
            1,
            json!({"id": "team", "declaredCount": 1}),
        ))
        .await
        .unwrap_err();
    assert!(
        matches!(error, crate::evaluation::EvaluationError::IndexError(crate::interface::IndexError::Other(source))
            if source.to_string().contains("clear and replay"))
    );
}

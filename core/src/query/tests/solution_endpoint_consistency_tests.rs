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

const AGGREGATE_QUERY: &str = "
MATCH
  (scope:Scope)-[:USES_GRAPH]->
  (graph:Graph)-[:HAS_VERTEX]->
  (root:Vertex)<-[:ROOT_OF]-
  (anchor:Item)-[:IN_SCOPE]->(scope)
MATCH
  (root)-[:BRANCHES_TO]->(leaf:Vertex)
OPTIONAL MATCH
  (member:Item)-[:AT_VERTEX]->(leaf)
OPTIONAL MATCH (member)-[link:LINKS_TO]->(anchor)
WITH scope, anchor, root, leaf,
  count(CASE
    WHEN member.scopeId = scope.id
      AND member.vertexId = leaf.id
      AND link IS NOT NULL
    THEN 1 ELSE null END) AS linkCount
WHERE anchor.scopeId = scope.id
  AND anchor.vertexId = root.id
RETURN scope.id AS scope, anchor.id AS anchor,
  leaf.id AS leaf, linkCount
";

const RAW_QUERY: &str = "
MATCH
  (scope:Scope)-[:USES_GRAPH]->
  (graph:Graph)-[:HAS_VERTEX]->
  (root:Vertex)<-[:ROOT_OF]-
  (anchor:Item)-[:IN_SCOPE]->(scope)
MATCH
  (root)-[branch:BRANCHES_TO]->(leaf:Vertex)
OPTIONAL MATCH
  (member:Item)-[:AT_VERTEX]->(leaf)
OPTIONAL MATCH (member)-[link:LINKS_TO]->(anchor)
WHERE anchor.scopeId = scope.id
  AND anchor.vertexId = root.id
  AND member.scopeId = scope.id
  AND member.vertexId = leaf.id
  AND link IS NOT NULL
RETURN scope.id AS scope, anchor.id AS anchor,
  leaf.id AS leaf, member.id AS member,
  branch.id AS branch, link.id AS link
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
            properties: ElementPropertyMap::from(json!({"id": id})),
        },
    }
}

fn graph_events() -> Vec<SourceChange> {
    vec![
        node("graph", "Graph", 1, json!({"id": "graph"})),
        node("root", "Vertex", 2, json!({"id": "root"})),
        relation("graph-root", "HAS_VERTEX", 3, "graph", "root"),
        node("leaf-a", "Vertex", 4, json!({"id": "leaf-a"})),
        relation("graph-leaf-a", "HAS_VERTEX", 5, "graph", "leaf-a"),
        relation("branch-a", "BRANCHES_TO", 6, "root", "leaf-a"),
        node("leaf-b", "Vertex", 7, json!({"id": "leaf-b"})),
        relation("graph-leaf-b", "HAS_VERTEX", 8, "graph", "leaf-b"),
        relation("branch-b", "BRANCHES_TO", 9, "root", "leaf-b"),
    ]
}

fn scope_events(start_time: u64) -> Vec<SourceChange> {
    vec![
        node("scope", "Scope", start_time, json!({"id": "scope"})),
        relation(
            "scope-graph",
            "USES_GRAPH",
            start_time + 1,
            "scope",
            "graph",
        ),
        node(
            "anchor",
            "Item",
            start_time + 2,
            json!({
                "id": "anchor",
                "scopeId": "scope",
                "vertexId": "root"
            }),
        ),
        relation("anchor-root", "ROOT_OF", start_time + 3, "anchor", "root"),
        relation(
            "anchor-scope",
            "IN_SCOPE",
            start_time + 4,
            "anchor",
            "scope",
        ),
        node(
            "member",
            "Item",
            start_time + 5,
            json!({
                "id": "member",
                "scopeId": "scope",
                "vertexId": "leaf-a"
            }),
        ),
        relation(
            "member-leaf",
            "AT_VERTEX",
            start_time + 6,
            "member",
            "leaf-a",
        ),
    ]
}

fn link(effective_from: u64) -> SourceChange {
    relation(
        "member-anchor",
        "LINKS_TO",
        effective_from,
        "member",
        "anchor",
    )
}

async fn process_all(subject: &mut MaterializedQuery, changes: Vec<SourceChange>) {
    for change in changes {
        subject.process(change).await;
    }
}

fn string_value<'a>(row: &'a QueryVariables, key: &str) -> &'a str {
    match row.get(key) {
        Some(VariableValue::String(value)) => value,
        other => panic!("expected string {key}, got {other:?}"),
    }
}

fn integer_value(row: &QueryVariables, key: &str) -> i64 {
    match row.get(key) {
        Some(VariableValue::Integer(value)) => value.as_i64().unwrap(),
        other => panic!("expected integer {key}, got {other:?}"),
    }
}

fn link_count(subject: &MaterializedQuery, leaf: &str) -> i64 {
    let row = subject
        .rows
        .values()
        .find(|row| row.get("leaf") == Some(&VariableValue::from(leaf)))
        .unwrap_or_else(|| panic!("missing row for {leaf}"));
    integer_value(row, "linkCount")
}

async fn build_single_scope(query: &str) -> MaterializedQuery {
    let mut subject = MaterializedQuery::new(query).await;
    process_all(&mut subject, graph_events()).await;
    process_all(&mut subject, scope_events(10)).await;
    subject
}

#[tokio::test]
async fn raw_solution_cardinality_excludes_cross_paired_relationships() {
    for _ in 0..32 {
        let mut subject = build_single_scope(RAW_QUERY).await;
        let changes = subject.process(link(18)).await;

        assert_eq!(changes.len(), 1);
        assert_eq!(subject.rows.len(), 1);
        let row = subject.rows.values().next().unwrap();
        assert_eq!(string_value(row, "scope"), "scope");
        assert_eq!(string_value(row, "anchor"), "anchor");
        assert_eq!(string_value(row, "leaf"), "leaf-a");
        assert_eq!(string_value(row, "member"), "member");
        assert_eq!(string_value(row, "branch"), "branch-a");
        assert_eq!(string_value(row, "link"), "member-anchor");
    }
}

#[tokio::test]
async fn aggregate_preserves_valid_matches_and_optional_defaults() {
    let mut subject = build_single_scope(AGGREGATE_QUERY).await;
    assert_eq!(subject.rows.len(), 2);
    assert_eq!(link_count(&subject, "leaf-a"), 0);
    assert_eq!(link_count(&subject, "leaf-b"), 0);

    let changes = subject.process(link(18)).await;
    assert_eq!(changes.len(), 1);
    assert_eq!(subject.rows.len(), 2);
    assert_eq!(link_count(&subject, "leaf-a"), 1);
    assert_eq!(link_count(&subject, "leaf-b"), 0);
}

#[tokio::test]
async fn converging_path_results_are_independent_of_event_order() {
    let mut interleaved = MaterializedQuery::new(AGGREGATE_QUERY).await;
    let mut nodes_first = MaterializedQuery::new(AGGREGATE_QUERY).await;
    let mut events = graph_events();
    events.extend(scope_events(10));
    events.push(link(18));

    process_all(&mut interleaved, events.clone()).await;

    let (mut relations, nodes): (Vec<_>, Vec<_>) = events.into_iter().partition(|change| {
        matches!(
            change,
            SourceChange::Insert {
                element: Element::Relation { .. }
            }
        )
    });
    relations.reverse();
    process_all(
        &mut nodes_first,
        nodes.into_iter().chain(relations).collect(),
    )
    .await;

    assert_eq!(interleaved.rows, nodes_first.rows);
    assert_eq!(interleaved.rows.len(), 2);
    assert_eq!(link_count(&interleaved, "leaf-a"), 1);
    assert_eq!(link_count(&interleaved, "leaf-b"), 0);
}

#[tokio::test]
async fn solver_preserves_direction_self_loops_and_reused_relationships() {
    let events = vec![
        node("a", "Point", 1, json!({"id": "a"})),
        node("b", "Point", 2, json!({"id": "b"})),
        relation("edge", "LINK", 3, "a", "b"),
        relation("loop", "LINK", 4, "a", "a"),
    ];

    let mut incoming = MaterializedQuery::new(
        "MATCH (end:Point)<-[link:LINK]-(start:Point)
         WHERE link.id = 'edge'
         RETURN start.id AS start, link.id AS link, end.id AS end",
    )
    .await;
    process_all(&mut incoming, events.clone()).await;
    assert_eq!(incoming.rows.len(), 1);
    let row = incoming.rows.values().next().unwrap();
    assert_eq!(string_value(row, "start"), "a");
    assert_eq!(string_value(row, "link"), "edge");
    assert_eq!(string_value(row, "end"), "b");

    let mut either = MaterializedQuery::new(
        "MATCH (left:Point)-[link:LINK]-(right:Point)
         WHERE link.id = 'edge'
         RETURN left.id AS left, link.id AS link, right.id AS right",
    )
    .await;
    process_all(&mut either, events.clone()).await;
    assert_eq!(either.rows.len(), 2);
    let orientations = either
        .rows
        .values()
        .map(|row| (string_value(row, "left"), string_value(row, "right")))
        .collect::<Vec<_>>();
    assert!(orientations.contains(&("a", "b")));
    assert!(orientations.contains(&("b", "a")));

    let mut self_loop = MaterializedQuery::new(
        "MATCH (point:Point)-[link:LINK]->(point)
         RETURN point.id AS point, link.id AS link",
    )
    .await;
    process_all(&mut self_loop, events.clone()).await;
    assert_eq!(self_loop.rows.len(), 1);
    let row = self_loop.rows.values().next().unwrap();
    assert_eq!(string_value(row, "point"), "a");
    assert_eq!(string_value(row, "link"), "loop");

    let mut reused_relationship = MaterializedQuery::new(
        "MATCH (start:Point)-[link:LINK]->(end:Point)
         MATCH (start)-[link]->(other:Point)
         WHERE link.id = 'edge'
         RETURN start.id AS start, link.id AS link,
           end.id AS end, other.id AS other",
    )
    .await;
    process_all(&mut reused_relationship, events).await;
    assert_eq!(reused_relationship.rows.len(), 1);
    let row = reused_relationship.rows.values().next().unwrap();
    assert_eq!(string_value(row, "start"), "a");
    assert_eq!(string_value(row, "link"), "edge");
    assert_eq!(string_value(row, "end"), "b");
    assert_eq!(string_value(row, "other"), "b");
}

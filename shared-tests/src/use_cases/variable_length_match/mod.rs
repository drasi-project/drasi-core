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

#![allow(clippy::unwrap_used)]

use std::{collections::BTreeMap, sync::Arc};

use drasi_core::{
    evaluation::{
        context::{QueryPartEvaluationContext, QueryVariables},
        functions::FunctionRegistry,
        variable_value::VariableValue,
    },
    models::{Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange},
    query::{ContinuousQuery, QueryBuilder},
};
use drasi_functions_cypher::CypherFunctionSet;
use drasi_query_cypher::CypherParser;
use drasi_query_gql::GQLParser;
use serde_json::{json, Value};

use crate::QueryTestConfig;

async fn build_query(config: &(impl QueryTestConfig + Send), text: &str) -> ContinuousQuery {
    let registry = Arc::new(FunctionRegistry::new()).with_cypher_function_set();
    let parser = Arc::new(CypherParser::new(registry.clone()));
    config
        .config_query(QueryBuilder::new(text, parser).with_function_registry(registry))
        .await
        .try_build()
        .await
        .unwrap()
}

fn metadata(id: &str, label: &str, time: u64) -> ElementMetadata {
    ElementMetadata {
        reference: ElementReference::new("paths", id),
        labels: Arc::new([Arc::from(label)]),
        effective_from: time,
    }
}

fn node(id: &str, label: &str) -> SourceChange {
    SourceChange::Insert {
        element: Element::Node {
            metadata: metadata(id, label, 1),
            properties: ElementPropertyMap::from(json!({"id": id})),
        },
    }
}

fn relationship(id: &str, from: &str, to: &str) -> SourceChange {
    typed_relationship(id, from, to, "R")
}

fn typed_relationship(id: &str, from: &str, to: &str, label: &str) -> SourceChange {
    SourceChange::Insert {
        element: Element::Relation {
            metadata: metadata(id, label, 1),
            in_node: ElementReference::new("paths", from),
            out_node: ElementReference::new("paths", to),
            properties: ElementPropertyMap::from(json!({"id": id, "enabled": true})),
        },
    }
}

fn delete(id: &str) -> SourceChange {
    SourceChange::Delete {
        metadata: metadata(id, "", 3),
    }
}

fn update_relationship(id: &str, from: &str, to: &str, label: &str, enabled: bool) -> SourceChange {
    SourceChange::Update {
        element: Element::Relation {
            metadata: metadata(id, label, 2),
            in_node: ElementReference::new("paths", from),
            out_node: ElementReference::new("paths", to),
            properties: ElementPropertyMap::from(json!({"id": id, "enabled": enabled})),
        },
    }
}

#[derive(Default)]
struct Results {
    rows: BTreeMap<u64, QueryVariables>,
}

impl Results {
    async fn apply(&mut self, query: &ContinuousQuery, change: SourceChange) {
        self.record(query.process_source_change(change).await.unwrap());
    }

    fn record(&mut self, changes: Vec<QueryPartEvaluationContext>) {
        for result in changes {
            match result {
                QueryPartEvaluationContext::Adding {
                    after,
                    row_signature,
                } => {
                    assert!(
                        self.rows.insert(row_signature, after).is_none(),
                        "a match must be added only once"
                    );
                }
                QueryPartEvaluationContext::Updating {
                    before,
                    after,
                    row_signature,
                } => {
                    assert_eq!(self.rows.insert(row_signature, after), Some(before));
                }
                QueryPartEvaluationContext::Removing {
                    before,
                    row_signature,
                } => {
                    assert_eq!(self.rows.remove(&row_signature), Some(before));
                }
                other => panic!("expected a non-aggregating change, got {other:?}"),
            }
        }
    }

    async fn apply_all(
        &mut self,
        query: &ContinuousQuery,
        changes: impl IntoIterator<Item = SourceChange>,
    ) {
        for change in changes {
            self.apply(query, change).await;
        }
    }

    fn assert_rows(&self, expected: &[Value]) {
        let mut actual: Vec<_> = self
            .rows
            .values()
            .map(|row| {
                Value::Object(
                    row.iter()
                        .map(|(key, value)| (key.to_string(), value.clone().into()))
                        .collect(),
                )
                .to_string()
            })
            .collect();
        let mut expected: Vec<_> = expected.iter().map(Value::to_string).collect();
        actual.sort();
        expected.sort();
        assert_eq!(actual, expected);
    }
}

pub async fn exact_length(config: &(impl QueryTestConfig + Send)) {
    let query = build_query(
        config,
        "MATCH (a:Start)-[:R*2]->(b:End) RETURN a.id AS start, b.id AS end",
    )
    .await;

    for change in [
        node("a", "Start"),
        node("middle", "Transit"),
        node("b", "End"),
        relationship("direct", "a", "b"),
        relationship("first", "a", "middle"),
    ] {
        assert!(
            query
                .process_source_change(change)
                .await
                .unwrap()
                .is_empty(),
            "an exact two-hop pattern must not emit a one-hop match"
        );
    }

    let results = query
        .process_source_change(relationship("second", "middle", "b"))
        .await
        .unwrap();
    assert_eq!(results.len(), 1);
    match &results[0] {
        QueryPartEvaluationContext::Adding { after, .. } => {
            assert_eq!(after.get("start"), Some(&json!("a").into()));
            assert_eq!(after.get("end"), Some(&json!("b").into()));
        }
        other => panic!("expected one two-hop addition, got {other:?}"),
    }
}

pub async fn ranges_and_lists(config: &(impl QueryTestConfig + Send)) {
    let query = build_query(
        config,
        "MATCH (a:Start)-[rs:R*1..3]->(b:End)
         RETURN b.id AS end, size(rs) AS hops, [r IN rs | r.id] AS edges",
    )
    .await;
    let mut results = Results::default();
    results
        .apply_all(
            &query,
            [
                node("a", "Start"),
                node("b", "End"),
                node("c", "End"),
                node("d", "End"),
                node("e", "End"),
                relationship("ab", "a", "b"),
                relationship("bc", "b", "c"),
                relationship("cd", "c", "d"),
                relationship("de", "d", "e"),
            ],
        )
        .await;
    results.assert_rows(&[
        json!({"end": "b", "hops": 1, "edges": ["ab"]}),
        json!({"end": "c", "hops": 2, "edges": ["ab", "bc"]}),
        json!({"end": "d", "hops": 3, "edges": ["ab", "bc", "cd"]}),
    ]);
    results.apply(&query, delete("bc")).await;
    results.assert_rows(&[json!({"end": "b", "hops": 1, "edges": ["ab"]})]);

    let query = build_query(
        config,
        "MATCH (a:Start)-[rs:R*..2]->(b:End) RETURN b.id AS end, size(rs) AS hops",
    )
    .await;
    let mut results = Results::default();
    results
        .apply_all(
            &query,
            [
                node("a", "Start"),
                node("b", "End"),
                node("c", "End"),
                node("d", "End"),
                relationship("ab", "a", "b"),
                relationship("bc", "b", "c"),
                relationship("cd", "c", "d"),
            ],
        )
        .await;
    results.assert_rows(&[
        json!({"end": "b", "hops": 1}),
        json!({"end": "c", "hops": 2}),
    ]);
}

pub async fn zero_hops(config: &(impl QueryTestConfig + Send)) {
    let query = build_query(
        config,
        "MATCH (a:Node)-[rs:R*0..1]->(b:Node)
         RETURN a.id AS start, b.id AS end, size(rs) AS hops",
    )
    .await;
    let mut results = Results::default();
    results
        .apply_all(&query, [node("a", "Node"), node("b", "Node")])
        .await;
    results.assert_rows(&[
        json!({"start": "a", "end": "a", "hops": 0}),
        json!({"start": "b", "end": "b", "hops": 0}),
    ]);
    results.apply(&query, relationship("ab", "a", "b")).await;
    results.apply(&query, relationship("loop", "a", "a")).await;
    results.assert_rows(&[
        json!({"start": "a", "end": "a", "hops": 0}),
        json!({"start": "b", "end": "b", "hops": 0}),
        json!({"start": "a", "end": "b", "hops": 1}),
        json!({"start": "a", "end": "a", "hops": 1}),
    ]);
    results.apply(&query, delete("a")).await;
    results.assert_rows(&[json!({"start": "b", "end": "b", "hops": 0})]);

    let query = build_query(
        config,
        "MATCH (a:Start)-[rs:Missing*0]->(b:End) RETURN size(rs) AS hops",
    )
    .await;
    let mut results = Results::default();
    results
        .apply_all(&query, [node("a", "Start"), node("b", "End")])
        .await;
    results.assert_rows(&[]);
    let mut both = metadata("both", "Start", 1);
    both.labels = Arc::new([Arc::from("Start"), Arc::from("End")]);
    results
        .apply(
            &query,
            SourceChange::Insert {
                element: Element::Node {
                    metadata: both,
                    properties: ElementPropertyMap::new(),
                },
            },
        )
        .await;
    results.assert_rows(&[json!({"hops": 0})]);
}

pub async fn internal_node_changes(config: &(impl QueryTestConfig + Send)) {
    let query = build_query(
        config,
        "MATCH (a:Start)-[rs:R*2]->(b:End) RETURN b.id AS end, size(rs) AS hops",
    )
    .await;
    let mut results = Results::default();
    results
        .apply_all(
            &query,
            [
                node("a", "Start"),
                node("b", "End"),
                relationship("first", "a", "middle"),
                relationship("second", "middle", "b"),
            ],
        )
        .await;
    results.assert_rows(&[]);
    results.apply(&query, node("middle", "Transit")).await;
    results.assert_rows(&[json!({"end": "b", "hops": 2})]);
    let signature = *results.rows.keys().next().unwrap();
    results
        .apply(
            &query,
            SourceChange::Update {
                element: Element::Node {
                    metadata: metadata("middle", "OtherTransit", 2),
                    properties: ElementPropertyMap::from(json!({"id": "middle", "value": 2})),
                },
            },
        )
        .await;
    assert_eq!(
        results.rows.keys().copied().collect::<Vec<_>>(),
        vec![signature]
    );
    results.apply(&query, delete("middle")).await;
    results.assert_rows(&[]);
    results.apply(&query, node("middle", "Transit")).await;
    results.assert_rows(&[json!({"end": "b", "hops": 2})]);
    assert_eq!(
        results.rows.keys().copied().collect::<Vec<_>>(),
        vec![signature]
    );
}

pub async fn path_multiplicity(config: &(impl QueryTestConfig + Send)) {
    let query = build_query(config, "MATCH (a:Start)-[:R*2]->(b:End) RETURN b.id AS end").await;
    let mut results = Results::default();
    results
        .apply_all(
            &query,
            [
                node("a", "Start"),
                node("left", "Transit"),
                node("right", "Transit"),
                node("b", "End"),
                relationship("al", "a", "left"),
                relationship("lb", "left", "b"),
                relationship("ar", "a", "right"),
                relationship("rb", "right", "b"),
                relationship("al2", "a", "left"),
            ],
        )
        .await;
    results.assert_rows(&[
        json!({"end": "b"}),
        json!({"end": "b"}),
        json!({"end": "b"}),
    ]);
    results.apply(&query, delete("al2")).await;
    results.assert_rows(&[json!({"end": "b"}), json!({"end": "b"})]);
    results.apply(&query, delete("lb")).await;
    results.assert_rows(&[json!({"end": "b"})]);
    results.apply(&query, delete("right")).await;
    results.assert_rows(&[]);
}

pub async fn directions_and_cycles(config: &(impl QueryTestConfig + Send)) {
    let query = build_query(
        config,
        "MATCH (a:Start)<-[rs:R*2]-(b:End) RETURN [r IN rs | r.id] AS edges",
    )
    .await;
    let mut results = Results::default();
    results
        .apply_all(
            &query,
            [
                node("a", "Start"),
                node("middle", "Transit"),
                node("b", "End"),
                relationship("first", "middle", "a"),
                relationship("second", "b", "middle"),
            ],
        )
        .await;
    results.assert_rows(&[json!({"edges": ["first", "second"]})]);

    let query = build_query(
        config,
        "MATCH (a:Start)-[rs:R*1..3]-(b:End) RETURN [r IN rs | r.id] AS edges",
    )
    .await;
    let mut results = Results::default();
    results
        .apply_all(
            &query,
            [
                node("a", "Start"),
                node("middle", "Transit"),
                node("b", "End"),
                relationship("first", "a", "middle"),
                relationship("second", "b", "middle"),
            ],
        )
        .await;
    results.assert_rows(&[json!({"edges": ["first", "second"]})]);

    let query = build_query(
        config,
        "MATCH (a:Start)-[rs:R*0..4]->(a) RETURN [r IN rs | r.id] AS edges",
    )
    .await;
    let mut results = Results::default();
    results
        .apply_all(
            &query,
            [
                node("a", "Start"),
                node("b", "Transit"),
                relationship("ab", "a", "b"),
                relationship("ba", "b", "a"),
            ],
        )
        .await;
    results.assert_rows(&[json!({"edges": []}), json!({"edges": ["ab", "ba"]})]);
}

pub async fn segment_boundaries(config: &(impl QueryTestConfig + Send)) {
    let query = build_query(
        config,
        "MATCH (a:Start)-[:R*1..2]->()-[:R*1..2]->(b:End) RETURN b.id AS end",
    )
    .await;
    let mut results = Results::default();
    results
        .apply_all(
            &query,
            [
                node("a", "Start"),
                node("one", "Transit"),
                node("two", "Transit"),
                node("b", "End"),
                relationship("first", "a", "one"),
                relationship("second", "one", "two"),
                relationship("third", "two", "b"),
            ],
        )
        .await;
    results.assert_rows(&[json!({"end": "b"}), json!({"end": "b"})]);
    results.apply(&query, delete("second")).await;
    results.assert_rows(&[]);
}

pub async fn relationship_updates(config: &(impl QueryTestConfig + Send)) {
    let query = build_query(
        config,
        "MATCH (a:Start)-[rs:R*2 {enabled: true}]->(b:End)
         RETURN b.id AS end, [r IN rs | r.id] AS edges",
    )
    .await;
    let mut results = Results::default();
    results
        .apply_all(
            &query,
            [
                node("a", "Start"),
                node("middle", "Transit"),
                node("b", "End"),
                node("c", "End"),
                relationship("first", "a", "middle"),
                relationship("second", "middle", "b"),
            ],
        )
        .await;
    results.assert_rows(&[json!({"end": "b", "edges": ["first", "second"]})]);
    results
        .apply(
            &query,
            update_relationship("second", "middle", "b", "R", false),
        )
        .await;
    results.assert_rows(&[]);
    results
        .apply(
            &query,
            update_relationship("second", "middle", "b", "R", true),
        )
        .await;
    results.assert_rows(&[json!({"end": "b", "edges": ["first", "second"]})]);
    results
        .apply(
            &query,
            update_relationship("second", "middle", "c", "R", true),
        )
        .await;
    results.assert_rows(&[json!({"end": "c", "edges": ["first", "second"]})]);
    results
        .apply(
            &query,
            update_relationship("second", "middle", "c", "Other", true),
        )
        .await;
    results.assert_rows(&[]);
    results
        .apply(
            &query,
            update_relationship("second", "middle", "c", "R", true),
        )
        .await;
    results.assert_rows(&[json!({"end": "c", "edges": ["first", "second"]})]);
    let signature = *results.rows.keys().next().unwrap();
    results
        .apply(
            &query,
            SourceChange::Update {
                element: Element::Relation {
                    metadata: metadata("first", "R", 3),
                    in_node: ElementReference::new("paths", "a"),
                    out_node: ElementReference::new("paths", "middle"),
                    properties: ElementPropertyMap::from(json!({"id": "renamed"})),
                },
            },
        )
        .await;
    results.assert_rows(&[json!({"end": "c", "edges": ["renamed", "second"]})]);
    assert_eq!(
        results.rows.keys().copied().collect::<Vec<_>>(),
        vec![signature]
    );
    results.apply(&query, delete("second")).await;
    results.assert_rows(&[]);
}

fn record_count(count: &mut i64, changes: Vec<QueryPartEvaluationContext>) {
    for result in changes {
        match result {
            QueryPartEvaluationContext::Aggregation { before, after, .. } => {
                if let Some(before) = before {
                    assert_eq!(
                        before.get("paths").and_then(|value| value.as_i64()),
                        Some(*count)
                    );
                }
                *count = after.get("paths").and_then(|value| value.as_i64()).unwrap();
            }
            other => panic!("expected an aggregate change, got {other:?}"),
        }
    }
}

pub async fn aggregated_multiplicity(config: &(impl QueryTestConfig + Send)) {
    let query = build_query(
        config,
        "MATCH (a:Start)-[:R*2]->(b:End) RETURN count(b) AS paths",
    )
    .await;
    let mut count = 0;
    for (change, expected) in [
        (node("a", "Start"), 0),
        (node("left", "Transit"), 0),
        (node("right", "Transit"), 0),
        (node("b", "End"), 0),
        (relationship("al", "a", "left"), 0),
        (relationship("lb", "left", "b"), 1),
        (relationship("ar", "a", "right"), 1),
        (relationship("rb", "right", "b"), 2),
        (relationship("al2", "a", "left"), 3),
        (delete("lb"), 1),
        (relationship("lb", "left", "b"), 3),
        (delete("al2"), 2),
        (delete("right"), 1),
        (delete("lb"), 0),
    ] {
        record_count(
            &mut count,
            query.process_source_change(change).await.unwrap(),
        );
        assert_eq!(count, expected);
    }
}

pub async fn rejected_patterns(config: &(impl QueryTestConfig + Send)) {
    for pattern in [
        "MATCH (a)-[:R*]->(b) RETURN a",
        "MATCH (a)-[:R*-1]->(b) RETURN a",
        "MATCH (a)-[:R*3..2]->(b) RETURN a",
        "OPTIONAL MATCH (a)-[:R*1..2]->(b) RETURN a",
        "MATCH (a)-[:R*1..2]->(b) OPTIONAL MATCH (b)-[:S]->(c) RETURN a",
        "MATCH (a)-[:R*1..2]->(b), (c:Disconnected) RETURN a",
        "MATCH (a)-[:R*1 {config: [{enabled: a.enabled}]}]->(b) RETURN b",
        "MATCH (a)-[:R*1 {config: [{enabled: toBoolean('true')}]}]->(b) RETURN b",
    ] {
        let registry = Arc::new(FunctionRegistry::new()).with_cypher_function_set();
        let parser = Arc::new(CypherParser::new(registry.clone()));
        let query = config
            .config_query(QueryBuilder::new(pattern, parser).with_function_registry(registry))
            .await
            .try_build()
            .await;
        assert!(
            query.is_err(),
            "unsupported pattern was accepted: {pattern}"
        );
    }
}

pub async fn literal_object_lists(config: &(impl QueryTestConfig + Send)) {
    for repetition in ["", "*1"] {
        let query = build_query(
            config,
            &format!(
                "MATCH (a:Start {{config: [{{enabled: true}}]}})\
                 -[:R{repetition} {{config: [{{enabled: true}}]}}]->(b:End) \
                 RETURN b.id AS end"
            ),
        )
        .await;
        let start = |enabled, time| Element::Node {
            metadata: metadata("a", "Start", time),
            properties: ElementPropertyMap::from(
                json!({"id": "a", "config": [{"enabled": enabled}]}),
            ),
        };
        let edge = |enabled, time| Element::Relation {
            metadata: metadata("edge", "R", time),
            in_node: ElementReference::new("paths", "a"),
            out_node: ElementReference::new("paths", "b"),
            properties: ElementPropertyMap::from(
                json!({"id": "edge", "config": [{"enabled": enabled}]}),
            ),
        };
        let mut results = Results::default();
        results
            .apply_all(
                &query,
                [
                    SourceChange::Insert {
                        element: start(true, 1),
                    },
                    node("b", "End"),
                    SourceChange::Insert {
                        element: edge(false, 1),
                    },
                ],
            )
            .await;
        results.assert_rows(&[]);
        results
            .apply(
                &query,
                SourceChange::Update {
                    element: edge(true, 2),
                },
            )
            .await;
        results.assert_rows(&[json!({"end": "b"})]);
        results
            .apply(
                &query,
                SourceChange::Update {
                    element: start(false, 3),
                },
            )
            .await;
        results.assert_rows(&[]);
        results
            .apply(
                &query,
                SourceChange::Update {
                    element: start(true, 4),
                },
            )
            .await;
        results.assert_rows(&[json!({"end": "b"})]);
    }
}

pub async fn gql_relationship_bindings(config: &(impl QueryTestConfig + Send)) {
    for repetition in ["", "*1"] {
        for statements in [
            "FILTER true",
            "LET x = 1",
            "YIELD rs",
            "LET x = 1 FILTER x = 1 YIELD rs",
        ] {
            let text =
                format!("MATCH (a:Start)-[rs:R{repetition}]->(b:End) {statements} RETURN rs");
            let registry = Arc::new(FunctionRegistry::new()).with_cypher_function_set();
            let parser = Arc::new(GQLParser::new(registry.clone()));
            let query = config
                .config_query(QueryBuilder::new(&text, parser).with_function_registry(registry))
                .await
                .try_build()
                .await
                .unwrap();
            let mut results = Results::default();
            results
                .apply_all(
                    &query,
                    [
                        node("a", "Start"),
                        node("b", "End"),
                        relationship("edge", "a", "b"),
                    ],
                )
                .await;
            assert_eq!(results.rows.len(), 1, "{text}");
            let binding = results.rows.values().next().unwrap().get("rs").unwrap();
            let element = match binding {
                VariableValue::Element(element) if repetition.is_empty() => element,
                VariableValue::List(items) if repetition == "*1" => {
                    assert_eq!(items.len(), 1, "{text}");
                    let VariableValue::Element(element) = &items[0] else {
                        panic!("expected a relationship in the list for {text}");
                    };
                    element
                }
                other => panic!("incorrect relationship binding for {text}: {other:?}"),
            };
            assert_eq!(element.get_reference().element_id.as_ref(), "edge");
            results.apply(&query, delete("edge")).await;
            results.assert_rows(&[]);
        }
    }
}

pub async fn future_reprocessing(config: &(impl QueryTestConfig + Send)) {
    let query = build_query(
        config,
        "MATCH (a:Start)-[rs:R*2]->(b:End)
         WHERE drasi.trueLater(true, 10)
         RETURN b.id AS end, [r IN rs | r.id] AS edges",
    )
    .await;
    let mut results = Results::default();
    results
        .apply_all(
            &query,
            [
                node("a", "Start"),
                node("middle", "Transit"),
                node("b", "End"),
                relationship("first", "a", "middle"),
                relationship("second", "middle", "b"),
            ],
        )
        .await;
    results.assert_rows(&[]);
    let due = query.process_due_futures().await.unwrap().unwrap();
    assert_eq!(due.source_id.as_ref(), "paths");
    results.record(due.results);
    results.assert_rows(&[json!({"end": "b", "edges": ["first", "second"]})]);
    assert!(query.process_due_futures().await.unwrap().is_none());

    results
        .apply(
            &query,
            SourceChange::Delete {
                metadata: metadata("middle", "", 20),
            },
        )
        .await;
    results.assert_rows(&[]);
}

pub async fn pending_future_survives_internal_update(config: &(impl QueryTestConfig + Send)) {
    for update_middle in [false, true] {
        let query = build_query(
            config,
            "MATCH (a:Start)-[rs:R*2]->(b:End)
             WHERE drasi.trueLater(true, 10) RETURN b.id AS end",
        )
        .await;
        let mut results = Results::default();
        results
            .apply_all(
                &query,
                [
                    node("a", "Start"),
                    node("b", "End"),
                    relationship("first", "a", "middle"),
                    relationship("second", "middle", "b"),
                    node("middle", "Transit"),
                ],
            )
            .await;
        results.assert_rows(&[]);
        if update_middle {
            results
                .apply(
                    &query,
                    SourceChange::Update {
                        element: Element::Node {
                            metadata: metadata("middle", "Transit", 20),
                            properties: ElementPropertyMap::from(json!({"id": "middle"})),
                        },
                    },
                )
                .await;
        }
        let mut wakes = 0;
        while let Some(due) = query.process_due_futures().await.unwrap() {
            wakes += 1;
            assert!(wakes <= 2, "the same deadline must not keep rescheduling");
            results.record(due.results);
        }
        results.assert_rows(&[json!({"end": "b"})]);
    }
}

pub async fn shared_anchor_futures_count_each_path_once(config: &(impl QueryTestConfig + Send)) {
    for pattern in [
        "MATCH (a:Start)-[rs:R*2]->(b:End)",
        "MATCH (a:Start)-[:R]->(m:Transit)-[:R]->(b:End)",
    ] {
        let query = build_query(
            config,
            &format!("{pattern} WHERE drasi.trueLater(true, 10) RETURN count(b) AS paths"),
        )
        .await;
        let mut count = 0;
        for change in [
            node("a1", "Start"),
            node("a2", "Start"),
            node("b", "End"),
            relationship("first", "a1", "middle"),
            relationship("other", "a2", "middle"),
            relationship("second", "middle", "b"),
            node("middle", "Transit"),
        ] {
            record_count(
                &mut count,
                query.process_source_change(change).await.unwrap(),
            );
            assert_eq!(count, 0);
        }
        let mut wakes = 0;
        while let Some(due) = query.process_due_futures().await.unwrap() {
            wakes += 1;
            assert!(wakes <= 2, "each input has one deadline: {pattern}");
            record_count(&mut count, due.results);
            assert!(count <= 2, "a wake counted another input again: {pattern}");
        }
        assert_eq!(count, 2, "{pattern}");
        record_count(
            &mut count,
            query
                .process_source_change(SourceChange::Delete {
                    metadata: metadata("first", "", 20),
                })
                .await
                .unwrap(),
        );
        assert_eq!(count, 1, "{pattern}");
        assert!(query.process_due_futures().await.unwrap().is_none());
    }
}

pub async fn match_scoped_uniqueness(config: &(impl QueryTestConfig + Send)) {
    for (pattern, single_edge_count, parallel_edge_count) in [
        ("MATCH (a:Start)-[rs:R*1]->(b:End), (a)-[r:R]->(b)", 0, 2),
        (
            "MATCH (a:Start)-[rs:R*1]->(b:End) MATCH (a)-[r:R]->(b)",
            1,
            4,
        ),
    ] {
        let query = build_query(config, &format!("{pattern} RETURN b.id AS end")).await;
        let mut results = Results::default();
        results
            .apply_all(
                &query,
                [
                    node("a", "Start"),
                    node("b", "End"),
                    relationship("ab", "a", "b"),
                ],
            )
            .await;
        assert_eq!(results.rows.len(), single_edge_count, "{pattern}");
        results
            .apply(&query, relationship("parallel", "a", "b"))
            .await;
        assert_eq!(results.rows.len(), parallel_edge_count, "{pattern}");
        results.apply(&query, delete("parallel")).await;
        assert_eq!(results.rows.len(), single_edge_count, "{pattern}");
    }
}

pub async fn mixed_segments(config: &(impl QueryTestConfig + Send)) {
    let query = build_query(
        config,
        "MATCH (root:Root)-[lead:PRE]->(a:Start)-[rs:R*2]->(b:End)-[tail:POST]->(end:Goal)
         RETURN lead.id AS lead, [r IN rs | r.id] AS edges, tail.id AS tail, end.id AS end",
    )
    .await;
    let mut results = Results::default();
    results
        .apply_all(
            &query,
            [
                node("root", "Root"),
                node("a", "Start"),
                node("middle", "Transit"),
                node("b", "End"),
                node("end", "Goal"),
                relationship("first", "a", "middle"),
                relationship("second", "middle", "b"),
                typed_relationship("tail", "b", "end", "POST"),
            ],
        )
        .await;
    results.assert_rows(&[]);
    results
        .apply(&query, typed_relationship("lead", "root", "a", "PRE"))
        .await;
    results.assert_rows(&[
        json!({"lead": "lead", "edges": ["first", "second"], "tail": "tail", "end": "end"}),
    ]);
    results.apply(&query, delete("tail")).await;
    results.assert_rows(&[]);
    results
        .apply(&query, typed_relationship("tail", "b", "end", "POST"))
        .await;
    results.assert_rows(&[
        json!({"lead": "lead", "edges": ["first", "second"], "tail": "tail", "end": "end"}),
    ]);
    results.apply(&query, delete("a")).await;
    results.assert_rows(&[]);
}

pub async fn repeated_node_constraints(config: &(impl QueryTestConfig + Send)) {
    let query = build_query(
        config,
        "MATCH (a:Start)-[:R*2]->(b:End), (b:Selected) RETURN b.id AS end",
    )
    .await;
    let mut results = Results::default();
    results
        .apply_all(
            &query,
            [
                node("a", "Start"),
                node("middle", "Transit"),
                node("b", "End"),
                relationship("first", "a", "middle"),
                relationship("second", "middle", "b"),
            ],
        )
        .await;
    results.assert_rows(&[]);
    let mut selected = metadata("b", "End", 2);
    selected.labels = Arc::new([Arc::from("End"), Arc::from("Selected")]);
    results
        .apply(
            &query,
            SourceChange::Update {
                element: Element::Node {
                    metadata: selected,
                    properties: ElementPropertyMap::new(),
                },
            },
        )
        .await;
    results.assert_rows(&[json!({"end": "b"})]);
    results
        .apply(
            &query,
            SourceChange::Update {
                element: Element::Node {
                    metadata: metadata("b", "Selected", 3),
                    properties: ElementPropertyMap::new(),
                },
            },
        )
        .await;
    results.assert_rows(&[]);
}

pub async fn endpoint_rewiring(config: &(impl QueryTestConfig + Send)) {
    let query = build_query(
        config,
        "MATCH (a:Start)-[:R]->(b:End) RETURN a.id AS start, b.id AS end",
    )
    .await;
    let mut results = Results::default();
    results
        .apply_all(
            &query,
            [
                node("a", "Start"),
                node("b", "End"),
                node("c", "End"),
                relationship("edge", "a", "b"),
            ],
        )
        .await;
    results
        .apply(&query, update_relationship("edge", "a", "c", "R", true))
        .await;
    results.assert_rows(&[json!({"start": "a", "end": "c"})]);
    results
        .apply(
            &query,
            SourceChange::Update {
                element: Element::Node {
                    metadata: metadata("c", "End", 4),
                    properties: ElementPropertyMap::from(json!({"id": "renamed"})),
                },
            },
        )
        .await;
    results.assert_rows(&[json!({"start": "a", "end": "renamed"})]);
    results.apply(&query, delete("b")).await;
    results.assert_rows(&[json!({"start": "a", "end": "renamed"})]);
}

#[derive(Default)]
struct TrailOracle {
    nodes: BTreeMap<String, bool>,
    relationships: BTreeMap<String, (String, String, bool)>,
    undirected: bool,
}

impl TrailOracle {
    fn apply(&mut self, change: &SourceChange) {
        match change {
            SourceChange::Insert { element } | SourceChange::Update { element } => match element {
                Element::Node { metadata, .. } => {
                    self.nodes.insert(
                        metadata.reference.element_id.to_string(),
                        metadata.labels.iter().any(|label| label.as_ref() == "Node"),
                    );
                }
                Element::Relation {
                    metadata,
                    in_node,
                    out_node,
                    ..
                } => {
                    self.relationships.insert(
                        metadata.reference.element_id.to_string(),
                        (
                            in_node.element_id.to_string(),
                            out_node.element_id.to_string(),
                            metadata.labels.iter().any(|label| label.as_ref() == "R"),
                        ),
                    );
                }
            },
            SourceChange::Delete { metadata } => {
                self.nodes.remove(metadata.reference.element_id.as_ref());
                self.relationships
                    .remove(metadata.reference.element_id.as_ref());
            }
            SourceChange::Future { .. } => panic!("the trail oracle has no clock predicates"),
        }
    }

    fn rows(&self) -> Vec<Value> {
        let mut rows = Vec::new();
        for (start, selected) in &self.nodes {
            if *selected {
                self.visit(start, start, &mut Vec::new(), &mut rows);
            }
        }
        rows
    }

    fn visit(&self, start: &str, current: &str, path: &mut Vec<String>, rows: &mut Vec<Value>) {
        if self.nodes.get(current) == Some(&true) {
            rows.push(json!({"start": start, "end": current, "edges": path}));
        }
        if path.len() == 3 {
            return;
        }
        for (id, (from, to, selected)) in &self.relationships {
            let next = if from == current {
                to
            } else if self.undirected && to == current {
                from
            } else {
                continue;
            };
            if !*selected || !self.nodes.contains_key(next) || path.contains(id) {
                continue;
            }
            path.push(id.clone());
            self.visit(start, next, path, rows);
            path.pop();
        }
    }
}

pub async fn exhaustive_trail_changes(config: &(impl QueryTestConfig + Send)) {
    exhaustive_changes(config, false).await;
}

pub async fn exhaustive_undirected_trail_changes(config: &(impl QueryTestConfig + Send)) {
    exhaustive_changes(config, true).await;
}

async fn exhaustive_changes(config: &(impl QueryTestConfig + Send), undirected: bool) {
    let arrow = if undirected { "-" } else { "->" };
    let query = build_query(
        config,
        &format!(
            "MATCH (a:Node)-[rs:R*0..3]{arrow}(b:Node)
         RETURN a.id AS start, b.id AS end, [r IN rs | r.id] AS edges"
        ),
    )
    .await;
    let changes = [
        node("a", "Node"),
        node("b", "Node"),
        node("c", "Node"),
        node("d", "Node"),
        relationship("ab", "a", "b"),
        relationship("bc", "b", "c"),
        relationship("ca", "c", "a"),
        relationship("loop", "b", "b"),
        relationship("parallel", "a", "b"),
        relationship("bd", "b", "d"),
        delete("c"),
        node("c", "Node"),
        SourceChange::Update {
            element: Element::Node {
                metadata: metadata("b", "Transit", 4),
                properties: ElementPropertyMap::from(json!({"id": "b"})),
            },
        },
        update_relationship("ab", "d", "a", "R", true),
        update_relationship("ca", "c", "a", "Other", true),
        update_relationship("ca", "c", "a", "R", true),
        delete("a"),
        node("a", "Node"),
        delete("parallel"),
    ];
    let mut oracle = TrailOracle {
        undirected,
        ..Default::default()
    };
    let mut results = Results::default();
    for change in changes {
        oracle.apply(&change);
        results.apply(&query, change).await;
        results.assert_rows(&oracle.rows());
    }
}

#[macro_export]
macro_rules! variable_length_match_tests {
    ($module:ident, $config:expr) => {
        mod $module {
            #[tokio::test]
            async fn exact_length() {
                let config = $config;
                $crate::use_cases::variable_length_match::exact_length(&config).await;
            }
            #[tokio::test]
            async fn ranges_and_lists() {
                let config = $config;
                $crate::use_cases::variable_length_match::ranges_and_lists(&config).await;
            }
            #[tokio::test]
            async fn zero_hops() {
                let config = $config;
                $crate::use_cases::variable_length_match::zero_hops(&config).await;
            }
            #[tokio::test]
            async fn internal_node_changes() {
                let config = $config;
                $crate::use_cases::variable_length_match::internal_node_changes(&config).await;
            }
            #[tokio::test]
            async fn path_multiplicity() {
                let config = $config;
                $crate::use_cases::variable_length_match::path_multiplicity(&config).await;
            }
            #[tokio::test]
            async fn directions_and_cycles() {
                let config = $config;
                $crate::use_cases::variable_length_match::directions_and_cycles(&config).await;
            }
            #[tokio::test]
            async fn segment_boundaries() {
                let config = $config;
                $crate::use_cases::variable_length_match::segment_boundaries(&config).await;
            }
            #[tokio::test]
            async fn relationship_updates() {
                let config = $config;
                $crate::use_cases::variable_length_match::relationship_updates(&config).await;
            }
            #[tokio::test]
            async fn match_scoped_uniqueness() {
                let config = $config;
                $crate::use_cases::variable_length_match::match_scoped_uniqueness(&config).await;
            }
            #[tokio::test]
            async fn endpoint_rewiring() {
                let config = $config;
                $crate::use_cases::variable_length_match::endpoint_rewiring(&config).await;
            }
            #[tokio::test]
            async fn aggregated_multiplicity() {
                let config = $config;
                $crate::use_cases::variable_length_match::aggregated_multiplicity(&config).await;
            }
            #[tokio::test]
            async fn rejected_patterns() {
                let config = $config;
                $crate::use_cases::variable_length_match::rejected_patterns(&config).await;
            }
            #[tokio::test]
            async fn literal_object_lists() {
                let config = $config;
                $crate::use_cases::variable_length_match::literal_object_lists(&config).await;
            }
            #[tokio::test]
            async fn gql_relationship_bindings() {
                let config = $config;
                $crate::use_cases::variable_length_match::gql_relationship_bindings(&config).await;
            }
            #[tokio::test]
            async fn future_reprocessing() {
                let config = $config;
                $crate::use_cases::variable_length_match::future_reprocessing(&config).await;
            }
            #[tokio::test]
            async fn pending_future_survives_internal_update() {
                let config = $config;
                $crate::use_cases::variable_length_match::pending_future_survives_internal_update(
                    &config,
                )
                .await;
            }
            #[tokio::test]
            async fn shared_anchor_futures_count_each_path_once() {
                let config = $config;
                $crate::use_cases::variable_length_match::shared_anchor_futures_count_each_path_once(
                    &config,
                )
                .await;
            }
            #[tokio::test]
            async fn original_sliding_window_max() {
                let config = $config;
                $crate::use_cases::windows::sliding_window_max(&config).await;
            }
            #[tokio::test]
            async fn original_sliding_window_average() {
                let config = $config;
                $crate::use_cases::windows::sliding_window_avg_grouped(&config).await;
            }
            #[tokio::test]
            async fn read_only_middleware_failure() {
                let config = $config;
                $crate::use_cases::parse_json::parse_json_test(&config).await;
            }
            #[tokio::test]
            async fn mixed_segments() {
                let config = $config;
                $crate::use_cases::variable_length_match::mixed_segments(&config).await;
            }
            #[tokio::test]
            async fn repeated_node_constraints() {
                let config = $config;
                $crate::use_cases::variable_length_match::repeated_node_constraints(&config).await;
            }
            #[tokio::test]
            async fn exhaustive_trail_changes() {
                let config = $config;
                $crate::use_cases::variable_length_match::exhaustive_trail_changes(&config).await;
            }
            #[tokio::test]
            async fn exhaustive_undirected_trail_changes() {
                let config = $config;
                $crate::use_cases::variable_length_match::exhaustive_undirected_trail_changes(
                    &config,
                )
                .await;
            }
        }
    };
}

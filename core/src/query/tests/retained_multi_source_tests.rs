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
    },
    models::{
        Element, ElementMetadata, ElementPropertyMap, ElementReference, QueryJoin, QueryJoinKey,
        SourceChange,
    },
    query::{ContinuousQuery, QueryBuilder},
};

const GRAPH_SOURCE: &str = "graph";
const STATUS_SOURCE: &str = "status";
const ITEM_STATUS_JOIN: &str = "ITEM_STATUS";

const PENDING_QUERY: &str = "
    MATCH (item:Item)-[:ITEM_STATUS]->(status:ItemStatus)
    OPTIONAL MATCH (item)-[:HAS_MARKER]->(marker:Marker)
    WITH item.id AS itemId,
         status.name AS statusName,
         count(marker) AS markerCount
    WHERE statusName = 'Pending' AND markerCount = 0
    RETURN itemId, statusName, markerCount
";

const READY_QUERY: &str = "
    MATCH (task:Task)-[:HAS_ITEM]->(item:Item)
    MATCH (task)-[:HAS_TRIGGER]->(trigger:Trigger)
    MATCH (item)-[:ITEM_STATUS]->(status:ItemStatus)
    OPTIONAL MATCH (task)-[:HAS_RUN]->(run:Run)
    WITH task.number AS taskNumber,
         status.name AS statusName,
         count(run) AS runCount
    WHERE statusName = 'Ready' AND runCount = 0
    RETURN taskNumber, statusName, runCount
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
        let join = QueryJoin {
            id: ITEM_STATUS_JOIN.to_string(),
            keys: vec![
                QueryJoinKey {
                    label: "Item".to_string(),
                    property: "id".to_string(),
                },
                QueryJoinKey {
                    label: "ItemStatus".to_string(),
                    property: "itemId".to_string(),
                },
            ],
        };
        let query = QueryBuilder::new(query_text, parser)
            .with_function_registry(functions)
            .with_join(join)
            .build()
            .await;

        Self {
            query,
            rows: HashMap::new(),
        }
    }

    async fn process(&mut self, change: SourceChange) {
        for change in self.query.process_source_change(change).await.unwrap() {
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
                    self.rows.insert(row_signature, after);
                }
                QueryPartEvaluationContext::Removing { row_signature, .. } => {
                    self.rows.remove(&row_signature);
                }
                QueryPartEvaluationContext::Noop => {}
            }
        }
    }
}

fn node(
    source: &str,
    id: &str,
    label: &str,
    effective_from: u64,
    properties: serde_json::Value,
) -> Element {
    Element::Node {
        metadata: ElementMetadata {
            reference: ElementReference::new(source, id),
            labels: Arc::from([Arc::from(label)]),
            effective_from,
        },
        properties: ElementPropertyMap::from(properties),
    }
}

fn relation(
    id: &str,
    label: &str,
    effective_from: u64,
    in_node: (&str, &str),
    out_node: (&str, &str),
) -> Element {
    Element::Relation {
        metadata: ElementMetadata {
            reference: ElementReference::new(GRAPH_SOURCE, id),
            labels: Arc::from([Arc::from(label)]),
            effective_from,
        },
        in_node: ElementReference::new(in_node.0, in_node.1),
        out_node: ElementReference::new(out_node.0, out_node.1),
        properties: ElementPropertyMap::new(),
    }
}

fn live_changes() -> Vec<SourceChange> {
    vec![
        SourceChange::Insert {
            element: node(GRAPH_SOURCE, "task-20", "Task", 1, json!({"number": 20})),
        },
        SourceChange::Insert {
            element: node(GRAPH_SOURCE, "item-20", "Item", 2, json!({"id": "item-20"})),
        },
        SourceChange::Insert {
            element: relation(
                "task-item-20",
                "HAS_ITEM",
                3,
                (GRAPH_SOURCE, "task-20"),
                (GRAPH_SOURCE, "item-20"),
            ),
        },
        SourceChange::Insert {
            element: node(
                STATUS_SOURCE,
                "status-item-20",
                "ItemStatus",
                4,
                json!({"itemId": "item-20", "name": "Pending"}),
            ),
        },
        SourceChange::Insert {
            element: node(
                GRAPH_SOURCE,
                "trigger-20",
                "Trigger",
                5,
                json!({"kind": "manual"}),
            ),
        },
        SourceChange::Insert {
            element: relation(
                "task-trigger-20",
                "HAS_TRIGGER",
                6,
                (GRAPH_SOURCE, "task-20"),
                (GRAPH_SOURCE, "trigger-20"),
            ),
        },
        SourceChange::Update {
            element: node(
                STATUS_SOURCE,
                "status-item-20",
                "ItemStatus",
                7,
                json!({"itemId": "item-20", "name": "Ready"}),
            ),
        },
    ]
}

fn fresh_snapshot() -> Vec<SourceChange> {
    let mut changes = live_changes();
    changes.remove(3);
    let final_status = changes.pop().unwrap();
    changes.insert(
        3,
        match final_status {
            SourceChange::Update { element } => SourceChange::Insert { element },
            _ => unreachable!(),
        },
    );
    changes
}

async fn run_queries(changes: Vec<SourceChange>) -> (MaterializedQuery, MaterializedQuery) {
    let mut pending = MaterializedQuery::new(PENDING_QUERY).await;
    let mut ready = MaterializedQuery::new(READY_QUERY).await;

    for change in changes {
        pending.process(change.clone()).await;
        ready.process(change).await;
    }

    (pending, ready)
}

#[tokio::test]
async fn retained_multi_source_group_migration_matches_fresh_reconstruction() {
    let (retained_pending, retained_ready) = run_queries(live_changes()).await;
    let (fresh_pending, fresh_ready) = run_queries(fresh_snapshot()).await;

    assert_eq!(retained_pending.rows, fresh_pending.rows);
    assert_eq!(retained_ready.rows, fresh_ready.rows);
    assert!(retained_pending.rows.is_empty());
    assert_eq!(retained_ready.rows.len(), 1);
}

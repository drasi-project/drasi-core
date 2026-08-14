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

const WORKGRAPH_SOURCE: &str = "github-workgraph";
const PROJECT_STATE_SOURCE: &str = "github-project-state";
const ITEM_STATUS_JOIN: &str = "ITEM_STATUS";

const INITIALIZER_QUERY: &str = "
    MATCH (item:ProjectItem)-[:ITEM_STATUS]->(status:ProjectItemStatus)
    OPTIONAL MATCH (item)-[:HAS_INITIALIZATION]->(initialization:Initialization)
    WITH item.id AS itemId,
         status.name AS statusName,
         count(initialization) AS matchingInitializationCount
    WHERE statusName = 'Todo' AND matchingInitializationCount = 0
    RETURN itemId, statusName, matchingInitializationCount
";

const LAUNCHER_QUERY: &str = "
    MATCH (issue:Issue)-[:HAS_PROJECT_ITEM]->(item:ProjectItem)
    MATCH (issue)-[:HAS_ROUTE_COMMENT]->(route:RouteComment)
    MATCH (item)-[:ITEM_STATUS]->(status:ProjectItemStatus)
    OPTIONAL MATCH (issue)-[:HAS_EXECUTION]->(execution:Execution)
    WITH issue.number AS issueNumber,
         status.name AS statusName,
         count(execution) AS matchingExecutionCount
    WHERE statusName = 'AwaitingValidation' AND matchingExecutionCount = 0
    RETURN issueNumber, statusName, matchingExecutionCount
";

struct RetainedQuery {
    query: ContinuousQuery,
    results: HashMap<u64, QueryVariables>,
}

impl RetainedQuery {
    async fn new(query_text: &str) -> Self {
        let functions = Arc::new(FunctionRegistry::new());
        functions.register_function("count", Function::Aggregating(Arc::new(Count {})));
        let parser = Arc::new(CypherParser::new(functions.clone()));
        let join = QueryJoin {
            id: ITEM_STATUS_JOIN.to_string(),
            keys: vec![
                QueryJoinKey {
                    label: "ProjectItem".to_string(),
                    property: "id".to_string(),
                },
                QueryJoinKey {
                    label: "ProjectItemStatus".to_string(),
                    property: "projectItemId".to_string(),
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
            results: HashMap::new(),
        }
    }

    async fn process(&mut self, change: SourceChange) {
        let changes = self.query.process_source_change(change).await.unwrap();
        for change in changes {
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
                    self.results.insert(row_signature, after);
                }
                QueryPartEvaluationContext::Removing { row_signature, .. } => {
                    self.results.remove(&row_signature);
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
            reference: ElementReference::new(WORKGRAPH_SOURCE, id),
            labels: Arc::from([Arc::from(label)]),
            effective_from,
        },
        in_node: ElementReference::new(in_node.0, in_node.1),
        out_node: ElementReference::new(out_node.0, out_node.1),
        properties: ElementPropertyMap::new(),
    }
}

fn ordered_live_changes() -> Vec<SourceChange> {
    vec![
        SourceChange::Insert {
            element: node(
                WORKGRAPH_SOURCE,
                "issue-20",
                "Issue",
                1,
                json!({"number": 20}),
            ),
        },
        SourceChange::Insert {
            element: node(
                WORKGRAPH_SOURCE,
                "item-20",
                "ProjectItem",
                2,
                json!({"id": "PVTI_20"}),
            ),
        },
        SourceChange::Insert {
            element: relation(
                "issue-item-20",
                "HAS_PROJECT_ITEM",
                3,
                (WORKGRAPH_SOURCE, "issue-20"),
                (WORKGRAPH_SOURCE, "item-20"),
            ),
        },
        SourceChange::Insert {
            element: node(
                PROJECT_STATE_SOURCE,
                "status-PVTI_20",
                "ProjectItemStatus",
                4,
                json!({"projectItemId": "PVTI_20", "name": "Todo"}),
            ),
        },
        SourceChange::Insert {
            element: node(
                WORKGRAPH_SOURCE,
                "route-20",
                "RouteComment",
                5,
                json!({"body": "/validate"}),
            ),
        },
        SourceChange::Insert {
            element: relation(
                "issue-route-20",
                "HAS_ROUTE_COMMENT",
                6,
                (WORKGRAPH_SOURCE, "issue-20"),
                (WORKGRAPH_SOURCE, "route-20"),
            ),
        },
        SourceChange::Update {
            element: node(
                PROJECT_STATE_SOURCE,
                "status-PVTI_20",
                "ProjectItemStatus",
                7,
                json!({"projectItemId": "PVTI_20", "name": "AwaitingValidation"}),
            ),
        },
    ]
}

fn fresh_snapshot() -> Vec<SourceChange> {
    let mut changes = ordered_live_changes();
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

async fn run_queries(changes: Vec<SourceChange>) -> (RetainedQuery, RetainedQuery) {
    let mut initializer = RetainedQuery::new(INITIALIZER_QUERY).await;
    let mut launcher = RetainedQuery::new(LAUNCHER_QUERY).await;

    for change in changes {
        initializer.process(change.clone()).await;
        launcher.process(change).await;
    }

    (initializer, launcher)
}

#[tokio::test]
async fn retained_multi_source_queries_converge_with_fresh_queries() {
    let (retained_initializer, retained_launcher) = run_queries(ordered_live_changes()).await;
    let (fresh_initializer, fresh_launcher) = run_queries(fresh_snapshot()).await;

    assert_eq!(retained_initializer.results, fresh_initializer.results);
    assert_eq!(retained_launcher.results, fresh_launcher.results);
    assert!(retained_initializer.results.is_empty());
    assert_eq!(retained_launcher.results.len(), 1);
}

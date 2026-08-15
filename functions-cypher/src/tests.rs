// Copyright 2025 The Drasi Authors.
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

use std::sync::Arc;

use drasi_core::{
    evaluation::{context::QueryPartEvaluationContext, functions::FunctionRegistry},
    models::{Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange},
    query::{ContinuousQuery, QueryBuilder},
};
use drasi_query_cypher::CypherParser;
use serde_json::json;

use crate::CypherFunctionSet;

const SOURCE: &str = "workgraph";
const ITEM_NODE_ID: &str = "PVTI_lADOABCDEF4AbcDEzgXYZ123";
const ISSUE_NODE_ID: &str = "I_kwDOABCDEF6ABCDE";
const BODY_DIGEST: &str = "sha256:09a16cabf7f29fd03469340079d25d1de2e818149c13f982d8133a87cbc8a5d1";

async fn build_query(query_text: &str) -> ContinuousQuery {
    let function_registry = Arc::new(FunctionRegistry::new()).with_cypher_function_set();
    let parser = Arc::new(CypherParser::new(function_registry.clone()));
    QueryBuilder::new(query_text, parser)
        .with_function_registry(function_registry)
        .build()
        .await
}

fn node(id: &str, label: &str, effective_from: u64, properties: serde_json::Value) -> SourceChange {
    SourceChange::Insert {
        element: Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new(SOURCE, id),
                labels: Arc::from([Arc::from(label)]),
                effective_from,
            },
            properties: ElementPropertyMap::from(properties),
        },
    }
}

fn relation(
    id: &str,
    label: &str,
    effective_from: u64,
    in_node: &str,
    out_node: &str,
) -> SourceChange {
    SourceChange::Insert {
        element: Element::Relation {
            metadata: ElementMetadata {
                reference: ElementReference::new(SOURCE, id),
                labels: Arc::from([Arc::from(label)]),
                effective_from,
            },
            in_node: ElementReference::new(SOURCE, in_node),
            out_node: ElementReference::new(SOURCE, out_node),
            properties: ElementPropertyMap::new(),
        },
    }
}

async fn run_dogfood_query(query_text: &str) -> Vec<(String, String)> {
    let query = build_query(query_text).await;

    for change in [
        node(
            "item-1",
            "ProjectItem",
            1,
            json!({ "nodeId": ITEM_NODE_ID }),
        ),
        node(
            "issue-1",
            "Issue",
            2,
            json!({ "nodeId": ISSUE_NODE_ID, "bodyDigest": BODY_DIGEST }),
        ),
        relation("tracks-1", "TRACKS", 3, "item-1", "issue-1"),
    ] {
        let results = query.process_source_change(change).await.unwrap();
        for result in results {
            if let QueryPartEvaluationContext::Adding { after, .. } = result {
                return after
                    .iter()
                    .map(|(key, value)| {
                        (
                            key.to_string(),
                            value
                                .as_str()
                                .unwrap_or_else(|| panic!("projection '{key}' is not a string"))
                                .to_string(),
                        )
                    })
                    .collect();
            }
        }
    }

    panic!("query produced no rows");
}

fn field<'a>(row: &'a [(String, String)], name: &str) -> &'a str {
    row.iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.as_str())
        .unwrap_or_else(|| panic!("projection '{name}' missing from {row:?}"))
}

/// The dogfood projections, expressed in the exact Cypher this test parses.
///
/// String literals are single quoted; `\n` inside them is Cypher's backslash-n
/// newline escape, which the parser decodes to a single U+000A byte. `+`
/// concatenates strings. Rust `concat!` doubles each backslash below (`\\n`),
/// so the Cypher text the parser actually receives is exactly:
///
/// ```cypher
/// MATCH (item:ProjectItem)-[:TRACKS]->(issue:Issue)
/// WITH
///   issue.bodyDigest AS contentDigest,
///   'workgraph.run/v1\n' + item.nodeId + '\n' + issue.nodeId + '\n' + issue.bodyDigest AS runPreimage
/// WITH
///   contentDigest,
///   runPreimage,
///   sha256(runPreimage) AS runHex
/// WITH
///   contentDigest,
///   runPreimage,
///   runHex,
///   'run:sha256:' + runHex AS runId
/// WITH
///   contentDigest,
///   runPreimage,
///   runHex,
///   runId,
///   'workgraph.event/v1\n' + runId + '\nResponsibilityAssigned' AS eventPreimage
/// RETURN
///   contentDigest,
///   runPreimage,
///   runHex,
///   runId,
///   eventPreimage,
///   'event:sha256:' + sha256(eventPreimage) AS eventId
/// ```
const DOGFOOD_QUERY: &str = concat!(
    "MATCH (item:ProjectItem)-[:TRACKS]->(issue:Issue)\n",
    "WITH\n",
    "  issue.bodyDigest AS contentDigest,\n",
    "  'workgraph.run/v1\\n' + item.nodeId + '\\n' + issue.nodeId + '\\n' + issue.bodyDigest AS runPreimage\n",
    "WITH\n",
    "  contentDigest,\n",
    "  runPreimage,\n",
    "  sha256(runPreimage) AS runHex\n",
    "WITH\n",
    "  contentDigest,\n",
    "  runPreimage,\n",
    "  runHex,\n",
    "  'run:sha256:' + runHex AS runId\n",
    "WITH\n",
    "  contentDigest,\n",
    "  runPreimage,\n",
    "  runHex,\n",
    "  runId,\n",
    "  'workgraph.event/v1\\n' + runId + '\\nResponsibilityAssigned' AS eventPreimage\n",
    "RETURN\n",
    "  contentDigest,\n",
    "  runPreimage,\n",
    "  runHex,\n",
    "  runId,\n",
    "  eventPreimage,\n",
    "  'event:sha256:' + sha256(eventPreimage) AS eventId"
);

#[tokio::test]
async fn sha256_is_registered_in_the_standard_cypher_function_set() {
    let registry = Arc::new(FunctionRegistry::new()).with_cypher_function_set();
    assert!(
        registry.get_function("sha256").is_some(),
        "sha256 must be part of the standard Cypher function set"
    );
}

#[tokio::test]
async fn dogfood_identifiers_are_derivable_from_a_real_cypher_query() {
    let row = run_dogfood_query(DOGFOOD_QUERY).await;

    assert_eq!(field(&row, "contentDigest"), BODY_DIGEST);
    assert_eq!(
        field(&row, "runHex"),
        "775813253e0b6106e5a5f40ea02dcee45021121ce3f79f2d23c180d9b3027664"
    );
    assert_eq!(
        field(&row, "runId"),
        "run:sha256:775813253e0b6106e5a5f40ea02dcee45021121ce3f79f2d23c180d9b3027664"
    );
    assert_eq!(
        field(&row, "eventId"),
        "event:sha256:f7157bb34419e97450897d17ed8e31444f76d25ef48b59c794778d8f8ffa3a91"
    );
}

#[tokio::test]
async fn cypher_newline_escape_produces_exactly_one_lf_byte_and_no_trailing_lf() {
    let row = run_dogfood_query(DOGFOOD_QUERY).await;

    let run_preimage = field(&row, "runPreimage");
    assert_eq!(
        run_preimage,
        format!("workgraph.run/v1\n{ITEM_NODE_ID}\n{ISSUE_NODE_ID}\n{BODY_DIGEST}")
    );
    // Each `\n` in the query became exactly one LF byte, and no backslash
    // survived the parse.
    assert_eq!(run_preimage.bytes().filter(|b| *b == b'\n').count(), 3);
    assert!(!run_preimage.contains('\\'));
    assert!(!run_preimage.ends_with('\n'));
    assert_eq!(
        run_preimage.split('\n').collect::<Vec<_>>(),
        vec!["workgraph.run/v1", ITEM_NODE_ID, ISSUE_NODE_ID, BODY_DIGEST]
    );

    let event_preimage = field(&row, "eventPreimage");
    assert_eq!(
        event_preimage,
        format!(
            "workgraph.event/v1\n{}\nResponsibilityAssigned",
            field(&row, "runId")
        )
    );
    assert_eq!(event_preimage.bytes().filter(|b| *b == b'\n').count(), 2);
    assert!(!event_preimage.contains('\\'));
    assert!(!event_preimage.ends_with('\n'));
}

// Copyright 2024 The Drasi Authors.
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
    evaluation::functions::FunctionRegistry,
    models::{Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange},
    query::QueryBuilder,
};
use serde_json::json;

use drasi_functions_cypher::CypherFunctionSet;
use drasi_query_cypher::CypherParser;

#[allow(clippy::print_stdout, clippy::unwrap_used)]
#[tokio::main]
async fn main() {
    let query_str = "
    MATCH 
        (v:Vehicle)-[:LOCATED_IN]->(:Zone {type:'Parking Lot'}) 
    RETURN 
        v.color AS color, 
        v.plate AS plate";

    let function_registry = Arc::new(FunctionRegistry::new()).with_cypher_function_set();
    let parser = Arc::new(CypherParser::new(function_registry.clone()));
    let query_builder =
        QueryBuilder::new(query_str, parser).with_function_registry(function_registry);
    let query = query_builder.build().await;

    for source_change in get_initial_data() {
        _ = query.process_source_change(source_change).await;
    }

    println!(
        "Result: {:?}",
        query
            .process_source_change(SourceChange::Insert {
                element: Element::Relation {
                    metadata: ElementMetadata {
                        reference: ElementReference::new("", "v1-location"),
                        labels: Arc::new([Arc::from("LOCATED_IN")]),
                        effective_from: 0,
                    },
                    properties: ElementPropertyMap::new(),
                    out_node: ElementReference::new("", "z1"),
                    in_node: ElementReference::new("", "v1"),
                },
            })
            .await
            .unwrap()
    );

    println!(
        "Result: {:?}",
        query
            .process_source_change(SourceChange::Update {
                element: Element::Node {
                    metadata: ElementMetadata {
                        reference: ElementReference::new("", "v1"),
                        labels: Arc::new([Arc::from("Vehicle")]),
                        effective_from: 0,
                    },
                    properties: ElementPropertyMap::from(json!({
                        "plate": "AAA-1234",
                        "color": "Green"
                    }))
                },
            })
            .await
            .unwrap()
    );

    println!(
        "Result: {:?}",
        query
            .process_source_change(SourceChange::Delete {
                metadata: ElementMetadata {
                    reference: ElementReference::new("", "v1-location"),
                    labels: Arc::new([Arc::from("LOCATED_AT")]),
                    effective_from: 0,
                },
            })
            .await
            .unwrap()
    );
}

fn get_initial_data() -> Vec<SourceChange> {
    vec![
        SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("", "v1"),
                    labels: Arc::new([Arc::from("Vehicle")]),
                    effective_from: 0,
                },
                properties: ElementPropertyMap::from(json!({
                    "plate": "AAA-1234",
                    "color": "Blue"
                })),
            },
        },
        SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("", "v2"),
                    labels: Arc::new([Arc::from("Vehicle")]),
                    effective_from: 0,
                },
                properties: ElementPropertyMap::from(json!({
                    "plate": "ZZZ-7890",
                    "color": "Red"
                })),
            },
        },
        SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("", "z1"),
                    labels: Arc::new([Arc::from("Zone")]),
                    effective_from: 0,
                },
                properties: ElementPropertyMap::from(json!({
                    "type": "Parking Lot"
                })),
            },
        },
    ]
}

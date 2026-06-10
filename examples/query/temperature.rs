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
    evaluation::context::QueryPartEvaluationContext,
    evaluation::functions::FunctionRegistry,
    models::{Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange},
    query::{ContinuousQuery, QueryBuilder},
};
use rand::Rng;
use serde_json::json;

use drasi_functions_cypher::CypherFunctionSet;
use drasi_query_cypher::CypherParser;

#[allow(clippy::print_stdout, clippy::unwrap_used)]
#[tokio::main]
async fn main() {
    let query_str = "
    MATCH 
        (c:Component)-[:HAS_LIMIT]->(l:Limit) 
    WHERE c.temperature > l.max_temperature 
    RETURN 
        c.name AS component_name, 
        c.temperature AS component_temperature, 
        l.max_temperature AS limit_temperature";

    let function_registry = Arc::new(FunctionRegistry::new()).with_cypher_function_set();
    let parser = Arc::new(CypherParser::new(function_registry.clone()));
    let query_builder =
        QueryBuilder::new(query_str, parser).with_function_registry(function_registry);
    let query = query_builder.build().await;

    println!("Loading initial data...");
    for source_change in get_initial_data() {
        _ = query.process_source_change(source_change).await;
    }
    println!("Initial data loaded.");

    let mut rng = rand::thread_rng();
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                break;
            }
            _ = tokio::time::sleep(tokio::time::Duration::from_secs(3)) => {
                let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64;

                let new_temp: usize = rng.gen_range(0..31);
                println!("Component1 - temperature: {new_temp}");
                process_change(&query, SourceChange::Update {
                    element: Element::Node {
                        metadata: ElementMetadata {
                            reference: ElementReference::new("", "Component1"),
                            labels: Arc::new([Arc::from("Component")]),
                            effective_from: now,
                        },
                        properties: ElementPropertyMap::from(json!({
                            "temperature": new_temp
                        }))
                    }
                }).await;

                let new_temp: usize = rng.gen_range(0..31);
                println!("Component2 - temperature: {new_temp}");
                process_change(&query, SourceChange::Update {
                    element: Element::Node {
                        metadata: ElementMetadata {
                            reference: ElementReference::new("", "Component2"),
                            labels: Arc::new([Arc::from("Component")]),
                            effective_from: now,
                        },
                        properties: ElementPropertyMap::from(json!({
                            "temperature": new_temp
                        }))
                    }
                }).await;

            }
        }
    }
}

#[allow(clippy::print_stdout, clippy::unwrap_used)]
async fn process_change(query: &ContinuousQuery, change: SourceChange) {
    let result = query.process_source_change(change).await.unwrap();
    println!("Results affected: {:?}", result.len());
    for context in result {
        match context {
            QueryPartEvaluationContext::Adding { after, .. } => {
                println!("Adding: {after:?}");
            }
            QueryPartEvaluationContext::Removing { before, .. } => {
                println!("Removing: {before:?}");
            }
            QueryPartEvaluationContext::Updating { before, after, .. } => {
                println!("Updating: {before:?} -> {after:?}");
            }
            _ => {}
        }
    }
}

#[allow(clippy::print_stdout, clippy::unwrap_used)]
fn get_initial_data() -> Vec<SourceChange> {
    vec![
        SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("", "Component1"),
                    labels: Arc::new([Arc::from("Component")]),
                    effective_from: 0,
                },
                properties: ElementPropertyMap::from(json!({
                    "name": "Component1",
                    "temperature": 10
                })),
            },
        },
        SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("", "Component2"),
                    labels: Arc::new([Arc::from("Component")]),
                    effective_from: 0,
                },
                properties: ElementPropertyMap::from(json!({
                    "name": "Component2",
                    "temperature": 15
                })),
            },
        },
        SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("", "Limit1"),
                    labels: Arc::new([Arc::from("Limit")]),
                    effective_from: 0,
                },
                properties: ElementPropertyMap::from(json!({
                    "max_temperature": 20
                })),
            },
        },
        SourceChange::Insert {
            element: Element::Relation {
                metadata: ElementMetadata {
                    reference: ElementReference::new("", "Component1-Limit1"),
                    labels: Arc::new([Arc::from("HAS_LIMIT")]),
                    effective_from: 0,
                },
                properties: ElementPropertyMap::new(),
                out_node: ElementReference::new("", "Limit1"),
                in_node: ElementReference::new("", "Component1"),
            },
        },
        SourceChange::Insert {
            element: Element::Relation {
                metadata: ElementMetadata {
                    reference: ElementReference::new("", "Component2-Limit1"),
                    labels: Arc::new([Arc::from("HAS_LIMIT")]),
                    effective_from: 0,
                },
                properties: ElementPropertyMap::new(),
                out_node: ElementReference::new("", "Component2"),
                in_node: ElementReference::new("", "Limit1"),
            },
        },
    ]
}

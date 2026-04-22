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

use serde_json::json;

use drasi_core::{
    evaluation::{
        context::QueryPartEvaluationContext, functions::FunctionRegistry,
        variable_value::VariableValue,
    },
    models::{Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange},
    query::{ContinuousQuery, QueryBuilder},
};
use drasi_functions_cypher::CypherFunctionSet;
use drasi_query_cypher::CypherParser;

use self::data::get_bootstrap_data;
use super::{contains_data, IGNORED_ROW_SIGNATURE};
use crate::QueryTestConfig;

mod data;
mod queries;

macro_rules! variablemap {
  ($( $key: expr => $val: expr ),*) => {{
       let mut map = ::std::collections::BTreeMap::new();
       $( map.insert($key.to_string().into(), $val); )*
       map
  }}
}

async fn bootstrap_query(query: &ContinuousQuery) -> Vec<QueryPartEvaluationContext> {
    let data = get_bootstrap_data();
    let mut all_results = Vec::new();
    for change in data {
        let mut results = query.process_source_change(change).await.unwrap();
        all_results.append(&mut results);
    }
    all_results
}

/// Test STARTS WITH: bootstrap data includes "DrasiDB Enterprise" which starts with "Drasi".
/// Asserts that the bootstrap produces exactly that matching row, then inserts additional
/// products and verifies matches and non-matches.
#[allow(clippy::print_stdout, clippy::unwrap_used)]
pub async fn starts_with(config: &(impl QueryTestConfig + Send)) {
    let query = {
        let function_registry = Arc::new(FunctionRegistry::new()).with_cypher_function_set();
        let parser = Arc::new(CypherParser::new(function_registry.clone()));
        let mut builder = QueryBuilder::new(queries::starts_with_query(), parser)
            .with_function_registry(function_registry);
        builder = config.config_query(builder).await;
        builder.build().await
    };

    let bootstrap_results = bootstrap_query(&query).await;
    println!("starts_with bootstrap results: {bootstrap_results:?}");
    // p1 "DrasiDB Enterprise" matches STARTS WITH 'Drasi'
    assert_eq!(bootstrap_results.len(), 1);
    assert!(contains_data(
        &bootstrap_results,
        &QueryPartEvaluationContext::Adding {
            after: variablemap!(
                "name" => VariableValue::from(json!("DrasiDB Enterprise"))
            ),
            row_signature: IGNORED_ROW_SIGNATURE,
        }
    ));

    // Insert a new product that starts with "Drasi"
    {
        let change = SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test_source", "p4"),
                    labels: Arc::new([Arc::from("Product")]),
                    effective_from: 100,
                },
                properties: ElementPropertyMap::from(
                    json!({ "id": "p4", "name": "DrasiQL Parser", "category": "tooling" }),
                ),
            },
        };

        let result = query.process_source_change(change).await.unwrap();
        println!("starts_with insert result: {result:?}");
        assert_eq!(result.len(), 1);
        assert!(contains_data(
            &result,
            &QueryPartEvaluationContext::Adding {
                after: variablemap!(
                    "name" => VariableValue::from(json!("DrasiQL Parser"))
                ),
                row_signature: IGNORED_ROW_SIGNATURE,
            }
        ));
    }

    // Insert a product that does NOT start with "Drasi"
    {
        let change = SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test_source", "p5"),
                    labels: Arc::new([Arc::from("Product")]),
                    effective_from: 200,
                },
                properties: ElementPropertyMap::from(
                    json!({ "id": "p5", "name": "Other Product", "category": "misc" }),
                ),
            },
        };

        let result = query.process_source_change(change).await.unwrap();
        println!("starts_with non-match insert result: {result:?}");
        assert_eq!(result.len(), 0);
    }
}

/// Test ENDS WITH: "Quick Query Tool" ends with "Tool".
/// Bootstrap asserts that p2 is matched; subsequent inserts test positive and negative cases.
#[allow(clippy::print_stdout, clippy::unwrap_used)]
pub async fn ends_with(config: &(impl QueryTestConfig + Send)) {
    let query = {
        let function_registry = Arc::new(FunctionRegistry::new()).with_cypher_function_set();
        let parser = Arc::new(CypherParser::new(function_registry.clone()));
        let mut builder = QueryBuilder::new(queries::ends_with_query(), parser)
            .with_function_registry(function_registry);
        builder = config.config_query(builder).await;
        builder.build().await
    };

    let bootstrap_results = bootstrap_query(&query).await;
    println!("ends_with bootstrap results: {bootstrap_results:?}");
    // p2 "Quick Query Tool" matches ENDS WITH 'Tool'
    assert_eq!(bootstrap_results.len(), 1);
    assert!(contains_data(
        &bootstrap_results,
        &QueryPartEvaluationContext::Adding {
            after: variablemap!(
                "name" => VariableValue::from(json!("Quick Query Tool"))
            ),
            row_signature: IGNORED_ROW_SIGNATURE,
        }
    ));

    // Insert a product that ends with "Tool"
    {
        let change = SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test_source", "p4"),
                    labels: Arc::new([Arc::from("Product")]),
                    effective_from: 100,
                },
                properties: ElementPropertyMap::from(
                    json!({ "id": "p4", "name": "Debug Tool", "category": "tooling" }),
                ),
            },
        };

        let result = query.process_source_change(change).await.unwrap();
        println!("ends_with insert result: {result:?}");
        assert_eq!(result.len(), 1);
        assert!(contains_data(
            &result,
            &QueryPartEvaluationContext::Adding {
                after: variablemap!(
                    "name" => VariableValue::from(json!("Debug Tool"))
                ),
                row_signature: IGNORED_ROW_SIGNATURE,
            }
        ));
    }

    // Insert a product that does NOT end with "Tool"
    {
        let change = SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test_source", "p5"),
                    labels: Arc::new([Arc::from("Product")]),
                    effective_from: 200,
                },
                properties: ElementPropertyMap::from(
                    json!({ "id": "p5", "name": "Toolbox Suite", "category": "misc" }),
                ),
            },
        };

        let result = query.process_source_change(change).await.unwrap();
        println!("ends_with non-match insert result: {result:?}");
        assert_eq!(result.len(), 0);
    }
}

/// Test CONTAINS: "Quick Query Tool" contains "Query".
/// Bootstrap asserts that p2 is matched; subsequent inserts test positive and negative cases.
#[allow(clippy::print_stdout, clippy::unwrap_used)]
pub async fn contains_op(config: &(impl QueryTestConfig + Send)) {
    let query = {
        let function_registry = Arc::new(FunctionRegistry::new()).with_cypher_function_set();
        let parser = Arc::new(CypherParser::new(function_registry.clone()));
        let mut builder = QueryBuilder::new(queries::contains_query(), parser)
            .with_function_registry(function_registry);
        builder = config.config_query(builder).await;
        builder.build().await
    };

    let bootstrap_results = bootstrap_query(&query).await;
    println!("contains_op bootstrap results: {bootstrap_results:?}");
    // p2 "Quick Query Tool" matches CONTAINS 'Query'
    assert_eq!(bootstrap_results.len(), 1);
    assert!(contains_data(
        &bootstrap_results,
        &QueryPartEvaluationContext::Adding {
            after: variablemap!(
                "name" => VariableValue::from(json!("Quick Query Tool"))
            ),
            row_signature: IGNORED_ROW_SIGNATURE,
        }
    ));

    // Insert a product that contains "Query"
    {
        let change = SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test_source", "p4"),
                    labels: Arc::new([Arc::from("Product")]),
                    effective_from: 100,
                },
                properties: ElementPropertyMap::from(
                    json!({ "id": "p4", "name": "MyQuery Engine", "category": "database" }),
                ),
            },
        };

        let result = query.process_source_change(change).await.unwrap();
        println!("contains insert result: {result:?}");
        assert_eq!(result.len(), 1);
        assert!(contains_data(
            &result,
            &QueryPartEvaluationContext::Adding {
                after: variablemap!(
                    "name" => VariableValue::from(json!("MyQuery Engine"))
                ),
                row_signature: IGNORED_ROW_SIGNATURE,
            }
        ));
    }

    // Insert a product that does NOT contain "Query"
    {
        let change = SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test_source", "p5"),
                    labels: Arc::new([Arc::from("Product")]),
                    effective_from: 200,
                },
                properties: ElementPropertyMap::from(
                    json!({ "id": "p5", "name": "Simple Lookup", "category": "misc" }),
                ),
            },
        };

        let result = query.process_source_change(change).await.unwrap();
        println!("contains non-match insert result: {result:?}");
        assert_eq!(result.len(), 0);
    }
}

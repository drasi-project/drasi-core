#![allow(clippy::unwrap_used)]
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
    query::QueryBuilder,
};
use drasi_functions_cypher::CypherFunctionSet;
use drasi_query_cypher::CypherParser;

use crate::QueryTestConfig;

mod queries;

macro_rules! variablemap {
  ($( $key: expr => $val: expr ),*) => {{
       let mut map = ::std::collections::BTreeMap::new();
       $( map.insert($key.to_string().into(), $val); )*
       map
  }}
}

pub async fn prev_unique(config: &(impl QueryTestConfig + Send)) {
    let opt_query = {
        let function_registry = Arc::new(FunctionRegistry::new()).with_cypher_function_set();
        let parser = Arc::new(CypherParser::new(function_registry.clone()));
        let mut builder = QueryBuilder::new(queries::prev_unique_query(), parser)
            .with_function_registry(function_registry);
        builder = config.config_query(builder).await;
        builder.build().await
    };

    //Add contract 1
    {
        println!("--- Inserting contract c1 with pending status ---");
        let change = SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "c1"),
                    labels: Arc::new([Arc::from("Contract")]),
                    effective_from: 0,
                },
                properties: ElementPropertyMap::from(json!({
                    "id": "c1",
                    "status": "pending",
                    "tags": "tag1"
                })),
            },
        };

        let result = opt_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        assert_eq!(result.len(), 0);        
    }
    
    //update contract 1 with active status
    {
        println!("--- Updating contract c1 with active status ---");
        let change = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "c1"),
                    labels: Arc::new([Arc::from("Contract")]),
                    effective_from: 0,
                },
                properties: ElementPropertyMap::from(json!({
                    "id": "c1",
                    "status": "active",
                    "tags": "tag1"
                })),
            },
        };

        let result = opt_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        
        assert_eq!(result.len(), 1);

        assert!(result.contains(&QueryPartEvaluationContext::Adding {
            after: variablemap!(
              "tags" => VariableValue::from(json!("tag1")),
              "status" => VariableValue::from(json!("active")),
              "id" => VariableValue::from(json!("c1"))
            ),
        }));
    }

    //update contract 1 tags
    {
        println!("--- Updating contract c1 tags ---");
        let change = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "c1"),
                    labels: Arc::new([Arc::from("Contract")]),
                    effective_from: 0,
                },
                properties: ElementPropertyMap::from(json!({
                    "id": "c1",
                    "status": "active",
                    "tags": "tag2"
                })),
            },
        };

        let result = opt_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        
        assert_eq!(result.len(), 1);

        println!("Result: {:#?}", result);

        assert!(result.contains(&QueryPartEvaluationContext::Updating { 
            before: variablemap!(
              "tags" => VariableValue::from(json!("tag1")),
              "status" => VariableValue::from(json!("active")),
              "id" => VariableValue::from(json!("c1"))
            ),
            after: variablemap!(
              "tags" => VariableValue::from(json!("tag2")),
              "status" => VariableValue::from(json!("active")),
              "id" => VariableValue::from(json!("c1"))
            ),
        }));
    }

    //Add contract 2
    {
        println!("--- Inserting contract c2 with cancelled status ---");
        let change = SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "c2"),
                    labels: Arc::new([Arc::from("Contract")]),
                    effective_from: 0,
                },
                properties: ElementPropertyMap::from(json!({
                    "id": "c2",
                    "status": "cancelled",
                    "tags": "tag1"
                })),
            },
        };

        let result = opt_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        assert_eq!(result.len(), 0);        
    }
    
    //update contract 2 with active status
    {
        println!("--- Updating contract c2 with active status ---");
        let change = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "c2"),
                    labels: Arc::new([Arc::from("Contract")]),
                    effective_from: 0,
                },
                properties: ElementPropertyMap::from(json!({
                    "id": "c2",
                    "status": "active",
                    "tags": "tag1"
                })),
            },
        };

        let result = opt_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        
        assert_eq!(result.len(), 0);
    }

    //update contract 2 with pending status
    {
        println!("--- Updating contract c2 with pending status ---");
        let change = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "c2"),
                    labels: Arc::new([Arc::from("Contract")]),
                    effective_from: 0,
                },
                properties: ElementPropertyMap::from(json!({
                    "id": "c2",
                    "status": "pending",
                    "tags": "tag1"
                })),
            },
        };

        let result = opt_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        
        assert_eq!(result.len(), 0);        
    }

    //update contract 2 with active status
    {
        println!("--- Updating contract c2 with active status ---");
        let change = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "c2"),
                    labels: Arc::new([Arc::from("Contract")]),
                    effective_from: 0,
                },
                properties: ElementPropertyMap::from(json!({
                    "id": "c2",
                    "status": "active",
                    "tags": "tag1"
                })),
            },
        };

        let result = opt_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        
        assert_eq!(result.len(), 1);
        assert!(result.contains(&QueryPartEvaluationContext::Adding {
            after: variablemap!(
              "tags" => VariableValue::from(json!("tag1")),
              "status" => VariableValue::from(json!("active")),
              "id" => VariableValue::from(json!("c2"))
            ),
        }));
    }

}

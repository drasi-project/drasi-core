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

pub async fn before_value(config: &(impl QueryTestConfig + Send)) {
    let opt_query = {
        let function_registry = Arc::new(FunctionRegistry::new()).with_cypher_function_set();
        let parser = Arc::new(CypherParser::new(function_registry.clone()));
        let mut builder = QueryBuilder::new(queries::increasing_value_query(), parser)
            .with_function_registry(function_registry);
        builder = config.config_query(builder).await;
        builder.build().await
    };

    //Add invoice 1
    {
        let change = SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "i1"),
                    labels: Arc::new([Arc::from("Invoice")]),
                    effective_from: 0,
                },
                properties: ElementPropertyMap::from(json!({
                    "id": "i1",
                    "amount": 100,
                })),
            },
        };

        let result = opt_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        assert_eq!(result.len(), 0);
    }

    //Add invoice 2
    {
        let change = SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "i2"),
                    labels: Arc::new([Arc::from("Invoice")]),
                    effective_from: 0,
                },
                properties: ElementPropertyMap::from(json!({
                    "id": "i2",
                    "amount": 200,
                })),
            },
        };

        let result = opt_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        assert_eq!(result.len(), 0);
    }

    //update invoice 1 with increasing amount
    {
        let change = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "i1"),
                    labels: Arc::new([Arc::from("Invoice")]),
                    effective_from: 0,
                },
                properties: ElementPropertyMap::from(json!({
                    "id": "i1",
                    "amount": 105,
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
              "amount" => VariableValue::from(json!(105.0)),
              "id" => VariableValue::from(json!("i1"))
            ),
        }));
    }

    //update invoice 1 with increasing amount
    {
        let change = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "i1"),
                    labels: Arc::new([Arc::from("Invoice")]),
                    effective_from: 0,
                },
                properties: ElementPropertyMap::from(json!({
                    "id": "i1",
                    "amount": 110,
                })),
            },
        };

        let result = opt_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        assert_eq!(result.len(), 1);

        assert!(result.contains(&QueryPartEvaluationContext::Updating {
            before: variablemap!(
              "amount" => VariableValue::from(json!(105.0)),
              "id" => VariableValue::from(json!("i1"))
            ),
            after: variablemap!(
              "amount" => VariableValue::from(json!(110.0)),
              "id" => VariableValue::from(json!("i1"))
            ),
        }));
    }

    //update invoice 1 with decreasing amount
    {
        let change = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "i1"),
                    labels: Arc::new([Arc::from("Invoice")]),
                    effective_from: 0,
                },
                properties: ElementPropertyMap::from(json!({
                    "id": "i1",
                    "amount": 90,
                })),
            },
        };

        let result = opt_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        assert_eq!(result.len(), 1);

        assert!(result.contains(&QueryPartEvaluationContext::Removing {
            before: variablemap!(
              "amount" => VariableValue::from(json!(110.0)),
              "id" => VariableValue::from(json!("i1"))
            )
        }));
    }

    //update invoice 1 with increasing amount
    {
        let change = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "i1"),
                    labels: Arc::new([Arc::from("Invoice")]),
                    effective_from: 0,
                },
                properties: ElementPropertyMap::from(json!({
                    "id": "i1",
                    "amount": 95,
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
              "amount" => VariableValue::from(json!(95.0)),
              "id" => VariableValue::from(json!("i1"))
            )
        }));
    }
}

pub async fn before_sum(config: &(impl QueryTestConfig + Send)) {
    let opt_query = {
        let function_registry = Arc::new(FunctionRegistry::new()).with_cypher_function_set();
        let parser = Arc::new(CypherParser::new(function_registry.clone()));
        let mut builder = QueryBuilder::new(queries::increasing_sum_query(), parser)
            .with_function_registry(function_registry);
        builder = config.config_query(builder).await;
        builder.build().await
    };

    //Add invoice 1
    {
        let change = SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "i1"),
                    labels: Arc::new([Arc::from("Invoice")]),
                    effective_from: 0,
                },
                properties: ElementPropertyMap::from(json!({
                    "id": "i1",
                    "amount": 100,
                })),
            },
        };

        let result = opt_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        assert_eq!(result.len(), 0);
    }

    //Add relation 1
    {
        let change = SourceChange::Insert {
            element: Element::Relation {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "r1"),
                    labels: Arc::new([Arc::from("HAS")]),
                    effective_from: 0,
                },
                in_node: ElementReference::new("test", "i1"),
                out_node: ElementReference::new("test", "l1"),
                properties: ElementPropertyMap::from(json!({
                    "id": "r1",
                })),
            },
        };

        let result = opt_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        assert_eq!(result.len(), 0);
    }

    //insert invoice line with increasing amount
    {
        let change = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "l1"),
                    labels: Arc::new([Arc::from("LineItem")]),
                    effective_from: 0,
                },
                properties: ElementPropertyMap::from(json!({
                    "id": "l1",
                    "amount": 10,
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
              "id" => VariableValue::from(json!("i1"))
            ),
        }));
    }

    //Add relation 2
    {
        let change = SourceChange::Insert {
            element: Element::Relation {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "r2"),
                    labels: Arc::new([Arc::from("HAS")]),
                    effective_from: 0,
                },
                in_node: ElementReference::new("test", "i1"),
                out_node: ElementReference::new("test", "l2"),
                properties: ElementPropertyMap::from(json!({
                    "id": "r2",
                })),
            },
        };

        let result = opt_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        assert_eq!(result.len(), 0);
    }

    //insert invoice line with decreasing amount
    {
        let change = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "l2"),
                    labels: Arc::new([Arc::from("LineItem")]),
                    effective_from: 0,
                },
                properties: ElementPropertyMap::from(json!({
                    "id": "l2",
                    "amount": -5,
                })),
            },
        };

        let result = opt_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        assert_eq!(result.len(), 1);

        assert!(result.contains(&QueryPartEvaluationContext::Removing {
            before: variablemap!(
              "id" => VariableValue::from(json!("i1"))
            ),
        }));
    }
}

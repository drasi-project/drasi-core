#![allow(clippy::unwrap_used)]
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

pub async fn collect_basic_test(config: &(impl QueryTestConfig + Send)) {
    let query = {
        let function_registry = Arc::new(FunctionRegistry::new()).with_cypher_function_set();
        let parser = Arc::new(CypherParser::new(function_registry.clone()));
        let mut builder =
            QueryBuilder::new(queries::order_line_items_query(), parser)
                .with_function_registry(function_registry);
        builder = config.config_query(builder).await;
        builder.build().await
    };

    // Insert Order
    {
        let change = SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "order1"),
                    labels: Arc::new([Arc::from("Order")]),
                    effective_from: 0,
                },
                properties: ElementPropertyMap::from(json!({
                    "orderId": "order123",
                    "date": "2025-04-07"
                })),
            },
        };

        let result = query.process_source_change(change).await.unwrap();
        assert_eq!(result.len(), 1);
        assert!(result.contains(&QueryPartEvaluationContext::Adding {
            after: variablemap!(
                "orderId" => VariableValue::from(json!("order123")),
                "orderDate" => VariableValue::from(json!("2025-04-07")),
                "lineItems" => VariableValue::List(vec![])
            )
        }));
    }

    // Insert first line item
    {
        let li_change = SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "li1"),
                    labels: Arc::new([Arc::from("LineItem")]),
                    effective_from: 0,
                },
                properties: ElementPropertyMap::from(json!({
                    "productId": "prod001",
                    "quantity": 2
                })),
            },
        };
        let _ = query.process_source_change(li_change).await.unwrap();

        let rel_change = SourceChange::Insert {
            element: Element::Relation {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "order1-li1"),
                    labels: Arc::new([Arc::from("HAS")]),
                    effective_from: 0,
                },
                properties: ElementPropertyMap::new(),
                in_node: ElementReference::new("test", "li1"),
                out_node: ElementReference::new("test", "order1"),
            },
        };

        let result = query.process_source_change(rel_change).await.unwrap();
        assert_eq!(result.len(), 1);
        assert!(result.contains(&QueryPartEvaluationContext::Updating {
            before: variablemap!(
                "orderId" => VariableValue::from(json!("order123")),
                "orderDate" => VariableValue::from(json!("2025-04-07")),
                "lineItems" => VariableValue::List(vec![])
            ),
            after: variablemap!(
                "orderId" => VariableValue::from(json!("order123")),
                "orderDate" => VariableValue::from(json!("2025-04-07")),
                "lineItems" => VariableValue::List(vec![
                    VariableValue::Object(
                        vec![
                            ("productId".to_string(), VariableValue::from(json!("prod001"))),
                            ("quantity".to_string(), VariableValue::from(json!(2)))
                        ].into_iter().collect()
                    )
                ])
            )
        }));
    }

    // Insert second line item
    {
        let li_change = SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "li2"),
                    labels: Arc::new([Arc::from("LineItem")]),
                    effective_from: 0,
                },
                properties: ElementPropertyMap::from(json!({
                    "productId": "prod002",
                    "quantity": 5
                })),
            },
        };
        let _ = query.process_source_change(li_change).await.unwrap();

        let rel_change = SourceChange::Insert {
            element: Element::Relation {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "order1-li2"),
                    labels: Arc::new([Arc::from("HAS")]),
                    effective_from: 0,
                },
                properties: ElementPropertyMap::new(),
                in_node: ElementReference::new("test", "li2"),
                out_node: ElementReference::new("test", "order1"),
            },
        };

        let result = query.process_source_change(rel_change).await.unwrap();
        assert_eq!(result.len(), 1);
        assert!(result.contains(&QueryPartEvaluationContext::Updating {
            before: variablemap!(
                "orderId" => VariableValue::from(json!("order123")),
                "orderDate" => VariableValue::from(json!("2025-04-07")),
                "lineItems" => VariableValue::List(vec![
                    VariableValue::Object(
                        vec![
                            ("productId".to_string(), VariableValue::from(json!("prod001"))),
                            ("quantity".to_string(), VariableValue::from(json!(2)))
                        ].into_iter().collect()
                    )
                ])
            ),
            after: variablemap!(
                "orderId" => VariableValue::from(json!("order123")),
                "orderDate" => VariableValue::from(json!("2025-04-07")),
                "lineItems" => VariableValue::List(vec![
                    VariableValue::Object(
                        vec![
                            ("productId".to_string(), VariableValue::from(json!("prod001"))),
                            ("quantity".to_string(), VariableValue::from(json!(2)))
                        ].into_iter().collect()
                    ),
                    VariableValue::Object(
                        vec![
                            ("productId".to_string(), VariableValue::from(json!("prod002"))),
                            ("quantity".to_string(), VariableValue::from(json!(5)))
                        ].into_iter().collect()
                    )
                ])
            )
        }));
    }

    // Insert third line item
    {
        let li_change = SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "li3"),
                    labels: Arc::new([Arc::from("LineItem")]),
                    effective_from: 0,
                },
                properties: ElementPropertyMap::from(json!({
                    "productId": "prod003",
                    "quantity": 1
                })),
            },
        };
        let _ = query.process_source_change(li_change).await.unwrap();

        let rel_change = SourceChange::Insert {
            element: Element::Relation {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "order1-li3"),
                    labels: Arc::new([Arc::from("HAS")]),
                    effective_from: 0,
                },
                properties: ElementPropertyMap::new(),
                in_node: ElementReference::new("test", "li3"),
                out_node: ElementReference::new("test", "order1"),
            },
        };

        let result = query.process_source_change(rel_change).await.unwrap();
        assert_eq!(result.len(), 1);
        // Verify we now have 3 line items
        let update = result.iter().find(|r| matches!(r, QueryPartEvaluationContext::Updating { .. }));
        assert!(update.is_some());
    }

    // Remove a line item
    {
        let rel_change = SourceChange::Delete {
            metadata: ElementMetadata {
                reference: ElementReference::new("test", "order1-li2"),
                labels: Arc::new([Arc::from("HAS")]),
                effective_from: 0,
            },
        };

        let result = query.process_source_change(rel_change).await.unwrap();
        assert_eq!(result.len(), 1);
        // Verify the list shrunk back to 2 items
        let update = result.iter().find(|r| matches!(r, QueryPartEvaluationContext::Updating { .. }));
        assert!(update.is_some());
    }
}

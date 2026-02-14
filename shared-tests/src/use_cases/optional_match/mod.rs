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

pub async fn optional_match(config: &(impl QueryTestConfig + Send)) {
    let opt_query = {
        let function_registry = Arc::new(FunctionRegistry::new()).with_cypher_function_set();
        let parser = Arc::new(CypherParser::new(function_registry.clone()));
        let mut builder = QueryBuilder::new(queries::optional_query(), parser)
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
        assert_eq!(result.len(), 1);
        assert!(result.contains(&QueryPartEvaluationContext::Adding {
            after: variablemap!(
              "amount" => VariableValue::from(json!(100.0)),
              "id" => VariableValue::from(json!("i1")),
              "payment_amount" => VariableValue::Null
            ),
        }));
    }

    //Add relation 1
    {
        let change = SourceChange::Insert {
            element: Element::Relation {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "r1"),
                    labels: Arc::new([Arc::from("RECONCILED_TO")]),
                    effective_from: 0,
                },
                in_node: ElementReference::new("test", "i1"),
                out_node: ElementReference::new("test", "p1"),
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

    //Add payment 1
    {
        let change = SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "p1"),
                    labels: Arc::new([Arc::from("Payment")]),
                    effective_from: 0,
                },
                properties: ElementPropertyMap::from(json!({
                    "id": "p1",
                    "amount": 70,
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
              "amount" => VariableValue::from(json!(100.0)),
              "id" => VariableValue::from(json!("i1")),
              "payment_amount" => VariableValue::Null
            ),
            after: variablemap!(
              "amount" => VariableValue::from(json!(100.0)),
              "id" => VariableValue::from(json!("i1")),
              "payment_amount" => VariableValue::from(json!(70.0))
            ),
        }));
    }

    //Add relation 2
    {
        let change = SourceChange::Insert {
            element: Element::Relation {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "r2"),
                    labels: Arc::new([Arc::from("RECONCILED_TO")]),
                    effective_from: 0,
                },
                in_node: ElementReference::new("test", "i1"),
                out_node: ElementReference::new("test", "p2"),
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

    //Add payment 2
    {
        let change = SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "p2"),
                    labels: Arc::new([Arc::from("Payment")]),
                    effective_from: 0,
                },
                properties: ElementPropertyMap::from(json!({
                    "id": "p2",
                    "amount": 10,
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
              "amount" => VariableValue::from(json!(100.0)),
              "id" => VariableValue::from(json!("i1")),
              "payment_amount" => VariableValue::Null
            ),
            after: variablemap!(
              "amount" => VariableValue::from(json!(100.0)),
              "id" => VariableValue::from(json!("i1")),
              "payment_amount" => VariableValue::from(json!(10.0))
            ),
        }));
    }

    //update payment 2
    {
        let change = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "p2"),
                    labels: Arc::new([Arc::from("Payment")]),
                    effective_from: 0,
                },
                properties: ElementPropertyMap::from(json!({
                    "id": "p2",
                    "amount": 15,
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
              "amount" => VariableValue::from(json!(100.0)),
              "id" => VariableValue::from(json!("i1")),
                "payment_amount" => VariableValue::from(json!(10.0))
            ),
            after: variablemap!(
              "amount" => VariableValue::from(json!(100.0)),
              "id" => VariableValue::from(json!("i1")),
              "payment_amount" => VariableValue::from(json!(15.0))
            ),
        }));
    }

    //delete payment 2
    {
        let change = SourceChange::Delete {
            metadata: ElementMetadata {
                reference: ElementReference::new("test", "p2"),
                labels: Arc::new([Arc::from("Payment")]),
                effective_from: 0,
            },
        };

        let result = opt_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        assert_eq!(result.len(), 1);
        assert!(result.contains(&QueryPartEvaluationContext::Updating {
            before: variablemap!(
              "amount" => VariableValue::from(json!(100.0)),
              "id" => VariableValue::from(json!("i1")),
                "payment_amount" => VariableValue::from(json!(15.0))
            ),
            after: variablemap!(
              "amount" => VariableValue::from(json!(100.0)),
              "id" => VariableValue::from(json!("i1")),
              "payment_amount" => VariableValue::Null
            ),
        }));
    }
}

pub async fn optional_match_aggregating(config: &(impl QueryTestConfig + Send)) {
    let opt_query = {
        let function_registry = Arc::new(FunctionRegistry::new()).with_cypher_function_set();
        let parser = Arc::new(CypherParser::new(function_registry.clone()));
        let mut builder = QueryBuilder::new(queries::optional_query_aggregating(), parser)
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
        assert_eq!(result.len(), 1);
        assert!(result.contains(&QueryPartEvaluationContext::Aggregation {
            grouping_keys: vec!["id".to_string(), "amount".to_string()],
            default_before: true,
            default_after: false,
            before: Some(variablemap!(
                "amount" => VariableValue::from(json!(100.0)),
                "balance" => VariableValue::from(json!(100.0)),
                "payment_amount" => VariableValue::from(json!(0.0)),
                "id" => VariableValue::from(json!("i1"))

            )),
            after: variablemap!(
                "amount" => VariableValue::from(json!(100.0)),
                "balance" => VariableValue::from(json!(100.0)),
                "payment_amount" => VariableValue::from(json!(0.0)),
                "id" => VariableValue::from(json!("i1"))
            ),
        }));
    }

    //Add relation 1
    {
        let change = SourceChange::Insert {
            element: Element::Relation {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "r1"),
                    labels: Arc::new([Arc::from("RECONCILED_TO")]),
                    effective_from: 0,
                },
                in_node: ElementReference::new("test", "i1"),
                out_node: ElementReference::new("test", "p1"),
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

    //Add payment 1
    {
        let change = SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "p1"),
                    labels: Arc::new([Arc::from("Payment")]),
                    effective_from: 0,
                },
                properties: ElementPropertyMap::from(json!({
                    "id": "p1",
                    "amount": 70,
                })),
            },
        };

        let result = opt_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        assert_eq!(result.len(), 1);
        assert!(result.contains(&QueryPartEvaluationContext::Aggregation {
            grouping_keys: vec!["id".to_string(), "amount".to_string()],
            default_before: false,
            default_after: false,
            before: Some(variablemap!(
                "amount" => VariableValue::from(json!(100.0)),
                "balance" => VariableValue::from(json!(100.0)),
                "payment_amount" => VariableValue::from(json!(0.0)),
                "id" => VariableValue::from(json!("i1"))

            )),
            after: variablemap!(
                "amount" => VariableValue::from(json!(100.0)),
                "balance" => VariableValue::from(json!(30.0)),
                "payment_amount" => VariableValue::from(json!(70.0)),
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
                    labels: Arc::new([Arc::from("RECONCILED_TO")]),
                    effective_from: 0,
                },
                in_node: ElementReference::new("test", "i1"),
                out_node: ElementReference::new("test", "p2"),
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

    //Add payment 2
    {
        let change = SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "p2"),
                    labels: Arc::new([Arc::from("Payment")]),
                    effective_from: 0,
                },
                properties: ElementPropertyMap::from(json!({
                    "id": "p2",
                    "amount": 30,
                })),
            },
        };

        let result = opt_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        assert_eq!(result.len(), 1);
        assert!(result.contains(&QueryPartEvaluationContext::Aggregation {
            grouping_keys: vec!["id".to_string(), "amount".to_string()],
            default_before: false,
            default_after: false,
            before: Some(variablemap!(
                "amount" => VariableValue::from(json!(100.0)),
                "balance" => VariableValue::from(json!(30.0)),
                "payment_amount" => VariableValue::from(json!(70.0)),
                "id" => VariableValue::from(json!("i1"))

            )),
            after: variablemap!(
                "amount" => VariableValue::from(json!(100.0)),
                "balance" => VariableValue::from(json!(0.0)),
                "payment_amount" => VariableValue::from(json!(100.0)),
                "id" => VariableValue::from(json!("i1"))
            ),
        }));
    }
}

pub async fn multi_optional_match(config: &(impl QueryTestConfig + Send)) {
    let opt_query = {
        let function_registry = Arc::new(FunctionRegistry::new()).with_cypher_function_set();
        let parser = Arc::new(CypherParser::new(function_registry.clone()));
        let mut builder = QueryBuilder::new(queries::multi_optional_query(), parser)
            .with_function_registry(function_registry);
        builder = config.config_query(builder).await;
        builder.build().await
    };

    //Add customer 1
    {
        let change = SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "c1"),
                    labels: Arc::new([Arc::from("Customer")]),
                    effective_from: 0,
                },
                properties: ElementPropertyMap::from(json!({
                    "id": "c1",
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
                "customer_id" => VariableValue::from(json!("c1")),
                "invoice_id" => VariableValue::Null,
                "payment_id" => VariableValue::Null
            ),
        }));
    }

    //Add has 1
    {
        let change = SourceChange::Insert {
            element: Element::Relation {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "h1"),
                    labels: Arc::new([Arc::from("HAS")]),
                    effective_from: 0,
                },
                in_node: ElementReference::new("test", "c1"),
                out_node: ElementReference::new("test", "i1"),
                properties: ElementPropertyMap::from(json!({
                    "id": "h1",
                })),
            },
        };

        let result = opt_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        assert_eq!(result.len(), 0);
    }

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
        assert_eq!(result.len(), 1);
        assert!(result.contains(&QueryPartEvaluationContext::Updating {
            before: variablemap!(
                "customer_id" => VariableValue::from(json!("c1")),
                "invoice_id" => VariableValue::Null,
                "payment_id" => VariableValue::Null
            ),
            after: variablemap!(
                "customer_id" => VariableValue::from(json!("c1")),
                "invoice_id" => VariableValue::from(json!("i1")),
                "payment_id" => VariableValue::Null
            ),
        }));
    }

    //Add relation 1
    {
        let change = SourceChange::Insert {
            element: Element::Relation {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "r1"),
                    labels: Arc::new([Arc::from("RECONCILED_TO")]),
                    effective_from: 0,
                },
                in_node: ElementReference::new("test", "i1"),
                out_node: ElementReference::new("test", "p1"),
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

    //Add payment 1
    {
        let change = SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "p1"),
                    labels: Arc::new([Arc::from("Payment")]),
                    effective_from: 0,
                },
                properties: ElementPropertyMap::from(json!({
                    "id": "p1",
                    "amount": 70,
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
                "customer_id" => VariableValue::from(json!("c1")),
                "invoice_id" => VariableValue::from(json!("i1")),
                "payment_id" => VariableValue::Null
            ),
            after: variablemap!(
                "customer_id" => VariableValue::from(json!("c1")),
                "invoice_id" => VariableValue::from(json!("i1")),
                "payment_id" => VariableValue::from(json!("p1"))
            ),
        }));
    }
}

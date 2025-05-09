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
    evaluation::{context::QueryPartEvaluationContext, variable_value::VariableValue},
    models::{Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange},
    query::QueryBuilder,
};

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
        let mut builder = QueryBuilder::new(queries::optional_query());
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
        println!("Result - Add i1: {:?}", result);
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
        println!("Result - Add r1: {:?}", result);
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
        println!("Result - Add p1: {:?}", result);
        assert!(result.contains(&QueryPartEvaluationContext::Adding {            
            after: variablemap!(
              "amount" => VariableValue::from(json!(100.0)),
              "id" => VariableValue::from(json!("i1")),
              "payment_amount" => VariableValue::from(json!(70.0))
            ),
        }));
    }

}


pub async fn optional_match_aggregating(config: &(impl QueryTestConfig + Send)) {
    let opt_query = {
        let mut builder = QueryBuilder::new(queries::optional_query_aggregating());
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
        //assert_eq!(result.len(), 1);
        println!("Result - Add i1: {:?}", result);
        assert!(result.contains(&QueryPartEvaluationContext::Aggregation {
            grouping_keys: vec![],
            default_before: true,
            default_after: false,
            before: Some(variablemap!(
                "min_value" => VariableValue::Null
            )),
            after: variablemap!(
              "min_value" => VariableValue::from(json!(5.0))
            ),
        }));
    }

}

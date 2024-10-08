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
    query::{ContinuousQuery, QueryBuilder},
};

use self::data::get_bootstrap_data;
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

async fn bootstrap_query(query: &ContinuousQuery) {
    let data = get_bootstrap_data();

    for change in data {
        let _ = query.process_source_change(change).await;
    }
}

// Query identifies when the total number of support calls on any day exceeds 10.
pub async fn greater_than_a_threshold(config: &(impl QueryTestConfig + Send)) {
    let greater_than_a_threshold_query = {
        let mut builder = QueryBuilder::new(queries::greater_than_a_threshold_query())
            .with_joins(queries::greater_than_a_threshold_metadata());
        builder = config.config_query(builder).await;
        builder.build().await
    };

    // Add initial values
    bootstrap_query(&greater_than_a_threshold_query).await;

    // Add a ninth call (5th call by customer_01)
    {
        let change = SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("Reflex.CRM", "call_09"),
                    labels: Arc::new([Arc::from("Call")]),
                    effective_from: 1000,
                },
                properties: ElementPropertyMap::from(
                    json!({ "cust_id": "customer_01", "timestamp": 1696150808, "type": "support" }),
                ), // Call Date: 2023-10-01
            },
        };

        let result = greater_than_a_threshold_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        //println!("Node Result - Add call 09: {:?}", result);
        assert_eq!(result.len(), 0);
    }

    // Add a tenth call (5th call by customer_02)
    {
        let change = SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("Reflex.CRM", "call_10"),
                    labels: Arc::new([Arc::from("Call")]),
                    effective_from: 2000,
                },
                properties: ElementPropertyMap::from(
                    json!({ "cust_id": "customer_02", "timestamp": 1696150809, "type": "support" }),
                ), // Call Date: 2023-10-01
            },
        };

        let result = greater_than_a_threshold_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        // println!("Node Result - Add call 10: {:?}", result);
        assert_eq!(result.len(), 0);
    }

    // Add an eleventh call (6th call by customer_01)
    // This should cause a result to be added to the query
    {
        let change = SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("Reflex.CRM", "call_11"),
                    labels: Arc::new([Arc::from("Call")]),
                    effective_from: 3000,
                },
                properties: ElementPropertyMap::from(
                    json!({ "cust_id": "customer_01", "timestamp": 1696150810, "type": "support" }),
                ), // Call Date: 2023-10-01
            },
        };

        let result = greater_than_a_threshold_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        // println!("Node Result - Add call 11: {:?}", result);
        assert_eq!(result.len(), 1);

        assert!(result.contains(&QueryPartEvaluationContext::Adding {
            after: variablemap!(
              "callYear" => VariableValue::from(json!(2023)),
              "callDayOfYear" => VariableValue::from(json!(274)),
              "callCount" => VariableValue::from(json!(11))
            )
        }));
    }
}

// Query identifies when the total number of support calls for a customer on any day exceeds 5.
pub async fn greater_than_a_threshold_by_customer(config: &(impl QueryTestConfig + Send)) {
    let greater_than_a_threshold_query = {
        let mut builder = QueryBuilder::new(queries::greater_than_a_threshold_by_customer_query())
            .with_joins(queries::greater_than_a_threshold_metadata());
        builder = config.config_query(builder).await;
        builder.build().await
    };

    // Add initial values
    bootstrap_query(&greater_than_a_threshold_query).await;

    // Add a 5th call by customer_01
    {
        let change = SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("Reflex.CRM", "call_09"),
                    labels: Arc::new([Arc::from("Call")]),
                    effective_from: 1000,
                },
                properties: ElementPropertyMap::from(
                    json!({ "cust_id": "customer_01", "timestamp": 1696150808, "type": "support" }),
                ), // Call Date: 2023-10-01
            },
        };

        let result = greater_than_a_threshold_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        // println!("Node Result - Add call 09: {:?}", result);
        assert_eq!(result.len(), 0);
    }

    // Add a 5th call by customer_02
    {
        let change = SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("Reflex.CRM", "call_10"),
                    labels: Arc::new([Arc::from("Call")]),
                    effective_from: 2000,
                },
                properties: ElementPropertyMap::from(
                    json!({ "cust_id": "customer_02", "timestamp": 1696150809, "type": "support" }),
                ), // Call Date: 2023-10-01
            },
        };

        let result = greater_than_a_threshold_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        // println!("Node Result - Add call 10: {:?}", result);
        assert_eq!(result.len(), 0);
    }

    // Add a 6th call by customer_01
    // This should cause a result to be added to the query
    {
        let change = SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("Reflex.CRM", "call_11"),
                    labels: Arc::new([Arc::from("Call")]),
                    effective_from: 3000,
                },
                properties: ElementPropertyMap::from(
                    json!({ "cust_id": "customer_01", "timestamp": 1696150810, "type": "support" }),
                ), // Call Date: 2023-10-01
            },
        };

        let result = greater_than_a_threshold_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        // println!("Rel Result - Add call 11: {:?}", result);
        assert_eq!(result.len(), 1);

        assert!(result.contains(&QueryPartEvaluationContext::Adding {
            after: variablemap!(
              "customerId" => VariableValue::from(json!("customer_01")),
              "customerName" => VariableValue::from(json!("Customer 01")),
              "callYear" => VariableValue::from(json!(2023)),
              "callDayOfYear" => VariableValue::from(json!(274)),
              "callCount" => VariableValue::from(json!(6))
            )
        }));
    }

    // Add a 6th call by customer_02
    // This should cause a result to be added to the query
    {
        let change = SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("Reflex.CRM", "call_12"),
                    labels: Arc::new([Arc::from("Call")]),
                    effective_from: 3000,
                },
                properties: ElementPropertyMap::from(
                    json!({ "cust_id": "customer_02", "timestamp": 1696150811, "type": "support" }),
                ), // Call Date: 2023-10-01
            },
        };

        let result = greater_than_a_threshold_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        // println!("Rel Result - Add call 11: {:?}", result);
        assert_eq!(result.len(), 1);

        assert!(result.contains(&QueryPartEvaluationContext::Adding {
            after: variablemap!(
              "customerId" => VariableValue::from(json!("customer_02")),
              "customerName" => VariableValue::from(json!("Customer 02")),
              "callYear" => VariableValue::from(json!(2023)),
              "callDayOfYear" => VariableValue::from(json!(274)),
              "callCount" => VariableValue::from(json!(6))
            )
        }));
    }
}

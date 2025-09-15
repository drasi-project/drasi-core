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
    query::{ContinuousQuery, QueryBuilder},
};
use drasi_functions_cypher::CypherFunctionSet;
use drasi_query_cypher::CypherParser;

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

#[allow(clippy::print_stdout, clippy::unwrap_used)]
pub async fn steps_happen_in_any_order(config: &(impl QueryTestConfig + Send)) {
    let steps_happen_in_any_order_query = {
        let function_registry = Arc::new(FunctionRegistry::new()).with_cypher_function_set();
        let parser = Arc::new(CypherParser::new(function_registry.clone()));
        let mut builder = QueryBuilder::new(queries::steps_happen_in_any_order_query(), parser)
            .with_function_registry(function_registry)
            .with_joins(queries::steps_happen_in_any_order_metadata());
        builder = config.config_query(builder).await;
        builder.build().await
    };

    // Add initial values
    bootstrap_query(&steps_happen_in_any_order_query).await;

    let mut timestamp = 1696150800; // 2023-10-01 09:00:00

    // Customer 01 - Completes steps but not within 30 Days
    // Add completion of Step X.
    {
        let change = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("Reflex.CRM", "cust_01_step_x"),
                    labels: Arc::new([Arc::from("CompletedStep")]),
                    effective_from: timestamp,
                },
                properties: ElementPropertyMap::from(
                    json!({ "id": "cust_01_step_x", "cust_id": "cust_01", "step_id": "step_x", "timestamp": timestamp }),
                ),
            },
        };

        let result = steps_happen_in_any_order_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        println!("Node Result - Add Completed Step X ({timestamp}): {result:?}");
        assert_eq!(result.len(), 0);

        // Add 10 days to timestamp
        timestamp += 60 * 60 * 24 * 10;
    }

    // Add completion of Step Y.
    {
        let change = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("Reflex.CRM", "cust_01_step_y"),
                    labels: Arc::new([Arc::from("CompletedStep")]),
                    effective_from: timestamp,
                },
                properties: ElementPropertyMap::from(
                    json!({ "id": "cust_01_step_y", "cust_id": "cust_01", "step_id": "step_y", "timestamp": timestamp }),
                ),
            },
        };

        let result = steps_happen_in_any_order_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        println!("Node Result - Add Completed Step Y ({timestamp}): {result:?}");
        assert_eq!(result.len(), 0);

        // Add 50 days to timestamp
        timestamp += 60 * 60 * 24 * 50;
    }

    // Add completion of Step Z.
    {
        let change = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("Reflex.CRM", "cust_01_step_z"),
                    labels: Arc::new([Arc::from("CompletedStep")]),
                    effective_from: timestamp,
                },
                properties: ElementPropertyMap::from(
                    json!({ "id": "cust_01_step_z", "cust_id": "cust_01", "step_id": "step_z", "timestamp": timestamp }),
                ),
            },
        };

        let result = steps_happen_in_any_order_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        println!("Node Result - Add Completed Step Z ({timestamp}): {result:?}");
        assert_eq!(result.len(), 0);

        // Add 10 days to timestamp
        timestamp += 60 * 60 * 24 * 10;
    }

    // Customer 02 - Completes steps within 30 Days
    // Add completion of Step X.
    {
        let change = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("Reflex.CRM", "cust_02_step_x"),
                    labels: Arc::new([Arc::from("CompletedStep")]),
                    effective_from: timestamp,
                },
                properties: ElementPropertyMap::from(
                    json!({ "id": "cust_02_step_x", "cust_id": "cust_02", "step_id": "step_x", "timestamp": timestamp }),
                ),
            },
        };

        let result = steps_happen_in_any_order_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        println!("Node Result - Add Completed Step ({timestamp}): {result:?}");
        assert_eq!(result.len(), 0);

        // Add 5 days to timestamp
        timestamp += 60 * 60 * 24 * 5;
    }

    // Add completion of Step Y.
    {
        let change = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("Reflex.CRM", "cust_02_step_y"),
                    labels: Arc::new([Arc::from("CompletedStep")]),
                    effective_from: timestamp,
                },
                properties: ElementPropertyMap::from(
                    json!({ "id": "cust_02_step_y", "cust_id": "cust_02", "step_id": "step_y", "timestamp": timestamp }),
                ),
            },
        };

        let result = steps_happen_in_any_order_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        println!("Node Result - Add Completed Step ({timestamp}): {result:?}");
        assert_eq!(result.len(), 0);

        // Add 5 days to timestamp
        timestamp += 60 * 60 * 24 * 5;
    }

    // Add completion of Step Z.
    {
        let change = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("Reflex.CRM", "cust_02_step_z"),
                    labels: Arc::new([Arc::from("CompletedStep")]),
                    effective_from: timestamp,
                },
                properties: ElementPropertyMap::from(
                    json!({ "id": "cust_02_step_z", "cust_id": "cust_02", "step_id": "step_z", "timestamp": timestamp }),
                ),
            },
        };

        let result = steps_happen_in_any_order_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        println!("Node Result - Add Completed Step ({timestamp}): {result:?}");
        assert_eq!(result.len(), 1);

        assert!(result.contains(&QueryPartEvaluationContext::Adding {
            after: variablemap!(
              "customerId" => VariableValue::from(json!("cust_02")),
              "customerEmail" => VariableValue::from(json!("cust_02@reflex.org"))
            )
        }));
    }
}

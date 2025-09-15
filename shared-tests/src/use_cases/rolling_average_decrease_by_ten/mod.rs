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
pub async fn rolling_average_decrease_by_ten(config: &(impl QueryTestConfig + Send)) {
    let rolling_average_decrease_by_ten_query = {
        let function_registry = Arc::new(FunctionRegistry::new()).with_cypher_function_set();
        let parser = Arc::new(CypherParser::new(function_registry.clone()));
        let mut builder =
            QueryBuilder::new(queries::rolling_average_decrease_by_ten_query(), parser)
                .with_function_registry(function_registry)
                .with_joins(queries::rolling_average_decrease_by_ten_metadata());
        builder = config.config_query(builder).await;
        builder.build().await
    };

    // Add initial values
    bootstrap_query(&rolling_average_decrease_by_ten_query).await;

    let mut timestamp = 1696150800; // 2023-10-01 09:00:00
    {
        // let change_minus_one = SourceChange::Update {
        //     element: Element::Node {
        //         metadata: ElementMetadata {
        //             reference: ElementReference::new("Reflex.FACILITIES", "equip_01_sensor_01"),
        //             labels: Arc::new([Arc::from("SensorValue")]),
        //             effective_from: timestamp-1,
        //         },
        //         properties: VariableValue::from(
        //             json!({ "id": "equip_01_sensor_01", "sensor_id": "sensor_01", "timestamp": timestamp, "value": 45.0  }),
        //         ),
        //     },
        // };

        // let _ = rolling_average_decrease_by_ten_query
        //     .process_source_change(change_minus_one)
        //     .await
        //     .unwrap();

        let change = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("Reflex.FACILITIES", "equip_01_sensor_01"),
                    labels: Arc::new([Arc::from("SensorValue")]),
                    effective_from: timestamp,
                },
                properties: ElementPropertyMap::from(
                    json!({ "id": "equip_01_sensor_01", "sensor_id": "sensor_01", "timestamp": timestamp, "value": 45.0  }),
                ),
            },
        };

        let result = rolling_average_decrease_by_ten_query
            .process_source_change(change)
            .await
            .unwrap();

        println!("Node Result - Update sensor value in loop ({timestamp}): {result:?}");

        timestamp += 60 * 60;
    }

    // Average is (45+44+43+42+41+40) /6 = 42.5
    {
        for i in 1..6 {
            let node_change = SourceChange::Update {
                element: Element::Node {
                    metadata: ElementMetadata {
                        reference: ElementReference::new("Reflex.FACILITIES", "equip_01_sensor_01"),
                        labels: Arc::new([Arc::from("SensorValue")]),
                        effective_from: timestamp,
                    },
                    properties: ElementPropertyMap::from(
                        json!({ "id": "equip_01_sensor_01", "sensor_id": "sensor_01", "timestamp": timestamp, "value": 45.0 - (i as f64) }),
                    ),
                },
            };

            let node_result = rolling_average_decrease_by_ten_query
                .process_source_change(node_change.clone())
                .await
                .unwrap();
            println!("Node Result - Update sensor value in loop ({timestamp}): {node_result:?}");
            assert_eq!(node_result.len(), 0);

            timestamp += 60 * 60;
        }
    }

    // Adding an entry that reduces the new average to 37.857ish?
    // Previous average is 42.5 and we should be getting a result
    {
        let node_change = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("Reflex.FACILITIES", "equip_01_sensor_01"),
                    labels: Arc::new([Arc::from("SensorValue")]),
                    effective_from: timestamp,
                },
                properties: ElementPropertyMap::from(
                    json!({ "id": "equip_01_sensor_01", "sensor_id": "sensor_01", "timestamp": timestamp, "value": 10.0 }),
                ),
            },
        };

        let node_result = rolling_average_decrease_by_ten_query
            .process_source_change(node_change.clone())
            .await
            .unwrap();
        // println!("Node Result - Update sensor value in loop ({}): {:?}", timestamp, node_result);
        assert_eq!(node_result.len(), 1);
        assert_eq!(
            node_result[0],
            QueryPartEvaluationContext::Adding {
                after: variablemap! (
                    "currentAverageTemp" => VariableValue::from(json!(37.857142857142854)),
                    "freezerId" => VariableValue::from(json!("equip_01")),
                    "previousAverageTemp" => VariableValue::from(json!(42.5))
                )
            }
        );
    }

    //Adding a new temperature that causes the average to go up
    //Result set should be empty
    {
        let node_change = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("Reflex.FACILITIES", "equip_01_sensor_01"),
                    labels: Arc::new([Arc::from("SensorValue")]),
                    effective_from: timestamp,
                },
                properties: ElementPropertyMap::from(
                    json!({ "id": "equip_01_sensor_01", "sensor_id": "sensor_01", "timestamp": timestamp, "value": 42.0}),
                ),
            },
        };
        let node_result = rolling_average_decrease_by_ten_query
            .process_source_change(node_change.clone())
            .await
            .unwrap();
        println!("Node Result - Update sensor value in loop ({timestamp}): {node_result:?}");
        assert_eq!(node_result.len(), 0);
    }
}

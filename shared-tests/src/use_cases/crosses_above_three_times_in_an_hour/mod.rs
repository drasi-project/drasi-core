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

// Query identifies when a sensor value exceeds 32 three times in an hour.
#[allow(clippy::print_stdout, clippy::unwrap_used)]
pub async fn crosses_above_three_times_in_an_hour(config: &(impl QueryTestConfig + Send)) {
    let crosses_above_three_times_in_an_hour_query = {
        let function_registry = Arc::new(FunctionRegistry::new()).with_cypher_function_set();
        let parser = Arc::new(CypherParser::new(function_registry.clone()));
        let mut builder = QueryBuilder::new(
            queries::crosses_above_three_times_in_an_hour_query(),
            parser,
        )
        .with_function_registry(function_registry)
        .with_joins(queries::crosses_above_three_times_in_an_hour_metadata());
        builder = config.config_query(builder).await;
        builder.build().await
    };

    // Add initial values
    bootstrap_query(&crosses_above_three_times_in_an_hour_query).await;

    let mut timestamp = 1696150800; // 2023-10-01 09:00:00

    // Add an initial sensor value below 32.
    {
        let change = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("Reflex.FACILITIES", "equip_01_sensor_01"),
                    labels: Arc::new([Arc::from("SensorValue")]),
                    effective_from: timestamp,
                },
                properties: ElementPropertyMap::from(
                    json!({ "id": "equip_01_sensor_01", "sensor_id": "sensor_01", "timestamp": timestamp, "value": 20.0  }),
                ),
            },
        };

        let result = crosses_above_three_times_in_an_hour_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        // println!("Node Result - Update sensor value ({}): {:?}", timestamp, result);
        assert_eq!(result.len(), 0);

        timestamp += 60;
    }

    // Add first sensor value above 32.
    {
        let change = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("Reflex.FACILITIES", "equip_01_sensor_01"),
                    labels: Arc::new([Arc::from("SensorValue")]),
                    effective_from: timestamp,
                },
                properties: ElementPropertyMap::from(
                    json!({ "id": "equip_01_sensor_01", "sensor_id": "sensor_01", "timestamp": timestamp, "value": 35.0  }),
                ),
            },
        };

        let result = crosses_above_three_times_in_an_hour_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        // println!("Node Result - Update sensor value ({}): {:?}", timestamp, result);
        assert_eq!(result.len(), 0);

        timestamp += 60;
    }

    // Add sensor value below 32.
    {
        let change = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("Reflex.FACILITIES", "equip_01_sensor_01"),
                    labels: Arc::new([Arc::from("SensorValue")]),
                    effective_from: timestamp,
                },
                properties: ElementPropertyMap::from(
                    json!({ "id": "equip_01_sensor_01", "sensor_id": "sensor_01", "timestamp": timestamp, "value": 15.0  }),
                ),
            },
        };

        let result = crosses_above_three_times_in_an_hour_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        // println!("Node Result - Update sensor value ({}): {:?}", timestamp, result);
        assert_eq!(result.len(), 0);

        timestamp += 60;
    }

    // Add second sensor value above 32.
    {
        let change = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("Reflex.FACILITIES", "equip_01_sensor_01"),
                    labels: Arc::new([Arc::from("SensorValue")]),
                    effective_from: timestamp,
                },
                properties: ElementPropertyMap::from(
                    json!({ "id": "equip_01_sensor_01", "sensor_id": "sensor_01", "timestamp": timestamp, "value": 35.0  }),
                ),
            },
        };

        let result = crosses_above_three_times_in_an_hour_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        // println!("Node Result - Update sensor value ({}): {:?}", timestamp, result);
        assert_eq!(result.len(), 0);

        timestamp += 60;
    }

    // Add sensor value below 32.
    {
        let change = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("Reflex.FACILITIES", "equip_01_sensor_01"),
                    labels: Arc::new([Arc::from("SensorValue")]),
                    effective_from: timestamp,
                },
                properties: ElementPropertyMap::from(
                    json!({ "id": "equip_01_sensor_01", "sensor_id": "sensor_01", "timestamp": timestamp, "value": 15.0  }),
                ),
            },
        };

        let result = crosses_above_three_times_in_an_hour_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        // println!("Node Result - Update sensor value ({}): {:?}", timestamp, result);
        assert_eq!(result.len(), 0);

        timestamp += 60;
    }

    // Add third sensor value above 32.
    {
        let change = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("Reflex.FACILITIES", "equip_01_sensor_01"),
                    labels: Arc::new([Arc::from("SensorValue")]),
                    effective_from: timestamp,
                },
                properties: ElementPropertyMap::from(
                    json!({ "id": "equip_01_sensor_01", "sensor_id": "sensor_01", "timestamp": timestamp, "value": 35.0  }),
                ),
            },
        };

        let result = crosses_above_three_times_in_an_hour_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        // println!("Node Result - Update sensor value ({}): {:?}", timestamp, result);
        assert_eq!(result.len(), 1);

        assert!(result.contains(&QueryPartEvaluationContext::Adding {
            after: variablemap!(
              "freezerId" => VariableValue::from(json!("equip_01")),
              "countTempExceededInTimeRange" => VariableValue::from(json!(3))
            )
        }));

        timestamp += 60;
    }

    // Add fourth sensor value above 32.
    {
        let change = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("Reflex.FACILITIES", "equip_01_sensor_01"),
                    labels: Arc::new([Arc::from("SensorValue")]),
                    effective_from: timestamp,
                },
                properties: ElementPropertyMap::from(
                    json!({ "id": "equip_01_sensor_01", "sensor_id": "sensor_01", "timestamp": timestamp, "value": 35.0  }),
                ),
            },
        };

        let result = crosses_above_three_times_in_an_hour_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        println!(
            "Node Result - Update sensor value ({}): {:?}",
            timestamp, result
        );
        assert_eq!(result.len(), 1);

        assert!(result.contains(&QueryPartEvaluationContext::Updating {
            before: variablemap!(
              "freezerId" => VariableValue::from(json!("equip_01")),
              "countTempExceededInTimeRange" => VariableValue::from(json!(3))
            ),
            after: variablemap!(
              "freezerId" => VariableValue::from(json!("equip_01")),
              "countTempExceededInTimeRange" => VariableValue::from(json!(4))
            )
        }));
    }
}

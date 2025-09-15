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
pub async fn logical_conditions(config: &(impl QueryTestConfig + Send)) {
    let logical_conditions_query = {
        let function_registry = Arc::new(FunctionRegistry::new()).with_cypher_function_set();
        let parser = Arc::new(CypherParser::new(function_registry.clone()));
        let mut builder = QueryBuilder::new(queries::logical_conditions_query(), parser)
            .with_function_registry(function_registry)
            .with_joins(queries::logical_conditions_metadata());
        builder = config.config_query(builder).await;
        builder.build().await
    };

    // Add initial values
    bootstrap_query(&logical_conditions_query).await;

    let mut timestamp = 1696150800; // 2023-10-01 09:00:00

    // Add a temperator value below 32.
    {
        let change = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("Reflex.FACILITIES", "temp_sensor_01_value"),
                    labels: Arc::new([Arc::from("SensorValue")]),
                    effective_from: timestamp,
                },
                properties: ElementPropertyMap::from(
                    json!({ "id": "temp_sensor_01_value", "sensor_id": "temp_sensor_01", "timestamp": timestamp, "value": 20.0  }),
                ),
            },
        };

        let result = logical_conditions_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        println!("Node Result - Update sensor value ({timestamp}): {result:?}");
        assert_eq!(result.len(), 0);

        timestamp += 60;
    }

    // Add a door sensor value of "closed" (0).
    {
        let change = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("Reflex.FACILITIES", "door_sensor_01_value"),
                    labels: Arc::new([Arc::from("SensorValue")]),
                    effective_from: timestamp,
                },
                properties: ElementPropertyMap::from(
                    json!({ "id": "door_sensor_01_value", "sensor_id": "door_sensor_01", "timestamp": timestamp, "value": 0  }),
                ),
            },
        };

        let result = logical_conditions_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        println!("Node Result - Update sensor value ({timestamp}): {result:?}");
        assert_eq!(result.len(), 0);

        timestamp += 60;
    }

    // Add a temperator value above 32.
    {
        let change = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("Reflex.FACILITIES", "temp_sensor_01_value"),
                    labels: Arc::new([Arc::from("SensorValue")]),
                    effective_from: timestamp,
                },
                properties: ElementPropertyMap::from(
                    json!({ "id": "temp_sensor_01_value", "sensor_id": "temp_sensor_01", "timestamp": timestamp, "value": 40.0  }),
                ),
            },
        };

        let result = logical_conditions_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        println!("Node Result - Update sensor value ({timestamp}): {result:?}");
        assert_eq!(result.len(), 0);

        timestamp += 60;
    }

    // Add a door sensor value of "open" (1).
    {
        let change = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("Reflex.FACILITIES", "door_sensor_01_value"),
                    labels: Arc::new([Arc::from("SensorValue")]),
                    effective_from: timestamp,
                },
                properties: ElementPropertyMap::from(
                    json!({ "id": "door_sensor_01_value", "sensor_id": "door_sensor_01", "timestamp": timestamp, "value": 1  }),
                ),
            },
        };

        let result = logical_conditions_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        println!("Node Result - Update sensor value ({timestamp}): {result:?}");
        assert_eq!(result.len(), 1);

        assert!(result.contains(&QueryPartEvaluationContext::Adding {
            after: variablemap!(
              "freezerId" => VariableValue::from(json!("equip_01"))
            )
        }));

        timestamp += 60;
    }

    // Add a door sensor value of "closed" (0).
    {
        let change = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("Reflex.FACILITIES", "door_sensor_01_value"),
                    labels: Arc::new([Arc::from("SensorValue")]),
                    effective_from: timestamp,
                },
                properties: ElementPropertyMap::from(
                    json!({ "id": "door_sensor_01_value", "sensor_id": "door_sensor_01", "timestamp": timestamp, "value": 0  }),
                ),
            },
        };

        let result = logical_conditions_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        println!("Node Result - Update sensor value ({timestamp}): {result:?}");
        assert_eq!(result.len(), 1);

        assert!(result.contains(&QueryPartEvaluationContext::Removing {
            before: variablemap!(
              "freezerId" => VariableValue::from(json!("equip_01"))
            )
        }));

        timestamp += 60;
    }

    // Add a temperator value below 32.
    {
        let change = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("Reflex.FACILITIES", "temp_sensor_01_value"),
                    labels: Arc::new([Arc::from("SensorValue")]),
                    effective_from: timestamp,
                },
                properties: ElementPropertyMap::from(
                    json!({ "id": "temp_sensor_01_value", "sensor_id": "temp_sensor_01", "timestamp": timestamp, "value": 20.0  }),
                ),
            },
        };

        let result = logical_conditions_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        println!("Node Result - Update sensor value ({timestamp}): {result:?}");
        assert_eq!(result.len(), 0);
    }
}

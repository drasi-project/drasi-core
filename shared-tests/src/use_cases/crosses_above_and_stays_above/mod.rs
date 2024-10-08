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

// Query identifies when a sensor value has been above 32 for the last 15 minutes.
pub async fn crosses_above_and_stays_above(config: &(impl QueryTestConfig + Send)) {
    let greater_than_a_threshold_query = {
        let mut builder = QueryBuilder::new(queries::crosses_above_and_stays_above_query())
            .with_joins(queries::crosses_above_and_stays_above_metadata());
        builder = config.config_query(builder).await;
        builder.build().await
    };

    // Add initial values
    bootstrap_query(&greater_than_a_threshold_query).await;

    let mut timestamp = 1696150800; // 2023-10-01 09:00:00

    // Add an initial sensor value below 32.
    {
        let node_change = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("Reflex.FACILITIES", "equip_01_sensor_01"),
                    labels: Arc::new([Arc::from("SensorValue")]),
                    effective_from: timestamp,
                },
                properties: ElementPropertyMap::from(
                    json!({ "sensor_id": "sensor_01", "timestamp": timestamp, "value": 20.0  }),
                ),
            },
        };

        let node_result = greater_than_a_threshold_query
            .process_source_change(node_change.clone())
            .await
            .unwrap();
        // println!("Node Result - Update sensor value ({}): {:?}", timestamp, node_result);
        assert_eq!(node_result.len(), 0);

        timestamp += 60;
    }

    // Loop 14 times and update node equip_01_sensor_01 each minute after epoch time 1696150800 with a value that is above 32.0
    {
        for i in 1..16 {
            let node_change = SourceChange::Update {
                element: Element::Node {
                    metadata: ElementMetadata {
                        reference: ElementReference::new("Reflex.FACILITIES", "equip_01_sensor_01"),
                        labels: Arc::new([Arc::from("SensorValue")]),
                        effective_from: timestamp,
                    },
                    properties: ElementPropertyMap::from(
                        json!({ "sensor_id": "sensor_01", "timestamp": timestamp, "value": 35.0 + (i as f64) }),
                    ),
                },
            };

            let node_result = greater_than_a_threshold_query
                .process_source_change(node_change.clone())
                .await
                .unwrap();
            // println!("Node Result - Update sensor value in loop ({}): {:?}", timestamp, node_result);
            assert_eq!(node_result.len(), 0);

            timestamp += 60;
        }
    }

    // Add a 15th sensor value above 32, this should add a result to the query result.
    {
        let node_change = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("Reflex.FACILITIES", "equip_01_sensor_01"),
                    labels: Arc::new([Arc::from("SensorValue")]),
                    effective_from: timestamp,
                },
                properties: ElementPropertyMap::from(
                    json!({ "sensor_id": "sensor_01", "timestamp": timestamp, "value": 50.0  }),
                ), // Sensor Date: 2023-10-01
            },
        };

        let node_result = greater_than_a_threshold_query
            .process_source_change(node_change.clone())
            .await
            .unwrap();
        // println!("Node Result - Update sensor value ({}): {:?}", timestamp, node_result);
        assert_eq!(node_result.len(), 1);

        assert!(node_result.contains(&QueryPartEvaluationContext::Adding {
            after: variablemap!(
              "freezerId" => VariableValue::from(json!("equip_01")),
              // "timeRangeStart" => VariableValue::from(json!(1696150860)),
              // "timeRangeEnd" => VariableValue::from(json!(1696151760)),
              "minTempInTimeRange" => VariableValue::from(json!(36))
            )
        }));

        timestamp += 60;
    }

    // Add a 16th sensor value below 32, this should remove a result from the query result.
    {
        let node_change = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("Reflex.FACILITIES", "equip_01_sensor_01"),
                    labels: Arc::new([Arc::from("SensorValue")]),
                    effective_from: timestamp,
                },
                properties: ElementPropertyMap::from(
                    json!({ "sensor_id": "sensor_01", "timestamp": timestamp, "value": 20.0  }),
                ), // Sensor Date: 2023-10-01
            },
        };

        let node_result = greater_than_a_threshold_query
            .process_source_change(node_change.clone())
            .await
            .unwrap();
        // println!("Node Result - Update sensor value ({}): {:?}", timestamp, node_result);
        assert_eq!(node_result.len(), 1);

        assert!(node_result.contains(&QueryPartEvaluationContext::Removing {
            before: variablemap!(
              "freezerId" => VariableValue::from(json!("equip_01")),
              // "timeRangeStart" => VariableValue::from(json!(1696150860)),
              // "timeRangeEnd" => VariableValue::from(json!(1696151760)),
              "minTempInTimeRange" => VariableValue::from(json!(36))
            )
        }));
    }
}

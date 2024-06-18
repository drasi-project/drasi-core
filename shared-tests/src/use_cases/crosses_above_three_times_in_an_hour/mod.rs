use std::sync::Arc;

use serde_json::json;

use drasi_core::{
    evaluation::{context::PhaseEvaluationContext, variable_value::VariableValue},
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

// Query identifies when a sensor value exceeds 32 three times in an hour.
pub async fn crosses_above_three_times_in_an_hour(config: &(impl QueryTestConfig + Send)) {
    let crosses_above_three_times_in_an_hour_query = {
        let mut builder = QueryBuilder::new(queries::crosses_above_three_times_in_an_hour_query())
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

        assert!(result.contains(&PhaseEvaluationContext::Adding {
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

        assert!(result.contains(&PhaseEvaluationContext::Updating {
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

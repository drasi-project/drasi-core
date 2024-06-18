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

pub async fn logical_conditions(config: &(impl QueryTestConfig + Send)) {
    let logical_conditions_query = {
        let mut builder = QueryBuilder::new(queries::logical_conditions_query())
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
        println!(
            "Node Result - Update sensor value ({}): {:?}",
            timestamp, result
        );
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
        println!(
            "Node Result - Update sensor value ({}): {:?}",
            timestamp, result
        );
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
        println!(
            "Node Result - Update sensor value ({}): {:?}",
            timestamp, result
        );
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
        println!(
            "Node Result - Update sensor value ({}): {:?}",
            timestamp, result
        );
        assert_eq!(result.len(), 1);

        assert!(result.contains(&PhaseEvaluationContext::Adding {
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
        println!(
            "Node Result - Update sensor value ({}): {:?}",
            timestamp, result
        );
        assert_eq!(result.len(), 1);

        assert!(result.contains(&PhaseEvaluationContext::Removing {
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
        println!(
            "Node Result - Update sensor value ({}): {:?}",
            timestamp, result
        );
        assert_eq!(result.len(), 0);
    }
}

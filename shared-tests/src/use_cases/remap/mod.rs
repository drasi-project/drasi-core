use std::sync::Arc;

use drasi_query_middleware::map::MapFactory;
use serde_json::json;

use drasi_query_core::{
    evaluation::{context::PhaseEvaluationContext, variable_value::VariableValue},
    middleware::MiddlewareTypeRegistry,
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

pub async fn remap(config: &(impl QueryTestConfig + Send)) {
    let mut middleware_registry = MiddlewareTypeRegistry::new();
    middleware_registry.register(Arc::new(MapFactory::new()));
    let middleware_registry = Arc::new(middleware_registry);

    let mq = Arc::new(queries::remap_query());
    let rm_query = {
        let mut builder = QueryBuilder::new(mq.clone());
        builder = config.config_query(builder, mq.clone()).await;
        builder = builder.with_middleware_registry(middleware_registry);
        for mw in queries::middlewares() {
            builder = builder.with_source_middleware(mw);
        }
        builder = builder.with_source_pipeline("test", &queries::source_pipeline());
        builder.build()
    };

    //Add initial value
    {
        let change = SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "t1"),
                    labels: Arc::new([Arc::from("Telemetry")]),
                    effective_from: 0,
                },
                properties: ElementPropertyMap::from(json!({
                    "signals": [
                        {
                            "name": "Vehicle.CurrentLocation.Heading",
                            "value": "96"
                        },
                        {
                            "name": "Vehicle.Speed",
                            "value": "119"
                        },
                        {
                            "name": "Vehicle.TraveledDistance",
                            "value": "4563"
                        }
                    ],
                    "additionalProperties": {
                        "Id": "7dada2cb-a85a-4f38-bc4a-3d74a22c04b0",
                        "Source": "netstar.telemetry"
                    },
                    "vehicleId": "v1"
                })),
            },
        };

        let result = rm_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        assert_eq!(result.len(), 1);
        println!("Node Result - Add t1: {:?}", result);
        assert!(result.contains(&PhaseEvaluationContext::Adding {
            after: variablemap!(
                "id" => VariableValue::from(json!("v1")),
                "currentSpeed" => VariableValue::from(json!("119"))
            )
        }));
    }

    //Add next value
    {
        let change = SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "t2"),
                    labels: Arc::new([Arc::from("Telemetry")]),
                    effective_from: 0,
                },
                properties: ElementPropertyMap::from(json!({
                    "signals": [
                        {
                            "name": "Vehicle.CurrentLocation.Heading",
                            "value": "98"
                        },
                        {
                            "name": "Vehicle.Speed",
                            "value": "121"
                        },
                        {
                            "name": "Vehicle.TraveledDistance",
                            "value": "4567"
                        }
                    ],
                    "additionalProperties": {
                        "Id": "7dada2cb-a85a-4f38-bc4a-3d74a22c04c2",
                        "Source": "netstar.telemetry"
                    },
                    "vehicleId": "v1"
                })),
            },
        };

        let result = rm_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        assert_eq!(result.len(), 1);
        println!("Node Result - Add t2: {:?}", result);
        assert!(result.contains(&PhaseEvaluationContext::Updating {
            before: variablemap!(
                "id" => VariableValue::from(json!("v1")),
                "currentSpeed" => VariableValue::from(json!("119"))
            ),
            after: variablemap!(
                "id" => VariableValue::from(json!("v1")),
                "currentSpeed" => VariableValue::from(json!("121"))
            )
        }));
    }

    //Add another vehicle
    {
        let change = SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "t3"),
                    labels: Arc::new([Arc::from("Telemetry")]),
                    effective_from: 0,
                },
                properties: ElementPropertyMap::from(json!({
                    "signals": [
                        {
                            "name": "Vehicle.CurrentLocation.Heading",
                            "value": "91"
                        },
                        {
                            "name": "Vehicle.Speed",
                            "value": "110"
                        },
                        {
                            "name": "Vehicle.TraveledDistance",
                            "value": "4063"
                        }
                    ],
                    "additionalProperties": {
                        "Id": "8dada2cb-b85a-4f38-bc4a-3d74a22c04b7",
                        "Source": "netstar.telemetry"
                    },
                    "vehicleId": "v2"
                })),
            },
        };

        let result = rm_query
            .process_source_change(change.clone())
            .await
            .unwrap();
        assert_eq!(result.len(), 1);
        println!("Node Result - Add t3: {:?}", result);
        assert!(result.contains(&PhaseEvaluationContext::Adding {
            after: variablemap!(
                "id" => VariableValue::from(json!("v2")),
                "currentSpeed" => VariableValue::from(json!("110"))
            )
        }));
    }
}

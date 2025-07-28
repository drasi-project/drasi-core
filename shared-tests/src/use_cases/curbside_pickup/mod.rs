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
use serde_json::json;

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

pub async fn order_ready_then_vehicle_arrives(config: &(impl QueryTestConfig + Send)) {
    let query = {
        let function_registry = Arc::new(FunctionRegistry::new()).with_cypher_function_set();
        let parser = Arc::new(CypherParser::new(function_registry.clone()));
        let mut builder = QueryBuilder::new(queries::pickup_order_ready_query(), parser)
            .with_function_registry(function_registry)
            .with_joins(queries::pickup_order_ready_metadata());
        builder = config.config_query(builder).await;
        builder.build().await
    };

    bootstrap_query(&query).await;

    //Order Becomes Ready for pickup - waiting for vehicle to arrive
    {
        let change = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("Contoso.RetailOperations", "order_01"),
                    labels: Arc::new([Arc::from("Order")]),
                    effective_from: 1000,
                },
                properties: ElementPropertyMap::from(
                    json!({ "name": "Order 01", "status": "ready" }),
                ),
            },
        };

        assert_eq!(
            query.process_source_change(change.clone()).await.unwrap(),
            vec![]
        );
    }

    //Vehicle arrives in curbside pickup zone - order is already ready/waiting
    {
        let vehicle_event = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("Contoso.PhysicalOperations", "vehicle_01"),
                    labels: Arc::new([Arc::from("Vehicle")]),
                    effective_from: 2000,
                },
                properties: ElementPropertyMap::from(
                    json!({ "name": "Vehicle 01", "licensePlate": "ABC123" }),
                ),
            },
        };

        assert_eq!(
            query
                .process_source_change(vehicle_event.clone())
                .await
                .unwrap(),
            vec![]
        );

        let located_in_event = SourceChange::Update {
            element: Element::Relation {
                metadata: ElementMetadata {
                    reference: ElementReference::new("Contoso.PhysicalOperations", "located_in_01"),
                    labels: Arc::new([Arc::from("LOCATED_IN")]),
                    effective_from: 2001,
                },
                properties: ElementPropertyMap::from(json!({})),
                in_node: ElementReference::new("Contoso.PhysicalOperations", "vehicle_01"),
                out_node: ElementReference::new("Contoso.PhysicalOperations", "zone_01"),
            },
        };

        let result = query
            .process_source_change(located_in_event.clone())
            .await
            .unwrap();
        assert_eq!(result.len(), 1);
        assert!(result.contains(&QueryPartEvaluationContext::Adding {
            after: variablemap!(
              "DriverName" => VariableValue::from(json!("Driver 01")),
              "OrderNumber" => VariableValue::from(json!("order_01")),
              "LicensePlate" => VariableValue::from(json!("ABC123"))
            ),
        }));
    }

    //Order Delivered to Waiting Vehicle - order complete
    {
        let change = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("Contoso.RetailOperations", "order_01"),
                    labels: Arc::new([Arc::from("Order")]),
                    effective_from: 3000,
                },
                properties: ElementPropertyMap::from(
                    json!({ "name": "Order 01", "status": "complete" }),
                ),
            },
        };

        let result = query.process_source_change(change.clone()).await.unwrap();
        assert_eq!(result.len(), 1);
        assert!(result.contains(&QueryPartEvaluationContext::Removing {
            before: variablemap!(
              "DriverName" => VariableValue::from(json!("Driver 01")),
              "OrderNumber" => VariableValue::from(json!("order_01")),
              "LicensePlate" => VariableValue::from(json!("ABC123"))
            ),
        }));
    }
}

pub async fn vehicle_arrives_then_order_ready(config: &(impl QueryTestConfig + Send)) {
    let query = {
        let function_registry = Arc::new(FunctionRegistry::new()).with_cypher_function_set();
        let parser = Arc::new(CypherParser::new(function_registry.clone()));
        let mut builder = QueryBuilder::new(queries::pickup_order_ready_query(), parser)
            .with_function_registry(function_registry)
            .with_joins(queries::pickup_order_ready_metadata());
        builder = config.config_query(builder).await;
        builder.build().await
    };

    bootstrap_query(&query).await;

    //Vehicle arrives in curbside pickup zone - order is already ready/waiting
    {
        let vehicle_event = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("Contoso.PhysicalOperations", "vehicle_02"),
                    labels: Arc::new([Arc::from("Vehicle")]),
                    effective_from: 4000,
                },
                properties: ElementPropertyMap::from(
                    json!({ "name": "Vehicle 02", "licensePlate": "XYZ789" }),
                ),
            },
        };

        assert_eq!(
            query
                .process_source_change(vehicle_event.clone())
                .await
                .unwrap(),
            vec![]
        );

        let located_in_event = SourceChange::Update {
            element: Element::Relation {
                metadata: ElementMetadata {
                    reference: ElementReference::new("Contoso.PhysicalOperations", "located_in_02"),
                    labels: Arc::new([Arc::from("LOCATED_IN")]),
                    effective_from: 4001,
                },
                properties: ElementPropertyMap::from(json!({})),
                in_node: ElementReference::new("Contoso.PhysicalOperations", "vehicle_02"),
                out_node: ElementReference::new("Contoso.PhysicalOperations", "zone_01"),
            },
        };

        let result = query
            .process_source_change(located_in_event.clone())
            .await
            .unwrap();
        assert_eq!(result.len(), 0);
    }

    //Order Becomes Ready for pickup - vehicle already waiting
    {
        let change = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("Contoso.RetailOperations", "order_02"),
                    labels: Arc::new([Arc::from("Order")]),
                    effective_from: 5000,
                },
                properties: ElementPropertyMap::from(
                    json!({ "name": "Order 02", "status": "ready" }),
                ),
            },
        };

        let result = query.process_source_change(change.clone()).await.unwrap();
        assert_eq!(result.len(), 1);
        assert!(result.contains(&QueryPartEvaluationContext::Adding {
            after: variablemap!(
              "DriverName" => VariableValue::from(json!("Driver 02")),
              "OrderNumber" => VariableValue::from(json!("order_02")),
              "LicensePlate" => VariableValue::from(json!("XYZ789"))
            ),
        }));
    }

    //Order Delivered to Waiting Vehicle - order complete
    {
        let change = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("Contoso.RetailOperations", "order_02"),
                    labels: Arc::new([Arc::from("Order")]),
                    effective_from: 6000,
                },
                properties: ElementPropertyMap::from(
                    json!({ "name": "Order 02", "status": "complete" }),
                ),
            },
        };

        let result = query.process_source_change(change.clone()).await.unwrap();
        assert_eq!(result.len(), 1);
        assert!(result.contains(&QueryPartEvaluationContext::Removing {
            before: variablemap!(
              "DriverName" => VariableValue::from(json!("Driver 02")),
              "OrderNumber" => VariableValue::from(json!("order_02")),
              "LicensePlate" => VariableValue::from(json!("XYZ789"))
            ),
        }));
    }
}

pub async fn vehicle_arrives_then_order_ready_duplicate(config: &(impl QueryTestConfig + Send)) {
    let query = {
        let function_registry = Arc::new(FunctionRegistry::new()).with_cypher_function_set();
        let parser = Arc::new(CypherParser::new(function_registry.clone()));
        let mut builder = QueryBuilder::new(queries::pickup_order_ready_query(), parser)
            .with_function_registry(function_registry)
            .with_joins(queries::pickup_order_ready_metadata());
        builder = config.config_query(builder).await;
        builder.build().await
    };

    bootstrap_query(&query).await;

    //Vehicle arrives in curbside pickup zone - order is already ready/waiting
    {
        let vehicle_event = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("Contoso.PhysicalOperations", "vehicle_03"),
                    labels: Arc::new([Arc::from("Vehicle")]),
                    effective_from: 4000,
                },
                properties: ElementPropertyMap::from(
                    json!({ "name": "Vehicle 03", "licensePlate": "drasi" }),
                ),
            },
        };

        assert_eq!(
            query
                .process_source_change(vehicle_event.clone())
                .await
                .unwrap(),
            vec![]
        );

        let located_in_event = SourceChange::Update {
            element: Element::Relation {
                metadata: ElementMetadata {
                    reference: ElementReference::new("Contoso.PhysicalOperations", "located_in_03"),
                    labels: Arc::new([Arc::from("LOCATED_IN")]),
                    effective_from: 4001,
                },
                properties: ElementPropertyMap::from(json!({})),
                in_node: ElementReference::new("Contoso.PhysicalOperations", "vehicle_03"),
                out_node: ElementReference::new("Contoso.PhysicalOperations", "zone_01"),
            },
        };

        let result = query
            .process_source_change(located_in_event.clone())
            .await
            .unwrap();
        assert_eq!(result.len(), 0);
    }

    {
        let change = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("Contoso.RetailOperations", "order_03"),
                    labels: Arc::new([Arc::from("Order")]),
                    effective_from: 5000,
                },
                properties: ElementPropertyMap::from(
                    json!({ "name": "Order 03", "status": "ready" }),
                ),
            },
        };

        let result = query.process_source_change(change.clone()).await.unwrap();
        assert_eq!(result.len(), 1);

        assert!(result.contains(&QueryPartEvaluationContext::Adding {
            after: variablemap!(
              "DriverName" => VariableValue::from(json!("Driver 03")),
              "OrderNumber" => VariableValue::from(json!("order_03")),
              "LicensePlate" => VariableValue::from(json!("drasi"))
            ),
        }));
    }

    {
        let change = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("Contoso.RetailOperations", "order_03"),
                    labels: Arc::new([Arc::from("Order")]),
                    effective_from: 5500,
                },
                properties: ElementPropertyMap::from(
                    json!({ "name": "Order 03", "status": "complete" }),
                ),
            },
        };

        let result = query.process_source_change(change.clone()).await.unwrap();
        assert_eq!(result.len(), 1);
        assert!(result.contains(&QueryPartEvaluationContext::Removing {
            before: variablemap!(
              "DriverName" => VariableValue::from(json!("Driver 03")),
              "OrderNumber" => VariableValue::from(json!("order_03")),
              "LicensePlate" => VariableValue::from(json!("drasi"))
            ),
        }));

        let delete_vehicle = SourceChange::Delete {
            metadata: ElementMetadata {
                reference: ElementReference::new("Contoso.PhysicalOperations", "vehicle_03"),
                labels: Arc::new([Arc::from("Vehicle")]),
                effective_from: 5500,
            },
        };

        let _ = query
            .process_source_change(delete_vehicle.clone())
            .await
            .unwrap();
    }
    {
        let vehicle_event = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("Contoso.PhysicalOperations", "vehicle_04"),
                    labels: Arc::new([Arc::from("Vehicle")]),
                    effective_from: 6000,
                },
                properties: ElementPropertyMap::from(
                    json!({ "name": "Vehicle 04", "licensePlate": "drasi" }),
                ),
            },
        };

        assert_eq!(
            query
                .process_source_change(vehicle_event.clone())
                .await
                .unwrap(),
            vec![]
        );

        let located_in_event = SourceChange::Update {
            element: Element::Relation {
                metadata: ElementMetadata {
                    reference: ElementReference::new("Contoso.PhysicalOperations", "located_in_04"),
                    labels: Arc::new([Arc::from("LOCATED_IN")]),
                    effective_from: 6001,
                },
                properties: ElementPropertyMap::from(json!({})),
                in_node: ElementReference::new("Contoso.PhysicalOperations", "vehicle_04"),
                out_node: ElementReference::new("Contoso.PhysicalOperations", "zone_01"),
            },
        };

        let _ = query
            .process_source_change(located_in_event.clone())
            .await
            .unwrap();
    }

    {
        let change = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("Contoso.RetailOperations", "order_04"),
                    labels: Arc::new([Arc::from("Order")]),
                    effective_from: 7000,
                },
                properties: ElementPropertyMap::from(
                    json!({ "name": "Order 04", "status": "ready" }),
                ),
            },
        };

        let result = query.process_source_change(change.clone()).await.unwrap();
        assert_eq!(result.len(), 1);
        assert!(result.contains(&QueryPartEvaluationContext::Adding {
            after: variablemap!(
              "DriverName" => VariableValue::from(json!("Driver 04")),
              "OrderNumber" => VariableValue::from(json!("order_04")),
              "LicensePlate" => VariableValue::from(json!("drasi"))
            ),
        }));
    }
}

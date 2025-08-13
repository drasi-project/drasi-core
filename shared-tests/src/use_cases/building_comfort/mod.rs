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

pub async fn building_comfort_use_case(config: &(impl QueryTestConfig + Send)) {
    let rclq = queries::room_comfort_level_calc_query();
    let room_comfort_level_calc_query = {
        let function_registry = Arc::new(FunctionRegistry::new()).with_cypher_function_set();
        let parser = Arc::new(CypherParser::new(function_registry.clone()));
        let mut builder = QueryBuilder::new(rclq, parser).with_function_registry(function_registry);
        builder = config.config_query(builder).await;
        builder.build().await
    };

    let fclq = queries::floor_comfort_level_calc_query();
    let floor_comfort_level_calc_query = {
        let function_registry = Arc::new(FunctionRegistry::new()).with_cypher_function_set();
        let parser = Arc::new(CypherParser::new(function_registry.clone()));
        let mut builder = QueryBuilder::new(fclq, parser).with_function_registry(function_registry);
        builder = config.config_query(builder).await;
        builder.build().await
    };

    let bclq = queries::building_comfort_level_calc_query();
    let building_comfort_level_calc_query = {
        let function_registry = Arc::new(FunctionRegistry::new()).with_cypher_function_set();
        let parser = Arc::new(CypherParser::new(function_registry.clone()));
        let mut builder = QueryBuilder::new(bclq, parser).with_function_registry(function_registry);
        builder = config.config_query(builder).await;
        builder.build().await
    };

    let rcaq = queries::room_comfort_level_alert_query();
    let room_comfort_level_alert_query = {
        let function_registry = Arc::new(FunctionRegistry::new()).with_cypher_function_set();
        let parser = Arc::new(CypherParser::new(function_registry.clone()));
        let mut builder = QueryBuilder::new(rcaq, parser).with_function_registry(function_registry);
        builder = config.config_query(builder).await;
        builder.build().await
    };

    let fcaq = queries::floor_comfort_level_alert_query();
    let floor_comfort_level_alert_query = {
        let function_registry = Arc::new(FunctionRegistry::new()).with_cypher_function_set();
        let parser = Arc::new(CypherParser::new(function_registry.clone()));
        let mut builder = QueryBuilder::new(fcaq, parser).with_function_registry(function_registry);
        builder = config.config_query(builder).await;
        builder.build().await
    };

    let bcaq = queries::building_comfort_level_alert_query();
    let building_comfort_level_alert_query = {
        let function_registry = Arc::new(FunctionRegistry::new()).with_cypher_function_set();
        let parser = Arc::new(CypherParser::new(function_registry.clone()));
        let mut builder = QueryBuilder::new(bcaq, parser).with_function_registry(function_registry);
        builder = config.config_query(builder).await;
        builder.build().await
    };

    let uiq = queries::ui_query();
    let ui_query = {
        let function_registry = Arc::new(FunctionRegistry::new()).with_cypher_function_set();
        let parser = Arc::new(CypherParser::new(function_registry.clone()));
        let mut builder = QueryBuilder::new(uiq, parser).with_function_registry(function_registry);
        builder = config.config_query(builder).await;
        builder.build().await
    };

    bootstrap_query(&room_comfort_level_calc_query).await;
    bootstrap_query(&floor_comfort_level_calc_query).await;
    bootstrap_query(&building_comfort_level_calc_query).await;
    bootstrap_query(&room_comfort_level_alert_query).await;
    bootstrap_query(&floor_comfort_level_alert_query).await;
    bootstrap_query(&building_comfort_level_alert_query).await;
    bootstrap_query(&ui_query).await;

    //room_comfort_level_inputs
    {
        // Room 01_01-01 changes, but not the inputs we are interested in.
        {
            let change = SourceChange::Update {
                element: Element::Node {
                    metadata: ElementMetadata {
                        reference: ElementReference::new("Contoso.Facilities", "room_01_01_01"),
                        labels: Arc::new([Arc::from("Room")]),
                        effective_from: 1000,
                    },
                    properties: ElementPropertyMap::from(
                        json!({ "name": "Room 01_01_01", "comfortLevel": 50, "temp": 72, "humidity": 42, "co2": 500, "foo": "bar", "tick": "tock" }),
                    ),
                },
            };

            assert_eq!(
                room_comfort_level_calc_query
                    .process_source_change(change.clone())
                    .await
                    .unwrap(),
                vec![]
            );
            assert_eq!(
                floor_comfort_level_calc_query
                    .process_source_change(change.clone())
                    .await
                    .unwrap(),
                vec![]
            );
            assert_eq!(
                building_comfort_level_calc_query
                    .process_source_change(change.clone())
                    .await
                    .unwrap(),
                vec![]
            );

            assert_eq!(
                room_comfort_level_alert_query
                    .process_source_change(change.clone())
                    .await
                    .unwrap(),
                vec![]
            );
            assert_eq!(
                floor_comfort_level_alert_query
                    .process_source_change(change.clone())
                    .await
                    .unwrap(),
                vec![]
            );
            assert_eq!(
                building_comfort_level_alert_query
                    .process_source_change(change.clone())
                    .await
                    .unwrap(),
                vec![]
            );

            assert_eq!(
                ui_query
                    .process_source_change(change.clone())
                    .await
                    .unwrap(),
                vec![]
            );
        }

        // Room 01_01-01 comfort level inputs change
        {
            let change = SourceChange::Update {
                element: Element::Node {
                    metadata: ElementMetadata {
                        reference: ElementReference::new("Contoso.Facilities", "room_01_01_01"),
                        labels: Arc::new([Arc::from("Room")]),
                        effective_from: 1100,
                    },
                    properties: ElementPropertyMap::from(
                        json!({ "name": "Room 01_01_01", "temp": 74, "humidity": 44, "co2": 500 }),
                    ),
                },
            };

            let result = room_comfort_level_calc_query
                .process_source_change(change.clone())
                .await
                .unwrap();
            assert_eq!(result.len(), 1);
            assert!(result.contains(&QueryPartEvaluationContext::Updating {
                before: variablemap!(
                  "RoomId" =>  VariableValue::from(json!("room_01_01_01")),
                  "ComfortLevel" =>  VariableValue::from(json!(50))
                ),
                after: variablemap!(
                  "RoomId" => VariableValue::from(json!("room_01_01_01")),
                  "ComfortLevel" =>  VariableValue::from(json!(54))
                ),
            }));

            let result = floor_comfort_level_calc_query
                .process_source_change(change.clone())
                .await
                .unwrap();
            assert_eq!(result.len(), 1);
            assert!(result.contains(&QueryPartEvaluationContext::Aggregation {
                default_before: false,
                default_after: false,
                before: Some(variablemap!(
                  "FloorId" =>  VariableValue::from(json!("floor_01_01")),
                  "ComfortLevel" =>  VariableValue::from(json!(50))
                )),
                after: variablemap!(
                  "FloorId" => VariableValue::from(json!("floor_01_01")),
                  "ComfortLevel" =>  VariableValue::from(json!(51))
                ),
                grouping_keys: vec!["FloorId".into()],
            }));

            let result = building_comfort_level_calc_query
                .process_source_change(change.clone())
                .await
                .unwrap();
            assert_eq!(result.len(), 1);
            // println!("res {:?}", result);
            assert!(result.contains(&QueryPartEvaluationContext::Aggregation {
                grouping_keys: vec!["BuildingId".into()],
                default_before: false,
                default_after: false,
                before: Some(variablemap!(
                  "BuildingId" => VariableValue::from(json!("building_01")),
                  "ComfortLevel" => VariableValue::from(json!(50.0))
                )),
                after: variablemap!(
                  "BuildingId" => VariableValue::from(json!("building_01")),
                  "ComfortLevel" => VariableValue::from(json!(50.5))
                ),
            }));

            let result = room_comfort_level_alert_query
                .process_source_change(change.clone())
                .await
                .unwrap();
            assert_eq!(result.len(), 1);
            assert!(result.contains(&QueryPartEvaluationContext::Adding {
                after: variablemap!(
                  "RoomId" => VariableValue::from(json!("room_01_01_01")),
                  "ComfortLevel" => VariableValue::from(json!(54))
                ),
            }));

            let result = floor_comfort_level_alert_query
                .process_source_change(change.clone())
                .await
                .unwrap();
            assert_eq!(result.len(), 1);
            assert!(result.contains(&QueryPartEvaluationContext::Adding {
                after: variablemap!(
                  "FloorId" => VariableValue::from(json!("floor_01_01")),
                  "ComfortLevel" => VariableValue::from(json!(51))
                ),
            }));

            let result = building_comfort_level_alert_query
                .process_source_change(change.clone())
                .await
                .unwrap();
            assert_eq!(result.len(), 1);
            assert!(result.contains(&QueryPartEvaluationContext::Adding {
                after: variablemap!(
                  "BuildingId" => VariableValue::from(json!("building_01")),
                  "ComfortLevel" => VariableValue::from(json!(50.5))
                ),
            }));

            let result = ui_query
                .process_source_change(change.clone())
                .await
                .unwrap();
            assert_eq!(result.len(), 1);
            assert!(result.contains(&QueryPartEvaluationContext::Updating {
                before: variablemap!(
                  "RoomId" => VariableValue::from(json!("room_01_01_01")),
                  "RoomName" => VariableValue::from(json!("Room 01_01_01")),
                  "FloorId" => VariableValue::from(json!("floor_01_01")),
                  "FloorName" => VariableValue::from(json!("Floor 01_01")),
                  "BuildingId" => VariableValue::from(json!("building_01")),
                  "BuildingName" => VariableValue::from(json!("Building 01")),
                  "Temperature" => VariableValue::from(json!(72)),
                  "CO2" => VariableValue::from(json!(500)),
                  "Humidity" => VariableValue::from(json!(42)),
                  "ComfortLevel" => VariableValue::from(json!(50))
                ),
                after: variablemap!(
                  "RoomId" => VariableValue::from(json!("room_01_01_01")),
                  "RoomName" => VariableValue::from(json!("Room 01_01_01")),
                  "FloorId" => VariableValue::from(json!("floor_01_01")),
                  "FloorName" => VariableValue::from(json!("Floor 01_01")),
                  "BuildingId" => VariableValue::from(json!("building_01")),
                  "BuildingName" => VariableValue::from(json!("Building 01")),
                  "Temperature" => VariableValue::from(json!(74)),
                  "CO2" => VariableValue::from(json!(500)),
                  "Humidity" => VariableValue::from(json!(44)),
                  "ComfortLevel" => VariableValue::from(json!(54))
                ),
            }));
        }
    }

    //room_comfort_level
    {
        //Room 01_01_01 comfort level decreases - resolves alert
        {
            let change = SourceChange::Update {
                element: Element::Node {
                    metadata: ElementMetadata {
                        reference: ElementReference::new("Contoso.Facilities", "room_01_01_01"),
                        labels: Arc::new([Arc::from("Room")]),
                        effective_from: 1300,
                    },
                    properties: ElementPropertyMap::from(
                        json!({ "name": "Room 01_01_01", "temp": 72, "humidity": 42, "co2": 500 }),
                    ),
                },
            };

            let result = room_comfort_level_calc_query
                .process_source_change(change.clone())
                .await
                .unwrap();
            assert_eq!(result.len(), 1);
            assert!(result.contains(&QueryPartEvaluationContext::Updating {
                before: variablemap!(
                  "RoomId" => VariableValue::from(json!("room_01_01_01")),
                  "ComfortLevel" => VariableValue::from(json!(54.0))
                ),
                after: variablemap!(
                  "RoomId" => VariableValue::from(json!("room_01_01_01")),
                  "ComfortLevel" => VariableValue::from(json!(50.0))
                ),
            }));

            let result = floor_comfort_level_calc_query
                .process_source_change(change.clone())
                .await
                .unwrap();
            assert_eq!(result.len(), 1);
            assert!(result.contains(&QueryPartEvaluationContext::Aggregation {
                grouping_keys: vec!["FloorId".into()],
                default_before: false,
                default_after: false,
                before: Some(variablemap!(
                  "FloorId" => VariableValue::from(json!("floor_01_01")),
                  "ComfortLevel" => VariableValue::from(json!(51.0))
                )),
                after: variablemap!(
                  "FloorId" => VariableValue::from(json!("floor_01_01")),
                  "ComfortLevel" => VariableValue::from(json!(50.0))
                ),
            }));

            let result = building_comfort_level_calc_query
                .process_source_change(change.clone())
                .await
                .unwrap();
            assert_eq!(result.len(), 1);
            assert!(result.contains(&QueryPartEvaluationContext::Aggregation {
                grouping_keys: vec!["BuildingId".into()],
                default_before: false,
                default_after: false,
                before: Some(variablemap!(
                  "BuildingId" => VariableValue::from(json!("building_01")),
                  "ComfortLevel" => VariableValue::from(json!(50.5))
                )),
                after: variablemap!(
                  "BuildingId" => VariableValue::from(json!("building_01")),
                  "ComfortLevel" => VariableValue::from(json!(50.0))
                ),
            }));

            let result = room_comfort_level_alert_query
                .process_source_change(change.clone())
                .await
                .unwrap();
            assert_eq!(result.len(), 1);
            assert!(result.contains(&QueryPartEvaluationContext::Removing {
                before: variablemap!(
                  "RoomId" => VariableValue::from(json!("room_01_01_01")),
                  "ComfortLevel" => VariableValue::from(json!(54))
                ),
            }));

            let result = floor_comfort_level_alert_query
                .process_source_change(change.clone())
                .await
                .unwrap();
            assert_eq!(result.len(), 1);
            assert!(result.contains(&QueryPartEvaluationContext::Removing {
                before: variablemap!(
                  "FloorId" => VariableValue::from(json!("floor_01_01")),
                  "ComfortLevel" => VariableValue::from(json!(51))
                ),
            }));

            let result = building_comfort_level_alert_query
                .process_source_change(change.clone())
                .await
                .unwrap();
            assert_eq!(result.len(), 1);
            assert!(result.contains(&QueryPartEvaluationContext::Removing {
                before: variablemap!(
                  "BuildingId" => VariableValue::from(json!("building_01")),
                  "ComfortLevel" => VariableValue::from(json!(50.5))
                ),
            }));

            let result = ui_query
                .process_source_change(change.clone())
                .await
                .unwrap();
            assert_eq!(result.len(), 1);
            assert!(result.contains(&QueryPartEvaluationContext::Updating {
                before: variablemap!(
                  "RoomId" => VariableValue::from(json!("room_01_01_01")),
                  "RoomName" => VariableValue::from(json!("Room 01_01_01")),
                  "FloorId" => VariableValue::from(json!("floor_01_01")),
                  "FloorName" => VariableValue::from(json!("Floor 01_01")),
                  "BuildingId" => VariableValue::from(json!("building_01")),
                  "BuildingName" => VariableValue::from(json!("Building 01")),
                  "ComfortLevel" => VariableValue::from(json!(54)),
                  "Temperature" => VariableValue::from(json!(74)),
                  "CO2" => VariableValue::from(json!(500)),
                  "Humidity" => VariableValue::from(json!(44)),
                  "ComfortLevel" => VariableValue::from(json!(54))
                ),
                after: variablemap!(
                  "RoomId" => VariableValue::from(json!("room_01_01_01")),
                  "RoomName" => VariableValue::from(json!("Room 01_01_01")),
                  "FloorId" => VariableValue::from(json!("floor_01_01")),
                  "FloorName" => VariableValue::from(json!("Floor 01_01")),
                  "BuildingId" => VariableValue::from(json!("building_01")),
                  "BuildingName" => VariableValue::from(json!("Building 01")),
                  "ComfortLevel" => VariableValue::from(json!(50)),
                  "Temperature" => VariableValue::from(json!(72)),
                  "CO2" => VariableValue::from(json!(500)),
                  "Humidity" => VariableValue::from(json!(42)),
                  "ComfortLevel" => VariableValue::from(json!(50))
                ),
            }));
        }
    }
}

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

use drasi_core::models::SourceMiddlewareConfig;
use serde_json::json;

pub fn remap_query() -> &'static str {
    "
  MATCH (v:Vehicle)
  RETURN
    v.id,
    v.currentSpeed
    "
}

pub fn middlewares() -> Vec<Arc<SourceMiddlewareConfig>> {
    let cfg: serde_json::Map<String, serde_json::Value> = json!({
        "Telemetry": {
            "insert": [{
                "selector": "$[?(@.additionalProperties.Source == 'telemetry')]",
                "op": "Update",
                "label": "Vehicle",
                "id": "$.vehicleId",
                "properties": {
                    "id": "$.vehicleId",
                    "currentSpeed": "$.signals[?(@.name == 'Vehicle.Speed')].value"
                }
            }]
        }
    })
    .as_object()
    .unwrap()
    .clone();

    vec![Arc::new(SourceMiddlewareConfig::new("map", "map", cfg))]
}

pub fn source_pipeline() -> Vec<String> {
    vec!["map".to_string()]
}

// Returns an intentionally invalid middleware configuration (bad selector)
pub fn invalid_middlewares() -> Vec<Arc<SourceMiddlewareConfig>> {
    let cfg: serde_json::Map<String, serde_json::Value> = json!({
        "Telemetry": {
            "insert": [{
                // Invalid JsonPath (leading 'z') to trigger config error
                "selector": "z$[?(@.additionalProperties.Source == 'telemetry')]",
                "op": "Update",
                "label": "Vehicle",
                "id": "$.vehicleId",
                "properties": {
                    "id": "$.vehicleId",
                    "currentSpeed": "$.signals[?(@.name == 'Vehicle.Speed')].value"
                }
            }]
        }
    })
    .as_object()
    .unwrap()
    .clone();

    vec![Arc::new(SourceMiddlewareConfig::new("map", "map", cfg))]
}

// Returns an incorrectly structured config (insert should be an array, but we pass an object)
pub fn incorrect_structure_middlewares() -> Vec<Arc<SourceMiddlewareConfig>> {
    let cfg: serde_json::Map<String, serde_json::Value> = json!({
        "Telemetry": {
            // incorrect type here: should be an array of mappings
            "insert": {
                "selector": "$[?(@.additionalProperties.Source == 'telemetry')]",
                "op": "Update",
                "label": "Vehicle",
                "id": "$.vehicleId",
                "properties": {
                    "id": "$.vehicleId",
                    "currentSpeed": "$.signals[?(@.name == 'Vehicle.Speed')].value"
                }
            }
        }
    })
    .as_object()
    .unwrap()
    .clone();

    vec![Arc::new(SourceMiddlewareConfig::new("map", "map", cfg))]
}

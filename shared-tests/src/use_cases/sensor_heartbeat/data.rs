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

use drasi_core::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange,
};

pub fn get_bootstrap_data(effective_from: u64) -> Vec<SourceChange> {
    let raw = json!(
      {
        "nodes": [
          { "type": "node", "id": "e1", "labels": ["Equipment"], "properties": { "name": "Turbine 1" } },
          { "type": "node", "id": "e2", "labels": ["Equipment"], "properties": { "name": "Turbine 2" } },

          { "type": "node", "id": "e1-s1", "labels": ["Sensor"], "properties": { "type": "RPM" } },
          { "type": "node", "id": "e1-s2", "labels": ["Sensor"], "properties": { "type": "Temp" } },

          { "type": "node", "id": "e2-s1", "labels": ["Sensor"], "properties": { "type": "RPM" } },
          { "type": "node", "id": "e2-s2", "labels": ["Sensor"], "properties": { "type": "Temp" } },

          { "type": "node", "id": "e1-s1-v1", "labels": ["SensorValue"], "properties": { "value": 1000 } },
          { "type": "node", "id": "e1-s2-v1", "labels": ["SensorValue"], "properties": { "value": 120 } },

          { "type": "node", "id": "e2-s1-v1", "labels": ["SensorValue"], "properties": { "value": 1000 } },
          { "type": "node", "id": "e2-s2-v1", "labels": ["SensorValue"], "properties": { "value": 120 } },


        ],
        "rels": [
            { "type": "rel", "id": "r-e1-s1", "startId": "e1", "labels": ["HAS_SENSOR"], "endId": "e1-s1" },
            { "type": "rel", "id": "r-e1-s2", "startId": "e1", "labels": ["HAS_SENSOR"], "endId": "e1-s2" },

            { "type": "rel", "id": "r-e2-s1", "startId": "e2", "labels": ["HAS_SENSOR"], "endId": "e2-s1" },
            { "type": "rel", "id": "r-e2-s2", "startId": "e2", "labels": ["HAS_SENSOR"], "endId": "e2-s2" },

            { "type": "rel", "id": "r-e1-s1-v1", "startId": "e1-s1", "labels": ["HAS_VALUE"], "endId": "e1-s1-v1" },
            { "type": "rel", "id": "r-e1-s2-v1", "startId": "e1-s2", "labels": ["HAS_VALUE"], "endId": "e1-s2-v1" },

            { "type": "rel", "id": "r-e2-s1-v1", "startId": "e2-s1", "labels": ["HAS_VALUE"], "endId": "e2-s1-v1" },
            { "type": "rel", "id": "r-e2-s2-v1", "startId": "e2-s2", "labels": ["HAS_VALUE"], "endId": "e2-s2-v1" },
        ]
      }
    );

    let mut result = Vec::new();

    for node in raw["nodes"].as_array().unwrap() {
        let node = node.as_object().unwrap();
        let id = node["id"].as_str().unwrap();
        let labels = node["labels"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| Arc::from(l.as_str().unwrap()))
            .collect();
        let properties = ElementPropertyMap::from(node["properties"].clone());
        result.push(SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", id),
                    labels,
                    effective_from,
                },
                properties,
            },
        });
    }

    for rel in raw["rels"].as_array().unwrap() {
        let rel = rel.as_object().unwrap();
        let id = rel["id"].as_str().unwrap();
        let labels = rel["labels"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| Arc::from(l.as_str().unwrap()))
            .collect();
        let start_id = rel["startId"].as_str().unwrap().to_string();
        let end_id = rel["endId"].as_str().unwrap().to_string();
        let properties = ElementPropertyMap::new();
        result.push(SourceChange::Insert {
            element: Element::Relation {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", id),
                    labels,
                    effective_from,
                },
                properties,
                in_node: ElementReference::new("test", start_id.as_str()),
                out_node: ElementReference::new("test", end_id.as_str()),
            },
        });
    }

    result
}

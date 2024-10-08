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

pub fn get_bootstrap_data() -> Vec<SourceChange> {
    let raw = json!(
      {
        "nodes": [
          { "type": "node", "id": "building_01", "labels": ["Building"], "properties": { "name": "Building 01" } },
          { "type": "node", "id": "floor_01_01", "labels": ["Floor"], "properties": { "name": "Floor 01_01" } },
          { "type": "node", "id": "floor_01_02", "labels": ["Floor"], "properties": { "name": "Floor 01_02" } },
          { "type": "node", "id": "room_01_01_01", "labels": ["Room"], "properties": { "name": "Room 01_01_01", "temp": 72, "humidity": 42, "co2": 500} },
          { "type": "node", "id": "room_01_01_02", "labels": ["Room"], "properties": { "name": "Room 01_01_02", "temp": 72, "humidity": 42, "co2": 500} },
          { "type": "node", "id": "room_01_01_03", "labels": ["Room"], "properties": { "name": "Room 01_01_03", "temp": 72, "humidity": 42, "co2": 500} },
          { "type": "node", "id": "room_01_01_04", "labels": ["Room"], "properties": { "name": "Room 01_01_04", "temp": 72, "humidity": 42, "co2": 500} },
          { "type": "node", "id": "room_01_02_01", "labels": ["Room"], "properties": { "name": "Room 01_02_01", "temp": 72, "humidity": 42, "co2": 500} },
          { "type": "node", "id": "room_01_02_02", "labels": ["Room"], "properties": { "name": "Room 01_02_02", "temp": 72, "humidity": 42, "co2": 500} },
          { "type": "node", "id": "room_01_02_03", "labels": ["Room"], "properties": { "name": "Room 01_02_03", "temp": 72, "humidity": 42, "co2": 500} },
          { "type": "node", "id": "room_01_02_04", "labels": ["Room"], "properties": { "name": "Room 01_02_04", "temp": 72, "humidity": 42, "co2": 500} }
        ],
        "rels": [
          { "type": "rel", "id": "rel001", "startId": "floor_01_01", "labels": ["PART_OF"], "endId": "building_01" },
          { "type": "rel", "id": "rel002", "startId": "floor_01_02", "labels": ["PART_OF"], "endId": "building_01" },
          { "type": "rel", "id": "rel003", "startId": "room_01_01_01", "labels": ["PART_OF"], "endId": "floor_01_01" },
          { "type": "rel", "id": "rel004", "startId": "room_01_01_02", "labels": ["PART_OF"], "endId": "floor_01_01" },
          { "type": "rel", "id": "rel005", "startId": "room_01_01_03", "labels": ["PART_OF"], "endId": "floor_01_01" },
          { "type": "rel", "id": "rel006", "startId": "room_01_01_04", "labels": ["PART_OF"], "endId": "floor_01_01" },
          { "type": "rel", "id": "rel007", "startId": "room_01_02_01", "labels": ["PART_OF"], "endId": "floor_01_02" },
          { "type": "rel", "id": "rel008", "startId": "room_01_02_02", "labels": ["PART_OF"], "endId": "floor_01_02" },
          { "type": "rel", "id": "rel009", "startId": "room_01_02_03", "labels": ["PART_OF"], "endId": "floor_01_02" },
          { "type": "rel", "id": "rel010", "startId": "room_01_02_04", "labels": ["PART_OF"], "endId": "floor_01_02" }
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
                    reference: ElementReference::new("Contoso.Facilities", id),
                    labels,
                    effective_from: 0,
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
                    reference: ElementReference::new("Contoso.Facilities", id),
                    labels,
                    effective_from: 0,
                },
                properties,
                in_node: ElementReference::new("Contoso.Facilities", start_id.as_str()),
                out_node: ElementReference::new("Contoso.Facilities", end_id.as_str()),
            },
        });
    }

    result
}

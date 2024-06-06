use std::{sync::Arc};

use serde_json::json;

use drasi_core::{
    models::{Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange},
};

pub fn get_bootstrap_data() -> Vec<SourceChange> {
    let raw = json!(
      {
        "nodes": [
          { "type": "node", "id": "equip_01", "labels": ["Equipment"], "properties": { "id": "equip_01", "name": "Freezer 01", "type": "freezer" } },
          { "type": "node", "id": "temp_sensor_01", "labels": ["Sensor"], "properties": { "id": "temp_sensor_01", "equip_id": "equip_01", "name": "Temp Sensor 01", "type": "temperature" } },
          { "type": "node", "id": "door_sensor_01", "labels": ["Sensor"], "properties": { "id": "door_sensor_01", "equip_id": "equip_01", "name": "Door Sensor 01", "type": "door" } },
        ],
        "rels": []
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
                    reference: ElementReference::new("Reflex.FACILITIES", id),
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
                    reference: ElementReference::new("Reflex.FACILITIES", id),
                    labels,
                    effective_from: 0,
                },
                properties,
                in_node: ElementReference::new("Reflex.FACILITIES", start_id.as_str()),
                out_node: ElementReference::new("Reflex.FACILITIES", end_id.as_str()),
            },
        });
    }

    result
}

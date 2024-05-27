use std::{collections::BTreeMap, sync::Arc};

use serde_json::json;

use drasi_query_core::{
    evaluation::variable_value::VariableValue,
    models::{Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange},
};

pub fn get_bootstrap_data() -> Vec<SourceChange> {
    let raw = json!(
      {
        "nodes": [
          { "type": "node", "id": "cust_01", "labels": ["Customer"], "properties": { "id": "cust_01", "name": "Customer 01", "email": "cust_01@reflex.org" } },
          { "type": "node", "id": "cust_02", "labels": ["Customer"], "properties": { "id": "cust_02", "name": "Customer 02", "email": "cust_02@reflex.org" } },
          { "type": "node", "id": "step_x", "labels": ["Step"], "properties": { "id": "step_x", "name": "Step X" } },
          { "type": "node", "id": "step_y", "labels": ["Step"], "properties": { "id": "step_y", "name": "Step Y" } },
          { "type": "node", "id": "step_z", "labels": ["Step"], "properties": { "id": "step_z", "name": "Step Z" } },
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
                    reference: ElementReference::new("Reflex.CRM", id),
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
                    reference: ElementReference::new("Reflex.CRM", id),
                    labels,
                    effective_from: 0,
                },
                properties,
                in_node: ElementReference::new("Reflex.CRM", start_id.as_str()),
                out_node: ElementReference::new("Reflex.CRM", end_id.as_str()),
            },
        });
    }

    result
}

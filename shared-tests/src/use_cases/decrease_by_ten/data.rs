use std::{sync::Arc};

use serde_json::json;

use drasi_core::{
    models::{Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange},
};

// Add 2 Customers each with 4 calls to the initial graph. The calls are on the same day (2023-10-01)
// and are 1 second apart using an epoch second timestamp
pub fn get_bootstrap_data() -> Vec<SourceChange> {
    let raw = json!(
      {
        "nodes": [
          { "type": "node", "id": "prod_01", "labels": ["Product"], "properties": { "id": "prod_01", "name": "Product 01", "prod_mgr_id": "emp_01" } },
          { "type": "node", "id": "emp_01", "labels": ["Employee"], "properties": { "id": "emp_01", "name": "Employee 01" } },
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
                    reference: ElementReference::new("Reflex.Sales", id),
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
                    reference: ElementReference::new("Reflex.Sales", id),
                    labels,
                    effective_from: 0,
                },
                properties,
                in_node: ElementReference::new("Reflex.Sales", start_id.as_str()),
                out_node: ElementReference::new("Reflex.Sales", end_id.as_str()),
            },
        });
    }

    result
}

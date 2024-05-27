use std::{collections::BTreeMap, sync::Arc};

use serde_json::json;

use drasi_query_core::{
    evaluation::variable_value::VariableValue,
    models::{Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange},
};

// Add 2 Customers each with 4 calls to the initial graph. The calls are on the same day (2023-10-01)
// and are 1 second apart using an epoch second timestamp
pub fn get_bootstrap_data() -> Vec<SourceChange> {
    let raw = json!(
      {
        "nodes": [
          { "type": "node", "id": "org_01", "labels": ["Organization"], "properties": { "id": "org_01", "name": "Organization 01" } },
          { "type": "node", "id": "customer_01", "labels": ["Customer"], "properties": { "id": "customer_01", "org_id": "org_01", "name": "Customer 01" } },
          { "type": "node", "id": "customer_02", "labels": ["Customer"], "properties": { "id": "customer_02", "org_id": "org_01", "name": "Customer 02"  } },
          { "type": "node", "id": "call_01", "labels": ["Call"], "properties": { "cust_id": "customer_01", "timestamp": 1696150800, "type": "support" } },
          { "type": "node", "id": "call_02", "labels": ["Call"], "properties": { "cust_id": "customer_01", "timestamp": 1696150801, "type": "support" } },
          { "type": "node", "id": "call_03", "labels": ["Call"], "properties": { "cust_id": "customer_01", "timestamp": 1696150802, "type": "support" } },
          { "type": "node", "id": "call_04", "labels": ["Call"], "properties": { "cust_id": "customer_01", "timestamp": 1696150803, "type": "support" } },
          { "type": "node", "id": "call_05", "labels": ["Call"], "properties": { "cust_id": "customer_02", "timestamp": 1696150804, "type": "support" } },
          { "type": "node", "id": "call_06", "labels": ["Call"], "properties": { "cust_id": "customer_02", "timestamp": 1696150805, "type": "support" } },
          { "type": "node", "id": "call_07", "labels": ["Call"], "properties": { "cust_id": "customer_02", "timestamp": 1696150806, "type": "support" } },
          { "type": "node", "id": "call_08", "labels": ["Call"], "properties": { "cust_id": "customer_02", "timestamp": 1696150807, "type": "support"  } },
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

use std::sync::Arc;

use serde_json::json;

use drasi_core::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange,
};

pub fn get_bootstrap_data() -> Vec<SourceChange> {
    let raw = json!(
      {
        "nodes": [
          { "type": "node", "id": "customer_01", "labels": ["Customer"], "properties": { "id": "customer_01", "am_id": "employee_01", "name": "Customer 01" } },
          { "type": "node", "id": "employee_01", "labels": ["Employee"], "properties": { "id": "employee_01", "name": "Employee 01", "email": "emp_01@reflex.com" } },
          { "type": "node", "id": "invoice_01", "labels": ["Invoice"], "properties": { "id": "invoice_01", "cust_id": "customer_01", "due_date": "2023-10-01" } },
          { "type": "node", "id": "invoice_status_invoice_01", "labels": ["InvoiceStatus"], "properties": { "id": "invoice_status_invoice_01", "invoice_id": "invoice_01", "timestamp": 1696118400, "status": "due" } },
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

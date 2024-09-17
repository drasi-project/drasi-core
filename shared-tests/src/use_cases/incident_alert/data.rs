#![allow(clippy::unwrap_used)]
use std::sync::Arc;

use serde_json::json;

use drasi_core::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange,
};

pub fn get_bootstrap_data() -> Vec<SourceChange> {
    let raw = json!(
      {
        "nodes": [
          // TEAM Nodes
          { "type": "node", "id": "rg", "labels": ["Team"], "properties": { "name": "Reactive Graph", "type": "aaa" } },
          // EMPLOYEE Nodes
          { "type": "node", "id": "allen", "labels": ["Employee"], "properties": { "name": "Allen", "email": "allen@contoso.com" } },
          { "type": "node", "id": "bob", "labels": ["Employee"], "properties": { "name": "Bob", "email": "bob@contoso.com" } },
          { "type": "node", "id": "claire", "labels": ["Employee"], "properties": { "name": "Claire", "email": "claire@contoso.com" } },
          { "type": "node", "id": "danny", "labels": ["Employee"], "properties": { "name": "Danny", "email": "danny@contoso.com" } },
          { "type": "node", "id": "emily", "labels": ["Employee"], "properties": { "name": "Emily", "email": "emily@contoso.com" } },
          // BUILDING Nodes
          { "type": "node", "id": "mv001", "labels": ["Building"], "properties": { "name": "Mission Viejo" } },
          { "type": "node", "id": "b040", "labels": ["Building"], "properties": { "name": "Building 40" } },
          // REGION Nodes
          { "type": "node", "id": "socal", "labels": ["Region"], "properties": { "name": "SoCal" } },
          { "type": "node", "id": "redmond_wa", "labels": ["Region"], "properties": { "name": "Redmond" } }
        ],
        "rels": [
          // MANAGES Relations
          { "type": "rel", "id":"r00", "labels": ["MANAGES"], "startId": "allen", "endId": "rg" },
          // ASSIGNED_TO Relations
          { "type": "rel", "id":"r01", "labels": ["ASSIGNED_TO"], "startId": "allen", "endId": "rg" },
          { "type": "rel", "id":"r02", "labels": ["ASSIGNED_TO"], "startId": "bob", "endId": "rg" },
          { "type": "rel", "id":"r03", "labels": ["ASSIGNED_TO"], "startId": "claire", "endId": "rg" },
          { "type": "rel", "id":"r04", "labels": ["ASSIGNED_TO"], "startId": "danny", "endId": "rg" },
          { "type": "rel", "id":"r05", "labels": ["ASSIGNED_TO"], "startId": "emily", "endId": "rg" },
          // LOCATED_IN Relations
          { "type": "rel", "id":"r06", "labels": ["LOCATED_IN"], "startId": "allen", "endId": "mv001" },
          { "type": "rel", "id":"r07", "labels": ["LOCATED_IN"], "startId": "bob", "endId": "mv001" },
          { "type": "rel", "id":"r08", "labels": ["LOCATED_IN"], "startId": "claire", "endId": "mv001" },
          { "type": "rel", "id":"r09", "labels": ["LOCATED_IN"], "startId": "danny", "endId": "b040" },
          { "type": "rel", "id":"r10", "labels": ["LOCATED_IN"], "startId": "emily", "endId": "b040" },
          { "type": "rel", "id":"r11", "labels": ["LOCATED_IN"], "startId": "mv001", "endId": "socal" },
          { "type": "rel", "id":"r12", "labels": ["LOCATED_IN"], "startId": "b040", "endId": "redmond_wa" }
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
                    reference: ElementReference::new("Contoso.HumanResources", id),
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
                    reference: ElementReference::new("Contoso.HumanResources", id),
                    labels,
                    effective_from: 0,
                },
                properties,
                in_node: ElementReference::new("Contoso.HumanResources", start_id.as_str()),
                out_node: ElementReference::new("Contoso.HumanResources", end_id.as_str()),
            },
        });
    }

    result
}

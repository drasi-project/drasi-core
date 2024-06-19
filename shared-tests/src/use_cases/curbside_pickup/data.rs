use std::sync::Arc;

use serde_json::json;

use drasi_core::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange,
};

pub fn get_bootstrap_data() -> Vec<SourceChange> {
    let physical_data = json!(
      {
        "nodes": [
          // ZONE Nodes
          { "type": "node", "id": "zone_01", "labels": ["Zone"], "properties": { "id": "zone_01", "name": "Zone 01", "type": "Curbside Queue" } }
        ],
        "rels": []
      }
    );

    let retail_data = json!(
      {
        "nodes": [
          // ORDER Nodes
          { "type": "node", "id": "order_01", "labels": ["Order"], "properties": { "name": "Order 01", "status": "cooking" } },
          { "type": "node", "id": "order_02", "labels": ["Order"], "properties": { "name": "Order 02", "status": "cooking" } },
          { "type": "node", "id": "order_03", "labels": ["Order"], "properties": { "name": "Order 03", "status": "cooking" } },
          { "type": "node", "id": "order_04", "labels": ["Order"], "properties": { "name": "Order 04", "status": "cooking" } },

          // ORDER PICKUP Nodes
          { "type": "node", "id": "order_pickup_01", "labels": ["OrderPickup"], "properties": { "name": "OrderPickup 01" } },
          { "type": "node", "id": "order_pickup_02", "labels": ["OrderPickup"], "properties": { "name": "OrderPickup 02" } },
          { "type": "node", "id": "order_pickup_03", "labels": ["OrderPickup"], "properties": { "name": "OrderPickup 03" } },
          { "type": "node", "id": "order_pickup_04", "labels": ["OrderPickup"], "properties": { "name": "OrderPickup 04" } },
          // DRIVER Nodes
          { "type": "node", "id": "driver_01", "labels": ["Driver"], "properties": { "name": "Driver 01", "vehicleLicensePlate": "ABC123" } },
          { "type": "node", "id": "driver_02", "labels": ["Driver"], "properties": { "name": "Driver 02", "vehicleLicensePlate": "XYZ789" } },
          { "type": "node", "id": "driver_03", "labels": ["Driver"], "properties": { "name": "Driver 03", "vehicleLicensePlate": "drasi" } },
          { "type": "node", "id": "driver_04", "labels": ["Driver"], "properties": { "name": "Driver 04", "vehicleLicensePlate": "drasi" } }
        ],
        "rels": [
          // PICKUP_ORDER Nodes
          { "type": "rel", "id": "r01", "startId": "order_pickup_01", "labels": ["PICKUP_ORDER"], "endId": "order_01" },
          { "type": "rel", "id": "r02", "startId": "order_pickup_02", "labels": ["PICKUP_ORDER"], "endId": "order_02" },
          // PICKUP_DRIVER Nodes
          { "type": "rel", "id": "r03", "startId": "order_pickup_01", "labels": ["PICKUP_DRIVER"], "endId": "driver_01" },
          { "type": "rel", "id": "r04", "startId": "order_pickup_02", "labels": ["PICKUP_DRIVER"], "endId": "driver_02" },
          { "type": "rel", "id": "r05", "startId": "order_pickup_03", "labels": ["PICKUP_ORDER"], "endId": "order_03" },
          { "type": "rel", "id": "r06", "startId": "order_pickup_03", "labels": ["PICKUP_DRIVER"], "endId": "driver_03" },
          { "type": "rel", "id": "r07", "startId": "order_pickup_04", "labels": ["PICKUP_ORDER"], "endId": "order_04" },
          { "type": "rel", "id": "r08", "startId": "order_pickup_04", "labels": ["PICKUP_DRIVER"], "endId": "driver_04" },
        ]
      }
    );

    let mut result = Vec::new();

    for node in physical_data["nodes"].as_array().unwrap() {
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
                    reference: ElementReference::new("Contoso.PhysicalOperations", id),
                    labels,
                    effective_from: 0,
                },
                properties,
            },
        });
    }

    for rel in physical_data["rels"].as_array().unwrap() {
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
                    reference: ElementReference::new("Contoso.PhysicalOperations", id),
                    labels,
                    effective_from: 0,
                },
                properties,
                in_node: ElementReference::new("Contoso.PhysicalOperations", start_id.as_str()),
                out_node: ElementReference::new("Contoso.PhysicalOperations", end_id.as_str()),
            },
        });
    }

    for node in retail_data["nodes"].as_array().unwrap() {
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
                    reference: ElementReference::new("Contoso.RetailOperations", id),
                    labels,
                    effective_from: 0,
                },
                properties,
            },
        });
    }

    for rel in retail_data["rels"].as_array().unwrap() {
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
                    reference: ElementReference::new("Contoso.RetailOperations", id),
                    labels,
                    effective_from: 0,
                },
                properties,
                in_node: ElementReference::new("Contoso.RetailOperations", start_id.as_str()),
                out_node: ElementReference::new("Contoso.RetailOperations", end_id.as_str()),
            },
        });
    }

    result
}

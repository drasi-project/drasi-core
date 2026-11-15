// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Source-data schema derived from webhook mappings.
//!
//! In webhook mode the mappings fully determine the labels and property
//! names the source can emit, so the schema is published without I/O. The
//! standard event routes accept arbitrary graph changes and publish none.

use async_trait::async_trait;
use drasi_computation_contracts::SourceSchemaReader;
use drasi_core_types::schema::{NodeSchema, PropertySchema, RelationSchema, SourceSchema};
use drasi_http_common::webhook::config::{ElementType, WebhookConfig};
use std::collections::BTreeMap;

fn property_schemas(properties: Option<&serde_json::Value>) -> Vec<PropertySchema> {
    match properties {
        Some(serde_json::Value::Object(map)) => map
            .keys()
            .map(|key| PropertySchema::new(key.clone()))
            .collect(),
        _ => Vec::new(),
    }
}

fn merge(entry: &mut Vec<PropertySchema>, properties: Vec<PropertySchema>) {
    for property in properties {
        if !entry.iter().any(|existing| existing.name == property.name) {
            entry.push(property);
        }
    }
}

/// The labels and property names emitted by the configured mappings, keyed
/// by the first label of each mapping. Ordering is deterministic.
pub(crate) fn derive(webhooks: &WebhookConfig) -> Option<SourceSchema> {
    let mut nodes: BTreeMap<String, Vec<PropertySchema>> = BTreeMap::new();
    let mut relations: BTreeMap<String, Vec<PropertySchema>> = BTreeMap::new();
    for mapping in webhooks.routes.iter().flat_map(|route| &route.mappings) {
        let Some(label) = mapping.template.labels.first().cloned() else {
            continue;
        };
        let properties = property_schemas(mapping.template.properties.as_ref());
        let target = match mapping.element_type {
            ElementType::Node => &mut nodes,
            ElementType::Relation => &mut relations,
        };
        merge(target.entry(label).or_default(), properties);
    }
    if nodes.is_empty() && relations.is_empty() {
        return None;
    }
    Some(SourceSchema {
        nodes: nodes
            .into_iter()
            .map(|(label, properties)| NodeSchema { label, properties })
            .collect(),
        relations: relations
            .into_iter()
            .map(|(label, properties)| RelationSchema {
                label,
                from: None,
                to: None,
                properties,
            })
            .collect(),
    })
}

/// Reads the schema captured from the configuration at construction or the
/// last applied update.
pub(crate) struct WebhookSchemaReader(pub(crate) crate::component::SharedSchema);

#[async_trait]
impl SourceSchemaReader for WebhookSchemaReader {
    async fn describe_schema(&self) -> anyhow::Result<Option<SourceSchema>> {
        Ok(self.0.load().as_ref().clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn mappings_define_labels_and_properties() -> anyhow::Result<()> {
        let config: crate::HttpSourceConfig = crate::HttpSourceConfig::from_value(json!({
            "stream":"s","webhooks":{"routes":[{
                "path":"/a","methods":["POST"],"mappings":[
                    {"operation":"insert","elementType":"node","template":{"id":"{{payload.id}}","labels":["Room"],"properties":{"temp":"{{payload.t}}"}}},
                    {"operation":"update","elementType":"node","template":{"id":"{{payload.id}}","labels":["Room"],"properties":{"temp":"{{payload.t}}","name":"{{payload.n}}"}}},
                    {"operation":"insert","elementType":"relation","template":{"id":"{{payload.id}}","labels":["IN"],"from":"{{payload.a}}","to":"{{payload.b}}"}}
                ]
            }]}
        }))?;
        let schema = derive(config.webhooks.as_ref().expect("webhooks")).expect("schema");
        assert_eq!(schema.nodes.len(), 1);
        assert_eq!(schema.nodes[0].label, "Room");
        let names: Vec<_> = schema.nodes[0]
            .properties
            .iter()
            .map(|p| p.name.as_str())
            .collect();
        assert_eq!(names, ["temp", "name"]);
        assert_eq!(schema.relations[0].label, "IN");
        Ok(())
    }
}

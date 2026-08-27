// Copyright 2025 The Drasi Authors.
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

use std::collections::BTreeMap;
use std::{error::Error, fmt};

use drasi_core::models::SourceChange;
use drasi_lib::schema::{NodeSchema, PropertySchema, RelationSchema, SourceSchema};
use drasi_source_mapping::{ElementType, SourceMapping, SourceMappingEngine};
use serde_json::{json, Value};
use tracing::debug;

use crate::config::WebSocketSourceConfig;

const MAX_ITEMS_PER_MESSAGE: usize = 1_000;

#[derive(Debug)]
pub(crate) enum FrameError {
    MessageTooLarge,
    MalformedJson(serde_json::Error),
    InvalidItemsPathType,
    TooManyItems,
}

impl FrameError {
    pub(crate) fn is_recoverable(&self) -> bool {
        matches!(self, Self::MalformedJson(_))
    }
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MessageTooLarge => f.write_str("WebSocket message exceeds maxMessageSizeBytes"),
            Self::MalformedJson(error) => {
                write!(f, "WebSocket text message contains invalid JSON: {error}")
            }
            Self::InvalidItemsPathType => f.write_str("itemsPath must select a top-level array"),
            Self::TooManyItems => write!(
                f,
                "WebSocket message selected more than {MAX_ITEMS_PER_MESSAGE} items"
            ),
        }
    }
}

impl Error for FrameError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::MalformedJson(error) => Some(error),
            _ => None,
        }
    }
}

pub(crate) struct FrameMapper {
    engine: SourceMappingEngine,
    items_path: String,
    mappings: Vec<SourceMapping>,
    max_message_size_bytes: usize,
}

impl FrameMapper {
    pub(crate) fn new(config: &WebSocketSourceConfig) -> Self {
        Self {
            engine: SourceMappingEngine::new(),
            items_path: config.items_path.clone(),
            mappings: config.mappings.clone(),
            max_message_size_bytes: config.max_message_size_bytes,
        }
    }

    pub(crate) fn map_text(
        &self,
        source_id: &str,
        text: &str,
    ) -> Result<Vec<SourceChange>, FrameError> {
        if text.len() > self.max_message_size_bytes {
            return Err(FrameError::MessageTooLarge);
        }

        let envelope: Value = serde_json::from_str(text).map_err(FrameError::MalformedJson)?;
        let selected_item_count = selected_item_count(&envelope, &self.items_path)?;
        if selected_item_count > MAX_ITEMS_PER_MESSAGE {
            return Err(FrameError::TooManyItems);
        }
        if selected_item_count == 0 {
            return Ok(Vec::new());
        }

        let mut changes = Vec::new();
        // An envelope.* condition has one result for the entire frame. Evaluate
        // it lazily, cache it across selected items, and expose the envelope as
        // payload.envelope because SourceMappingEngine resolves these fields
        // relative to payload.
        let mut envelope_condition_context = None;
        let mut envelope_matches = vec![None; self.mappings.len()];
        let mut context = json!({
            "payload": Value::Null,
            "envelope": envelope,
            "source_id": source_id,
        });

        for index in 0..selected_item_count {
            let payload = selected_item_at(&context["envelope"], &self.items_path, index)
                .expect("validated WebSocket item selection")
                .clone();
            context["payload"] = payload;

            for (mapping_index, mapping) in self.mappings.iter().enumerate() {
                let envelope_match = if uses_envelope_condition(mapping) {
                    let matches = envelope_matches[mapping_index].get_or_insert_with(|| {
                        let condition_context =
                            envelope_condition_context.get_or_insert_with(|| {
                                json!({
                                    "payload": {
                                        "envelope": context["envelope"].clone(),
                                    },
                                })
                            });
                        self.engine.condition_matches(
                            mapping
                                .when
                                .as_ref()
                                .expect("envelope condition must exist"),
                            condition_context,
                            None,
                        )
                    });
                    Some(*matches)
                } else {
                    None
                };

                if !self.mapping_matches(mapping, &context, envelope_match) {
                    continue;
                }

                match self.engine.process_mapping(mapping, &context, source_id) {
                    Ok(change) => changes.push(change),
                    Err(_) => {
                        debug!("[{source_id}] Skipping WebSocket item that could not be mapped")
                    }
                }
                break;
            }
        }

        Ok(changes)
    }

    fn mapping_matches(
        &self,
        mapping: &SourceMapping,
        context: &Value,
        envelope_match: Option<bool>,
    ) -> bool {
        if let Some(matches) = envelope_match {
            return matches;
        }

        mapping
            .when
            .as_ref()
            .map(|condition| self.engine.condition_matches(condition, context, None))
            .unwrap_or(true)
    }
}

fn uses_envelope_condition(mapping: &SourceMapping) -> bool {
    mapping
        .when
        .as_ref()
        .and_then(|condition| condition.field.as_deref())
        .is_some_and(|field| field == "envelope" || field.starts_with("envelope."))
}

pub(crate) fn derive_schema(mappings: &[SourceMapping]) -> Option<SourceSchema> {
    let mut nodes = BTreeMap::<String, Vec<PropertySchema>>::new();
    let mut relations = BTreeMap::<String, Vec<PropertySchema>>::new();

    for mapping in mappings {
        let properties = property_schemas(mapping.template.properties.as_ref());
        for label in &mapping.template.labels {
            if label.is_empty() || label.contains("{{") {
                continue;
            }

            let target = match mapping.element_type {
                ElementType::Node => &mut nodes,
                ElementType::Relation => &mut relations,
            };
            let entry = target.entry(label.clone()).or_default();
            for property in &properties {
                if !entry.iter().any(|existing| existing.name == property.name) {
                    entry.push(property.clone());
                }
            }
        }
    }

    let nodes = nodes
        .into_iter()
        .map(|(label, properties)| NodeSchema { label, properties })
        .collect::<Vec<_>>();
    let relations = relations
        .into_iter()
        .map(|(label, properties)| RelationSchema {
            label,
            from: None,
            to: None,
            properties,
        })
        .collect::<Vec<_>>();

    if nodes.is_empty() && relations.is_empty() {
        None
    } else {
        Some(SourceSchema { nodes, relations })
    }
}

fn selected_item_count(envelope: &Value, path: &str) -> Result<usize, FrameError> {
    let selected = if path == "$" {
        envelope
    } else {
        let Some(selected) = envelope.as_object().and_then(|object| object.get(path)) else {
            return Ok(0);
        };
        if !selected.is_array() {
            return Err(FrameError::InvalidItemsPathType);
        }
        selected
    };

    match selected {
        Value::Array(items) => Ok(items.len()),
        _ => Ok(1),
    }
}

fn selected_item_at<'a>(envelope: &'a Value, path: &str, index: usize) -> Option<&'a Value> {
    let selected = if path == "$" {
        envelope
    } else {
        envelope.as_object()?.get(path)?
    };

    match selected {
        Value::Array(items) => items.get(index),
        item if index == 0 => Some(item),
        _ => None,
    }
}

fn property_schemas(properties: Option<&Value>) -> Vec<PropertySchema> {
    match properties {
        Some(Value::Object(properties)) => properties
            .keys()
            .map(|name| PropertySchema::new(name.clone()))
            .collect(),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use drasi_core::models::{Element, SourceChange};
    use drasi_source_mapping::{
        EffectiveFromConfig, ElementTemplate, ElementType, MappingCondition, OperationType,
        SourceMapping, TimestampFormat,
    };

    use super::*;

    fn mapping(label: &str) -> SourceMapping {
        SourceMapping {
            when: Some(MappingCondition {
                header: None,
                field: Some("payload.kind".to_string()),
                equals: Some("sensor".to_string()),
                contains: None,
                regex: None,
            }),
            operation: Some(OperationType::Insert),
            operation_from: None,
            operation_map: None,
            element_type: ElementType::Node,
            effective_from: None,
            template: ElementTemplate {
                id: "{{payload.id}}".to_string(),
                labels: vec![label.to_string()],
                properties: Some(json!({"value": "{{payload.value}}"})),
                from: None,
                to: None,
            },
        }
    }

    fn config() -> WebSocketSourceConfig {
        WebSocketSourceConfig {
            url: "wss://example.com".to_string(),
            items_path: "items".to_string(),
            mappings: vec![mapping("First"), mapping("Second")],
            ..Default::default()
        }
    }

    mod selection {
        use super::*;

        #[test]
        fn maps_array_field_in_order() {
            let mapper = FrameMapper::new(&config());
            let changes = mapper
            .map_text(
                "source",
                r#"{"type":"batch","items":[{"kind":"sensor","id":"a","value":1},{"kind":"sensor","id":"b","value":2}]}"#,
            )
            .unwrap();

            let ids = changes
                .iter()
                .map(|change| change.get_reference().element_id.as_ref())
                .collect::<Vec<_>>();
            assert_eq!(ids, vec!["a", "b"]);
        }

        #[test]
        fn maps_top_level_array_in_order() {
            let mut config = config();
            config.items_path = "$".to_string();
            let mapper = FrameMapper::new(&config);
            let changes = mapper
            .map_text(
                "source",
                r#"[{"kind":"sensor","id":"a","value":1},{"kind":"sensor","id":"b","value":2}]"#,
            )
            .unwrap();

            let ids = changes
                .iter()
                .map(|change| change.get_reference().element_id.as_ref())
                .collect::<Vec<_>>();
            assert_eq!(ids, vec!["a", "b"]);
        }

        #[test]
        fn applies_only_the_first_matching_mapping() {
            let mapper = FrameMapper::new(&config());
            let changes = mapper
                .map_text(
                    "source",
                    r#"{"items":[{"kind":"sensor","id":"a","value":1}]}"#,
                )
                .unwrap();
            assert_eq!(changes.len(), 1);

            match &changes[0] {
                SourceChange::Insert {
                    element: Element::Node { metadata, .. },
                } => assert_eq!(metadata.labels[0].as_ref(), "First"),
                other => panic!("expected node insert, got {other:?}"),
            }
        }

        #[test]
        fn missing_array_field_selects_no_items() {
            let mapper = FrameMapper::new(&config());
            assert!(mapper
                .map_text("source", r#"{"type":"heartbeat"}"#)
                .unwrap()
                .is_empty());
        }
    }

    mod conditions {
        use super::*;

        #[test]
        fn matches_condition_against_envelope_field() {
            let mut config = config();
            config.mappings.truncate(1);
            config.mappings[0].when = Some(MappingCondition {
                header: None,
                field: Some("envelope.type".to_string()),
                equals: Some("batch".to_string()),
                contains: None,
                regex: None,
            });
            let mapper = FrameMapper::new(&config);

            let changes = mapper
                .map_text(
                    "source",
                    r#"{"type":"batch","items":[{"id":"a","value":1},{"id":"b","value":2}]}"#,
                )
                .unwrap();
            assert_eq!(changes.len(), 2);
        }
    }

    mod selection_edge_cases {
        use super::*;

        #[test]
        fn whole_message_null_is_one_selected_item() {
            let envelope = Value::Null;
            assert_eq!(selected_item_count(&envelope, "$").unwrap(), 1);
            assert_eq!(selected_item_at(&envelope, "$", 0), Some(&Value::Null));
        }
    }

    mod schema_derivation {
        use super::*;

        #[test]
        fn derives_static_node_and_relation_schema() {
            let mut relation = mapping("READS");
            relation.element_type = ElementType::Relation;
            relation.template.from = Some("{{payload.reader}}".to_string());
            relation.template.to = Some("{{payload.sensor}}".to_string());

            let schema = derive_schema(&[mapping("Sensor"), relation]).unwrap();
            assert_eq!(schema.nodes[0].label, "Sensor");
            assert_eq!(schema.nodes[0].properties[0].name, "value");
            assert_eq!(schema.relations[0].label, "READS");
            assert_eq!(schema.relations[0].properties[0].name, "value");
        }
    }

    mod transformation {
        use super::*;

        #[test]
        fn exposes_payload_envelope_and_source_id_to_templates() {
            let mut config = config();
            config.mappings.truncate(1);
            config.mappings[0].when = None;
            config.mappings[0].template.id =
                "{{source_id}}/{{envelope.type}}/{{payload.id}}".to_string();
            let mapper = FrameMapper::new(&config);

            let change = mapper
                .map_text(
                    "source",
                    r#"{"type":"batch","items":[{"id":"sensor-1","value":1}]}"#,
                )
                .unwrap()
                .pop()
                .unwrap();

            assert_eq!(
                change.get_reference().element_id.as_ref(),
                "source/batch/sensor-1"
            );
        }

        #[test]
        fn processes_maximum_items_across_maximum_mappings() {
            let mut config = config();
            config.mappings = (0..64)
                .map(|index| mapping(&format!("Sensor{index}")))
                .collect();
            let items = (0..1_000)
                .map(|index| json!({"kind": "other", "id": index, "value": index}))
                .collect::<Vec<_>>();

            let changes = FrameMapper::new(&config)
                .map_text("source", &json!({"items": items}).to_string())
                .unwrap();
            assert!(changes.is_empty());
        }

        #[test]
        fn rejects_more_than_maximum_selected_items() {
            let config = config();
            let items = (0..=MAX_ITEMS_PER_MESSAGE)
                .map(|index| json!({"kind": "other", "id": index}))
                .collect::<Vec<_>>();

            let error = FrameMapper::new(&config)
                .map_text("source", &json!({"items": items}).to_string())
                .unwrap_err();
            assert_eq!(
                error.to_string(),
                format!("WebSocket message selected more than {MAX_ITEMS_PER_MESSAGE} items")
            );
            assert!(!error.is_recoverable());
        }

        #[test]
        fn skips_items_with_missing_or_unmapped_dynamic_operations() {
            let mut config = config();
            config.items_path = "$".to_string();
            config.mappings = vec![SourceMapping {
                when: None,
                operation: None,
                operation_from: Some("payload.op".to_string()),
                operation_map: Some(std::collections::HashMap::from([(
                    "insert".to_string(),
                    OperationType::Insert,
                )])),
                element_type: ElementType::Node,
                effective_from: None,
                template: ElementTemplate {
                    id: "{{payload.id}}".to_string(),
                    labels: vec!["Sensor".to_string()],
                    properties: None,
                    from: None,
                    to: None,
                },
            }];
            let mapper = FrameMapper::new(&config);

            assert!(mapper
                .map_text("source", r#"{"type":"heartbeat"}"#)
                .unwrap()
                .is_empty());
            assert!(mapper
                .map_text("source", r#"{"op":"subscribed"}"#)
                .unwrap()
                .is_empty());
        }

        #[test]
        fn maps_delete_metadata() {
            let mut config = config();
            config.items_path = "$".to_string();
            config.mappings = vec![SourceMapping {
                when: None,
                operation: Some(OperationType::Delete),
                operation_from: None,
                operation_map: None,
                element_type: ElementType::Node,
                effective_from: None,
                template: ElementTemplate {
                    id: "{{payload.id}}".to_string(),
                    labels: vec!["Sensor".to_string()],
                    properties: None,
                    from: None,
                    to: None,
                },
            }];

            let change = FrameMapper::new(&config)
                .map_text("source", r#"{"id":"sensor-1"}"#)
                .unwrap()
                .pop()
                .unwrap();

            match change {
                SourceChange::Delete { metadata } => {
                    assert_eq!(metadata.reference.source_id.as_ref(), "source");
                    assert_eq!(metadata.reference.element_id.as_ref(), "sensor-1");
                    assert_eq!(metadata.labels[0].as_ref(), "Sensor");
                }
                other => panic!("expected delete, got {other:?}"),
            }
        }

        #[test]
        fn maps_explicit_unix_millisecond_timestamp() {
            let mut config = config();
            config.items_path = "$".to_string();
            config.mappings.truncate(1);
            config.mappings[0].effective_from = Some(EffectiveFromConfig::Explicit {
                value: "{{payload.timestamp}}".to_string(),
                format: TimestampFormat::UnixMillis,
            });

            let change = FrameMapper::new(&config)
                .map_text(
                    "source",
                    r#"{"kind":"sensor","id":"sensor-1","value":10,"timestamp":1770000000000}"#,
                )
                .unwrap()
                .pop()
                .unwrap();

            match change {
                SourceChange::Insert {
                    element: Element::Node { metadata, .. },
                } => assert_eq!(metadata.effective_from, 1_770_000_000_000),
                other => panic!("expected node insert, got {other:?}"),
            }
        }

        #[test]
        fn maps_relation_metadata_and_endpoints() {
            let mut config = config();
            config.items_path = "$".to_string();
            config.mappings = vec![SourceMapping {
                when: None,
                operation: Some(OperationType::Insert),
                operation_from: None,
                operation_map: None,
                element_type: ElementType::Relation,
                effective_from: None,
                template: ElementTemplate {
                    id: "{{payload.id}}".to_string(),
                    labels: vec!["READS".to_string()],
                    properties: None,
                    from: Some("{{payload.reader}}".to_string()),
                    to: Some("{{payload.sensor}}".to_string()),
                },
            }];

            let change = FrameMapper::new(&config)
                .map_text(
                    "source",
                    r#"{"id":"reading-1","reader":"reader-1","sensor":"sensor-1"}"#,
                )
                .unwrap()
                .pop()
                .unwrap();

            match change {
                SourceChange::Insert {
                    element:
                        Element::Relation {
                            metadata,
                            out_node,
                            in_node,
                            ..
                        },
                } => {
                    assert_eq!(metadata.reference.element_id.as_ref(), "reading-1");
                    assert_eq!(metadata.labels[0].as_ref(), "READS");
                    assert_eq!(out_node.element_id.as_ref(), "reader-1");
                    assert_eq!(in_node.element_id.as_ref(), "sensor-1");
                }
                other => panic!("expected relation insert, got {other:?}"),
            }
        }

        #[test]
        fn classifies_malformed_json_as_recoverable_without_exposing_payload() {
            let mapper = FrameMapper::new(&config());
            let malformed = mapper.map_text("source", "{invalid").unwrap_err();
            assert!(malformed.is_recoverable());
            assert!(!malformed.to_string().contains("{invalid"));
        }

        #[test]
        fn classifies_non_array_items_field_as_fatal() {
            let mapper = FrameMapper::new(&config());
            let invalid_items = mapper
                .map_text("source", r#"{"items":"not-an-array"}"#)
                .unwrap_err();
            assert_eq!(
                invalid_items.to_string(),
                "itemsPath must select a top-level array"
            );
            assert!(!invalid_items.is_recoverable());
        }
    }
}

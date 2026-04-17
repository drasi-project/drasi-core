// Copyright 2026 The Drasi Authors.
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

use matchit::{Params, Router};

use crate::schema::MqttSourceChange;
use crate::utils::MqttPacket;
use crate::{
    config::{InjectId, MappingMode, MappingNode, MappingRelation, TopicMapping},
    MqttElement,
};
use log::{error, info};

#[derive(Debug, Clone)]
pub struct PatternMatcher {
    router: Router<TopicMapping>,
}

impl PatternMatcher {
    pub fn new(topic_mappings: &Vec<TopicMapping>) -> Self {
        let mut router = Router::new();
        for mapping in topic_mappings {
            if let Err(e) = router.insert(&mapping.pattern, mapping.clone()) {
                error!(
                    "Failed to insert topic mapping pattern '{}': {}",
                    mapping.pattern, e
                );
            } else {
                info!("Inserted topic mapping pattern: {}", mapping.pattern);
            }
        }
        Self { router }
    }

    pub fn generate_schema(&self, packet: &MqttPacket) -> anyhow::Result<Vec<MqttSourceChange>> {
        match self.router.at(&packet.topic) {
            Ok(matched) => {
                let mut result = Vec::new();
                let mapping = matched.value;
                let params = matched.params;

                match Self::create_entity(mapping, &params, packet) {
                    Ok(change) => result.push(change),
                    Err(e) => {
                        error!(
                            "Error processing matched pattern for topic {}: {}",
                            packet.topic, e
                        );
                        return Err(e);
                    }
                }

                let timestamp = packet.timestamp;
                Self::create_hierarchy(mapping, &params, timestamp, &mut result);

                Ok(result)
            }
            Err(_) => {
                // no pattern is matched
                Err(anyhow::anyhow!(
                    "No pattern matched for topic: {}",
                    packet.topic
                ))
            }
        }
    }

    fn create_entity(
        mapping: &TopicMapping,
        params: &Params,
        packet: &MqttPacket,
    ) -> anyhow::Result<MqttSourceChange> {
        // mapping is validated from an initial step, so we can safely unwrap the field_name for PayloadAsField mode

        // payload parsing based on the mapping mode
        let mut properties = match &mapping.properties.mode {
            MappingMode::PayloadAsField => {
                let field_name = Self::map_template(
                    mapping.properties.field_name.as_ref().ok_or_else(|| {
                        anyhow::anyhow!("Field name is required for PayloadAsField mode")
                    })?,
                    params,
                );
                let mut props = serde_json::Map::new();
                let val = serde_json::from_slice(&packet.payload).unwrap_or_else(|_| {
                    serde_json::Value::String(String::from_utf8_lossy(&packet.payload).into_owned())
                });
                props.insert(field_name, val);
                props
            }
            MappingMode::PayloadSpread => {
                match serde_json::from_slice::<serde_json::Value>(&packet.payload) {
                    Ok(serde_json::Value::Object(map)) => map,
                    Ok(_) => {
                        error!("Expected JSON object in payload for PayloadSpread mode, got different type.");
                        return Err(anyhow::anyhow!("Expected JSON object in payload for PayloadSpread mode, got different type."));
                    }
                    Err(e) => {
                        error!("Failed to parse payload as JSON for PayloadSpread mode: {e}.");
                        return Err(anyhow::anyhow!(
                            "Failed to parse payload as JSON for PayloadSpread mode: {e}"
                        ));
                    }
                }
            }
        };

        // injection of topic variables as properties
        for inject in &mapping.properties.inject {
            for (key, template) in inject {
                let value = Self::map_template(template, params);
                properties.insert(key.clone(), serde_json::Value::String(value));
            }
        }

        // entity main label and id mapping
        let entity_label = Self::map_template(&mapping.entity.label, params);
        let entity_id = Self::map_template(&mapping.entity.id, params);

        if let Some(InjectId::True) = &mapping.properties.inject_id {
            properties.insert(
                "id".to_string(),
                serde_json::Value::String(entity_id.clone()),
            );
        }

        let element = MqttElement::Node {
            id: entity_id.clone(),
            labels: vec![entity_label.clone()],
            properties,
        };
        Ok(MqttSourceChange::Update {
            element,
            timestamp: Some(packet.timestamp),
        })
    }

    fn create_hierarchy(
        mapping: &TopicMapping,
        params: &Params,
        timestamp: u64,
        result: &mut Vec<MqttSourceChange>,
    ) {
        result.append(&mut Self::create_nodes(
            mapping.nodes.as_ref(),
            params,
            timestamp,
        ));

        result.append(&mut Self::create_relations(
            mapping.relations.as_ref(),
            params,
            timestamp,
            result,
        ));
    }

    fn create_nodes(
        nodes: &Vec<MappingNode>,
        params: &Params,
        timestamp: u64,
    ) -> Vec<MqttSourceChange> {
        let mut result = Vec::new();
        for node in nodes {
            let node_id = Self::map_template(&node.id, params);
            let element = MqttElement::Node {
                id: node_id.clone(),
                labels: vec![node.label.clone()],
                properties: {
                    let mut props = serde_json::Map::new();
                    props.insert("id".to_string(), serde_json::Value::String(node_id.clone()));
                    props
                },
            };
            result.push(MqttSourceChange::Update {
                element,
                timestamp: Some(timestamp),
            });
        }
        result
    }

    fn create_relations(
        relations: &Vec<MappingRelation>,
        params: &Params,
        timestamp: u64,
        nodes: &Vec<MqttSourceChange>,
    ) -> Vec<MqttSourceChange> {
        let mut result = Vec::new();
        for relation in relations {
            let relation_id = Self::map_template(&relation.id, params);

            let from_node = Self::get_node_with_label(&relation.from, nodes);
            let to_node = Self::get_node_with_label(&relation.to, nodes);

            if let Ok(from) = from_node {
                if let Ok(to) = to_node {
                    let element = MqttElement::Relation {
                        id: relation_id.clone(),
                        labels: vec![relation.label.clone()],
                        from,
                        to,
                        properties: {
                            let mut props = serde_json::Map::new();
                            props.insert(
                                "id".to_string(),
                                serde_json::Value::String(relation_id.clone()),
                            );
                            props
                        },
                    };
                    result.push(MqttSourceChange::Update {
                        element,
                        timestamp: Some(timestamp),
                    });
                }
            }
        }
        result
    }

    fn get_node_with_label(label: &str, changes: &Vec<MqttSourceChange>) -> anyhow::Result<String> {
        for change in changes {
            if let MqttSourceChange::Update {
                element:
                    MqttElement::Node {
                        id,
                        labels,
                        properties,
                    },
                ..
            } = change
            {
                if labels.contains(&label.to_string()) {
                    return Ok(id.clone());
                }
            }
        }
        Err(anyhow::anyhow!("Node with label '{label}' not found"))
    }

    fn map_template(template: &str, params: &Params) -> String {
        let mut id = template.to_string();
        for (key, value) in params.iter() {
            id = id.replace(&format!("{{{key}}}"), value);
        }
        id
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{MappingEntity, MappingProperties};
    use bytes::Bytes;

    fn topic_mapping(mode: MappingMode, field_name: Option<&str>) -> TopicMapping {
        let resolved_field_name = match mode {
            MappingMode::PayloadAsField => Some(field_name.unwrap_or("reading").to_string()),
            MappingMode::PayloadSpread => None,
        };

        TopicMapping {
            pattern: "building/{floor}/{room}/{device}".to_string(),
            entity: MappingEntity {
                label: "DEVICE".to_string(),
                id: "{room}:{device}".to_string(),
            },
            properties: MappingProperties {
                mode,
                field_name: resolved_field_name,
                inject: vec![std::collections::HashMap::from([
                    ("floor".to_string(), "{floor}".to_string()),
                    ("room".to_string(), "{room}".to_string()),
                ])],
                inject_id: Some(InjectId::True),
            },
            nodes: vec![
                MappingNode {
                    label: "FLOOR".to_string(),
                    id: "{floor}".to_string(),
                },
                MappingNode {
                    label: "ROOM".to_string(),
                    id: "{room}".to_string(),
                },
            ],
            relations: vec![
                MappingRelation {
                    label: "CONTAINS".to_string(),
                    from: "FLOOR".to_string(),
                    to: "ROOM".to_string(),
                    id: "{floor}_contains_{room}".to_string(),
                },
                MappingRelation {
                    label: "LOCATED_IN".to_string(),
                    from: "DEVICE".to_string(),
                    to: "ROOM".to_string(),
                    id: "{room}_located_in_{device}".to_string(),
                },
            ],
        }
    }

    fn packet(topic: &str, payload: &[u8], timestamp: u64) -> MqttPacket {
        MqttPacket {
            topic: topic.to_string(),
            payload: Bytes::from(payload.to_vec()),
            timestamp,
        }
    }

    #[test]
    fn generate_schema_returns_error_when_no_pattern_matches() {
        let matcher = PatternMatcher::new(&vec![topic_mapping(
            MappingMode::PayloadAsField,
            Some("reading"),
        )]);
        let input = packet("sensors/temperature", br#"{"value":42}"#, 1);

        let err = matcher
            .generate_schema(&input)
            .expect_err("expected no match error");

        assert!(err.to_string().contains("No pattern matched"));
    }

    #[test]
    fn generate_schema_payload_as_field_parses_json_and_adds_injected_fields() {
        let matcher = PatternMatcher::new(&vec![topic_mapping(
            MappingMode::PayloadAsField,
            Some("reading"),
        )]);
        let input = packet("building/f1/r12/thermostat", br#"25"#, 12345);

        let changes = matcher
            .generate_schema(&input)
            .expect("expected schema generation to succeed");

        assert_eq!(changes.len(), 5);

        match &changes[0] {
            MqttSourceChange::Update { element, timestamp } => {
                assert_eq!(*timestamp, Some(12345));
                match element {
                    MqttElement::Node {
                        id,
                        labels,
                        properties,
                    } => {
                        assert_eq!(id, "r12:thermostat");
                        assert_eq!(labels, &vec!["DEVICE".to_string()]);
                        assert_eq!(
                            properties.get("floor"),
                            Some(&serde_json::Value::String("f1".to_string()))
                        );
                        assert_eq!(
                            properties.get("room"),
                            Some(&serde_json::Value::String("r12".to_string()))
                        );
                        assert_eq!(
                            properties.get("reading"),
                            Some(&serde_json::Value::Number(25.into()))
                        );
                    }
                    _ => panic!("first change should be node update"),
                }
            }
            _ => panic!("first change should be update"),
        }
    }

    #[test]
    fn generate_schema_payload_as_field_falls_back_to_string_for_non_json_payload() {
        let matcher = PatternMatcher::new(&vec![topic_mapping(
            MappingMode::PayloadAsField,
            Some("reading"),
        )]);
        let input = packet("building/f2/r3/sensor", b"plain-text", 100);

        let changes = matcher
            .generate_schema(&input)
            .expect("expected schema generation to succeed");

        match &changes[0] {
            MqttSourceChange::Update { element, .. } => match element {
                MqttElement::Node { properties, .. } => {
                    assert_eq!(
                        properties.get("reading"),
                        Some(&serde_json::Value::String("plain-text".to_string()))
                    );
                }
                _ => panic!("first change should be node update"),
            },
            _ => panic!("first change should be update"),
        }
    }

    #[test]
    fn generate_schema_payload_spread_merges_payload_object_and_injected_fields() {
        let matcher = PatternMatcher::new(&vec![topic_mapping(MappingMode::PayloadSpread, None)]);
        let input = packet(
            "building/f3/r9/hvac",
            br#"{"status":"ok","room":"payload-room"}"#,
            999,
        );

        let changes = matcher
            .generate_schema(&input)
            .expect("expected schema generation to succeed");

        match &changes[0] {
            MqttSourceChange::Update { element, .. } => match element {
                MqttElement::Node { properties, .. } => {
                    assert_eq!(
                        properties.get("status"),
                        Some(&serde_json::Value::String("ok".to_string()))
                    );
                    assert_eq!(
                        properties.get("room"),
                        Some(&serde_json::Value::String("r9".to_string()))
                    );
                    assert_eq!(
                        properties.get("floor"),
                        Some(&serde_json::Value::String("f3".to_string()))
                    );
                }
                _ => panic!("first change should be node update"),
            },
            _ => panic!("first change should be update"),
        }
    }

    #[test]
    fn generate_schema_payload_spread_with_non_object_payload_uses_only_injected_fields() {
        let matcher = PatternMatcher::new(&vec![topic_mapping(MappingMode::PayloadSpread, None)]);
        let input = packet("building/f4/r2/smoke", br#"34.5"#, 321);

        let changes = matcher.generate_schema(&input);
        assert_eq!(changes.is_err(), true);
        assert!(changes
            .err()
            .unwrap()
            .to_string()
            .contains("Expected JSON object in payload for PayloadSpread mode"));
    }

    #[test]
    fn generate_schema_creates_hierarchy_nodes_and_relations_with_timestamp() {
        let matcher = PatternMatcher::new(&vec![topic_mapping(MappingMode::PayloadSpread, None)]);
        let input = packet("building/f6/r8/light", br#"{"lux":90}"#, 777);

        let changes = matcher
            .generate_schema(&input)
            .expect("expected schema generation to succeed");

        assert_eq!(changes.len(), 5);

        match &changes[0] {
            MqttSourceChange::Update { element, timestamp } => {
                assert_eq!(*timestamp, Some(777));
                match element {
                    MqttElement::Node {
                        id,
                        labels,
                        properties,
                    } => {
                        assert_eq!(id, "r8:light");
                        assert_eq!(labels, &vec!["DEVICE".to_string()]);
                        assert_eq!(properties.get("lux"), Some(&serde_json::json!(90)));
                    }
                    _ => panic!("first change should be device node"),
                }
            }
            _ => panic!("first change should be update"),
        }

        match &changes[1] {
            MqttSourceChange::Update { element, timestamp } => {
                assert_eq!(*timestamp, Some(777));
                match element {
                    MqttElement::Node { id, labels, .. } => {
                        assert_eq!(id, "f6");
                        assert_eq!(labels, &vec!["FLOOR".to_string()]);
                    }
                    _ => panic!("second change should be floor node"),
                }
            }
            _ => panic!("second change should be update"),
        }

        match &changes[2] {
            MqttSourceChange::Update { element, timestamp } => {
                assert_eq!(*timestamp, Some(777));
                match element {
                    MqttElement::Node { id, labels, .. } => {
                        assert_eq!(id, "r8");
                        assert_eq!(labels, &vec!["ROOM".to_string()]);
                    }
                    _ => panic!("third change should be room node"),
                }
            }
            _ => panic!("third change should be update"),
        }

        match &changes[3] {
            MqttSourceChange::Update { element, timestamp } => {
                assert_eq!(*timestamp, Some(777));
                match element {
                    MqttElement::Relation {
                        id,
                        labels,
                        from,
                        to,
                        properties,
                    } => {
                        assert_eq!(id, "f6_contains_r8");
                        assert_eq!(labels, &vec!["CONTAINS".to_string()]);
                        assert_eq!(from, "f6");
                        assert_eq!(to, "r8");
                        assert_eq!(
                            properties.get("id"),
                            Some(&serde_json::json!("f6_contains_r8"))
                        );
                    }
                    _ => panic!("fourth change should be relation"),
                }
            }
            _ => panic!("fourth change should be update"),
        }

        match &changes[4] {
            MqttSourceChange::Update { element, timestamp } => {
                assert_eq!(*timestamp, Some(777));
                match element {
                    MqttElement::Relation {
                        id,
                        labels,
                        from,
                        to,
                        properties,
                    } => {
                        assert_eq!(id, "r8_located_in_light");
                        assert_eq!(labels, &vec!["LOCATED_IN".to_string()]);
                        assert_eq!(from, "r8:light");
                        assert_eq!(to, "r8");
                        assert_eq!(
                            properties.get("id"),
                            Some(&serde_json::json!("r8_located_in_light"))
                        );
                    }
                    _ => panic!("fifth change should be relation"),
                }
            }
            _ => panic!("fifth change should be update"),
        }
    }

    #[test]
    fn topic_mapping_defaults_field_name_for_payload_as_field() {
        let matcher = PatternMatcher::new(&vec![topic_mapping(MappingMode::PayloadAsField, None)]);
        let input = packet("building/f6/r8/light", br#"{"lux":90}"#, 777);

        let changes = matcher
            .generate_schema(&input)
            .expect("expected schema generation to succeed");

        match &changes[0] {
            MqttSourceChange::Update { element, .. } => match element {
                MqttElement::Node { properties, .. } => {
                    assert_eq!(
                        properties.get("reading"),
                        Some(&serde_json::json!({"lux":90}))
                    );
                }
                _ => panic!("first change should be node update"),
            },
            _ => panic!("first change should be update"),
        }
    }

    #[test]
    fn topic_mapping_ignores_field_name_for_payload_spread() {
        let matcher = PatternMatcher::new(&vec![topic_mapping(
            MappingMode::PayloadSpread,
            Some("bad"),
        )]);
        let input = packet("building/f6/r8/light", br#"{"lux":90}"#, 777);

        let changes = matcher
            .generate_schema(&input)
            .expect("expected schema generation to succeed");

        match &changes[0] {
            MqttSourceChange::Update { element, .. } => match element {
                MqttElement::Node { properties, .. } => {
                    assert_eq!(properties.get("lux"), Some(&serde_json::json!(90)));
                    assert_eq!(properties.get("bad"), None);
                }
                _ => panic!("first change should be node update"),
            },
            _ => panic!("first change should be update"),
        }
    }
}

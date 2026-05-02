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

use std::collections::{HashMap, HashSet};

use matchit::{Params, Router};

use crate::schema::MqttSourceChange;
use crate::utils::MqttPacket;
use crate::FROM_TO_STARTS_WITH;
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
        let matched = self
            .router
            .at(&packet.topic)
            .map_err(|_| anyhow::anyhow!("No pattern matched for topic: {}", packet.topic))?;
        let mapping = matched.value;
        let params = matched.params;
        let timestamp = packet.timestamp;

        let mut result = Vec::new();
        let mut emitted_ids: HashSet<String> = HashSet::new();

        let mut properties = Self::generate_packet_properties(mapping, &params, packet)?;

        // entity
        let entity_id = Self::map_template(&mapping.entity.id, &params);
        result.push(Self::build_entity(
            mapping,
            &params,
            &entity_id,
            timestamp,
            &mut properties,
        ));
        emitted_ids.insert(entity_id);

        // declared parent nodes
        for node in &mapping.nodes {
            let node_id = Self::map_template(&node.id, &params);
            if emitted_ids.insert(node_id.clone()) {
                result.push(Self::build_node(&node_id, &node.label, timestamp));
            }
        }

        // relations + on-demand endpoint upsert
        Self::append_relations(
            mapping,
            &params,
            timestamp,
            &properties,
            &mut emitted_ids,
            &mut result,
        )?;

        Ok(result)
    }

    fn generate_packet_properties(
        mapping: &TopicMapping,
        params: &Params,
        packet: &MqttPacket,
    ) -> anyhow::Result<serde_json::Map<String, serde_json::Value>> {
        // mapping is validated from an initial step, so we can safely unwrap the field_name for PayloadAsField mode
        // payload parsing based on the mapping mode
        match &mapping.properties.mode {
            MappingMode::PayloadAsField => {
                let field_name = Self::map_template(
                    mapping.properties.field_name.as_ref().ok_or_else(|| {
                        anyhow::anyhow!("Field name is required for PayloadAsField mode")
                    })?,
                    params,
                );
                let mut props = serde_json::Map::new();
                let val = match serde_json::from_slice(&packet.payload) {
                    // throw error if payload is not a value.
                    Ok(serde_json::Value::Object(_)) => {
                        error!("Expected primitive value in payload for PayloadAsField mode, got JSON object.");
                        Err(anyhow::anyhow!("Expected primitive value in payload for PayloadAsField mode, got JSON object."))
                    }
                    Ok(val) => Ok(val),
                    Err(_e) => Ok(serde_json::Value::String(
                        String::from_utf8_lossy(&packet.payload).to_string(),
                    )),
                }?;

                props.insert(field_name, val);
                Ok(props)
            }
            MappingMode::PayloadSpread => {
                match serde_json::from_slice::<serde_json::Value>(&packet.payload) {
                    Ok(serde_json::Value::Object(map)) => Ok(map),
                    Ok(_) => {
                        error!("Expected JSON object in payload for PayloadSpread mode, got different type.");
                        Err(anyhow::anyhow!("Expected JSON object in payload for PayloadSpread mode, got different type."))
                    }
                    Err(e) => {
                        error!("Failed to parse payload as JSON for PayloadSpread mode: {e}.");
                        Err(anyhow::anyhow!(
                            "Failed to parse payload as JSON for PayloadSpread mode: {e}."
                        ))
                    }
                }
            }
        }
    }

    fn build_entity(
        mapping: &TopicMapping,
        params: &Params,
        entity_id: &str,
        timestamp: u64,
        properties: &mut serde_json::Map<String, serde_json::Value>,
    ) -> MqttSourceChange {
        for inject in &mapping.properties.inject {
            for (key, template) in inject {
                let value = Self::map_template(template, params);
                properties.insert(key.clone(), serde_json::Value::String(value));
            }
        }

        if let Some(InjectId::True) = &mapping.properties.inject_id {
            properties.insert(
                "id".to_string(),
                serde_json::Value::String(entity_id.to_string()),
            );
        }

        MqttSourceChange::Update {
            element: MqttElement::Node {
                id: entity_id.to_string(),
                labels: vec![mapping.entity.label.clone()],
                properties: properties.clone(),
            },
            timestamp: Some(timestamp),
        }
    }

    fn build_node(node_id: &str, label: &str, timestamp: u64) -> MqttSourceChange {
        let mut props = serde_json::Map::new();
        props.insert(
            "id".to_string(),
            serde_json::Value::String(node_id.to_string()),
        );
        MqttSourceChange::Update {
            element: MqttElement::Node {
                id: node_id.to_string(),
                labels: vec![label.to_string()],
                properties: props,
            },
            timestamp: Some(timestamp),
        }
    }

    fn build_relation(
        relation_id: &str,
        label: &str,
        from: &str,
        to: &str,
        timestamp: u64,
    ) -> MqttSourceChange {
        let mut props = serde_json::Map::new();
        props.insert(
            "id".to_string(),
            serde_json::Value::String(relation_id.to_string()),
        );
        MqttSourceChange::Update {
            element: MqttElement::Relation {
                id: relation_id.to_string(),
                labels: vec![label.to_string()],
                from: from.to_string(),
                to: to.to_string(),
                properties: props,
            },
            timestamp: Some(timestamp),
        }
    }

    fn append_relations(
        mapping: &TopicMapping,
        params: &Params,
        timestamp: u64,
        properties: &serde_json::Map<String, serde_json::Value>,
        emitted_ids: &mut HashSet<String>,
        result: &mut Vec<MqttSourceChange>,
    ) -> anyhow::Result<()> {
        for relation in &mapping.relations {
            let from_id =
                Self::resolve_template(&relation.from.id, params, properties).map_err(|e| {
                    anyhow::anyhow!(
                        "Failed to resolve 'from.id' for relation '{}': {e}",
                        relation.label
                    )
                })?;
            if emitted_ids.insert(from_id.clone()) {
                result.push(Self::build_node(&from_id, &relation.from.label, timestamp));
            }

            let to_id =
                Self::resolve_template(&relation.to.id, params, properties).map_err(|e| {
                    anyhow::anyhow!(
                        "Failed to resolve 'to.id' for relation '{}': {e}",
                        relation.label
                    )
                })?;
            if emitted_ids.insert(to_id.clone()) {
                result.push(Self::build_node(&to_id, &relation.to.label, timestamp));
            }

            let relation_id =
                Self::resolve_template(&relation.id, params, properties).map_err(|e| {
                    anyhow::anyhow!(
                        "Failed to resolve 'id' for relation '{}': {e}",
                        relation.label
                    )
                })?;
            result.push(Self::build_relation(
                &relation_id,
                &relation.label,
                &from_id,
                &to_id,
                timestamp,
            ));
        }
        Ok(())
    }

    /// Resolve a template that may contain `{var}` topic placeholders and `$.path` payload
    /// references in the same string. Topic placeholders are substituted from `params`;
    /// payload references read primitive values from `properties`.
    fn resolve_template(
        template: &str,
        params: &Params,
        properties: &serde_json::Map<String, serde_json::Value>,
    ) -> anyhow::Result<String> {
        let with_vars = Self::map_template(template, params);
        Self::resolve_payload_refs(&with_vars, properties)
    }

    fn resolve_payload_refs(
        s: &str,
        properties: &serde_json::Map<String, serde_json::Value>,
    ) -> anyhow::Result<String> {
        let mut out = String::with_capacity(s.len());
        let bytes = s.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'$' && i + 1 < bytes.len() && bytes[i + 1] == b'.' {
                let start = i + 2;
                let mut end = start;
                while end < bytes.len() {
                    let c = bytes[end];
                    if c.is_ascii_alphanumeric() || c == b'_' || c == b'.' {
                        end += 1;
                    } else {
                        break;
                    }
                }
                if end == start {
                    out.push_str("$.");
                    i += 2;
                    continue;
                }
                let path = &s[start..end];
                let value = properties.get(path).ok_or_else(|| {
                    anyhow::anyhow!("Property '{path}' not found for payload reference")
                })?;
                let stringified = match value {
                    serde_json::Value::String(v) => v.clone(),
                    serde_json::Value::Number(n) => n.to_string(),
                    serde_json::Value::Bool(b) => b.to_string(),
                    serde_json::Value::Null => "null".to_string(),
                    serde_json::Value::Array(_) | serde_json::Value::Object(_) => {
                        return Err(anyhow::anyhow!(
                            "Property '{path}' must be a primitive value for payload reference"
                        ));
                    }
                };
                out.push_str(&stringified);
                i = end;
            } else {
                out.push(bytes[i] as char);
                i += 1;
            }
        }
        Ok(out)
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
                    id: "{floor}_contains_{room}".to_string(),
                    from: crate::config::MappingRelationEndpoint {
                        label: "FLOOR".to_string(),
                        id: "{floor}".to_string(),
                    },
                    to: crate::config::MappingRelationEndpoint {
                        label: "ROOM".to_string(),
                        id: "{room}".to_string(),
                    },
                },
                MappingRelation {
                    label: "LOCATED_IN".to_string(),
                    id: "{room}:{device}_located_in_{room}".to_string(),
                    from: crate::config::MappingRelationEndpoint {
                        label: "DEVICE".to_string(),
                        id: "{room}:{device}".to_string(),
                    },
                    to: crate::config::MappingRelationEndpoint {
                        label: "ROOM".to_string(),
                        id: "{room}".to_string(),
                    },
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
        let input = packet("sensors/temperature", br#"{"value":43}"#, 1);

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
        assert!(changes.is_err());
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
                        assert_eq!(id, "r8:light_located_in_r8");
                        assert_eq!(labels, &vec!["LOCATED_IN".to_string()]);
                        assert_eq!(from, "r8:light");
                        assert_eq!(to, "r8");
                        assert_eq!(
                            properties.get("id"),
                            Some(&serde_json::json!("r8:light_located_in_r8"))
                        );
                    }
                    _ => panic!("fifth change should be relation"),
                }
            }
            _ => panic!("fifth change should be update"),
        }
    }

    #[test]
    fn topic_mapping_rejects_json_with_payload_as_field_mode() {
        let matcher = PatternMatcher::new(&vec![topic_mapping(MappingMode::PayloadAsField, None)]);
        let input = packet("building/f6/r8/light", br#"{"lux":90}"#, 777);

        let changes = matcher.generate_schema(&input);

        assert!(changes.is_err());
    }

    #[test]
    fn topic_mapping_accepts_arr_with_payload_as_field_mode() {
        let matcher = PatternMatcher::new(&vec![topic_mapping(MappingMode::PayloadAsField, None)]);
        let input = packet("building/f6/r8/light", br#"[3,4]"#, 777);

        let changes = matcher.generate_schema(&input);

        assert!(changes.is_ok());
    }

    #[test]
    fn topic_mapping_rejects_arr_with_payload_spread_mode() {
        let matcher = PatternMatcher::new(&vec![topic_mapping(MappingMode::PayloadSpread, None)]);
        let input = packet("building/f6/r8/light", br#"[3,4]"#, 777);

        let changes = matcher.generate_schema(&input);

        assert!(changes.is_err());
    }

    #[test]
    fn topic_mapping_rejects_non_json_with_payload_spread_mode() {
        let matcher = PatternMatcher::new(&vec![topic_mapping(MappingMode::PayloadSpread, None)]);
        let input = packet("building/f6/r8/light", b"plain-text", 777);

        let changes = matcher.generate_schema(&input);

        assert!(changes.is_err());
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

    #[test]
    fn topic_mapping_for_relation_from_id_from_properties() {
        let mut mapping = topic_mapping(MappingMode::PayloadSpread, Some("reading"));
        // Replace second relation's `from` endpoint with a payload-derived id.
        // The endpoint label drives upsert of the new "Level" node since "5" is not in nodes[].
        mapping.relations[1].from = crate::config::MappingRelationEndpoint {
            label: "LEVEL".to_string(),
            id: "$.level".to_string(),
        };
        // hyphen separates the payload path from following text so the path scan stops cleanly
        mapping.relations[1].id = "level-$.level-for-{room}".to_string();

        let matcher = PatternMatcher::new(&vec![mapping]);
        let input = packet("building/f6/r8/light", br#"{"lux":99,"level": 5}"#, 777);

        let changes = matcher
            .generate_schema(&input)
            .expect("expected schema generation to succeed");

        // entity + 2 declared nodes + relation 0 (no new endpoints) + endpoint upsert for "5" + relation 1 = 6
        assert_eq!(changes.len(), 6);

        // changes[4] is the upserted Level endpoint node
        match &changes[4] {
            MqttSourceChange::Update { element, timestamp } => {
                assert_eq!(*timestamp, Some(777));
                match element {
                    MqttElement::Node { id, labels, .. } => {
                        assert_eq!(id, "5");
                        assert_eq!(labels, &vec!["LEVEL".to_string()]);
                    }
                    _ => panic!("fifth change should be the upserted Level node"),
                }
            }
            _ => panic!("fifth change should be update"),
        }

        match &changes[5] {
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
                        assert_eq!(id, "level-5-for-r8");
                        assert_eq!(labels, &vec!["LOCATED_IN".to_string()]);
                        assert_eq!(from, "5");
                        assert_eq!(to, "r8");
                        assert_eq!(
                            properties.get("id"),
                            Some(&serde_json::json!("level-5-for-r8"))
                        );
                    }
                    _ => panic!("sixth change should be relation"),
                }
            }
            _ => panic!("sixth change should be update"),
        }
    }

    #[test]
    fn endpoint_upsert_skipped_when_endpoint_id_matches_declared_node() {
        // Relation endpoints all reference ids that are already covered by entity or nodes[];
        // no extra endpoint Node updates should be emitted.
        let matcher = PatternMatcher::new(&vec![topic_mapping(MappingMode::PayloadSpread, None)]);
        let input = packet("building/f1/r2/sensor", br#"{"v":1}"#, 1);
        let changes = matcher.generate_schema(&input).expect("schema");
        // 1 entity + 2 nodes + 2 relations, no extra upserts
        assert_eq!(changes.len(), 5);
    }

    #[test]
    fn endpoint_upsert_emits_node_when_endpoint_id_not_in_nodes() {
        let mut mapping = topic_mapping(MappingMode::PayloadSpread, None);
        // Drop the room/floor parents and let the relation's `to` endpoint carry the label.
        mapping.nodes.clear();
        mapping.relations = vec![MappingRelation {
            label: "ON_FLOOR".to_string(),
            id: "{room}:{device}_on_floor_{floor}".to_string(),
            from: crate::config::MappingRelationEndpoint {
                label: "DEVICE".to_string(),
                id: "{room}:{device}".to_string(),
            },
            to: crate::config::MappingRelationEndpoint {
                label: "FLOOR".to_string(),
                id: "{floor}".to_string(),
            },
        }];

        let matcher = PatternMatcher::new(&vec![mapping]);
        let input = packet("building/f1/r2/sensor", br#"{"v":1}"#, 7);
        let changes = matcher.generate_schema(&input).expect("schema");

        // entity + endpoint Floor (upserted by relation.to) + relation = 3
        // (from endpoint matches the entity id and is skipped)
        assert_eq!(changes.len(), 3);

        match &changes[1] {
            MqttSourceChange::Update {
                element: MqttElement::Node { id, labels, .. },
                ..
            } => {
                assert_eq!(id, "f1");
                assert_eq!(labels, &vec!["FLOOR".to_string()]);
            }
            _ => panic!("second change should be the upserted Floor node"),
        }
    }

    #[test]
    fn relation_id_supports_mixed_topic_var_and_payload_ref() {
        let mut mapping = topic_mapping(MappingMode::PayloadSpread, None);
        mapping.relations[0].id = "{floor}-to-$.gateway".to_string();
        mapping.relations.truncate(1);

        let matcher = PatternMatcher::new(&vec![mapping]);
        let input = packet("building/f9/r3/dev", br#"{"gateway":"gw7","v":1}"#, 42);
        let changes = matcher.generate_schema(&input).expect("schema");

        let last = changes.last().expect("at least one change");
        match last {
            MqttSourceChange::Update {
                element: MqttElement::Relation { id, .. },
                ..
            } => {
                assert_eq!(id, "f9-to-gw7");
            }
            _ => panic!("last change should be the relation"),
        }
    }

    #[test]
    fn relation_payload_ref_errors_when_property_missing() {
        let mut mapping = topic_mapping(MappingMode::PayloadSpread, None);
        mapping.relations[0].to = crate::config::MappingRelationEndpoint {
            label: "GATEWAY".to_string(),
            id: "$.missing".to_string(),
        };

        let matcher = PatternMatcher::new(&vec![mapping]);
        let input = packet("building/f1/r1/dev", br#"{"v":1}"#, 1);
        let err = matcher.generate_schema(&input).expect_err("should error");
        assert!(err.to_string().contains("not found for payload reference"));
    }
}

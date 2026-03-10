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

//! Open511-to-Drasi mapping helpers.

use crate::models::{Open511Area, Open511Event, Open511Road};
use chrono::{DateTime, Utc};
use drasi_core::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, ElementValue, SourceChange,
};
use std::collections::HashSet;
use std::sync::Arc;

pub const ROAD_EVENT_LABEL: &str = "RoadEvent";
pub const ROAD_LABEL: &str = "Road";
pub const AREA_LABEL: &str = "Area";
pub const AFFECTS_ROAD_LABEL: &str = "AFFECTS_ROAD";
pub const IN_AREA_LABEL: &str = "IN_AREA";

/// Map a newly discovered Open511 event to source changes.
pub fn map_new_event(
    event: &Open511Event,
    source_id: &str,
    known_areas: &mut HashSet<String>,
) -> Vec<SourceChange> {
    let mut changes = Vec::new();
    let effective_from = event_effective_from(event);

    changes.push(SourceChange::Insert {
        element: build_event_node(event, source_id, effective_from),
    });

    append_road_inserts(event, source_id, effective_from, &mut changes);
    append_area_inserts(event, source_id, effective_from, known_areas, &mut changes);

    changes
}

/// Map a modified Open511 event to source changes.
pub fn map_updated_event(
    previous: &Open511Event,
    current: &Open511Event,
    source_id: &str,
    known_areas: &mut HashSet<String>,
) -> Vec<SourceChange> {
    let mut changes = Vec::new();
    let effective_from = event_effective_from(current);

    changes.push(SourceChange::Update {
        element: build_event_node(current, source_id, effective_from),
    });

    append_road_deletes(previous, source_id, effective_from, &mut changes);
    append_road_inserts(current, source_id, effective_from, &mut changes);

    append_area_relation_deletes(previous, source_id, effective_from, &mut changes);
    append_area_inserts(
        current,
        source_id,
        effective_from,
        known_areas,
        &mut changes,
    );

    changes
}

/// Map a removed Open511 event to source changes.
pub fn map_removed_event(event: &Open511Event, source_id: &str) -> Vec<SourceChange> {
    let mut changes = Vec::new();
    let effective_from = event_effective_from(event);

    changes.push(SourceChange::Delete {
        metadata: metadata(source_id, &event.id, &[ROAD_EVENT_LABEL], effective_from),
    });

    append_road_deletes(event, source_id, effective_from, &mut changes);
    append_area_relation_deletes(event, source_id, effective_from, &mut changes);

    changes
}

fn append_road_inserts(
    event: &Open511Event,
    source_id: &str,
    effective_from: u64,
    changes: &mut Vec<SourceChange>,
) {
    if let Some(roads) = &event.roads {
        for (index, road) in roads.iter().enumerate() {
            let road_id = road_node_id(event, index);
            let relation_id = affects_road_relation_id(event, index);

            changes.push(SourceChange::Insert {
                element: build_road_node(event, road, index, source_id, effective_from),
            });
            changes.push(SourceChange::Insert {
                element: build_affects_road_relation(
                    source_id,
                    &relation_id,
                    &event.id,
                    &road_id,
                    effective_from,
                ),
            });
        }
    }
}

fn append_road_deletes(
    event: &Open511Event,
    source_id: &str,
    effective_from: u64,
    changes: &mut Vec<SourceChange>,
) {
    if let Some(roads) = &event.roads {
        for index in 0..roads.len() {
            let road_id = road_node_id(event, index);
            let relation_id = affects_road_relation_id(event, index);

            changes.push(SourceChange::Delete {
                metadata: metadata(
                    source_id,
                    &relation_id,
                    &[AFFECTS_ROAD_LABEL],
                    effective_from,
                ),
            });
            changes.push(SourceChange::Delete {
                metadata: metadata(source_id, &road_id, &[ROAD_LABEL], effective_from),
            });
        }
    }
}

fn append_area_inserts(
    event: &Open511Event,
    source_id: &str,
    effective_from: u64,
    known_areas: &mut HashSet<String>,
    changes: &mut Vec<SourceChange>,
) {
    if let Some(areas) = &event.areas {
        for (index, area) in areas.iter().enumerate() {
            let area_id = area_node_id(event, area, index);
            if known_areas.insert(area_id.clone()) {
                changes.push(SourceChange::Insert {
                    element: build_area_node(event, area, index, source_id, effective_from),
                });
            }

            let relation_id = in_area_relation_id(event, &area_id);
            changes.push(SourceChange::Insert {
                element: build_in_area_relation(
                    source_id,
                    &relation_id,
                    &event.id,
                    &area_id,
                    effective_from,
                ),
            });
        }
    }
}

fn append_area_relation_deletes(
    event: &Open511Event,
    source_id: &str,
    effective_from: u64,
    changes: &mut Vec<SourceChange>,
) {
    if let Some(areas) = &event.areas {
        for (index, area) in areas.iter().enumerate() {
            let area_id = area_node_id(event, area, index);
            let relation_id = in_area_relation_id(event, &area_id);
            changes.push(SourceChange::Delete {
                metadata: metadata(source_id, &relation_id, &[IN_AREA_LABEL], effective_from),
            });
        }
    }
}

fn build_event_node(event: &Open511Event, source_id: &str, effective_from: u64) -> Element {
    let mut properties = ElementPropertyMap::new();
    properties.insert("id", string_value(&event.id));
    properties.insert("status", string_value(&event.status));
    insert_opt_string(&mut properties, "url", event.url.as_deref());
    insert_opt_string(
        &mut properties,
        "jurisdiction_url",
        event.jurisdiction_url.as_deref(),
    );
    insert_opt_string(&mut properties, "headline", event.headline.as_deref());
    insert_opt_string(&mut properties, "description", event.description.as_deref());
    insert_opt_string(&mut properties, "event_type", event.event_type.as_deref());
    insert_opt_string(&mut properties, "severity", event.severity.as_deref());
    insert_opt_string(&mut properties, "created", event.created.as_deref());
    insert_opt_string(&mut properties, "updated", event.updated.as_deref());
    insert_opt_string(&mut properties, "ivr_message", event.ivr_message.as_deref());

    if let Some(subtypes) = &event.event_subtypes {
        let list = subtypes
            .iter()
            .map(|s| ElementValue::String(Arc::from(s.as_str())))
            .collect();
        properties.insert("event_subtypes", ElementValue::List(list));
    }

    if let Some(schedule) = &event.schedule {
        if let Ok(json_val) = serde_json::to_value(schedule) {
            properties.insert("schedule", json_to_element_value(&json_val));
        }
    }

    if let Some(geography) = &event.geography {
        if let Ok(json_val) = serde_json::to_value(geography) {
            properties.insert("geography", json_to_element_value(&json_val));
        }
    }

    let road_count = event.roads.as_ref().map(std::vec::Vec::len).unwrap_or(0);
    properties.insert("road_count", ElementValue::Integer(road_count as i64));
    let area_count = event.areas.as_ref().map(std::vec::Vec::len).unwrap_or(0);
    properties.insert("area_count", ElementValue::Integer(area_count as i64));

    Element::Node {
        metadata: metadata(source_id, &event.id, &[ROAD_EVENT_LABEL], effective_from),
        properties,
    }
}

fn build_road_node(
    event: &Open511Event,
    road: &Open511Road,
    index: usize,
    source_id: &str,
    effective_from: u64,
) -> Element {
    let mut properties = ElementPropertyMap::new();
    let road_id = road_node_id(event, index);
    properties.insert("id", string_value(&road_id));
    properties.insert("event_id", string_value(&event.id));
    insert_opt_string(&mut properties, "name", road.name.as_deref());
    insert_opt_string(&mut properties, "from", road.from.as_deref());
    insert_opt_string(&mut properties, "to", road.to.as_deref());
    insert_opt_string(&mut properties, "direction", road.direction.as_deref());
    insert_opt_string(&mut properties, "state", road.state.as_deref());
    insert_opt_string(&mut properties, "delay", road.delay.as_deref());

    Element::Node {
        metadata: metadata(source_id, &road_id, &[ROAD_LABEL], effective_from),
        properties,
    }
}

fn build_area_node(
    event: &Open511Event,
    area: &Open511Area,
    index: usize,
    source_id: &str,
    effective_from: u64,
) -> Element {
    let mut properties = ElementPropertyMap::new();
    let area_id = area_node_id(event, area, index);
    properties.insert("id", string_value(&area_id));
    insert_opt_string(&mut properties, "name", area.name.as_deref());
    insert_opt_string(&mut properties, "url", area.url.as_deref());

    Element::Node {
        metadata: metadata(source_id, &area_id, &[AREA_LABEL], effective_from),
        properties,
    }
}

fn build_affects_road_relation(
    source_id: &str,
    relation_id: &str,
    event_id: &str,
    road_id: &str,
    effective_from: u64,
) -> Element {
    let mut properties = ElementPropertyMap::new();
    properties.insert("event_id", string_value(event_id));

    Element::Relation {
        metadata: metadata(
            source_id,
            relation_id,
            &[AFFECTS_ROAD_LABEL],
            effective_from,
        ),
        // Convention in this repository is in_node=start, out_node=end.
        in_node: ElementReference::new(source_id, event_id),
        out_node: ElementReference::new(source_id, road_id),
        properties,
    }
}

fn build_in_area_relation(
    source_id: &str,
    relation_id: &str,
    event_id: &str,
    area_id: &str,
    effective_from: u64,
) -> Element {
    Element::Relation {
        metadata: metadata(source_id, relation_id, &[IN_AREA_LABEL], effective_from),
        // Convention in this repository is in_node=start, out_node=end.
        in_node: ElementReference::new(source_id, event_id),
        out_node: ElementReference::new(source_id, area_id),
        properties: ElementPropertyMap::new(),
    }
}

fn metadata(
    source_id: &str,
    element_id: &str,
    labels: &[&str],
    effective_from: u64,
) -> ElementMetadata {
    ElementMetadata {
        reference: ElementReference::new(source_id, element_id),
        labels: Arc::from(
            labels
                .iter()
                .map(|label| Arc::<str>::from(*label))
                .collect::<Vec<_>>(),
        ),
        effective_from,
    }
}

fn insert_opt_string(map: &mut ElementPropertyMap, key: &str, value: Option<&str>) {
    if let Some(value) = value {
        map.insert(key, string_value(value));
    }
}

fn string_value(value: &str) -> ElementValue {
    ElementValue::String(Arc::from(value))
}

/// Recursively convert a `serde_json::Value` into an `ElementValue`.
fn json_to_element_value(value: &serde_json::Value) -> ElementValue {
    match value {
        serde_json::Value::Null => ElementValue::Null,
        serde_json::Value::Bool(b) => ElementValue::Bool(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                ElementValue::Integer(i)
            } else if let Some(f) = n.as_f64() {
                ElementValue::Float(ordered_float::OrderedFloat(f))
            } else {
                ElementValue::Null
            }
        }
        serde_json::Value::String(s) => ElementValue::String(Arc::from(s.as_str())),
        serde_json::Value::Array(arr) => {
            ElementValue::List(arr.iter().map(json_to_element_value).collect())
        }
        serde_json::Value::Object(obj) => {
            let mut map = ElementPropertyMap::new();
            for (key, val) in obj {
                map.insert(key, json_to_element_value(val));
            }
            ElementValue::Object(map)
        }
    }
}

pub fn event_effective_from(event: &Open511Event) -> u64 {
    if let Some(ts) = event.updated_or_created() {
        if let Ok(parsed) = DateTime::parse_from_rfc3339(ts) {
            let millis = parsed.timestamp_millis();
            if millis >= 0 {
                return millis as u64;
            }
        }
    }

    Utc::now().timestamp_millis() as u64
}

pub fn road_node_id(event: &Open511Event, index: usize) -> String {
    format!("{}::road::{}", event.id, index)
}

pub fn area_node_id(event: &Open511Event, area: &Open511Area, index: usize) -> String {
    area.id
        .clone()
        .unwrap_or_else(|| format!("{}::area::{}", event.id, index))
}

fn affects_road_relation_id(event: &Open511Event, index: usize) -> String {
    format!("{}::affects_road::{}", event.id, index)
}

fn in_area_relation_id(event: &Open511Event, area_id: &str) -> String {
    format!("{}::in_area::{}", event.id, area_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{Open511Area, Open511Event, Open511Road};

    fn sample_event(id: &str) -> Open511Event {
        Open511Event {
            id: id.to_string(),
            status: "ACTIVE".to_string(),
            headline: Some("INCIDENT".to_string()),
            event_type: Some("INCIDENT".to_string()),
            severity: Some("MAJOR".to_string()),
            updated: Some("2026-01-01T00:00:00Z".to_string()),
            roads: Some(vec![Open511Road {
                name: Some("Highway 1".to_string()),
                from: Some("A".to_string()),
                to: Some("B".to_string()),
                direction: Some("BOTH".to_string()),
                state: Some("CLOSED".to_string()),
                delay: None,
            }]),
            areas: Some(vec![Open511Area {
                id: Some("drivebc.ca/3".to_string()),
                name: Some("Rocky Mountain District".to_string()),
                url: None,
            }]),
            ..Default::default()
        }
    }

    #[test]
    fn map_new_event_emits_event_road_area_graph() {
        let event = sample_event("event-1");
        let mut known_areas = HashSet::new();
        let changes = map_new_event(&event, "open511-source", &mut known_areas);

        assert_eq!(changes.len(), 5);
        assert!(known_areas.contains("drivebc.ca/3"));
    }

    #[test]
    fn map_removed_event_emits_deletes() {
        let event = sample_event("event-2");
        let changes = map_removed_event(&event, "open511-source");

        // event node delete + road relation delete + road delete + area relation delete
        assert_eq!(changes.len(), 4);
        let delete_count = changes
            .iter()
            .filter(|change| matches!(change, SourceChange::Delete { .. }))
            .count();
        assert_eq!(delete_count, 4);
    }
}

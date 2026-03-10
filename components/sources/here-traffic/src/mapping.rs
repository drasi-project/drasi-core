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

//! Mapping and conversion utilities for HERE Traffic API responses.

use crate::client::{FlowResponse, IncidentResult, IncidentsResponse, Shape};
use crate::config::HereTrafficConfig;
use chrono::Utc;
use drasi_core::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange,
};
use drasi_lib::sources::convert_json_to_element_properties;
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq)]
pub struct TrafficSegmentSnapshot {
    pub id: String,
    pub road_name: Option<String>,
    pub current_speed: Option<f64>,
    pub speed_uncapped: Option<f64>,
    pub free_flow_speed: Option<f64>,
    pub jam_factor: Option<f64>,
    pub confidence: Option<f64>,
    pub functional_class: Option<i32>,
    pub length_meters: Option<f64>,
    pub latitude: f64,
    pub longitude: f64,
    pub last_updated: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TrafficIncidentSnapshot {
    pub id: String,
    pub incident_type: Option<String>,
    pub severity: Option<String>,
    pub description: Option<String>,
    pub status: String,
    pub start_time: Option<String>,
    pub end_time: Option<String>,
    pub latitude: f64,
    pub longitude: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RelationSnapshot {
    pub id: String,
    pub incident_id: String,
    pub segment_id: String,
    pub distance_meters: f64,
}

/// Conversion factor: HERE API returns speed in m/s; we expose km/h.
const MS_TO_KMH: f64 = 3.6;

pub fn extract_segments(flow: FlowResponse) -> Vec<TrafficSegmentSnapshot> {
    let last_updated = Utc::now().to_rfc3339();
    let mut segments = Vec::new();

    for result in flow.results {
        let location = match result.location.as_ref() {
            Some(loc) => loc,
            None => continue,
        };

        let (lat, lon) = match location.shape.as_ref().and_then(resolve_shape_centroid) {
            Some(coords) => coords,
            None => continue,
        };

        let segment_id = segment_id_from_coords(lat, lon);
        let current_flow = result.current_flow.as_ref();
        let road_name = location.description.clone();
        let length_meters = location.length;

        segments.push(TrafficSegmentSnapshot {
            id: segment_id,
            road_name,
            current_speed: current_flow.and_then(|f| f.speed).map(|s| s * MS_TO_KMH),
            speed_uncapped: current_flow
                .and_then(|f| f.speed_uncapped)
                .map(|s| s * MS_TO_KMH),
            free_flow_speed: current_flow
                .and_then(|f| f.free_flow)
                .map(|s| s * MS_TO_KMH),
            jam_factor: current_flow.and_then(|f| f.jam_factor),
            confidence: current_flow.and_then(|f| f.confidence),
            functional_class: None,
            length_meters,
            latitude: lat,
            longitude: lon,
            last_updated: last_updated.clone(),
        });
    }

    segments
}

pub fn extract_incidents(incidents: IncidentsResponse) -> Vec<TrafficIncidentSnapshot> {
    let mut snapshots = Vec::new();

    for result in incidents.results {
        if let Some(snapshot) = incident_snapshot(result) {
            snapshots.push(snapshot);
        }
    }

    snapshots
}

fn incident_snapshot(result: IncidentResult) -> Option<TrafficIncidentSnapshot> {
    let details = result.incident_details?;
    let id = details.id?;

    let location = result.location?;
    let (lat, lon) = location.shape.as_ref().and_then(resolve_shape_centroid)?;

    let description = details
        .summary
        .as_ref()
        .and_then(|s| s.value.clone())
        .or_else(|| details.description.as_ref().and_then(|d| d.value.clone()))
        .or_else(|| {
            details
                .type_description
                .as_ref()
                .and_then(|t| t.value.clone())
        });

    let severity = details.severity.as_ref().map(|v| match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Number(n) => n.to_string(),
        other => other.to_string(),
    });

    Some(TrafficIncidentSnapshot {
        id,
        incident_type: details.incident_type,
        severity,
        description,
        status: "ACTIVE".to_string(),
        start_time: details.start_time,
        end_time: details.end_time,
        latitude: lat,
        longitude: lon,
    })
}

pub fn segment_changed(
    previous: &TrafficSegmentSnapshot,
    current: &TrafficSegmentSnapshot,
    config: &HereTrafficConfig,
) -> bool {
    let jam_delta =
        (current.jam_factor.unwrap_or_default() - previous.jam_factor.unwrap_or_default()).abs();
    if jam_delta >= config.flow_change_threshold {
        return true;
    }

    let speed_delta = (current.current_speed.unwrap_or_default()
        - previous.current_speed.unwrap_or_default())
    .abs();
    speed_delta >= config.speed_change_threshold
}

pub fn build_segment_change(
    source_id: &str,
    segment: &TrafficSegmentSnapshot,
    change: ChangeKind,
) -> SourceChange {
    let element_id = segment.id.as_str();
    let metadata = ElementMetadata {
        reference: ElementReference::new(source_id, element_id),
        labels: Arc::from([Arc::from("TrafficSegment")]),
        effective_from: current_timestamp_millis(),
    };

    match change {
        ChangeKind::Delete => SourceChange::Delete { metadata },
        ChangeKind::Insert | ChangeKind::Update => {
            let properties = build_segment_properties(segment);
            let element = Element::Node {
                metadata,
                properties,
            };
            match change {
                ChangeKind::Insert => SourceChange::Insert { element },
                ChangeKind::Update => SourceChange::Update { element },
                ChangeKind::Delete => unreachable!(),
            }
        }
    }
}

pub fn build_incident_change(
    source_id: &str,
    incident: &TrafficIncidentSnapshot,
    change: ChangeKind,
) -> SourceChange {
    let element_id = incident.id.as_str();
    let metadata = ElementMetadata {
        reference: ElementReference::new(source_id, element_id),
        labels: Arc::from([Arc::from("TrafficIncident")]),
        effective_from: current_timestamp_millis(),
    };

    match change {
        ChangeKind::Delete => SourceChange::Delete { metadata },
        ChangeKind::Insert | ChangeKind::Update => {
            let properties = build_incident_properties(incident);
            let element = Element::Node {
                metadata,
                properties,
            };
            match change {
                ChangeKind::Insert => SourceChange::Insert { element },
                ChangeKind::Update => SourceChange::Update { element },
                ChangeKind::Delete => unreachable!(),
            }
        }
    }
}

pub fn build_relation_change(
    source_id: &str,
    relation: &RelationSnapshot,
    change: ChangeKind,
) -> SourceChange {
    let metadata = ElementMetadata {
        reference: ElementReference::new(source_id, relation.id.as_str()),
        labels: Arc::from([Arc::from("AFFECTS")]),
        effective_from: current_timestamp_millis(),
    };

    match change {
        ChangeKind::Delete => SourceChange::Delete { metadata },
        ChangeKind::Insert | ChangeKind::Update => {
            let properties = build_relation_properties(relation);
            let element = Element::Relation {
                metadata,
                in_node: ElementReference::new(source_id, relation.incident_id.as_str()),
                out_node: ElementReference::new(source_id, relation.segment_id.as_str()),
                properties,
            };

            match change {
                ChangeKind::Insert => SourceChange::Insert { element },
                ChangeKind::Update => SourceChange::Update { element },
                ChangeKind::Delete => unreachable!(),
            }
        }
    }
}

pub fn build_relations(
    incidents: &HashMap<String, TrafficIncidentSnapshot>,
    segments: &HashMap<String, TrafficSegmentSnapshot>,
    config: &HereTrafficConfig,
) -> HashMap<String, RelationSnapshot> {
    let mut relations = HashMap::new();
    if incidents.is_empty() || segments.is_empty() {
        return relations;
    }

    for incident in incidents.values() {
        for segment in segments.values() {
            let distance = haversine_distance_meters(
                incident.latitude,
                incident.longitude,
                segment.latitude,
                segment.longitude,
            );
            if distance <= config.incident_match_distance_meters {
                let relation_id = relation_id(&incident.id, &segment.id);
                relations.insert(
                    relation_id.clone(),
                    RelationSnapshot {
                        id: relation_id,
                        incident_id: incident.id.clone(),
                        segment_id: segment.id.clone(),
                        distance_meters: distance,
                    },
                );
            }
        }
    }

    relations
}

pub fn relation_id(incident_id: &str, segment_id: &str) -> String {
    format!("affects_{incident_id}_{segment_id}")
}

fn build_segment_properties(segment: &TrafficSegmentSnapshot) -> ElementPropertyMap {
    let mut map = Map::new();
    map.insert("id".to_string(), json!(segment.id));
    insert_optional_string(&mut map, "road_name", segment.road_name.as_ref());
    insert_optional_number(&mut map, "current_speed", segment.current_speed);
    insert_optional_number(&mut map, "speed_uncapped", segment.speed_uncapped);
    insert_optional_number(&mut map, "free_flow_speed", segment.free_flow_speed);
    insert_optional_number(&mut map, "jam_factor", segment.jam_factor);
    insert_optional_number(&mut map, "confidence", segment.confidence);
    insert_optional_number(
        &mut map,
        "functional_class",
        segment.functional_class.map(f64::from),
    );
    insert_optional_number(&mut map, "length_meters", segment.length_meters);
    map.insert("latitude".to_string(), json!(segment.latitude));
    map.insert("longitude".to_string(), json!(segment.longitude));
    map.insert("last_updated".to_string(), json!(segment.last_updated));

    convert_json_to_element_properties(&map).unwrap_or_else(|_| ElementPropertyMap::new())
}

fn build_incident_properties(incident: &TrafficIncidentSnapshot) -> ElementPropertyMap {
    let mut map = Map::new();
    map.insert("id".to_string(), json!(incident.id));
    insert_optional_string(&mut map, "type", incident.incident_type.as_ref());
    insert_optional_string(&mut map, "severity", incident.severity.as_ref());
    insert_optional_string(&mut map, "description", incident.description.as_ref());
    map.insert("status".to_string(), json!(incident.status));
    insert_optional_string(&mut map, "start_time", incident.start_time.as_ref());
    insert_optional_string(&mut map, "end_time", incident.end_time.as_ref());
    map.insert("latitude".to_string(), json!(incident.latitude));
    map.insert("longitude".to_string(), json!(incident.longitude));

    convert_json_to_element_properties(&map).unwrap_or_else(|_| ElementPropertyMap::new())
}

fn build_relation_properties(relation: &RelationSnapshot) -> ElementPropertyMap {
    let mut map = Map::new();
    map.insert(
        "distance_meters".to_string(),
        json!(relation.distance_meters),
    );
    convert_json_to_element_properties(&map).unwrap_or_else(|_| ElementPropertyMap::new())
}

fn insert_optional_number(map: &mut Map<String, Value>, key: &str, value: Option<f64>) {
    if let Some(value) = value {
        if let Some(number) = serde_json::Number::from_f64(value) {
            map.insert(key.to_string(), Value::Number(number));
        } else {
            log::warn!("Dropping non-finite f64 for key '{key}': {value}");
        }
    }
}

fn insert_optional_string(map: &mut Map<String, Value>, key: &str, value: Option<&String>) {
    if let Some(value) = value {
        map.insert(key.to_string(), Value::String(value.clone()));
    }
}

/// Compute the centroid of all points across all links in a Shape.
fn resolve_shape_centroid(shape: &Shape) -> Option<(f64, f64)> {
    let links = shape.links.as_ref()?;
    let mut lat_sum = 0.0;
    let mut lon_sum = 0.0;
    let mut count = 0usize;

    for link in links {
        if let Some(points) = link.points.as_ref() {
            for pt in points {
                lat_sum += pt.lat;
                lon_sum += pt.lng;
                count += 1;
            }
        }
    }

    if count == 0 {
        return None;
    }

    let n = count as f64;
    Some((lat_sum / n, lon_sum / n))
}

pub fn segment_id_from_coords(lat: f64, lon: f64) -> String {
    format!("segment_{lat:.5}_{lon:.5}")
}

fn current_timestamp_millis() -> u64 {
    Utc::now().timestamp_millis() as u64
}

pub fn haversine_distance_meters(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let radius_earth = 6_371_000.0_f64;
    let lat1_rad = lat1.to_radians();
    let lat2_rad = lat2.to_radians();
    let delta_lat = (lat2 - lat1).to_radians();
    let delta_lon = (lon2 - lon1).to_radians();

    let a = (delta_lat / 2.0).sin().powi(2)
        + lat1_rad.cos() * lat2_rad.cos() * (delta_lon / 2.0).sin().powi(2);
    let c = 2.0 * a.sqrt().atan2((1.0 - a).sqrt());
    radius_earth * c
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    Insert,
    Update,
    Delete,
}

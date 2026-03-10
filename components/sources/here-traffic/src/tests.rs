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

use super::*;
use crate::mapping::{ChangeKind, RelationSnapshot};
use drasi_core::models::SourceChange;

fn sample_segment(jam_factor: f64, speed: f64) -> TrafficSegmentSnapshot {
    TrafficSegmentSnapshot {
        id: "segment_52.50000_13.40000".to_string(),
        road_name: Some("Test Road".to_string()),
        current_speed: Some(speed),
        speed_uncapped: None,
        free_flow_speed: Some(50.0),
        jam_factor: Some(jam_factor),
        confidence: Some(90.0),
        functional_class: Some(2),
        length_meters: Some(1500.0),
        latitude: 52.5,
        longitude: 13.4,
        last_updated: "2024-03-10T12:00:00Z".to_string(),
    }
}

fn sample_incident(id: &str, severity: &str) -> TrafficIncidentSnapshot {
    TrafficIncidentSnapshot {
        id: id.to_string(),
        incident_type: Some("ACCIDENT".to_string()),
        severity: Some(severity.to_string()),
        description: Some("Test incident".to_string()),
        status: "ACTIVE".to_string(),
        start_time: Some("2024-03-10T11:00:00Z".to_string()),
        end_time: None,
        latitude: 52.5005,
        longitude: 13.4005,
    }
}

#[test]
fn test_config_validation_invalid_bbox() {
    let mut config = HereTrafficConfig::new("key", "invalid");
    config.bounding_box = "1,2,3".to_string();
    assert!(config.validate().is_err());
}

#[test]
fn test_detect_flow_change_new_segment() {
    let config = HereTrafficConfig::new("key", "52.5,13.3,52.6,13.5");
    let mut state = state::SourceState::default();
    let changes = state.update_flow("source", &config, vec![sample_segment(3.0, 45.0)]);
    assert_eq!(changes.len(), 1);
    assert!(matches!(changes[0], SourceChange::Insert { .. }));
}

#[test]
fn test_detect_flow_change_thresholds() {
    let config = HereTrafficConfig::new("key", "52.5,13.3,52.6,13.5");
    let mut state = state::SourceState::default();
    state.update_flow("source", &config, vec![sample_segment(3.0, 45.0)]);

    let changes = state.update_flow("source", &config, vec![sample_segment(3.3, 47.0)]);
    assert!(
        changes.is_empty(),
        "Changes below threshold should be ignored"
    );

    let changes = state.update_flow("source", &config, vec![sample_segment(3.9, 47.0)]);
    assert_eq!(changes.len(), 1);
    assert!(matches!(changes[0], SourceChange::Update { .. }));
}

#[test]
fn test_detect_incident_resolved() {
    let mut state = state::SourceState::default();
    state
        .incidents
        .insert("INC_1".to_string(), sample_incident("INC_1", "HIGH"));

    let changes = state.update_incidents("source", vec![]);
    assert_eq!(changes.len(), 1);
    assert!(matches!(changes[0], SourceChange::Delete { .. }));
}

#[test]
fn test_relation_generation() {
    let config = HereTrafficConfig::new("key", "52.5,13.3,52.6,13.5");
    let mut state = state::SourceState::default();
    state.flow_segments.insert(
        "segment_52.50000_13.40000".to_string(),
        sample_segment(3.0, 45.0),
    );
    state
        .incidents
        .insert("INC_1".to_string(), sample_incident("INC_1", "HIGH"));

    let changes = state.update_relations("source", &config);
    assert_eq!(changes.len(), 1);
    assert!(matches!(changes[0], SourceChange::Insert { .. }));

    state.relations.insert(
        "affects_INC_1_segment_52.50000_13.40000".to_string(),
        RelationSnapshot {
            id: "affects_INC_1_segment_52.50000_13.40000".to_string(),
            incident_id: "INC_1".to_string(),
            segment_id: "segment_52.50000_13.40000".to_string(),
            distance_meters: 10.0,
        },
    );

    let changes = state.update_relations("source", &config);
    assert!(
        changes.is_empty(),
        "No relation changes expected when state matches"
    );

    let relation_change = mapping::build_relation_change(
        "source",
        &RelationSnapshot {
            id: "affects_INC_1_segment_52.50000_13.40000".to_string(),
            incident_id: "INC_1".to_string(),
            segment_id: "segment_52.50000_13.40000".to_string(),
            distance_meters: 10.0,
        },
        ChangeKind::Delete,
    );
    assert!(matches!(relation_change, SourceChange::Delete { .. }));
}

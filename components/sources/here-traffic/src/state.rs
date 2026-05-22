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

//! In-memory state and change detection for HERE Traffic polling.

use crate::config::HereTrafficConfig;
use crate::mapping::{
    build_incident_change, build_relation_change, build_relations, build_segment_change,
    segment_changed, ChangeKind, RelationSnapshot, TrafficIncidentSnapshot, TrafficSegmentSnapshot,
};
use chrono::{DateTime, Utc};
use drasi_core::models::SourceChange;
use std::collections::{HashMap, HashSet};

#[derive(Debug, Default)]
pub struct SourceState {
    pub flow_segments: HashMap<String, TrafficSegmentSnapshot>,
    pub incidents: HashMap<String, TrafficIncidentSnapshot>,
    pub relations: HashMap<String, RelationSnapshot>,
    pub last_poll: Option<DateTime<Utc>>,
}

impl SourceState {
    pub fn update_flow(
        &mut self,
        source_id: &str,
        config: &HereTrafficConfig,
        segments: Vec<TrafficSegmentSnapshot>,
    ) -> Vec<SourceChange> {
        let mut changes = Vec::new();
        let mut next_map = HashMap::new();

        for segment in segments {
            let id = segment.id.clone();
            if let Some(previous) = self.flow_segments.get(&id) {
                if segment_changed(previous, &segment, config) {
                    changes.push(build_segment_change(
                        source_id,
                        &segment,
                        ChangeKind::Update,
                    ));
                }
            } else {
                changes.push(build_segment_change(
                    source_id,
                    &segment,
                    ChangeKind::Insert,
                ));
            }
            next_map.insert(id, segment);
        }

        for (id, old_segment) in &self.flow_segments {
            if !next_map.contains_key(id) {
                changes.push(build_segment_change(
                    source_id,
                    old_segment,
                    ChangeKind::Delete,
                ));
            }
        }

        self.flow_segments = next_map;
        changes
    }

    pub fn update_incidents(
        &mut self,
        source_id: &str,
        incidents: Vec<TrafficIncidentSnapshot>,
    ) -> Vec<SourceChange> {
        let mut changes = Vec::new();
        let mut next_map = HashMap::new();

        for incident in incidents {
            let id = incident.id.clone();
            if let Some(previous) = self.incidents.get(&id) {
                if previous != &incident {
                    changes.push(build_incident_change(
                        source_id,
                        &incident,
                        ChangeKind::Update,
                    ));
                }
            } else {
                changes.push(build_incident_change(
                    source_id,
                    &incident,
                    ChangeKind::Insert,
                ));
            }
            next_map.insert(id, incident);
        }

        for (id, old_incident) in &self.incidents {
            if !next_map.contains_key(id) {
                changes.push(build_incident_change(
                    source_id,
                    old_incident,
                    ChangeKind::Delete,
                ));
            }
        }

        self.incidents = next_map;
        changes
    }

    pub fn update_relations(
        &mut self,
        source_id: &str,
        config: &HereTrafficConfig,
    ) -> Vec<SourceChange> {
        let mut changes = Vec::new();
        let next_relations = build_relations(&self.incidents, &self.flow_segments, config);
        let next_ids: HashSet<String> = next_relations.keys().cloned().collect();
        let prev_ids: HashSet<String> = self.relations.keys().cloned().collect();

        for (id, relation) in &next_relations {
            if let Some(prev) = self.relations.get(id) {
                if (prev.distance_meters - relation.distance_meters).abs() > 1.0 {
                    changes.push(build_relation_change(
                        source_id,
                        relation,
                        ChangeKind::Update,
                    ));
                }
            } else {
                changes.push(build_relation_change(
                    source_id,
                    relation,
                    ChangeKind::Insert,
                ));
            }
        }

        for id in prev_ids {
            if !next_ids.contains(&id) {
                if let Some(relation) = self.relations.get(&id) {
                    changes.push(build_relation_change(
                        source_id,
                        relation,
                        ChangeKind::Delete,
                    ));
                }
            }
        }

        self.relations = next_relations;
        changes
    }
}

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

//! Hybrid incremental/full-sweep change detection.

use crate::mapping::{map_new_event, map_removed_event, map_updated_event};
use crate::models::Open511Event;
use drasi_core::models::SourceChange;
use std::collections::{HashMap, HashSet};

/// In-memory polling state tracked per source instance.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
pub struct PollState {
    /// Last known event payloads by event ID.
    pub events_by_id: HashMap<String, Open511Event>,
    /// Shared `Area` node IDs already emitted.
    pub known_areas: HashSet<String>,
    /// RFC3339 timestamp used for incremental `updated>` polling.
    pub last_poll_time: Option<String>,
    /// Poll cycle counter used for periodic full sweeps.
    pub poll_count: u32,
}

/// Output of one change detection cycle.
#[derive(Debug, Default)]
pub struct PollCycleResult {
    pub changes: Vec<SourceChange>,
    pub next_state: PollState,
}

/// Detect changes from an incremental poll.
///
/// This mode only receives changed/new events; removals are handled by full sweep.
/// When `auto_delete_archived` is true, events with status `ARCHIVED` are treated
/// as removals.
pub fn detect_incremental(
    previous: &PollState,
    changed_events: Vec<Open511Event>,
    source_id: &str,
    auto_delete_archived: bool,
) -> PollCycleResult {
    let mut next_state = previous.clone();
    let mut changes = Vec::new();

    for event in changed_events {
        let event_id = event.id.clone();

        if auto_delete_archived && event.status.eq_ignore_ascii_case("ARCHIVED") {
            if let Some(previous_event) = next_state.events_by_id.remove(&event_id) {
                changes.extend(map_removed_event(&previous_event, source_id));
            }
            continue;
        }

        if let Some(previous_event) = next_state.events_by_id.get(&event_id).cloned() {
            changes.extend(map_updated_event(
                &previous_event,
                &event,
                source_id,
                &mut next_state.known_areas,
            ));
        } else {
            changes.extend(map_new_event(
                &event,
                source_id,
                &mut next_state.known_areas,
            ));
        }

        next_state.events_by_id.insert(event_id, event);
    }

    PollCycleResult {
        changes,
        next_state,
    }
}

/// Detect changes from a full sweep poll.
///
/// This mode can detect inserts, updates, and removals.
/// When `auto_delete_archived` is true, events with status `ARCHIVED` are treated
/// as removals even if the API still returns them.
pub fn detect_full_sweep(
    previous: &PollState,
    current_events: Vec<Open511Event>,
    source_id: &str,
    auto_delete_archived: bool,
) -> PollCycleResult {
    let mut next_state = previous.clone();
    let mut changes = Vec::new();
    let mut current_map = HashMap::new();

    for event in current_events {
        current_map.insert(event.id.clone(), event);
    }

    // Removed events.
    for (event_id, previous_event) in &previous.events_by_id {
        if !current_map.contains_key(event_id) {
            changes.extend(map_removed_event(previous_event, source_id));
        }
    }

    // Inserts and updates.
    for (event_id, current_event) in &current_map {
        if auto_delete_archived && current_event.status.eq_ignore_ascii_case("ARCHIVED") {
            if previous.events_by_id.contains_key(event_id) {
                changes.extend(map_removed_event(current_event, source_id));
            }
            continue;
        }

        match previous.events_by_id.get(event_id) {
            None => {
                changes.extend(map_new_event(
                    current_event,
                    source_id,
                    &mut next_state.known_areas,
                ));
            }
            Some(previous_event) => {
                if event_changed(previous_event, current_event) {
                    changes.extend(map_updated_event(
                        previous_event,
                        current_event,
                        source_id,
                        &mut next_state.known_areas,
                    ));
                }
            }
        }
    }

    // When auto_delete_archived, exclude archived events from next state.
    if auto_delete_archived {
        current_map.retain(|_, event| !event.status.eq_ignore_ascii_case("ARCHIVED"));
    }
    next_state.events_by_id = current_map;

    PollCycleResult {
        changes,
        next_state,
    }
}

fn event_changed(previous: &Open511Event, current: &Open511Event) -> bool {
    match (&previous.updated, &current.updated) {
        (Some(prev), Some(cur)) => prev != cur,
        _ => previous != current,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{Open511Area, Open511Road};

    fn event(id: &str, updated: &str) -> Open511Event {
        Open511Event {
            id: id.to_string(),
            status: "ACTIVE".to_string(),
            updated: Some(updated.to_string()),
            roads: Some(vec![Open511Road {
                name: Some("Highway 1".to_string()),
                ..Default::default()
            }]),
            areas: Some(vec![Open511Area {
                id: Some("drivebc.ca/3".to_string()),
                name: Some("Rocky Mountain District".to_string()),
                ..Default::default()
            }]),
            ..Default::default()
        }
    }

    #[test]
    fn incremental_new_event_emits_insert_graph() {
        let previous = PollState::default();
        let result = detect_incremental(
            &previous,
            vec![event("e1", "2026-01-01T00:00:00Z")],
            "s1",
            false,
        );
        assert!(!result.changes.is_empty());
        assert!(result.next_state.events_by_id.contains_key("e1"));
    }

    #[test]
    fn incremental_update_emits_update() {
        let mut previous = PollState::default();
        previous
            .events_by_id
            .insert("e1".to_string(), event("e1", "2026-01-01T00:00:00Z"));

        let result = detect_incremental(
            &previous,
            vec![event("e1", "2026-01-02T00:00:00Z")],
            "s1",
            false,
        );
        assert!(!result.changes.is_empty());
    }

    #[test]
    fn full_sweep_detects_delete() {
        let mut previous = PollState::default();
        previous
            .events_by_id
            .insert("e1".to_string(), event("e1", "2026-01-01T00:00:00Z"));

        let result = detect_full_sweep(&previous, vec![], "s1", false);
        assert!(!result.changes.is_empty());
        let has_delete = result
            .changes
            .iter()
            .any(|change| matches!(change, SourceChange::Delete { .. }));
        assert!(has_delete);
    }

    #[test]
    fn incremental_auto_delete_archived_emits_delete() {
        let mut previous = PollState::default();
        previous
            .events_by_id
            .insert("e1".to_string(), event("e1", "2026-01-01T00:00:00Z"));

        let mut archived = event("e1", "2026-01-02T00:00:00Z");
        archived.status = "ARCHIVED".to_string();

        let result = detect_incremental(&previous, vec![archived], "s1", true);

        let has_delete = result
            .changes
            .iter()
            .any(|change| matches!(change, SourceChange::Delete { .. }));
        assert!(has_delete, "archived event should produce deletes");
        assert!(
            !result.next_state.events_by_id.contains_key("e1"),
            "archived event should be removed from state"
        );
    }

    #[test]
    fn incremental_archived_without_flag_emits_update() {
        let mut previous = PollState::default();
        previous
            .events_by_id
            .insert("e1".to_string(), event("e1", "2026-01-01T00:00:00Z"));

        let mut archived = event("e1", "2026-01-02T00:00:00Z");
        archived.status = "ARCHIVED".to_string();

        let result = detect_incremental(&previous, vec![archived], "s1", false);

        let has_update = result
            .changes
            .iter()
            .any(|change| matches!(change, SourceChange::Update { .. }));
        assert!(
            has_update,
            "without flag, archived should be treated as update"
        );
        assert!(result.next_state.events_by_id.contains_key("e1"));
    }

    #[test]
    fn full_sweep_auto_delete_archived_emits_delete() {
        let mut previous = PollState::default();
        previous
            .events_by_id
            .insert("e1".to_string(), event("e1", "2026-01-01T00:00:00Z"));

        let mut archived = event("e1", "2026-01-02T00:00:00Z");
        archived.status = "ARCHIVED".to_string();

        let result = detect_full_sweep(&previous, vec![archived], "s1", true);

        let has_delete = result
            .changes
            .iter()
            .any(|change| matches!(change, SourceChange::Delete { .. }));
        assert!(
            has_delete,
            "archived event should produce deletes on full sweep"
        );
        assert!(
            !result.next_state.events_by_id.contains_key("e1"),
            "archived event should not be in next state"
        );
    }
}

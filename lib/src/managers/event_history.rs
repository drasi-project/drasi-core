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

//! Component event history tracking.
//!
//! This module provides centralized storage for component lifecycle events,
//! allowing managers to track and query the history of status changes for
//! sources, queries, and reactions.

use std::collections::{HashMap, VecDeque};

use crate::channels::{ComponentEvent, ComponentStatus};

/// Default maximum number of events to retain per component.
pub const DEFAULT_MAX_EVENTS_PER_COMPONENT: usize = 100;

/// Stores component event history with a bounded size per component.
///
/// Events are stored in a FIFO manner - when the limit is reached for a component,
/// the oldest events are discarded to make room for new ones.
#[derive(Debug)]
pub struct ComponentEventHistory {
    /// Events stored per component ID
    events: HashMap<String, VecDeque<ComponentEvent>>,
    /// Maximum events to retain per component
    max_events_per_component: usize,
}

impl Default for ComponentEventHistory {
    fn default() -> Self {
        Self::new()
    }
}

impl ComponentEventHistory {
    /// Create a new event history with default capacity (100 events per component).
    pub fn new() -> Self {
        Self {
            events: HashMap::new(),
            max_events_per_component: DEFAULT_MAX_EVENTS_PER_COMPONENT,
        }
    }

    /// Create a new event history with custom capacity per component.
    pub fn with_capacity(max_events_per_component: usize) -> Self {
        Self {
            events: HashMap::new(),
            max_events_per_component,
        }
    }

    /// Record a component event in the history.
    ///
    /// If the component has reached its maximum event count, the oldest
    /// event is removed before adding the new one.
    pub fn record_event(&mut self, event: ComponentEvent) {
        let component_id = event.component_id.clone();
        let events = self.events.entry(component_id).or_default();

        // Remove oldest event if at capacity
        if events.len() >= self.max_events_per_component {
            events.pop_front();
        }

        events.push_back(event);
    }

    /// Get all events for a specific component.
    ///
    /// Returns events in chronological order (oldest first).
    pub fn get_events(&self, component_id: &str) -> Vec<ComponentEvent> {
        self.events
            .get(component_id)
            .map(|events| events.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Get the most recent error message for a component.
    ///
    /// Searches backwards through the component's event history to find the
    /// most recent event with `ComponentStatus::Error` and returns its message.
    pub fn get_last_error(&self, component_id: &str) -> Option<String> {
        self.events.get(component_id).and_then(|events| {
            events
                .iter()
                .rev()
                .find(|event| event.status == ComponentStatus::Error)
                .and_then(|event| event.message.clone())
        })
    }

    /// Get all events across all components.
    ///
    /// Returns events sorted by timestamp (oldest first).
    pub fn get_all_events(&self) -> Vec<ComponentEvent> {
        let mut all_events: Vec<ComponentEvent> = self
            .events
            .values()
            .flat_map(|events| events.iter().cloned())
            .collect();

        // Sort by timestamp
        all_events.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));
        all_events
    }

    /// Remove all events for a specific component.
    ///
    /// This should be called when a component is deleted to clean up its history.
    pub fn remove_component(&mut self, component_id: &str) {
        self.events.remove(component_id);
    }

    /// Get the number of events stored for a specific component.
    pub fn event_count(&self, component_id: &str) -> usize {
        self.events
            .get(component_id)
            .map(|events| events.len())
            .unwrap_or(0)
    }

    /// Get the total number of events stored across all components.
    pub fn total_event_count(&self) -> usize {
        self.events.values().map(|events| events.len()).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channels::ComponentType;
    use chrono::Utc;

    fn create_test_event(
        component_id: &str,
        status: ComponentStatus,
        message: Option<&str>,
    ) -> ComponentEvent {
        ComponentEvent {
            component_id: component_id.to_string(),
            component_type: ComponentType::Source,
            status,
            timestamp: Utc::now(),
            message: message.map(|s| s.to_string()),
        }
    }

    #[test]
    fn test_record_and_get_events() {
        let mut history = ComponentEventHistory::new();

        let event1 = create_test_event("source1", ComponentStatus::Starting, None);
        let event2 = create_test_event("source1", ComponentStatus::Running, None);

        history.record_event(event1);
        history.record_event(event2);

        let events = history.get_events("source1");
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].status, ComponentStatus::Starting);
        assert_eq!(events[1].status, ComponentStatus::Running);
    }

    #[test]
    fn test_max_events_per_component() {
        let mut history = ComponentEventHistory::with_capacity(3);

        for i in 0..5 {
            let event = create_test_event(
                "source1",
                ComponentStatus::Running,
                Some(&format!("event {i}")),
            );
            history.record_event(event);
        }

        let events = history.get_events("source1");
        assert_eq!(events.len(), 3);
        // Should have events 2, 3, 4 (oldest removed)
        assert_eq!(events[0].message, Some("event 2".to_string()));
        assert_eq!(events[2].message, Some("event 4".to_string()));
    }

    #[test]
    fn test_get_last_error() {
        let mut history = ComponentEventHistory::new();

        history.record_event(create_test_event(
            "source1",
            ComponentStatus::Starting,
            None,
        ));
        history.record_event(create_test_event(
            "source1",
            ComponentStatus::Error,
            Some("First error"),
        ));
        history.record_event(create_test_event(
            "source1",
            ComponentStatus::Starting,
            None,
        ));
        history.record_event(create_test_event(
            "source1",
            ComponentStatus::Error,
            Some("Second error"),
        ));
        history.record_event(create_test_event("source1", ComponentStatus::Running, None));

        let last_error = history.get_last_error("source1");
        assert_eq!(last_error, Some("Second error".to_string()));
    }

    #[test]
    fn test_get_last_error_none() {
        let mut history = ComponentEventHistory::new();

        history.record_event(create_test_event(
            "source1",
            ComponentStatus::Starting,
            None,
        ));
        history.record_event(create_test_event("source1", ComponentStatus::Running, None));

        let last_error = history.get_last_error("source1");
        assert!(last_error.is_none());
    }

    #[test]
    fn test_get_all_events() {
        let mut history = ComponentEventHistory::new();

        history.record_event(create_test_event(
            "source1",
            ComponentStatus::Starting,
            None,
        ));
        history.record_event(create_test_event("query1", ComponentStatus::Starting, None));
        history.record_event(create_test_event("source1", ComponentStatus::Running, None));

        let all_events = history.get_all_events();
        assert_eq!(all_events.len(), 3);
    }

    #[test]
    fn test_remove_component() {
        let mut history = ComponentEventHistory::new();

        history.record_event(create_test_event(
            "source1",
            ComponentStatus::Starting,
            None,
        ));
        history.record_event(create_test_event(
            "source2",
            ComponentStatus::Starting,
            None,
        ));

        history.remove_component("source1");

        assert_eq!(history.get_events("source1").len(), 0);
        assert_eq!(history.get_events("source2").len(), 1);
    }

    #[test]
    fn test_event_count() {
        let mut history = ComponentEventHistory::new();

        history.record_event(create_test_event(
            "source1",
            ComponentStatus::Starting,
            None,
        ));
        history.record_event(create_test_event("source1", ComponentStatus::Running, None));
        history.record_event(create_test_event(
            "source2",
            ComponentStatus::Starting,
            None,
        ));

        assert_eq!(history.event_count("source1"), 2);
        assert_eq!(history.event_count("source2"), 1);
        assert_eq!(history.event_count("nonexistent"), 0);
        assert_eq!(history.total_event_count(), 3);
    }
}

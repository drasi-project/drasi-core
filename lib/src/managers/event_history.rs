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

//! Component event history tracking with live streaming support.
//!
//! This module provides centralized storage for component lifecycle events,
//! allowing managers to track and query the history of status changes for
//! sources, queries, and reactions. It also supports live streaming of events
//! via broadcast channels.

use std::collections::{HashMap, VecDeque};

use tokio::sync::broadcast;

use crate::channels::{ComponentEvent, ComponentStatus};

/// Default maximum number of events to retain per component.
pub const DEFAULT_MAX_EVENTS_PER_COMPONENT: usize = 100;

/// Default broadcast channel capacity for live event streaming.
pub const DEFAULT_EVENT_CHANNEL_CAPACITY: usize = 256;

/// Per-component event storage and broadcast channel.
struct ComponentEventChannel {
    /// Recent event history
    history: VecDeque<ComponentEvent>,
    /// Maximum history size
    max_history: usize,
    /// Broadcast sender for live streaming
    sender: broadcast::Sender<ComponentEvent>,
}

impl ComponentEventChannel {
    fn new(max_history: usize, channel_capacity: usize) -> Self {
        let (sender, _) = broadcast::channel(channel_capacity);
        Self {
            history: VecDeque::with_capacity(max_history),
            max_history,
            sender,
        }
    }

    fn record(&mut self, event: ComponentEvent) {
        // Add to history
        if self.history.len() >= self.max_history {
            self.history.pop_front();
        }
        self.history.push_back(event.clone());

        // Broadcast to live subscribers (ignore if no subscribers)
        let _ = self.sender.send(event);
    }

    fn get_history(&self) -> Vec<ComponentEvent> {
        self.history.iter().cloned().collect()
    }

    fn subscribe(&self) -> broadcast::Receiver<ComponentEvent> {
        self.sender.subscribe()
    }

    fn get_last_error(&self) -> Option<String> {
        self.history
            .iter()
            .rev()
            .find(|event| event.status == ComponentStatus::Error)
            .and_then(|event| event.message.clone())
    }
}

/// Stores component event history with a bounded size per component and live streaming support.
///
/// Events are stored in a FIFO manner - when the limit is reached for a component,
/// the oldest events are discarded to make room for new ones.
///
/// Subscribers can receive live events as they occur via broadcast channels.
pub struct ComponentEventHistory {
    /// Event channels per component ID
    channels: HashMap<String, ComponentEventChannel>,
    /// Maximum events to retain per component
    max_events_per_component: usize,
    /// Broadcast channel capacity
    channel_capacity: usize,
}

impl std::fmt::Debug for ComponentEventHistory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ComponentEventHistory")
            .field("max_events_per_component", &self.max_events_per_component)
            .field("channel_capacity", &self.channel_capacity)
            .field("component_count", &self.channels.len())
            .finish()
    }
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
            channels: HashMap::new(),
            max_events_per_component: DEFAULT_MAX_EVENTS_PER_COMPONENT,
            channel_capacity: DEFAULT_EVENT_CHANNEL_CAPACITY,
        }
    }

    /// Create a new event history with custom capacity per component.
    pub fn with_capacity(max_events_per_component: usize, channel_capacity: usize) -> Self {
        Self {
            channels: HashMap::new(),
            max_events_per_component,
            channel_capacity,
        }
    }

    /// Record a component event in the history and broadcast to subscribers.
    ///
    /// If the component has reached its maximum event count, the oldest
    /// event is removed before adding the new one.
    pub fn record_event(&mut self, event: ComponentEvent) {
        let component_id = event.component_id.clone();
        let channel = self.channels.entry(component_id).or_insert_with(|| {
            ComponentEventChannel::new(self.max_events_per_component, self.channel_capacity)
        });
        channel.record(event);
    }

    /// Get all events for a specific component.
    ///
    /// Returns events in chronological order (oldest first).
    pub fn get_events(&self, component_id: &str) -> Vec<ComponentEvent> {
        self.channels
            .get(component_id)
            .map(|channel| channel.get_history())
            .unwrap_or_default()
    }

    /// Subscribe to live events for a component.
    ///
    /// Returns the current history and a broadcast receiver for new events.
    /// Creates the component's channel if it doesn't exist.
    pub fn subscribe(
        &mut self,
        component_id: &str,
    ) -> (Vec<ComponentEvent>, broadcast::Receiver<ComponentEvent>) {
        let channel = self.channels.entry(component_id.to_string()).or_insert_with(|| {
            ComponentEventChannel::new(self.max_events_per_component, self.channel_capacity)
        });

        let history = channel.get_history();
        let receiver = channel.subscribe();
        (history, receiver)
    }

    /// Get the most recent error message for a component.
    ///
    /// Searches backwards through the component's event history to find the
    /// most recent event with `ComponentStatus::Error` and returns its message.
    pub fn get_last_error(&self, component_id: &str) -> Option<String> {
        self.channels
            .get(component_id)
            .and_then(|channel| channel.get_last_error())
    }

    /// Get all events across all components.
    ///
    /// Returns events sorted by timestamp (oldest first).
    pub fn get_all_events(&self) -> Vec<ComponentEvent> {
        let mut all_events: Vec<ComponentEvent> = self
            .channels
            .values()
            .flat_map(|channel| channel.get_history())
            .collect();

        // Sort by timestamp
        all_events.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));
        all_events
    }

    /// Remove all events for a specific component.
    ///
    /// This should be called when a component is deleted to clean up its history.
    pub fn remove_component(&mut self, component_id: &str) {
        self.channels.remove(component_id);
    }

    /// Get the number of events stored for a specific component.
    pub fn event_count(&self, component_id: &str) -> usize {
        self.channels
            .get(component_id)
            .map(|channel| channel.history.len())
            .unwrap_or(0)
    }

    /// Get the total number of events stored across all components.
    pub fn total_event_count(&self) -> usize {
        self.channels
            .values()
            .map(|channel| channel.history.len())
            .sum()
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
        let mut history = ComponentEventHistory::with_capacity(3, 10);

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

    #[test]
    fn test_subscribe_gets_history() {
        let mut history = ComponentEventHistory::new();

        history.record_event(create_test_event(
            "source1",
            ComponentStatus::Starting,
            None,
        ));
        history.record_event(create_test_event("source1", ComponentStatus::Running, None));

        let (events, _receiver) = history.subscribe("source1");
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].status, ComponentStatus::Starting);
        assert_eq!(events[1].status, ComponentStatus::Running);
    }

    #[tokio::test]
    async fn test_subscribe_receives_live_events() {
        let mut history = ComponentEventHistory::new();

        // Subscribe before recording
        let (_history, mut receiver) = history.subscribe("source1");

        // Record a new event
        let event = create_test_event("source1", ComponentStatus::Running, Some("live event"));
        history.record_event(event);

        // Should receive the live event
        let received = receiver.try_recv().unwrap();
        assert_eq!(received.status, ComponentStatus::Running);
        assert_eq!(received.message, Some("live event".to_string()));
    }
}

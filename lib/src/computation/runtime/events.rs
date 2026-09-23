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

use super::*;
use crate::{
    channels::{ComponentEvent, ComponentEventBroadcastSender, ComponentType},
    managers::ComponentEventHistory,
};
use std::sync::Mutex as SyncMutex;

/// An event journal, not a component registry. Membership and statuses are
/// derived from each pair of authoritative controller publications.
pub(super) struct Events {
    pub(super) broadcast: ComponentEventBroadcastSender,
    history: SyncMutex<ComponentEventHistory>,
}

impl Events {
    pub(super) fn new() -> Arc<Self> {
        Arc::new(Self {
            broadcast: tokio::sync::broadcast::channel(1000).0,
            history: SyncMutex::new(ComponentEventHistory::new()),
        })
    }

    pub(super) fn component(&self, id: &str) -> Vec<ComponentEvent> {
        self.history
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_events(id)
    }

    pub(super) fn all(&self) -> Vec<ComponentEvent> {
        self.history
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_all_events()
    }

    pub(super) fn subscribe(
        &self,
        id: &str,
    ) -> (
        Vec<ComponentEvent>,
        tokio::sync::broadcast::Receiver<ComponentEvent>,
    ) {
        self.history
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .subscribe(id)
    }

    fn record(&self, event: ComponentEvent) {
        self.history
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .record_event(event.clone());
        let _ = self.broadcast.send(event);
    }
}

fn component_type(snapshot: &GraphSnapshot, id: &ComponentId) -> Option<ComponentType> {
    let specification = snapshot.specifications.get(id)?;
    if specification.implementation.name.as_ref() != "drasi/lib-runtime-component" {
        return None;
    }
    match specification.configuration.get("kind")? {
        ConfigurationValue::Literal(serde_json::Value::String(kind)) => match kind.as_str() {
            "source" => Some(ComponentType::Source),
            "query" => Some(ComponentType::Query),
            "reaction" => Some(ComponentType::Reaction),
            _ => None,
        },
        _ => None,
    }
}

pub(super) fn observed_status(
    observed: &ObservedComponent,
    initial: ComponentStatus,
) -> ComponentStatus {
    if observed.failure.is_some() || observed.lifecycle == ComponentLifecycle::Failed {
        ComponentStatus::Error
    } else if observed.lifecycle == ComponentLifecycle::Starting {
        ComponentStatus::Starting
    } else if observed.lifecycle == ComponentLifecycle::Stopping {
        ComponentStatus::Stopping
    } else if !observed.lifecycle_requested || observed.realization != RealizationState::Created {
        initial
    } else if observed.exhausted || observed.lifecycle == ComponentLifecycle::Stopped {
        ComponentStatus::Stopped
    } else {
        ComponentStatus::Running
    }
}

impl PublicationObserver for Events {
    fn publish(&self, previous: &ComputationInspection, current: &ComputationInspection) {
        for (id, observed) in &current.observed.components {
            let Some(kind) = component_type(&current.desired, id) else {
                continue;
            };
            let old = previous.observed.components.get(id);
            let replaced = old.is_some()
                && record_token(&previous.desired, id) != record_token(&current.desired, id);
            let status = observed_status(observed, ComponentStatus::Added);
            let emit = |status, message| {
                self.record(ComponentEvent {
                    component_id: id.to_string(),
                    component_type: kind.clone(),
                    status,
                    timestamp: current.timestamp,
                    message,
                });
            };
            if old.is_none() {
                emit(ComponentStatus::Added, Some(format!("{kind:?} added")));
            } else if replaced {
                emit(
                    ComponentStatus::Reconfiguring,
                    Some(format!("Reconfiguring {kind:?}")),
                );
            }
            let old_status = old.map(|old| observed_status(old, ComponentStatus::Added));
            // Requesting a first start changes intent before the lifecycle
            // transitions. It is not a stop of a previously running component.
            if !replaced
                && status == ComponentStatus::Stopped
                && !observed.started
                && old
                    .is_some_and(|old| !old.started && old.lifecycle == ComponentLifecycle::Stopped)
            {
                continue;
            }
            if replaced || old_status != Some(status) && status != ComponentStatus::Added {
                emit(
                    if replaced && status == ComponentStatus::Added {
                        ComponentStatus::Stopped
                    } else {
                        status
                    },
                    observed
                        .failure
                        .as_ref()
                        .map(|failure| format!("{:#}", failure.cause)),
                );
            }
        }
        for id in previous.observed.components.keys() {
            if !current.observed.components.contains_key(id) {
                if let Some(kind) = component_type(&previous.desired, id) {
                    self.record(ComponentEvent {
                        component_id: id.to_string(),
                        component_type: kind,
                        status: ComponentStatus::Removed,
                        timestamp: current.timestamp,
                        message: Some("Component removed".into()),
                    });
                    self.history
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .remove_component(id.as_str());
                }
            }
        }
    }
}

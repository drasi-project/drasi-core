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

//! Event-based test helpers that replace `sleep()` calls with deterministic waits.
//!
//! These helpers use the existing `ComponentEvent` broadcast channel to wait for
//! specific status transitions, avoiding timing-dependent tests.
//!
//! # Usage
//!
//! ```ignore
//! use crate::test_helpers::wait_for_component_status;
//!
//! // Subscribe BEFORE triggering the action
//! let mut event_rx = graph.read().await.subscribe();
//!
//! manager.start_source("my-source").await.unwrap();
//!
//! // Wait for the event (no sleep!)
//! wait_for_component_status(
//!     &mut event_rx,
//!     "my-source",
//!     ComponentStatus::Running,
//!     Duration::from_secs(5),
//! ).await;
//! ```

use std::time::Duration;

use tokio::sync::broadcast;

use crate::channels::{ComponentEvent, ComponentStatus};

/// Wait for a specific component to reach a target status via the broadcast channel.
///
/// Consumes events from the receiver until a matching event is found, or the
/// timeout expires. Returns the matching event on success.
///
/// # Panics
///
/// Panics if the timeout expires before a matching event is received (test failure).
pub async fn wait_for_component_status(
    event_rx: &mut broadcast::Receiver<ComponentEvent>,
    component_id: &str,
    expected_status: ComponentStatus,
    timeout: Duration,
) -> ComponentEvent {
    wait_for_event(
        event_rx,
        |e| e.component_id == component_id && e.status == expected_status,
        timeout,
        &format!("component '{component_id}' to reach {expected_status:?}"),
    )
    .await
}

/// Wait for ANY event matching a predicate on the broadcast channel.
///
/// # Panics
///
/// Panics with `description` if the timeout expires.
pub async fn wait_for_event(
    event_rx: &mut broadcast::Receiver<ComponentEvent>,
    predicate: impl Fn(&ComponentEvent) -> bool,
    timeout: Duration,
    description: &str,
) -> ComponentEvent {
    match tokio::time::timeout(timeout, async {
        loop {
            match event_rx.recv().await {
                Ok(event) if predicate(&event) => return event,
                Ok(_) => continue,
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    eprintln!("Warning: event receiver lagged by {n} events");
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => {
                    panic!("Event channel closed while waiting for {description}");
                }
            }
        }
    })
    .await
    {
        Ok(event) => event,
        Err(_) => panic!("Timed out ({timeout:?}) waiting for {description}"),
    }
}

/// Wait for multiple components to each reach a target status.
///
/// All targets must be reached within the timeout. Events for different
/// components can arrive in any order.
///
/// # Panics
///
/// Panics if the timeout expires before all targets are satisfied.
pub async fn wait_for_all_statuses(
    event_rx: &mut broadcast::Receiver<ComponentEvent>,
    targets: &[(&str, ComponentStatus)],
    timeout: Duration,
) {
    let mut remaining: Vec<(String, ComponentStatus)> =
        targets.iter().map(|(id, s)| (id.to_string(), *s)).collect();

    match tokio::time::timeout(timeout, async {
        while !remaining.is_empty() {
            match event_rx.recv().await {
                Ok(event) => {
                    remaining.retain(|(id, status)| {
                        !(event.component_id == *id && event.status == *status)
                    });
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    eprintln!("Warning: event receiver lagged by {n} events");
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => {
                    panic!(
                        "Event channel closed while waiting for statuses. Remaining: {remaining:?}",
                    );
                }
            }
        }
    })
    .await
    {
        Ok(()) => {}
        Err(_) => panic!(
            "Timed out ({timeout:?}) waiting for component statuses. Remaining: {remaining:?}",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channels::ComponentType;

    fn make_event(id: &str, status: ComponentStatus) -> ComponentEvent {
        ComponentEvent {
            component_id: id.to_string(),
            component_type: ComponentType::Source,
            status,
            timestamp: chrono::Utc::now(),
            message: None,
        }
    }

    #[tokio::test]
    async fn test_wait_for_component_status_immediate() {
        let (tx, mut rx) = broadcast::channel(16);
        tx.send(make_event("s1", ComponentStatus::Running)).unwrap();
        let event = wait_for_component_status(
            &mut rx,
            "s1",
            ComponentStatus::Running,
            Duration::from_secs(1),
        )
        .await;
        assert_eq!(event.component_id, "s1");
    }

    #[tokio::test]
    async fn test_wait_for_component_status_skips_non_matching() {
        let (tx, mut rx) = broadcast::channel(16);
        tx.send(make_event("s1", ComponentStatus::Starting))
            .unwrap();
        tx.send(make_event("s2", ComponentStatus::Running)).unwrap();
        tx.send(make_event("s1", ComponentStatus::Running)).unwrap();
        let event = wait_for_component_status(
            &mut rx,
            "s1",
            ComponentStatus::Running,
            Duration::from_secs(1),
        )
        .await;
        assert_eq!(event.component_id, "s1");
        assert_eq!(event.status, ComponentStatus::Running);
    }

    #[tokio::test]
    async fn test_wait_for_all_statuses() {
        let (tx, mut rx) = broadcast::channel(16);
        tx.send(make_event("s1", ComponentStatus::Running)).unwrap();
        tx.send(make_event("s2", ComponentStatus::Running)).unwrap();
        wait_for_all_statuses(
            &mut rx,
            &[("s1", ComponentStatus::Running), ("s2", ComponentStatus::Running)],
            Duration::from_secs(1),
        )
        .await;
    }

    #[tokio::test]
    #[should_panic(expected = "Timed out")]
    async fn test_wait_for_component_status_timeout() {
        let (_tx, mut rx) = broadcast::channel::<ComponentEvent>(16);
        wait_for_component_status(
            &mut rx,
            "s1",
            ComponentStatus::Running,
            Duration::from_millis(50),
        )
        .await;
    }
}

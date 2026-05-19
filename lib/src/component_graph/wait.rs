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

use std::sync::Arc;

use tokio::sync::RwLock;

use crate::channels::ComponentStatus;

use super::graph::ComponentGraph;

// ============================================================================
// Async Status Waiter
// ============================================================================

/// Wait for a component to reach one of the target statuses, with a timeout.
///
/// This replaces polling loops that use `sleep()` + status check. It uses the
/// graph's [`Notify`] to wake up only when a status actually changes, avoiding
/// busy-waiting.
///
/// # Pattern
///
/// Uses the same register-before-check pattern as `PriorityQueue::enqueue_wait()`:
/// 1. Register `notified()` interest
/// 2. Acquire read lock, check condition
/// 3. If not met, release lock and await notification
/// 4. Repeat until condition met or timeout
///
/// # Arguments
///
/// * `graph` — The shared graph handle
/// * `component_id` — ID of the component to watch
/// * `target_statuses` — One or more acceptable statuses to wait for
/// * `timeout` — Maximum time to wait before returning an error
///
/// # Errors
///
/// Returns an error if the timeout expires before the component reaches any
/// target status, or if the component is not found in the graph.
pub async fn wait_for_status(
    graph: &Arc<RwLock<ComponentGraph>>,
    component_id: &str,
    target_statuses: &[ComponentStatus],
    timeout: std::time::Duration,
) -> anyhow::Result<ComponentStatus> {
    let deadline = tokio::time::Instant::now() + timeout;

    // Get the Notify handle once (doesn't require holding the lock)
    let notify = {
        let g = graph.read().await;
        g.status_notifier()
    };

    loop {
        // Register interest BEFORE checking condition (avoid race)
        let notified = notify.notified();

        // Check current status under read lock
        {
            let g = graph.read().await;
            if let Some(node) = g.get_component(component_id) {
                if target_statuses.contains(&node.status) {
                    return Ok(node.status);
                }
            } else {
                return Err(anyhow::anyhow!(
                    "Component '{component_id}' not found in graph"
                ));
            }
        }
        // Lock released

        // Wait for a status change or timeout
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err(anyhow::anyhow!(
                "Timed out waiting for component '{component_id}' to reach {target_statuses:?}",
            ));
        }

        tokio::select! {
            _ = notified => {
                // A status changed somewhere — loop back and re-check
            }
            _ = tokio::time::sleep(remaining) => {
                return Err(anyhow::anyhow!(
                    "Timed out waiting for component '{component_id}' to reach {target_statuses:?}",
                ));
            }
        }
    }
}

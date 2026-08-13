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

//! Deterministic (stable) ID generation.
//!
//! IDs are derived with UUIDv5 (namespaced SHA-1) so that recomputing them
//! from the same inputs — including after a crash and restart — always
//! yields the same value. This is what makes the reservation key and the
//! `executionId` stable across recovery.

use uuid::Uuid;

/// Fixed namespace for this reaction's deterministic IDs. Generated once via
/// `uuid::Uuid::new_v4()` and frozen here — it must never change, or all
/// previously generated `executionId`s become unreproducible.
const EXECUTION_ID_NAMESPACE: Uuid = Uuid::from_bytes([
    0x8f, 0x2c, 0x9e, 0x11, 0x4a, 0x9d, 0x4b, 0x66, 0xa3, 0x0d, 0x1b, 0x8e, 0x77, 0x21, 0xfa, 0x4c,
]);

/// Compute the stable `executionId` for one launch attempt.
///
/// `executionId` is this reaction's own private correlation ID for the
/// attempt. It is embedded in the agent prompt and in the workgraph execution
/// comment so that the reconciliation seam can find a task that was created
/// but whose HTTP response was lost (see `crate::github`).
pub fn execution_id(
    reaction_id: &str,
    route_id: &str,
    responsibility_id: &str,
    attempt: u32,
) -> String {
    let name = format!("{reaction_id}|{route_id}|{responsibility_id}|{attempt}");
    format!(
        "execution:{}",
        Uuid::new_v5(&EXECUTION_ID_NAMESPACE, name.as_bytes())
    )
}

/// Compute the canonical expected completion event ID for an execution.
pub fn expected_event_id(execution_id: &str, required_event_type: &str) -> String {
    format!("event:{execution_id}:{required_event_type}")
}

/// The state-store key for a reservation/execution record.
pub fn reservation_key(route_id: &str, responsibility_id: &str, attempt: u32) -> String {
    format!("execution:{route_id}:{responsibility_id}:{attempt}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn execution_id_is_stable() {
        let a = execution_id("r1", "route-1", "resp-1", 1);
        let b = execution_id("r1", "route-1", "resp-1", 1);
        assert_eq!(a, b);
        assert!(a.starts_with("execution:"));
    }

    #[test]
    fn execution_id_varies_with_inputs() {
        let a = execution_id("r1", "route-1", "resp-1", 1);
        let b = execution_id("r1", "route-2", "resp-1", 1);
        let c = execution_id("r1", "route-1", "resp-2", 1);
        let d = execution_id("r1", "route-1", "resp-1", 2);
        let e = execution_id("r2", "route-1", "resp-1", 1);
        let all = [a.clone(), b, c, d, e];
        for i in 0..all.len() {
            for j in (i + 1)..all.len() {
                assert_ne!(all[i], all[j], "IDs at {i} and {j} collided");
            }
        }
    }

    #[test]
    fn reservation_key_format() {
        assert_eq!(
            reservation_key("route-1", "resp-1", 1),
            "execution:route-1:resp-1:1"
        );
    }

    #[test]
    fn expected_event_id_uses_canonical_correlations() {
        assert_eq!(
            expected_event_id("execution:abc", "CompletedIssueValidation"),
            "event:execution:abc:CompletedIssueValidation"
        );
    }
}

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

//! Deterministic (stable) ID generation for one execution.
//!
//! The `executionId` and the durable reservation key are pure functions of the
//! `runId` alone, so recomputing them from the same run — including after a
//! crash and restart — always yields the same value. That is what guarantees
//! **exactly one** execution (and one reservation) per run: the reaction never
//! folds the reaction ID or an attempt number into the identity, so a
//! redelivered row can only ever resolve to the same execution.

use drasi_workgraph_common::event::ExecutionId;
use uuid::Uuid;

/// Fixed namespace for this reaction's deterministic `executionId`. Generated
/// once via `uuid::Uuid::new_v4()` and frozen here — it must never change, or
/// all previously generated `executionId`s become unreproducible.
const EXECUTION_ID_NAMESPACE: Uuid = Uuid::from_bytes([
    0x8f, 0x2c, 0x9e, 0x11, 0x4a, 0x9d, 0x4b, 0x66, 0xa3, 0x0d, 0x1b, 0x8e, 0x77, 0x21, 0xfa, 0x4c,
]);

/// Compute the stable `executionId` for one run.
///
/// `executionId` is a UUIDv5 (namespaced SHA-1) over the `runId` and nothing
/// else, so exactly one execution exists per run. It is embedded in the agent
/// prompt so the reconciliation seam can find a task that was created but whose
/// HTTP response was lost (see [`crate::github`]).
pub fn execution_id(run_id: &str) -> ExecutionId {
    let uuid = Uuid::new_v5(&EXECUTION_ID_NAMESPACE, run_id.as_bytes());
    ExecutionId::from_suffix(&uuid.to_string())
        .expect("a UUIDv5 string is always a valid execution suffix")
}

/// The state-store key for one run's durable execution record.
///
/// Keyed by the `runId` so the durable reservation is one-per-run and survives
/// restarts.
pub fn reservation_key(run_id: &str) -> String {
    format!("execution:{run_id}")
}

#[cfg(test)]
mod tests {
    use super::*;

    const RUN_A: &str = "run:0000000000000000000000000000000000000000000000000000000000000001";
    const RUN_B: &str = "run:0000000000000000000000000000000000000000000000000000000000000002";

    #[test]
    fn execution_id_is_a_pure_function_of_the_run_id() {
        let a = execution_id(RUN_A);
        let b = execution_id(RUN_A);
        assert_eq!(a, b, "same run must yield the same execution id");
        assert!(a.as_str().starts_with("execution:"));
    }

    #[test]
    fn execution_id_is_unique_per_run() {
        assert_ne!(
            execution_id(RUN_A),
            execution_id(RUN_B),
            "distinct runs must yield distinct execution ids"
        );
    }

    #[test]
    fn reservation_key_is_derived_from_the_run_id() {
        assert_eq!(reservation_key(RUN_A), format!("execution:{RUN_A}"));
        assert_ne!(reservation_key(RUN_A), reservation_key(RUN_B));
    }
}

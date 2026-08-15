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
use drasi_workgraph_common::event::RunId;

/// Compute the stable `executionId` for one run.
///
/// `executionId` is exactly `execution:<runId>`, so every participant can derive
/// it without hashing or a component-specific namespace.
pub fn execution_id(run_id: &RunId) -> ExecutionId {
    ExecutionId::from_run_id(run_id)
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

    const RUN_A: &str =
        "validation:PVTI_example:sha256:9faac769ff6962c7f331881d97518ff6a9df338da679c5d4851577cb7404a7fa";
    const RUN_B: &str =
        "validation:PVTI_other:sha256:9faac769ff6962c7f331881d97518ff6a9df338da679c5d4851577cb7404a7fa";

    fn run(value: &str) -> RunId {
        RunId::try_from(value.to_string()).expect("valid run")
    }

    #[test]
    fn execution_id_is_a_pure_function_of_the_run_id() {
        let a = execution_id(&run(RUN_A));
        let b = execution_id(&run(RUN_A));
        assert_eq!(a, b, "same run must yield the same execution id");
        assert_eq!(a.as_str(), format!("execution:{RUN_A}"));
    }

    #[test]
    fn execution_id_is_unique_per_run() {
        assert_ne!(
            execution_id(&run(RUN_A)),
            execution_id(&run(RUN_B)),
            "distinct runs must yield distinct execution ids"
        );
    }

    #[test]
    fn reservation_key_is_derived_from_the_run_id() {
        assert_eq!(reservation_key(RUN_A), format!("execution:{RUN_A}"));
        assert_ne!(reservation_key(RUN_A), reservation_key(RUN_B));
    }
}

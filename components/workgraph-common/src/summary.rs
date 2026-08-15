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

//! Exact human summary lines for WorkGraph comments.
//!
//! Summaries are display-only and never parsed for routing, trust, or identity.

use crate::event::{
    NextResponsibilityType, ValidationOutcome, WorkGraphEvent, WorkGraphEventPayload,
};

/// Generate the canonical summary line for an event.
pub fn summary_for(event: &WorkGraphEvent) -> String {
    match &event.payload {
        WorkGraphEventPayload::ResponsibilityAssigned(_) => {
            "Issue validation assigned.".to_string()
        }
        WorkGraphEventPayload::ExecutionStarted(_) => "Issue validation started.".to_string(),
        WorkGraphEventPayload::CompletedIssueValidation(payload) => match payload.outcome {
            ValidationOutcome::Passed => "Issue validation passed.".to_string(),
            ValidationOutcome::Failed => "Issue validation failed.".to_string(),
        },
        WorkGraphEventPayload::RoutingDecided(payload) => match payload.next_responsibility_type {
            NextResponsibilityType::IssueRiskProfiling => {
                "Issue routed to risk profiling.".to_string()
            }
            NextResponsibilityType::IssueCorrection => "Issue routed for correction.".to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::comment::validate_summary;
    use crate::event::{
        AssignedResponsibilityType, CompletedIssueValidationPayload, ExecutionId,
        ExecutionStartedPayload, ProfileRef, ResponsibilityAssignedPayload, RoutingDecidedPayload,
        Sha256Digest, ValidationReasonCode, WorkGraphEventPayload,
    };
    use crate::ids::run_id;

    const ITEM: &str = "PVTI_example";
    const SUBJECT: &str = "I_example";
    const BLOB: &str = "0123456789abcdef0123456789abcdef01234567";
    const DIGEST: &str = "sha256:9faac769ff6962c7f331881d97518ff6a9df338da679c5d4851577cb7404a7fa";

    fn digest() -> Sha256Digest {
        Sha256Digest::try_from(DIGEST.to_string()).expect("valid digest")
    }

    fn event(payload: WorkGraphEventPayload) -> WorkGraphEvent {
        WorkGraphEvent::new(run_id(ITEM, &digest()), ITEM, SUBJECT, payload).expect("valid event")
    }

    #[test]
    fn summaries_are_exact_and_valid() {
        let run = run_id(ITEM, &digest());
        let execution_id = ExecutionId::from_run_id(&run);
        let cases = [
            (
                WorkGraphEventPayload::ResponsibilityAssigned(ResponsibilityAssignedPayload {
                    responsibility_type: AssignedResponsibilityType::IssueValidation,
                    profile_ref: ProfileRef::new("issue-validator", BLOB).expect("valid"),
                    content_digest: digest(),
                }),
                "Issue validation assigned.",
            ),
            (
                WorkGraphEventPayload::ExecutionStarted(ExecutionStartedPayload {
                    execution_id: execution_id.clone(),
                    task_id: "task-1".to_string(),
                }),
                "Issue validation started.",
            ),
            (
                WorkGraphEventPayload::CompletedIssueValidation(CompletedIssueValidationPayload {
                    execution_id,
                    outcome: ValidationOutcome::Failed,
                    reason_code: ValidationReasonCode::RequiredMarkerMissing,
                }),
                "Issue validation failed.",
            ),
            (
                WorkGraphEventPayload::RoutingDecided(RoutingDecidedPayload::for_outcome(
                    ValidationOutcome::Passed,
                )),
                "Issue routed to risk profiling.",
            ),
            (
                WorkGraphEventPayload::RoutingDecided(RoutingDecidedPayload::for_outcome(
                    ValidationOutcome::Failed,
                )),
                "Issue routed for correction.",
            ),
        ];

        for (payload, expected) in cases {
            let summary = summary_for(&event(payload));
            assert_eq!(summary, expected);
            validate_summary(&summary).expect("generated summaries are always renderable");
        }
    }
}

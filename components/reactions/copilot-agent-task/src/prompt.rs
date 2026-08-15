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

//! The coding-agent prompt.
//!
//! The prompt hands the reporter **exactly two** values — `subjectNumber` and
//! `executionId` — and instructs it to call `workgraph/report_completion`
//! exactly once with only those two arguments. Every other correlation value
//! (`runId`, `projectItemNodeId`, `subjectNodeId`, `eventId`) is derived by the
//! reporter from the trusted WorkGraph comments on the issue, never from the
//! prompt, so a tampered prompt cannot redirect or forge a completion event.
//!
//! The prompt is data handed to an external agent; it must **never** be logged.

/// Build the coding-agent prompt for one execution.
///
/// `execution_id` is the stable `execution:<runId>` identifier for this run (see
/// [`crate::ids::execution_id`]); embedding it lets the reconciliation seam find
/// a task whose creation response was lost.
pub fn build_prompt(subject_number: u64, execution_id: &str) -> String {
    format!(
        "You are validating GitHub issue #{subject_number} in this repository.\n\
         \n\
         When you have finished, call the `workgraph/report_completion` tool exactly once, \
         with exactly these two arguments and no others:\n\
         - subjectNumber: {subject_number}\n\
         - executionId: {execution_id}\n\
         \n\
         Do not pass any other correlation values. The reporter derives everything else it needs \
         (runId, projectItemNodeId, subjectNodeId, eventId) from the trusted WorkGraph comments on \
         the issue, not from this prompt. Do not open a pull request.\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXECUTION_ID: &str = "execution:validation:PVTI_item:sha256:\
        aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    #[test]
    fn prompt_carries_only_the_two_correlation_inputs() {
        let prompt = build_prompt(742, EXECUTION_ID);
        assert!(prompt.contains("subjectNumber"), "must name subjectNumber");
        assert!(prompt.contains("executionId"), "must name executionId");
        assert!(
            prompt.contains("742"),
            "must carry the subject number value"
        );
        assert!(
            prompt.contains(EXECUTION_ID),
            "must carry the execution id value"
        );
        assert!(
            prompt.contains("workgraph/report_completion"),
            "must instruct the single report_completion call"
        );
        assert!(
            prompt.contains("exactly once"),
            "must require exactly one report"
        );
    }

    #[test]
    fn prompt_omits_every_removed_field() {
        let prompt = build_prompt(742, EXECUTION_ID);
        for forbidden in [
            "routeId",
            "responsibilityId",
            "contentVersion",
            "profileRef",
            "expectedEventId",
            "AwaitingRouting",
            "WorkGraphEvent/v1",
            "requiredEventType",
            "actualModel",
            "requestedModel",
        ] {
            assert!(
                !prompt.contains(forbidden),
                "prompt must not mention '{forbidden}'"
            );
        }
    }
}
